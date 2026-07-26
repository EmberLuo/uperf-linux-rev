//! D-Bus-backed system observers.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use futures_util::StreamExt;
use tokio::{sync::watch, task::JoinHandle};
use uperf_linux::{EvdevInputSource, GestureConfig};
use zbus::{
    Connection, Proxy,
    fdo::DBusProxy,
    zvariant::{OwnedFd, OwnedObjectPath},
};

use crate::runtime::{ObserverIngress, RuntimeHandle, SessionState};

const INHIBITOR_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESTORE_TIMEOUT: Duration = Duration::from_secs(10);
const RESTORE_DEADLINE_MARGIN: Duration = Duration::from_millis(250);
const SESSION_STATE_INTERVAL: Duration = Duration::from_secs(1);
type LogindSessionRow = (String, u32, String, String, OwnedObjectPath);

#[zbus::proxy(
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    interface = "org.freedesktop.login1.Manager",
    gen_blocking = false
)]
trait LogindManager {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    fn list_sessions(&self) -> zbus::Result<Vec<LogindSessionRow>>;

    #[zbus(property, name = "InhibitDelayMaxUSec")]
    fn inhibit_delay_max_usec(&self) -> zbus::Result<u64>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, sleeping: bool) -> zbus::Result<()>;
}

/// Subscribe to logind's suspend/resume barrier.
///
/// Failure is returned from the task and can be surfaced as daemon health;
/// it never disables independent load or thermal observation.
#[must_use]
pub fn spawn_logind_observer(
    connection: Connection,
    ingress: ObserverIngress,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let health = ingress.clone();
        let result = run_logind_observer(connection, ingress, &mut shutdown).await;
        if let Err(error) = &result {
            health.report_logind_health(Err(error.clone()));
        }
        result
    })
}

/// Watch for the disappearance of D-Bus peers holding a focus lease.
///
/// A compositor extension that crashes never sends `ClearForegroundProcess`,
/// so without this the lease would linger until its TTU expires. Reference
/// implementations poll `kill(pid, 0)` instead, which is both slower and
/// vulnerable to PID reuse.
#[must_use]
pub fn spawn_focus_peer_watcher(
    connection: Connection,
    runtime: RuntimeHandle,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<Result<(), String>> {
    tokio::spawn(async move { run_focus_peer_watcher(connection, runtime, &mut shutdown).await })
}

async fn run_focus_peer_watcher(
    connection: Connection,
    runtime: RuntimeHandle,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let proxy = DBusProxy::new(&connection)
        .await
        .map_err(|error| format!("connect to the bus driver: {error}"))?;
    let mut owner_changes = proxy
        .receive_name_owner_changed()
        .await
        .map_err(|error| format!("subscribe to NameOwnerChanged: {error}"))?;
    loop {
        tokio::select! {
            owner_change = owner_changes.next() => {
                let owner_change = owner_change
                    .ok_or_else(|| "NameOwnerChanged stream ended".to_owned())?;
                let arguments = owner_change.args().map_err(|error| error.to_string())?;
                // Only a vanished unique name matters: a lease is keyed by the
                // reporter's unique bus name, never by a well-known alias.
                if arguments.new_owner().is_none()
                    && let Some(old) = arguments.old_owner().as_ref()
                {
                    let _ = runtime.forget_foreground_peer(old.to_string()).await;
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_logind_observer(
    connection: Connection,
    ingress: ObserverIngress,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let proxy = LogindManagerProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    let restore_timeout = restore_timeout(
        proxy
            .inhibit_delay_max_usec()
            .await
            .map_err(|error| format!("read logind delay-inhibitor deadline: {error}"))?,
    )?;
    let mut inhibitor = Some(acquire_sleep_inhibitor(&proxy).await?);
    let mut signals = proxy
        .receive_prepare_for_sleep()
        .await
        .map_err(|error| error.to_string())?;
    let mut session_poll = tokio::time::interval(SESSION_STATE_INTERVAL);
    session_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_session_state = None;
    ingress.report_logind_health(Ok(()));
    loop {
        tokio::select! {
            signal = signals.next() => {
                let signal = signal
                    .ok_or_else(|| "logind PrepareForSleep stream ended".to_owned())?;
                let arguments = signal.args().map_err(|error| error.to_string())?;
                if *arguments.sleeping() {
                    let held_inhibitor = inhibitor.take();
                    if held_inhibitor.is_none() {
                        ingress.report_logind_health(Err(
                            "received PrepareForSleep without a held delay inhibitor".to_owned(),
                        ));
                    }
                    let result = ingress.prepare_for_sleep(true, restore_timeout).await;
                    // Closing the descriptor is the acknowledgement to logind.
                    // It must happen after the reducer has verified restoration,
                    // or at the bounded deadline when waiting any longer cannot
                    // extend logind's delay.
                    drop(held_inhibitor);
                    if let Err(error) = result {
                        ingress.report_logind_health(Err(format!(
                            "pre-sleep safety barrier failed: {error}"
                        )));
                    }
                } else {
                    match acquire_sleep_inhibitor(&proxy).await {
                        Ok(descriptor) => {
                            inhibitor = Some(descriptor);
                            ingress.report_logind_health(Ok(()));
                        }
                        Err(error) => {
                            inhibitor = None;
                            ingress.report_logind_health(Err(error));
                        }
                    }
                    if let Err(error) = ingress.prepare_for_sleep(false, restore_timeout).await {
                        ingress.report_logind_health(Err(format!(
                            "post-wake state transition failed: {error}"
                        )));
                    }
                }
            }
            _ = session_poll.tick() => {
                match active_local_session_state(&connection, &proxy).await {
                    Ok(Some(state)) => {
                        if last_session_state != Some(state) {
                            ingress.report_session_state(state).await?;
                            last_session_state = Some(state);
                        }
                    }
                    Ok(None) => {
                        last_session_state = None;
                    }
                    Err(error) => {
                        ingress.report_logind_health(Err(format!(
                            "observe logind IdleHint/LockedHint: {error}"
                        )));
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

async fn active_local_session_state(
    connection: &Connection,
    manager: &LogindManagerProxy<'_>,
) -> Result<Option<SessionState>, String> {
    let sessions = manager
        .list_sessions()
        .await
        .map_err(|error| format!("list sessions: {error}"))?;
    let mut found = false;
    let mut any_locked = false;
    let mut any_idle = false;
    for (_, _, _, _, path) in sessions {
        let session = Proxy::new(
            connection,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )
        .await
        .map_err(|error| format!("connect to session properties: {error}"))?;
        let active: bool = session
            .get_property("Active")
            .await
            .map_err(|error| format!("read session Active: {error}"))?;
        let remote: bool = session
            .get_property("Remote")
            .await
            .map_err(|error| format!("read session Remote: {error}"))?;
        if !active || remote {
            continue;
        }
        found = true;
        let locked: bool = session
            .get_property("LockedHint")
            .await
            .map_err(|error| format!("read session LockedHint: {error}"))?;
        let idle: bool = session
            .get_property("IdleHint")
            .await
            .map_err(|error| format!("read session IdleHint: {error}"))?;
        if !locked && !idle {
            return Ok(Some(SessionState::Active));
        }
        any_locked |= locked;
        any_idle |= idle;
    }
    Ok(if any_locked {
        Some(SessionState::Locked)
    } else if any_idle {
        Some(SessionState::Idle)
    } else if found {
        Some(SessionState::Active)
    } else {
        None
    })
}

async fn acquire_sleep_inhibitor(proxy: &LogindManagerProxy<'_>) -> Result<OwnedFd, String> {
    tokio::time::timeout(
        INHIBITOR_ACQUIRE_TIMEOUT,
        proxy.inhibit(
            "sleep",
            "uperf-linux",
            "restore managed performance resources before sleep",
            "delay",
        ),
    )
    .await
    .map_err(|_| "timed out acquiring logind sleep delay inhibitor".to_owned())?
    .map_err(|error| format!("acquire logind sleep delay inhibitor: {error}"))
}

fn restore_timeout(inhibit_delay_max_usec: u64) -> Result<Duration, String> {
    if inhibit_delay_max_usec == 0 {
        return Err("logind reported a zero delay-inhibitor deadline".to_owned());
    }
    let maximum = Duration::from_micros(inhibit_delay_max_usec);
    let margin = RESTORE_DEADLINE_MARGIN.min(maximum / 5);
    Ok(maximum
        .saturating_sub(margin)
        .min(MAX_RESTORE_TIMEOUT)
        .max(Duration::from_millis(1)))
}

/// Dedicated blocking evdev reader with bounded cooperative shutdown.
pub struct InputObserverTask {
    cancelled: Arc<AtomicBool>,
    join: thread::JoinHandle<()>,
}

impl InputObserverTask {
    pub async fn stop(self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = tokio::task::spawn_blocking(move || self.join.join()).await;
    }
}

/// Start evdev observation when input policy is enabled.
///
/// # Errors
///
/// Returns an error for invalid normalized gesture thresholds or if the
/// operating-system thread cannot be created.
pub fn spawn_input_observer(ingress: ObserverIngress) -> Result<Option<InputObserverTask>, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let thread_cancelled = cancelled.clone();
    let mut settings = ingress.settings();
    let join = thread::Builder::new()
        .name("uperf-evdev".to_owned())
        .spawn(move || {
            while !thread_cancelled.load(Ordering::Acquire) {
                let configuration = settings.borrow_and_update().input.clone();
                if !configuration.enabled {
                    ingress.report_input_health(Ok(()));
                    wait_for_input_change(&thread_cancelled, &settings, Duration::from_millis(100));
                    continue;
                }
                let gesture = match GestureConfig::from_input_config(&configuration) {
                    Ok(gesture) => gesture,
                    Err(error) => {
                        ingress.report_input_health(Err(error.to_string()));
                        wait_for_input_change(&thread_cancelled, &settings, Duration::from_secs(1));
                        continue;
                    }
                };
                match EvdevInputSource::host(gesture) {
                    Ok(mut source) => {
                        report_input_source_health(&ingress, &source);
                        loop {
                            let outcome = source
                                .next_event_until_interrupt(&thread_cancelled, || {
                                    settings.has_changed().unwrap_or(true)
                                });
                            report_input_source_health(&ingress, &source);
                            match outcome {
                                Ok(Some(event)) => {
                                    if !ingress.send_observed_input(event, &thread_cancelled) {
                                        return;
                                    }
                                }
                                Ok(None) if thread_cancelled.load(Ordering::Acquire) => return,
                                Ok(None) => {
                                    if !ingress.send_input_with_backpressure(
                                        uperf_platform::InputEvent::Resync { device: None },
                                        &thread_cancelled,
                                    ) {
                                        return;
                                    }
                                    break;
                                }
                                Err(error) => {
                                    if !ingress.send_input_with_backpressure(
                                        uperf_platform::InputEvent::Resync { device: None },
                                        &thread_cancelled,
                                    ) {
                                        return;
                                    }
                                    ingress.report_input_health(Err(error.to_string()));
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        ingress.report_input_health(Err(error.to_string()));
                    }
                }
                if !thread_cancelled.load(Ordering::Acquire) {
                    wait_for_input_change(&thread_cancelled, &settings, Duration::from_secs(1));
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(Some(InputObserverTask { cancelled, join }))
}

fn report_input_source_health(ingress: &ObserverIngress, source: &EvdevInputSource) {
    if source.device_count() == 0 {
        let detail = source.device_errors().iter().next().map_or_else(
            || {
                "no supported keyboard, relative-pointer, or type-B multitouch device is available"
                    .to_owned()
            },
            |(path, error)| format!("cannot use {}: {error}", path.display()),
        );
        ingress.report_input_health(Err(detail));
    } else if let Some((path, error)) = source.device_errors().iter().next() {
        ingress.report_input_health(Err(format!(
            "input observation is partially degraded; {}: {error}",
            path.display()
        )));
    } else {
        ingress.report_input_health(Ok(()));
    }
}

fn wait_for_input_change(
    cancelled: &AtomicBool,
    settings: &watch::Receiver<crate::runtime::ObserverSettings>,
    maximum: Duration,
) {
    let started = std::time::Instant::now();
    while !cancelled.load(Ordering::Acquire)
        && !settings.has_changed().unwrap_or(true)
        && started.elapsed() < maximum
    {
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_deadline_leaves_margin_for_logind() {
        assert_eq!(
            restore_timeout(5_000_000).unwrap(),
            Duration::from_millis(4_750)
        );
        assert_eq!(restore_timeout(100_000).unwrap(), Duration::from_millis(80));
    }

    #[test]
    fn restore_deadline_is_bounded_and_rejects_zero() {
        assert_eq!(restore_timeout(60_000_000).unwrap(), MAX_RESTORE_TIMEOUT);
        assert!(restore_timeout(0).is_err());
    }
}
