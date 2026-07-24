//! Operating-system ports used by the scheduler, observers and actuator.
//!
//! The traits in this crate deliberately expose logical observations and typed
//! mutations, rather than Linux implementation details.  Production adapters
//! live in `uperf-linux`; deterministic fakes live in `uperf-testkit`.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
pub use uperf_core::SchedulingClass;
use uperf_core::{CpuId, CpuSet, MonotonicMillis, ProcessId, ProcessInfo, ThermalReading};

/// Result type shared by all operating-system ports.
pub type PlatformResult<T> = Result<T, PlatformError>;

/// A failure while observing or mutating an operating-system resource.
#[derive(Debug, Error)]
pub enum PlatformError {
    /// A filesystem or operating-system call failed.
    #[error("{operation} failed for {path}: {source}")]
    Io {
        /// Short operation name suitable for diagnostics.
        operation: &'static str,
        /// Resource involved in the operation.
        path: PathBuf,
        /// Original I/O error.
        #[source]
        source: io::Error,
    },
    /// An operating-system value was present but malformed.
    #[error("invalid data in {path}: {message}")]
    InvalidData {
        /// Resource containing the malformed value.
        path: PathBuf,
        /// Human-readable validation failure.
        message: String,
    },
    /// The caller requested an operation outside the adapter's authority.
    #[error("access to {path} was denied: {reason}")]
    AccessDenied {
        /// Rejected resource.
        path: PathBuf,
        /// Reason the resource was rejected.
        reason: String,
    },
    /// The platform does not implement the requested capability.
    #[error("unsupported platform capability: {0}")]
    Unsupported(&'static str),
    /// A transient race occurred, for example a process exiting during a scan.
    #[error("resource disappeared while being observed: {0}")]
    Disappeared(String),
}

impl PlatformError {
    /// Construct an I/O error while retaining the resource path.
    #[must_use]
    pub fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    /// Construct a malformed-input error.
    #[must_use]
    pub fn invalid(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::InvalidData {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Monotonic time source used for TTLs, sampling and deterministic tests.
pub trait Clock: Send + Sync {
    /// Return milliseconds elapsed on a monotonic clock.
    fn monotonic_millis(&self) -> MonotonicMillis;
}

/// Narrow text I/O surface for sysfs.
///
/// Paths are logical absolute `/sys/...` paths.  An implementation may map
/// them to a fixture root, but it must never interpret `..` components.
pub trait SysfsIo: Send + Sync {
    /// Read and trim a sysfs text attribute.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the path is outside the adapter root, is
    /// unreadable, or does not contain valid text.
    fn read_string(&self, path: &Path) -> PlatformResult<String>;

    /// Replace a sysfs text attribute.
    ///
    /// Implementations are expected to deny writes by default and require an
    /// explicit allowlist for writable instances.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the target is not explicitly authorized,
    /// the value is invalid, or the operating-system write fails.
    fn write_string(&self, path: &Path, value: &str) -> PlatformResult<()>;
}

/// Linux CPU time counters, in scheduler ticks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub io_wait: u64,
    pub irq: u64,
    pub soft_irq: u64,
    pub steal: u64,
}

impl CpuTimes {
    /// Compute busy utilization between two monotonic counter samples.
    ///
    /// Counter regression and a zero-width interval return `None`; callers
    /// should mark that CPU sample stale rather than inventing a load value.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "scheduler tick deltas are intentionally normalized to f64 utilization"
    )]
    pub fn utilization_since(self, previous: Self) -> Option<f64> {
        let deltas = [
            self.user.checked_sub(previous.user)?,
            self.nice.checked_sub(previous.nice)?,
            self.system.checked_sub(previous.system)?,
            self.idle.checked_sub(previous.idle)?,
            self.io_wait.checked_sub(previous.io_wait)?,
            self.irq.checked_sub(previous.irq)?,
            self.soft_irq.checked_sub(previous.soft_irq)?,
            self.steal.checked_sub(previous.steal)?,
        ];
        let total = deltas.into_iter().try_fold(0_u64, u64::checked_add)?;
        let idle = deltas[3].checked_add(deltas[4])?;
        if total == 0 || idle > total {
            return None;
        }
        Some((total - idle) as f64 / total as f64)
    }
}

/// One atomic-enough read of `/proc/stat`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuTimeSnapshot {
    pub observed_at: MonotonicMillis,
    pub aggregate: CpuTimes,
    /// Per-CPU counters keyed by the real kernel CPU ID; IDs need not be dense.
    pub cpus: BTreeMap<CpuId, CpuTimes>,
}

/// Process and CPU observations from procfs.
pub trait ProcReader: Send + Sync {
    /// Read aggregate and per-CPU cumulative counters.
    ///
    /// # Errors
    ///
    /// Returns a platform error when procfs is unreadable or malformed.
    fn cpu_times(&self) -> PlatformResult<CpuTimeSnapshot>;

    /// List process IDs currently present in procfs.
    ///
    /// Returned IDs are snapshots only and must be resolved through
    /// [`Self::process_identity`] before they are displayed or controlled.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the procfs root cannot be enumerated.
    fn list_processes(&self) -> PlatformResult<Vec<ProcessId>>;

    /// Return the thread IDs currently belonging to a process.
    ///
    /// The returned IDs are snapshots only. Callers must resolve and verify a
    /// stable identity again immediately before mutation because a thread can
    /// exit and its TID can be reused between these calls.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the task directory cannot be listed.
    fn list_threads(&self, process: ProcessId) -> PlatformResult<Vec<ProcessId>>;

    /// Resolve stable process identity and display metadata.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the process disappeared, cannot be read,
    /// or contains malformed identity fields.
    fn process_identity(&self, pid: ProcessId) -> PlatformResult<ProcessInfo>;
}

/// Observe the CPU set currently online for affinity validation.
pub trait OnlineCpuSource: Send + Sync {
    /// Read the live, potentially sparse CPU-online mask.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the mask is unavailable or malformed.
    fn online_cpus(&self) -> PlatformResult<CpuSet>;
}

/// Complete read-only port used by the daemon reducer and reconciler.
///
/// Keeping this trait in the platform crate lets production Linux adapters and
/// deterministic testkit fakes drive the exact same actor implementation.
pub trait RuntimePlatform: Clock + ProcReader + OnlineCpuSource {}

impl<T> RuntimePlatform for T where T: Clock + ProcReader + OnlineCpuSource {}

/// A temperature observation tied to a discovered thermal-zone identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThermalSample {
    pub zone_id: String,
    pub zone_type: String,
    /// Exact logical sysfs directory from which this sample was read.
    pub path: PathBuf,
    pub reading: ThermalReading,
}

/// Opaque identity assigned to one open physical input-device instance.
///
/// The Linux adapter never reuses an identity during the lifetime of an input
/// source. A device that disappears and later reopens therefore cannot release
/// a contact that belonged to its previous incarnation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputDeviceId(u64);

impl InputDeviceId {
    /// Construct an adapter-scoped device identity.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Stable identity of one contact for the duration of its touch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchContactId {
    pub device: InputDeviceId,
    pub tracking_id: u32,
}

impl TouchContactId {
    /// Combine a device instance with the kernel tracking ID.
    #[must_use]
    pub const fn new(device: InputDeviceId, tracking_id: u32) -> Self {
        Self {
            device,
            tracking_id,
        }
    }
}

/// Normalized interaction events consumed by the scene reducer.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InputEvent {
    TouchDown {
        contact: TouchContactId,
        x: f64,
        y: f64,
    },
    TouchUp {
        contact: TouchContactId,
        x: f64,
        y: f64,
    },
    Gesture {
        contact: TouchContactId,
        distance: f64,
    },
    /// Discard contacts for one device, or every device when `device` is
    /// absent.
    Resync { device: Option<InputDeviceId> },
}

/// Scheduler state whose original value must be journaled before mutation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSchedulingState {
    pub affinity: CpuSet,
    pub nice: i8,
    pub policy: SchedulingClass,
    pub uclamp_min: Option<u16>,
    pub uclamp_max: Option<u16>,
}

/// Typed process/thread scheduling mutations.
pub trait ProcessController: Send + Sync {
    /// Read the verified scheduler state.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the process disappeared or its scheduler
    /// attributes cannot be read.
    fn read_scheduling(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState>;

    /// Apply and read back a scheduler state.
    ///
    /// # Errors
    ///
    /// Returns a platform error when any mutation or its verification fails.
    fn write_scheduling(
        &self,
        process: ProcessId,
        desired: &ProcessSchedulingState,
    ) -> PlatformResult<ProcessSchedulingState>;
}

/// Safe subset of transient systemd unit properties used by the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemdUnitProperties {
    pub cpu_weight: Option<u64>,
    pub allowed_cpus: Option<CpuSet>,
}

/// Stable identity of one activation of a systemd unit.
///
/// Unit names are reusable: stopping and starting `game.scope` creates a new
/// instance with the same name.  Callers that journal or later restore unit
/// state must therefore retain this identity as well as the name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemdUnitInstanceIdentity {
    /// Validated systemd unit name.
    pub unit: String,
    /// Best stable key exported for this activation.
    pub key: SystemdUnitInstanceKey,
}

/// Stable key used to distinguish activations of a systemd unit.
///
/// `InvocationId` is authoritative when systemd exports it.  `ControlGroup`
/// is a compatibility fallback for unit types or systemd versions that do not
/// expose a usable invocation ID.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SystemdUnitInstanceKey {
    /// Nonzero systemd `InvocationID`, represented as its exact 16 bytes.
    InvocationId([u8; 16]),
    /// Absolute systemd `ControlGroup` path.
    ControlGroup(String),
}

/// Port for systemd-owned cgroup changes.
pub trait SystemdClient: Send + Sync {
    /// Resolve the systemd unit containing a process.
    ///
    /// # Errors
    ///
    /// Returns a platform error when systemd cannot be queried.
    fn unit_for_process(&self, process: ProcessId) -> PlatformResult<Option<String>>;

    /// Resolve the stable instance identity of the unit containing a process.
    ///
    /// Adapters should override this method when they can read the unit name
    /// and activation identity from one platform object, avoiding a name reuse
    /// race between separate calls.
    ///
    /// # Errors
    ///
    /// Returns a platform error when systemd cannot be queried or does not
    /// expose a usable instance identity.
    fn unit_instance_for_process(
        &self,
        process: ProcessId,
    ) -> PlatformResult<Option<SystemdUnitInstanceIdentity>> {
        let Some(unit) = self.unit_for_process(process)? else {
            return Ok(None);
        };
        self.unit_instance_identity(&unit).map(Some)
    }

    /// Read the current stable instance identity for a named unit.
    ///
    /// A later read returning a different value means that the original unit
    /// activation disappeared, even when the reusable unit name is unchanged.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the unit disappeared, cannot be queried,
    /// or does not expose a usable instance identity.
    fn unit_instance_identity(&self, _unit: &str) -> PlatformResult<SystemdUnitInstanceIdentity> {
        Err(PlatformError::Unsupported(
            "stable systemd unit instance identity",
        ))
    }

    /// Return the process IDs currently contained in a unit.
    ///
    /// This is an ownership-checking primitive, not a process-migration API.
    /// Callers must conservatively reject a mutation when the returned members
    /// cannot all be attributed to the intended workload.
    ///
    /// # Errors
    ///
    /// Returns a platform error when systemd cannot enumerate the unit.
    fn unit_processes(&self, unit: &str) -> PlatformResult<Vec<ProcessId>>;

    /// Read supported unit properties.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the unit disappeared or cannot be queried.
    fn read_unit_properties(&self, unit: &str) -> PlatformResult<SystemdUnitProperties>;

    /// Apply and read back supported unit properties.
    ///
    /// # Errors
    ///
    /// Returns a platform error when systemd rejects or cannot verify a change.
    fn write_unit_properties(
        &self,
        unit: &str,
        desired: &SystemdUnitProperties,
    ) -> PlatformResult<SystemdUnitProperties>;
}

/// Durable byte store used by the actuator journal.
pub trait StateStore: Send + Sync {
    /// Load a durable state blob.
    ///
    /// # Errors
    ///
    /// Returns a platform error when storage cannot be read safely.
    fn load(&self) -> PlatformResult<Option<Vec<u8>>>;

    /// Atomically persist and sync a state blob.
    ///
    /// # Errors
    ///
    /// Returns a platform error unless the blob and containing directory are
    /// durably synchronized.
    fn store_durable(&self, bytes: &[u8]) -> PlatformResult<()>;

    /// Durably remove a state blob.
    ///
    /// # Errors
    ///
    /// Returns a platform error when removal or directory synchronization fails.
    fn remove_durable(&self) -> PlatformResult<()>;
}

#[cfg(test)]
mod tests {
    use super::CpuTimes;

    #[test]
    fn cpu_utilization_rejects_any_individual_counter_regression() {
        let previous = CpuTimes {
            user: 100,
            nice: 10,
            system: 20,
            idle: 200,
            io_wait: 5,
            irq: 2,
            soft_irq: 3,
            steal: 1,
        };
        let mut current = previous;
        current.user = 99;
        current.system = 200;

        assert_eq!(current.utilization_since(previous), None);
    }

    #[test]
    fn cpu_utilization_uses_checked_per_field_deltas() {
        let previous = CpuTimes {
            user: 10,
            idle: 10,
            ..CpuTimes::default()
        };
        let current = CpuTimes {
            user: 15,
            idle: 15,
            ..CpuTimes::default()
        };

        assert_eq!(current.utilization_since(previous), Some(0.5));
    }
}
