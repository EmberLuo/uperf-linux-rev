//! Aggregated Linux environment and JSON probe report.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uperf_core::{CpuSet, DeviceCapabilities, ProcessId, ProcessInfo};
use uperf_platform::{
    Clock, CpuTimeSnapshot, OnlineCpuSource, PlatformError, PlatformResult, ProcReader, SysfsIo,
    ThermalSample,
};

use crate::{
    FrequencyTargetPaths, LinuxClock, LinuxDiscovery, LinuxProc, RootedSysfs,
    discovery::{
        discover, discover_device_identity, discover_recovery_targets, read_thermal_samples,
        read_thermal_samples_at,
    },
    scheduler::parse_cpu_list,
};

/// Physical roots used by Linux adapters.
///
/// Supplying roots explicitly is what makes production discovery reusable in
/// unprivileged fixture tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemRoots {
    pub sys: PathBuf,
    pub proc: PathBuf,
    pub etc: PathBuf,
}

impl SystemRoots {
    /// Host Linux roots.
    #[must_use]
    pub fn host() -> Self {
        Self {
            sys: PathBuf::from("/sys"),
            proc: PathBuf::from("/proc"),
            etc: PathBuf::from("/etc"),
        }
    }

    /// Roots below a self-contained test fixture directory.
    #[must_use]
    pub fn below(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            sys: root.join("sys"),
            proc: root.join("proc"),
            etc: root.join("etc"),
        }
    }
}

/// Read-mostly collection of Linux platform adapters.
#[derive(Clone, Debug)]
pub struct LinuxEnvironment {
    roots: SystemRoots,
    clock: LinuxClock,
    sysfs: RootedSysfs,
    procfs: LinuxProc,
}

impl LinuxEnvironment {
    /// Open the real host environment.  The contained sysfs adapter is
    /// deliberately read-only.
    ///
    /// # Errors
    ///
    /// Returns an error if `/sys` or `/proc` cannot be opened safely.
    pub fn host() -> PlatformResult<Self> {
        Self::new(SystemRoots::host())
    }

    /// Open host-shaped fixture roots.
    ///
    /// # Errors
    ///
    /// Returns an error if a supplied root does not exist or cannot be
    /// canonicalized.
    pub fn new(roots: SystemRoots) -> PlatformResult<Self> {
        let clock = LinuxClock::default();
        let sysfs = RootedSysfs::read_only(&roots.sys)?;
        let procfs = LinuxProc::new(&roots.proc, clock.clone())?;
        Ok(Self {
            roots,
            clock,
            sysfs,
            procfs,
        })
    }

    /// Physical roots in use.
    #[must_use]
    pub fn roots(&self) -> &SystemRoots {
        &self.roots
    }

    /// Read-only sysfs adapter.  Calling `write_string` on it always fails.
    #[must_use]
    pub fn sysfs(&self) -> &RootedSysfs {
        &self.sysfs
    }

    /// Read the current CPU-online mask without relying on the startup
    /// discovery snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the kernel node is unavailable or malformed.
    pub fn online_cpus(&self) -> PlatformResult<CpuSet> {
        let path = Path::new("/sys/devices/system/cpu/online");
        let contents = self.sysfs.read_string(path)?;
        parse_cpu_list(path, &contents)
    }

    /// Discover capabilities and the private logical-target registry.
    ///
    /// # Errors
    ///
    /// Returns an error if a discovery directory cannot be enumerated.
    pub fn discover(&self) -> PlatformResult<LinuxDiscovery> {
        discover(&self.roots.sys, &self.sysfs, &self.clock)
    }

    /// Read only immutable board identity used to bind a recovery journal.
    ///
    /// This does not enumerate frequency, thermal, or input resources and is
    /// therefore safe to perform before crash recovery.
    #[must_use]
    pub fn discover_device_identity(&self) -> LinuxDiscovery {
        discover_device_identity(&self.roots.sys)
    }

    /// Discover only CPU/devfreq identities needed to validate journal-owned
    /// frequency targets during pre-configuration recovery.
    ///
    /// Full thermal and input capability discovery deliberately happens only
    /// after recovery has completed or failed closed.
    ///
    /// # Errors
    ///
    /// Returns an error when frequency target roots cannot be enumerated.
    pub fn discover_recovery_targets(&self) -> PlatformResult<LinuxDiscovery> {
        discover_recovery_targets(&self.roots.sys, &self.sysfs)
    }

    /// Read only the exact thermal-zone paths selected by trusted device
    /// configuration.
    ///
    /// Missing or replaced nodes are returned as unavailable samples so the
    /// runtime can enter its sensor-failure envelope.
    #[must_use]
    pub fn read_thermal_paths(&self, paths: &[PathBuf]) -> Vec<ThermalSample> {
        read_thermal_samples_at(&self.sysfs, &self.clock, paths)
    }

    /// Build an exact-allowlist writer for validated, selected resources.
    ///
    /// Merely calling this method performs no writes.
    ///
    /// # Errors
    ///
    /// Returns an error if any discovered attribute cannot be canonicalized
    /// beneath the configured sysfs root.
    pub fn open_actuator_sysfs<'a>(
        &self,
        targets: impl IntoIterator<Item = &'a FrequencyTargetPaths>,
    ) -> PlatformResult<RootedSysfs> {
        let allowed = targets
            .into_iter()
            .flat_map(|target| [&target.minimum, &target.maximum]);
        RootedSysfs::with_write_allowlist(&self.roots.sys, allowed)
    }

    /// Build an exact-allowlist writer from paths in a validated durable
    /// recovery manifest.
    ///
    /// This deliberately does not require current device configuration.
    /// Canonicalization still confines every path beneath this environment's
    /// sysfs root, and an empty slice creates a read-only writer.
    ///
    /// # Errors
    ///
    /// Returns an error if a path is missing, is not a normalized logical
    /// `/sys/...` path, or resolves outside the configured sysfs root.
    pub fn open_recovery_sysfs(&self, allowed_paths: &[PathBuf]) -> PlatformResult<RootedSysfs> {
        RootedSysfs::with_write_allowlist(&self.roots.sys, allowed_paths)
    }

    /// Produce a privacy-preserving, read-only JSON-ready capability report.
    ///
    /// # Errors
    ///
    /// Returns an error if hardware, CPU time, thermal, or system metadata
    /// observations fail.
    pub fn probe(&self) -> PlatformResult<ProbeReport> {
        let discovery = self.discover()?;
        let cpu_times = self.cpu_times()?;
        let thermal = read_thermal_samples(&self.roots.sys, &self.sysfs, &self.clock)?;
        let system = read_system_info(&self.roots)?;
        Ok(ProbeReport {
            schema_version: 1,
            system,
            capabilities: discovery.capabilities,
            cpu_times,
            thermal,
            warnings: discovery.warnings,
        })
    }
}

impl Clock for LinuxEnvironment {
    fn monotonic_millis(&self) -> uperf_core::MonotonicMillis {
        self.clock.monotonic_millis()
    }
}

impl ProcReader for LinuxEnvironment {
    fn cpu_times(&self) -> PlatformResult<CpuTimeSnapshot> {
        self.procfs.cpu_times()
    }

    fn list_threads(&self, process: ProcessId) -> PlatformResult<Vec<ProcessId>> {
        self.procfs.list_threads(process)
    }

    fn process_identity(&self, pid: ProcessId) -> PlatformResult<ProcessInfo> {
        self.procfs.process_identity(pid)
    }
}

impl OnlineCpuSource for LinuxEnvironment {
    fn online_cpus(&self) -> PlatformResult<CpuSet> {
        LinuxEnvironment::online_cpus(self)
    }
}

/// Stable top-level output emitted by `uperf-probe`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeReport {
    pub schema_version: u32,
    pub system: SystemInfo,
    pub capabilities: DeviceCapabilities,
    pub cpu_times: CpuTimeSnapshot,
    pub thermal: Vec<ThermalSample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Non-sensitive operating-system metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemInfo {
    pub architecture: String,
    pub kernel_release: Option<String>,
    pub os_id: Option<String>,
    pub os_pretty_name: Option<String>,
    pub boot_id: Option<String>,
}

fn read_system_info(roots: &SystemRoots) -> PlatformResult<SystemInfo> {
    let kernel_release = read_optional_trimmed(roots.proc.join("sys/kernel/osrelease"))?;
    let boot_id = read_optional_trimmed(roots.proc.join("sys/kernel/random/boot_id"))?;
    let os_release = read_optional_trimmed(roots.etc.join("os-release"))?;
    let parsed = os_release
        .as_deref()
        .map(parse_os_release)
        .unwrap_or_default();
    Ok(SystemInfo {
        architecture: std::env::consts::ARCH.to_owned(),
        kernel_release,
        os_id: parsed.0,
        os_pretty_name: parsed.1,
        boot_id,
    })
}

fn read_optional_trimmed(path: PathBuf) -> PlatformResult<Option<String>> {
    match fs::read_to_string(&path) {
        Ok(value) => Ok(Some(value.trim().to_owned()).filter(|value| !value.is_empty())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(None),
        Err(error) => Err(PlatformError::io("read system metadata", path, error)),
    }
}

fn parse_os_release(contents: &str) -> (Option<String>, Option<String>) {
    let mut id = None;
    let mut pretty = None;
    for line in contents.lines() {
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote_os_release(raw_value.trim());
        match key {
            "ID" => id = Some(value),
            "PRETTY_NAME" => pretty = Some(value),
            _ => {}
        }
    }
    (id, pretty)
}

fn unquote_os_release(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use uperf_platform::SysfsIo;

    use super::*;

    #[test]
    fn parses_selected_os_release_fields() {
        assert_eq!(
            parse_os_release("ID=ubuntu\nPRETTY_NAME=\"Ubuntu 26.04\"\nX=ignored\n"),
            (Some("ubuntu".to_owned()), Some("Ubuntu 26.04".to_owned()))
        );
    }

    #[test]
    fn recovery_writer_uses_only_the_exact_manifest_allowlist() {
        let temporary = tempdir().expect("temporary root");
        for directory in ["sys/devices/test", "proc", "etc"] {
            fs::create_dir_all(temporary.path().join(directory)).expect("fixture directory");
        }
        let physical_minimum = temporary.path().join("sys/devices/test/min");
        let physical_maximum = temporary.path().join("sys/devices/test/max");
        fs::write(&physical_minimum, "1000\n").expect("minimum");
        fs::write(&physical_maximum, "3000\n").expect("maximum");
        let environment =
            LinuxEnvironment::new(SystemRoots::below(temporary.path())).expect("environment");
        let logical_minimum = PathBuf::from("/sys/devices/test/min");
        let logical_maximum = PathBuf::from("/sys/devices/test/max");

        let writer = environment
            .open_recovery_sysfs(std::slice::from_ref(&logical_minimum))
            .expect("recovery writer");
        writer
            .write_string(&logical_minimum, "2000")
            .expect("allowlisted write");
        assert!(matches!(
            writer.write_string(&logical_maximum, "2500"),
            Err(PlatformError::AccessDenied { .. })
        ));
        assert_eq!(
            fs::read_to_string(physical_minimum).expect("read minimum"),
            "2000"
        );
        assert_eq!(
            fs::read_to_string(physical_maximum).expect("read maximum"),
            "3000\n"
        );
    }
}
