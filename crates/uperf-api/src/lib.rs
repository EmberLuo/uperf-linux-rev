//! Versioned D-Bus contract and client for uperf-linux.
//!
//! The types in this crate deliberately do not expose sysfs paths or positional
//! cluster indexes. Callers address discovered resources by stable target IDs
//! and submit only a PID for workload selection. The daemon captures
//! `(pid, start_time_ticks, uid)` itself, which lets it reject PID reuse and
//! ownership confusion safely.

mod client;
mod contract;
mod error;

pub use client::{Daemon1Proxy, DaemonClient, MAX_DECISION_TRACE_PAGE};
pub use contract::{
    ActiveWorkload, ApiVersion, AppRule, Capabilities, CpuLoad, DaemonStatus, DecisionFrequency,
    DecisionScalar, DecisionTraceEntry, DecisionTraceEntryV2, DiagnosticCheck, DiagnosticReport,
    FrameHintEvent, FrequencyOverride, FrequencyStatus, GovernorDiagnosticsStatus, GovernorStatus,
    GovernorTargetStatus, HealthIssue, HealthStatus, ModeInfo, MutationReceipt, ReloadReport,
    RunningWorkload, SchedulerStatus, TargetCapability, TelemetrySnapshot, ThermalStatus,
    WorkloadIdentity, WorkloadRequest,
};
pub use error::{ClientError, ServiceError};

/// Well-known name for API major version 1.
pub const SERVICE_NAME: &str = "org.uperflinux.Daemon1";
/// Object path for API major version 1.
pub const OBJECT_PATH: &str = "/org/uperflinux/Daemon1";
/// Interface name for API major version 1.
pub const INTERFACE_NAME: &str = "org.uperflinux.Daemon1";
/// Prefix used by stable service-side D-Bus errors.
pub const ERROR_PREFIX: &str = "org.uperflinux.Daemon1.Error";

/// Stable identifiers returned in [`Capabilities::features`].
///
/// Clients must compare these values exactly. Feature names are not tags, so
/// substring matching can incorrectly enable controls for an unrelated future
/// capability.
pub mod feature {
    /// Automatic CPU demand policy is configured.
    pub const LOAD_GOVERNOR: &str = "load-governor";
    /// Trusted thermal sensors constrain all mutations.
    pub const THERMAL_GUARD: &str = "thermal-guard";
    /// Explicit active-workload registration is available.
    pub const ACTIVE_WORKLOAD: &str = "active-workload";
    /// Transactional frequency overrides are available.
    pub const FREQUENCY_TRANSACTIONS: &str = "frequency-transactions";
    /// Transactional schema-v2 configuration reload is available.
    pub const CONFIG_RELOAD_V2: &str = "config-reload-v2";
    /// logind sleep/wake observation is available.
    pub const LOGIND_SLEEP_WAKE: &str = "logind-sleep-wake";
    /// evdev-driven touch and gesture scenes are available.
    pub const EVDEV_SCENES: &str = "evdev-scenes";
    /// Per-task affinity, nice, scheduling class, and uclamp are available.
    pub const TASK_SCHEDULER: &str = "task-scheduler";
    /// Explicitly configured, bounded experimental `SCHED_FIFO` plans are understood.
    pub const REALTIME_FIFO_V1: &str = "realtime-fifo-v1";
    /// Owned systemd-unit CPU controls are available.
    pub const SYSTEMD_CGROUP: &str = "systemd-cgroup";
    /// Read-only discovery of running game-like processes and scheduler state.
    pub const RUNNING_WORKLOADS: &str = "running-workloads";
    /// A device profile was selected from the catalog or administrator override.
    pub const DEVICE_PROFILE: &str = "device-profile";
    /// Compositor-reported focus can supply the active workload.
    pub const FOREGROUND_FOCUS: &str = "foreground-focus";
    /// A bounded, read-only policy and reconciliation timeline is available.
    pub const DECISION_TRACE_V1: &str = "decision-trace-v1";
    /// Extended decision records include governor and typed-scalar diagnostics.
    pub const DECISION_TRACE_V2: &str = "decision-trace-v2";
    /// The reference-compatible energy governor is available.
    pub const ENERGY_GOVERNOR_V1: &str = "energy-governor-v1";
    /// Scheduler task plans can vary by dominant scene.
    pub const SCENE_SCHEDULER_V1: &str = "scene-scheduler-v1";
    /// Keyboard and pointer activity can generate interaction scenes.
    pub const DESKTOP_INPUT_V1: &str = "desktop-input-v1";
    /// Administrator-declared, typed scalar targets are available.
    pub const SCALAR_TARGETS_V1: &str = "scalar-targets-v1";
    /// Authenticated compositor frame lifecycle hints are available.
    pub const FRAME_HINTS_V1: &str = "frame-hints-v1";
}

/// Oldest configuration schema accepted by this API generation.
pub const MIN_CONFIG_SCHEMA_VERSION: u32 = 2;
/// Newest configuration schema accepted by this API generation.
pub const CONFIG_SCHEMA_VERSION: u32 = 2;

/// Canonical built-in balanced profile name.
pub const MODE_BALANCE: &str = "balance";
/// Canonical built-in power-saving profile name.
pub const MODE_POWERSAVE: &str = "powersave";
/// Canonical built-in performance profile name.
pub const MODE_PERFORMANCE: &str = "performance";
