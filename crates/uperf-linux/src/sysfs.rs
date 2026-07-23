//! Root-confined sysfs text access.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use uperf_platform::{PlatformError, PlatformResult, SysfsIo};

const MAX_ATTRIBUTE_BYTES: u64 = 64 * 1024;
const MAX_WRITE_BYTES: usize = 4096;

/// A sysfs adapter confined to one physical root.
///
/// Instances are read-only unless constructed with an exact write allowlist.
/// Both the root and every target are canonicalized, preventing `..` and
/// symlink escapes from a fixture or the host `/sys` tree.
#[derive(Clone, Debug)]
pub struct RootedSysfs {
    root: PathBuf,
    writable: BTreeSet<PathBuf>,
    read_paths: Arc<Mutex<BTreeMap<PathBuf, PathBuf>>>,
}

impl RootedSysfs {
    /// Open a read-only adapter rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if `root` does not exist or cannot be canonicalized.
    pub fn read_only(root: impl AsRef<Path>) -> PlatformResult<Self> {
        let requested = root.as_ref();
        let root = requested
            .canonicalize()
            .map_err(|error| PlatformError::io("canonicalize sysfs root", requested, error))?;
        Ok(Self {
            root,
            writable: BTreeSet::new(),
            read_paths: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Open an adapter that may write only the listed logical `/sys/...` files.
    ///
    /// This is intended for the actuator after it has resolved configuration
    /// target IDs through discovered capabilities.  Probe code must use
    /// [`Self::read_only`].
    ///
    /// # Errors
    ///
    /// Returns an error if the root or any allowlisted target cannot be
    /// canonicalized safely beneath that root.
    pub fn with_write_allowlist<I, P>(
        root: impl AsRef<Path>,
        allowed_paths: I,
    ) -> PlatformResult<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut adapter = Self::read_only(root)?;
        for path in allowed_paths {
            let resolved = adapter.resolve_existing(path.as_ref())?;
            adapter.writable.insert(resolved);
        }
        Ok(adapter)
    }

    /// Physical root used by this adapter.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Map a logical `/sys/...` path into this adapter's physical root.
    ///
    /// The returned path is canonical and must already exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is not a logical `/sys/...` path, does
    /// not exist, or resolves outside this adapter's root.
    pub fn resolve_existing(&self, logical: &Path) -> PlatformResult<PathBuf> {
        let relative = logical_relative(logical)?;
        let candidate = self.root.join(relative);
        let resolved = candidate
            .canonicalize()
            .map_err(|error| PlatformError::io("canonicalize sysfs path", logical, error))?;
        if !resolved.starts_with(&self.root) {
            return Err(PlatformError::AccessDenied {
                path: logical.to_path_buf(),
                reason: "canonical target escapes the configured sysfs root".to_owned(),
            });
        }
        Ok(resolved)
    }

    fn open_for_read(&self, logical: &Path) -> PlatformResult<File> {
        let resolved = self.resolve_read_path(logical)?;
        match File::open(&resolved) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.invalidate_read_path(logical, &resolved);
                let refreshed = self.resolve_read_path(logical)?;
                File::open(&refreshed)
                    .map_err(|error| PlatformError::io("open for read", logical, error))
            }
            Err(error) => Err(PlatformError::io("open for read", logical, error)),
        }
    }

    fn resolve_read_path(&self, logical: &Path) -> PlatformResult<PathBuf> {
        // Validate on every call even when a cache entry exists.  This keeps
        // the public boundary fail-closed for paths containing `..` or a
        // non-/sys prefix while avoiding repeated canonicalize(2) calls for
        // trusted attributes sampled several times per second.
        logical_relative(logical)?;
        if let Some(resolved) = self
            .read_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(logical)
            .cloned()
        {
            return Ok(resolved);
        }

        let resolved = self.resolve_existing(logical)?;
        self.read_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(logical.to_path_buf(), resolved.clone());
        Ok(resolved)
    }

    fn invalidate_read_path(&self, logical: &Path, expected: &Path) {
        let mut paths = self
            .read_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if paths.get(logical).is_some_and(|cached| cached == expected) {
            paths.remove(logical);
        }
    }
}

impl SysfsIo for RootedSysfs {
    fn read_string(&self, path: &Path) -> PlatformResult<String> {
        let file = self.open_for_read(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_ATTRIBUTE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| PlatformError::io("read", path, error))?;
        if bytes.len() as u64 > MAX_ATTRIBUTE_BYTES {
            return Err(PlatformError::invalid(
                path,
                format!("attribute exceeds {MAX_ATTRIBUTE_BYTES} bytes"),
            ));
        }
        let value = String::from_utf8(bytes)
            .map_err(|error| PlatformError::invalid(path, format!("not UTF-8: {error}")))?;
        Ok(value.trim().to_owned())
    }

    fn write_string(&self, path: &Path, value: &str) -> PlatformResult<()> {
        if value.len() > MAX_WRITE_BYTES {
            return Err(PlatformError::invalid(
                path,
                format!("write exceeds {MAX_WRITE_BYTES} bytes"),
            ));
        }
        if value.as_bytes().contains(&0) {
            return Err(PlatformError::invalid(path, "write contains a NUL byte"));
        }

        let resolved = self.resolve_existing(path)?;
        if !self.writable.contains(&resolved) {
            return Err(PlatformError::AccessDenied {
                path: path.to_path_buf(),
                reason: "adapter is read-only or target is not allowlisted".to_owned(),
            });
        }

        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&resolved)
            .map_err(|error| PlatformError::io("open for write", path, error))?;
        file.write_all(value.as_bytes())
            .map_err(|error| PlatformError::io("write", path, error))?;
        file.flush()
            .map_err(|error| PlatformError::io("flush", path, error))
    }
}

fn logical_relative(logical: &Path) -> PlatformResult<PathBuf> {
    let relative = logical
        .strip_prefix("/sys")
        .map_err(|_| PlatformError::AccessDenied {
            path: logical.to_path_buf(),
            reason: "sysfs paths must be absolute and begin with /sys".to_owned(),
        })?;
    if relative.as_os_str().is_empty() {
        return Ok(PathBuf::new());
    }

    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            _ => {
                return Err(PlatformError::AccessDenied {
                    path: logical.to_path_buf(),
                    reason: "non-normal path component".to_owned(),
                });
            }
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn read_trims_and_write_requires_exact_allowlist() {
        let temporary = tempdir().unwrap();
        let sys = temporary.path().join("sys");
        let target = sys.join("devices/policy0/scaling_min_freq");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "300000\n").unwrap();

        let read_only = RootedSysfs::read_only(&sys).unwrap();
        let logical = Path::new("/sys/devices/policy0/scaling_min_freq");
        assert_eq!(read_only.read_string(logical).unwrap(), "300000");
        assert!(matches!(
            read_only.write_string(logical, "400000"),
            Err(PlatformError::AccessDenied { .. })
        ));

        let writable = RootedSysfs::with_write_allowlist(&sys, [logical]).unwrap();
        writable.write_string(logical, "400000").unwrap();
        assert_eq!(fs::read_to_string(target).unwrap(), "400000");
    }

    #[test]
    fn rejects_paths_outside_logical_sysfs() {
        let temporary = tempdir().unwrap();
        let sys = temporary.path().join("sys");
        fs::create_dir_all(&sys).unwrap();
        let adapter = RootedSysfs::read_only(sys).unwrap();

        assert!(matches!(
            adapter.read_string(Path::new("/etc/passwd")),
            Err(PlatformError::AccessDenied { .. })
        ));
        assert!(matches!(
            adapter.read_string(Path::new("/sys/../etc/passwd")),
            Err(PlatformError::AccessDenied { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refreshes_a_cached_path_after_the_target_disappears() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let sys = temporary.path().join("sys");
        let first = sys.join("devices/policy0/scaling_cur_freq");
        let second = sys.join("devices/policy1/scaling_cur_freq");
        let class = sys.join("class/cpufreq");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::create_dir_all(&class).unwrap();
        fs::write(&first, "300000\n").unwrap();
        fs::write(&second, "400000\n").unwrap();
        symlink("../../devices/policy0", class.join("policy")).unwrap();

        let adapter = RootedSysfs::read_only(&sys).unwrap();
        let logical = Path::new("/sys/class/cpufreq/policy/scaling_cur_freq");
        assert_eq!(adapter.read_string(logical).unwrap(), "300000");

        fs::remove_dir_all(sys.join("devices/policy0")).unwrap();
        fs::remove_file(class.join("policy")).unwrap();
        symlink("../../devices/policy1", class.join("policy")).unwrap();
        assert_eq!(adapter.read_string(logical).unwrap(), "400000");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let sys = temporary.path().join("sys");
        fs::create_dir_all(&sys).unwrap();
        let outside = temporary.path().join("secret");
        fs::write(&outside, "secret").unwrap();
        symlink(&outside, sys.join("escape")).unwrap();

        let adapter = RootedSysfs::read_only(sys).unwrap();
        assert!(matches!(
            adapter.read_string(Path::new("/sys/escape")),
            Err(PlatformError::AccessDenied { .. })
        ));
    }
}
