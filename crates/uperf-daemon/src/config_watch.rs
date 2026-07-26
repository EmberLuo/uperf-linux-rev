//! Inotify-backed transactional configuration reload trigger.
//!
//! Parent directories are watched rather than the files themselves so editors
//! that save with `rename(2)` cannot silently detach the watch.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    future::Future,
    io,
    mem::MaybeUninit,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use rustix::{
    fd::OwnedFd,
    fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags},
    io::Errno,
};
use tokio::{
    io::unix::AsyncFd,
    sync::watch,
    task::JoinHandle,
    time::{Instant, sleep_until},
};

use crate::{config::ConfigurationPaths, runtime::RuntimeHandle};

const RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);
const EVENT_BUFFER_BYTES: usize = 16 * 1_024;

/// Prepared parent-directory watches for the three mutable configuration
/// documents.
pub struct ConfigWatcher {
    inotify: AsyncFd<OwnedFd>,
    names_by_watch: BTreeMap<i32, BTreeSet<OsString>>,
}

impl ConfigWatcher {
    /// Register watches without starting a task.
    ///
    /// # Errors
    ///
    /// Returns an error when a target has no filename, a parent directory is
    /// unavailable, inotify cannot be initialized, or Tokio cannot register
    /// the nonblocking descriptor.
    pub fn new(paths: &ConfigurationPaths) -> Result<Self> {
        Self::for_paths([&paths.device_override, &paths.policy, &paths.apps])
    }

    fn for_paths<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Result<Self> {
        let descriptor = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)
            .context("initialize configuration inotify descriptor")?;
        let mut targets_by_parent = BTreeMap::<PathBuf, BTreeSet<OsString>>::new();
        for path in paths {
            let parent = path
                .parent()
                .filter(|value| !value.as_os_str().is_empty())
                .with_context(|| format!("configuration path {} has no parent", path.display()))?;
            let name = path.file_name().with_context(|| {
                format!("configuration path {} has no filename", path.display())
            })?;
            targets_by_parent
                .entry(parent.to_path_buf())
                .or_default()
                .insert(name.to_os_string());
        }

        let flags = WatchFlags::CLOSE_WRITE
            | WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ONLYDIR;
        let mut names_by_watch = BTreeMap::<i32, BTreeSet<OsString>>::new();
        for (parent, names) in targets_by_parent {
            let watch = inotify::add_watch(&descriptor, &parent, flags)
                .with_context(|| format!("watch configuration directory {}", parent.display()))?;
            names_by_watch.entry(watch).or_default().extend(names);
        }
        let inotify = AsyncFd::new(descriptor).context("register configuration inotify fd")?;
        Ok(Self {
            inotify,
            names_by_watch,
        })
    }

    async fn wait_for_change(&self) -> Result<()> {
        loop {
            let mut readiness = self
                .inotify
                .readable()
                .await
                .context("wait for configuration inotify readiness")?;
            let changed = self.drain_events();
            // `drain_events` reads until EAGAIN, so clearing the edge-triggered
            // readiness here is correct even when it found a relevant event.
            readiness.clear_ready();
            if changed? {
                return Ok(());
            }
        }
    }

    fn drain_events(&self) -> io::Result<bool> {
        let mut buffer = [MaybeUninit::uninit(); EVENT_BUFFER_BYTES];
        let mut reader = inotify::Reader::new(self.inotify.get_ref(), &mut buffer);
        let mut relevant = false;
        loop {
            match reader.next() {
                Ok(event) => {
                    let events = event.events();
                    if events.contains(ReadFlags::QUEUE_OVERFLOW) {
                        // Lost events mean the safe response is to reload the
                        // complete generation, not guess which file changed.
                        relevant = true;
                        continue;
                    }
                    if events.intersects(ReadFlags::IGNORED | ReadFlags::UNMOUNT) {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "configuration directory watch was removed",
                        ));
                    }
                    let Some(watched_names) = self.names_by_watch.get(&event.wd()) else {
                        continue;
                    };
                    let Some(file_name) = event.file_name() else {
                        if events.intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF) {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "configuration directory was removed or replaced",
                            ));
                        }
                        continue;
                    };
                    if watched_names.contains(OsStr::from_bytes(file_name.to_bytes())) {
                        relevant = true;
                    }
                }
                Err(Errno::AGAIN) => return Ok(relevant),
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn wait_for_quiet_period(&self, shutdown: &mut watch::Receiver<bool>) -> Result<bool> {
        let quiet = sleep_until(Instant::now() + RELOAD_DEBOUNCE);
        tokio::pin!(quiet);
        loop {
            tokio::select! {
                result = self.wait_for_change() => {
                    result?;
                    quiet.as_mut().reset(Instant::now() + RELOAD_DEBOUNCE);
                }
                () = &mut quiet => return Ok(true),
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        return Ok(false);
                    }
                }
            }
        }
    }

    async fn run(self, runtime: RuntimeHandle, shutdown: watch::Receiver<bool>) -> Result<()> {
        self.run_with_reload(shutdown, move || {
            let runtime = runtime.clone();
            async move {
                match runtime.reload().await {
                    Ok(report) => tracing::info!(
                        source = "config-watch",
                        event = "reload-accepted",
                        generation = report.config_generation,
                        "configuration reload accepted"
                    ),
                    Err(error) => tracing::warn!(
                        source = "config-watch",
                        event = "reload-rejected",
                        old_generation_retained = true,
                        error = %error,
                        "configuration reload rejected"
                    ),
                }
            }
        })
        .await
    }

    async fn run_with_reload<F, Fut>(
        self,
        mut shutdown: watch::Receiver<bool>,
        mut reload: F,
    ) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        if *shutdown.borrow() {
            return Ok(());
        }
        loop {
            tokio::select! {
                result = self.wait_for_change() => result?,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        return Ok(());
                    }
                    continue;
                }
            }
            if !self.wait_for_quiet_period(&mut shutdown).await? {
                return Ok(());
            }
            reload().await;
        }
    }
}

/// Start a prepared watcher and log any terminal observer failure.
#[must_use]
pub fn spawn_config_watcher(
    watcher: ConfigWatcher,
    runtime: RuntimeHandle,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = watcher.run(runtime, shutdown).await {
            tracing::error!(
                source = "config-watch",
                event = "observer-failed",
                hot_reload_available = false,
                error = %format!("{error:#}"),
                "configuration watcher stopped"
            );
        }
    })
}

/// Await watcher shutdown and convert a panic/cancellation into a startup
/// orchestration error.
///
/// # Errors
///
/// Returns an error if the Tokio task panicked or was aborted.
pub async fn join_config_watcher(task: JoinHandle<()>) -> Result<()> {
    task.await.context("join configuration watcher")
}

#[cfg(test)]
mod tests {
    use std::{
        fs, future,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tempfile::tempdir;
    use tokio::{sync::watch, time::timeout};

    use super::{ConfigWatcher, RELOAD_DEBOUNCE};
    use crate::config::ConfigurationPaths;

    fn paths(root: &Path) -> ConfigurationPaths {
        let config = root.join("etc");
        let state = root.join("state");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(config.join("policy.json"), b"old").unwrap();
        ConfigurationPaths::below(config, state)
    }

    #[tokio::test]
    async fn parent_watch_observes_atomic_replacement() {
        let temporary = tempdir().unwrap();
        let paths = paths(temporary.path());
        let watcher = ConfigWatcher::new(&paths).unwrap();
        let replacement = paths.policy.with_extension("new");
        fs::write(&replacement, b"new").unwrap();
        fs::rename(replacement, &paths.policy).unwrap();
        timeout(Duration::from_secs(1), watcher.wait_for_change())
            .await
            .expect("watcher timed out")
            .expect("watcher failed");
    }

    #[tokio::test]
    async fn unrelated_files_do_not_trigger_reload() {
        let temporary = tempdir().unwrap();
        let paths = paths(temporary.path());
        let watcher = ConfigWatcher::new(&paths).unwrap();
        fs::write(
            paths.policy.parent().unwrap().join("unrelated.json"),
            b"new",
        )
        .unwrap();
        assert!(
            timeout(Duration::from_millis(75), watcher.wait_for_change())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rapid_replacements_coalesce_into_one_reload() {
        let temporary = tempdir().unwrap();
        let paths = paths(temporary.path());
        let watcher = ConfigWatcher::new(&paths).unwrap();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let reloads = Arc::new(AtomicUsize::new(0));
        let observed_reloads = Arc::clone(&reloads);
        let task = tokio::spawn(watcher.run_with_reload(shutdown_rx, move || {
            observed_reloads.fetch_add(1, Ordering::Relaxed);
            future::ready(())
        }));

        for suffix in ["first", "second"] {
            let replacement = paths.policy.with_extension(suffix);
            fs::write(&replacement, suffix).unwrap();
            fs::rename(replacement, &paths.policy).unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(RELOAD_DEBOUNCE + Duration::from_millis(100)).await;
        assert_eq!(reloads.load(Ordering::Relaxed), 1);
        shutdown.send_replace(true);
        task.await.unwrap().unwrap();
    }
}
