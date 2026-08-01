use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedValue, Type, Value};

/// Semantic version of the D-Bus contract, independent of daemon releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
#[zvariant(crate = "zbus::zvariant")]
pub struct ApiVersion {
    /// Breaking contract generation.
    pub major: u32,
    /// Exact contract revision within the generation.
    pub minor: u32,
}

impl ApiVersion {
    /// Contract version implemented by this crate.
    pub const CURRENT: Self = Self { major: 2, minor: 0 };

    /// Whether this is exactly the contract implemented by this crate.
    #[must_use]
    pub const fn is_current(self) -> bool {
        self.major == Self::CURRENT.major && self.minor == Self::CURRENT.minor
    }
}

impl Default for ApiVersion {
    fn default() -> Self {
        Self::CURRENT
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// A single health problem. Codes are stable; messages are for humans.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
#[zvariant(crate = "zbus::zvariant")]
pub struct HealthIssue {
    /// Stable machine-readable identifier, for example `thermal.sensor_stale`.
    pub code: String,
    /// `info`, `warning`, `error`, or `critical`.
    pub severity: String,
    /// Component reporting the problem.
    pub component: String,
    /// Human-readable explanation.
    pub message: String,
}

/// Aggregate daemon health embedded in every status response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
#[zvariant(crate = "zbus::zvariant")]
pub struct HealthStatus {
    /// `healthy`, `degraded`, or `failed`.
    pub state: String,
    /// Whether privileged mutations are currently inhibited.
    pub read_only: bool,
    /// Whether an unfinished journal still requires recovery.
    pub recovery_pending: bool,
    /// Short human-readable summary.
    pub summary: String,
    /// Detailed health findings.
    pub issues: Vec<HealthIssue>,
}

/// Stable Linux process identity used to defeat PID reuse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct WorkloadIdentity {
    /// Process ID.
    pub pid: u32,
    /// Field 22 from `/proc/<pid>/stat`, measured in clock ticks since boot.
    pub start_time_ticks: u64,
    /// UID observed by the daemon.
    pub uid: u32,
}

/// Active workload information in [`DaemonStatus`].
///
/// D-Bus lacks a portable native optional type, so `present` explicitly gates
/// the remaining fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct ActiveWorkload {
    /// Whether an active workload is selected.
    pub present: bool,
    /// Stable process identity; ignored when `present` is false.
    pub identity: WorkloadIdentity,
    /// Redacted process display name (never a full command line).
    pub name: String,
    /// Requested mode, or an empty string when none was requested.
    pub requested_mode: String,
    /// Effective mode after policy and safety constraints.
    pub effective_mode: String,
    /// Selection source: `explicit` registration or compositor `focus`.
    pub source: String,
}

/// Request to select a process as the active workload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct WorkloadRequest {
    /// Process selected by the caller.
    ///
    /// The daemon resolves this PID to `(pid, start_time_ticks, uid)` and
    /// verifies ownership before accepting the request.
    pub pid: u32,
    /// Optional requested mode; an empty string delegates to policy. A nonempty
    /// value additionally requires global-control authorization.
    pub mode: String,
    /// Short audit reason supplied by the client.
    pub reason: String,
}

/// Authenticated compositor lifecycle hints.
///
/// The D-Bus method carries the kebab-case spelling as a string so future
/// compatible event kinds do not change the wire signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameHintEvent {
    /// Rendering associated with the current interaction has begun.
    RenderStarted,
    /// Rendering has become idle; interaction hints may end after a slack
    /// interval.
    RenderIdle,
    /// The compositor missed a presentation deadline.
    DeadlineMissed,
    /// The compositor confirms that the physical display is blank.
    DisplayBlanked,
    /// The compositor confirms that the physical display is visible again.
    DisplayUnblanked,
}

impl FrameHintEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RenderStarted => "render-started",
            Self::RenderIdle => "render-idle",
            Self::DeadlineMissed => "deadline-missed",
            Self::DisplayBlanked => "display-blanked",
            Self::DisplayUnblanked => "display-unblanked",
        }
    }
}

impl FromStr for FrameHintEvent {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "render-started" => Ok(Self::RenderStarted),
            "render-idle" => Ok(Self::RenderIdle),
            "deadline-missed" => Ok(Self::DeadlineMissed),
            "display-blanked" => Ok(Self::DisplayBlanked),
            "display-unblanked" => Ok(Self::DisplayUnblanked),
            _ => Err(()),
        }
    }
}

impl fmt::Display for FrameHintEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Read-only scheduler and cgroup state associated with a running workload.
///
/// Empty strings mean that no process rule, cgroup class, unit, or warning is
/// currently associated with the workload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct SchedulerStatus {
    /// Whether task scheduling is enabled in the loaded policy.
    pub enabled: bool,
    /// Name of the first matching process rule.
    pub matched_rule: String,
    /// Number of tasks selected by the latest scheduler classification.
    pub managed_tasks: u32,
    /// Number of selected tasks with daemon-verified applied state.
    pub applied_tasks: u32,
    /// Logical configured cgroup class.
    pub cgroup_class: String,
    /// Owned systemd unit selected for the active workload.
    pub systemd_unit: String,
    /// Whether the unit's requested properties have verified applied state.
    pub cgroup_applied: bool,
    /// Current non-fatal scheduler/cgroup warning.
    pub warning: String,
}

/// A running process discovered as a possible game workload.
///
/// Discovery is observational only. Returning an entry never registers it as
/// the active workload and never changes the global mode.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct RunningWorkload {
    /// Stable process identity captured during the scan.
    pub identity: WorkloadIdentity,
    /// Kernel `comm` display name.
    pub name: String,
    /// Case-insensitive built-in pattern that selected this candidate.
    pub matched_pattern: String,
    /// Whether this exact stable identity is the explicitly active workload.
    pub active: bool,
    /// Scheduler state; populated only for the active workload.
    pub scheduler: SchedulerStatus,
}

/// Thermal state reported by the independent safety path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct ThermalStatus {
    /// `normal`, `warning`, `throttled`, `critical`, `stale`, or `unavailable`.
    pub state: String,
    /// Highest valid sensor reading in millidegrees Celsius.
    pub max_temperature_millicelsius: i32,
    /// Whether thermal policy currently caps one or more targets.
    pub cap_active: bool,
    /// Whether any required sensor reading is stale.
    pub sensors_stale: bool,
}

/// Observed, desired, and applied frequency state for one stable target.
#[allow(
    clippy::struct_excessive_bools,
    reason = "D-Bus DTO uses explicit availability and status flags instead of nullable values"
)]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct FrequencyStatus {
    /// Stable capability target ID.
    pub target_id: String,
    /// Whether the observed fields came from a successful kernel read.
    pub observed_available: bool,
    /// Latest observed lower limit in hertz.
    pub observed_min_hz: u64,
    /// Latest observed upper limit in hertz.
    pub observed_max_hz: u64,
    /// Policy output before reconciliation, in hertz.
    pub desired_min_hz: u64,
    /// Policy output before reconciliation, in hertz.
    pub desired_max_hz: u64,
    /// Whether a desired plan currently exists for this target.
    pub desired_available: bool,
    /// Last successfully read-back lower limit in hertz.
    pub applied_min_hz: u64,
    /// Last successfully read-back upper limit in hertz.
    pub applied_max_hz: u64,
    /// Whether the applied fields represent a daemon-verified readback.
    pub applied_verified: bool,
    /// Whether a manual override contributes to the desired state.
    pub override_active: bool,
    /// Whether the observed values have exceeded their freshness deadline.
    pub stale: bool,
}

/// Complete runtime snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct DaemonStatus {
    /// D-Bus API generation returned by the daemon.
    pub api_version: ApiVersion,
    /// Daemon package version.
    pub daemon_version: String,
    /// Lifecycle state such as `starting`, `running`, `degraded`, or `stopping`.
    pub state: String,
    /// Current aggregate health.
    pub health: HealthStatus,
    /// Requested global mode.
    pub mode: String,
    /// Effective profile after workload and safety constraints.
    pub effective_profile: String,
    /// Dominant scene currently influencing policy.
    pub dominant_scene: String,
    /// Selected workload, if any.
    pub active_workload: ActiveWorkload,
    /// Independent thermal safety status.
    pub thermal: ThermalStatus,
    /// Dynamic frequency target state.
    pub frequencies: Vec<FrequencyStatus>,
    /// Successfully loaded configuration generation.
    pub config_generation: u64,
    /// Last completely reconciled desired-state generation.
    pub reconcile_generation: u64,
}

/// Bounded-rate observational telemetry.
///
/// The daemon emits this DTO at no more than 4 Hz. It is intentionally
/// separate from properties so observers do not need to poll and the control
/// plane cannot make the safety loop run faster.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct TelemetrySnapshot {
    /// Strictly increasing sample sequence within one daemon process.
    pub sequence: u64,
    /// Monotonic timestamp in milliseconds.
    pub monotonic_ms: u64,
    /// Utilization keyed by real, potentially sparse Linux CPU IDs.
    pub cpu_loads: Vec<CpuLoad>,
    /// Independent thermal safety state.
    pub thermal: ThermalStatus,
    /// Dynamic frequency target state.
    pub frequencies: Vec<FrequencyStatus>,
}

/// Utilization sample for one real Linux CPU ID.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct CpuLoad {
    /// Logical CPU ID as reported by the kernel; IDs need not be contiguous.
    pub cpu_id: u32,
    /// Utilization in hundredths of one percent (`0..=10_000`).
    pub utilization_basis_points: u16,
}

/// Desired or verified-applied frequency limits captured in a decision trace.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct DecisionFrequency {
    /// Stable target ID from [`Capabilities::targets`].
    pub target_id: String,
    /// Lower bound in hertz.
    pub min_hz: u64,
    /// Upper bound in hertz.
    pub max_hz: u64,
}

/// One bounded, process-local policy/reconciliation timeline entry.
///
/// `decision_id` and `reconcile_id` are strictly increasing for the lifetime
/// of one daemon process. An empty `error` means every attempted domain
/// completed successfully.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct DecisionTraceEntry {
    /// Exclusive pagination key and monotonic decision sequence.
    pub decision_id: u64,
    /// Monotonic reconciliation sequence.
    pub reconcile_id: u64,
    /// Start time from the daemon's monotonic clock, in milliseconds.
    pub monotonic_ms: u64,
    /// End-to-end blocking-worker duration in microseconds.
    pub duration_us: u64,
    /// Desired-state generation submitted to reconciliation.
    pub generation: u64,
    /// Effective profile selected by the policy engine.
    pub profile: String,
    /// Dominant scene selected by the policy engine.
    pub scene: String,
    /// Whether frequency reconciliation was requested.
    pub frequency_attempted: bool,
    /// Whether scheduler reconciliation was requested.
    pub scheduler_attempted: bool,
    /// Frequency limits submitted to the reconciler.
    pub desired_frequencies: Vec<DecisionFrequency>,
    /// Last verified frequency values returned by the reconciler.
    pub applied_frequencies: Vec<DecisionFrequency>,
    /// Whether every attempted reconciliation domain succeeded.
    pub success: bool,
    /// Stable human-readable failure summary, empty on success.
    pub error: String,
    pub trigger_source: String,
    /// Reducer acceptance timestamp for the event that produced this job.
    pub trigger_monotonic_ms: u64,
    /// Trigger acceptance through verified actuator readback.
    pub verified_apply_latency_us: u64,
    pub governor: GovernorDiagnosticsStatus,
    pub desired_scalars: Vec<DecisionScalar>,
    pub applied_scalars: Vec<DecisionScalar>,
}

/// One governor target captured at policy-evaluation time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct GovernorTargetStatus {
    pub target_id: String,
    pub raw_load_basis_points: u16,
    pub ema_load_basis_points: u16,
    pub predicted_load_basis_points: u16,
    pub selected_load_basis_points: u16,
    pub effective_demand_basis_points: u16,
    pub prediction_bypassed_ramp: bool,
    pub estimated_power_mw: u64,
    pub requested_floor_hz: u64,
    pub selected_floor_hz: u64,
    pub selected_cap_hz: u64,
    /// Stable explanation such as `prediction-bypass` or `demand-floor`.
    pub opp_reason: String,
}

/// Integer, string, or CPU-list scalar encoded as canonical JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct DecisionScalar {
    pub target_id: String,
    pub value_json: String,
}

/// Unit-stable snapshot of the stateful governor's diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct GovernorDiagnosticsStatus {
    pub available: bool,
    pub elapsed_ms: u64,
    pub estimated_package_power_mw: u64,
    pub slow_limit_power_mw: u64,
    pub fast_limit_power_mw: u64,
    pub effective_budget_mw: u64,
    pub bucket_remaining_mj: i64,
    pub bypassed_power_budget: bool,
    pub shared_ramp_basis_points: u16,
    pub targets: Vec<GovernorTargetStatus>,
}

/// Current policy/governor state without changing any actuator resource.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct GovernorStatus {
    pub generation: u64,
    pub profile: String,
    pub scene: String,
    pub trigger_source: String,
    pub diagnostics: GovernorDiagnosticsStatus,
    pub desired_scalars: Vec<DecisionScalar>,
    pub applied_scalars: Vec<DecisionScalar>,
}

/// Human-readable mode advertised by the daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct ModeInfo {
    /// Stable mode ID used by `SetMode`.
    pub id: String,
    /// Localizable display name.
    pub display_name: String,
    /// Short description of the policy intent.
    pub description: String,
}

/// A discovered actuator target clients may address by ID.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct TargetCapability {
    /// Stable ID, for example `cpu.policy0`.
    pub id: String,
    /// Target kind such as `cpufreq` or `devfreq`.
    pub kind: String,
    /// Human-readable label.
    pub label: String,
    /// Associated logical CPU IDs, empty for non-CPU targets.
    pub cpus: Vec<u32>,
    /// Lowest discovered operating point in hertz.
    pub minimum_hz: u64,
    /// Highest discovered operating point in hertz.
    pub maximum_hz: u64,
    /// Sorted operating points in hertz, empty when the kernel exposes a
    /// continuous range.
    pub available_hz: Vec<u64>,
    /// Whether the caller may request a bounded override for this target.
    pub can_override: bool,
}

/// Runtime capabilities negotiated before issuing mutating calls.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct Capabilities {
    /// D-Bus API generation returned by the daemon.
    pub api_version: ApiVersion,
    /// Stable feature identifiers.
    pub features: Vec<String>,
    /// Supported global modes.
    pub modes: Vec<ModeInfo>,
    /// Dynamically discovered actuator targets.
    pub targets: Vec<TargetCapability>,
    /// Exact accepted configuration schema.
    pub config_schema_version: u32,
}

impl Capabilities {
    /// Return whether an exact stable feature identifier is advertised.
    #[must_use]
    pub fn supports(&self, feature: &str) -> bool {
        self.features.iter().any(|candidate| candidate == feature)
    }
}

/// Bounded frequency limits requested for one stable target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct FrequencyOverride {
    /// Stable target ID from [`Capabilities::targets`].
    pub target_id: String,
    /// Requested lower limit in hertz.
    pub min_hz: u64,
    /// Requested upper limit in hertz.
    pub max_hz: u64,
    /// Lifetime in milliseconds; zero asks the daemon to keep it until cleared.
    pub ttl_ms: u64,
    /// Short audit reason supplied by the client.
    pub reason: String,
}

/// Receipt returned by successful mutating methods.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct MutationReceipt {
    /// Desired-state generation containing the mutation.
    pub generation: u64,
    /// Stable IDs changed by the operation.
    pub changed_ids: Vec<String>,
    /// Human-readable summary.
    pub message: String,
}

/// Result of a transactional configuration reload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct ReloadReport {
    /// New configuration generation.
    pub config_generation: u64,
    /// Non-fatal validation or capability warnings.
    pub warnings: Vec<String>,
    /// Human-readable summary.
    pub message: String,
}

/// Persistent application rule.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct AppRule {
    /// Stable rule ID. Empty IDs are never valid on the wire.
    pub id: String,
    /// Whether the rule participates in matching.
    pub enabled: bool,
    /// Owning UID. Application rules are administrator-owned and global, so
    /// callers must use `u32::MAX`.
    pub owner_uid: u32,
    /// Exact path matched against `/proc/<pid>/exe`, or no executable
    /// constraint.
    ///
    /// D-Bus encodes this option as a zero-or-one-element string array.
    pub executable: Option<String>,
    /// Rust-compatible regex matched against the kernel `comm` value, or no
    /// process-name constraint.
    ///
    /// D-Bus encodes this option as a zero-or-one-element string array.
    pub comm_regex: Option<String>,
    /// Requested mode when matched.
    pub mode: String,
    /// Higher values win when multiple rules match.
    pub priority: i32,
}

/// One item in a diagnostic report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct DiagnosticCheck {
    /// Stable check identifier.
    pub id: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Human-readable result.
    pub message: String,
}

/// Client-composed diagnostics using versioned status and capability data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct DiagnosticReport {
    /// API version observed during diagnostics.
    pub api_version: ApiVersion,
    /// Overall result.
    pub healthy: bool,
    /// Individual checks.
    pub checks: Vec<DiagnosticCheck>,
}

#[cfg(test)]
mod tests {
    use super::{
        ApiVersion, AppRule, Capabilities, DecisionFrequency, DecisionScalar, DecisionTraceEntry,
        FrameHintEvent, FrequencyOverride, FrequencyStatus, GovernorDiagnosticsStatus,
        GovernorStatus, GovernorTargetStatus, RunningWorkload, SchedulerStatus, TargetCapability,
        WorkloadIdentity, WorkloadRequest,
    };
    use crate::feature;
    use zvariant::{LE, Type, serialized::Context, to_bytes};

    #[test]
    fn api_version_must_match_exactly() {
        assert!(ApiVersion::CURRENT.is_current());
        assert!(!ApiVersion { major: 2, minor: 1 }.is_current());
        assert!(!ApiVersion { major: 1, minor: 5 }.is_current());
    }

    #[test]
    fn frequency_override_json_round_trip_preserves_sub_kilohertz_values() {
        let original = FrequencyOverride {
            target_id: "gpu.generic".into(),
            min_hz: 1_001,
            max_hz: 1_003,
            ttl_ms: 30_000,
            reason: "test".into(),
        };
        let encoded = serde_json::to_string(&original).expect("serialize DTO");
        let decoded = serde_json::from_str(&encoded).expect("deserialize DTO");
        assert_eq!(original, decoded);
        assert!(encoded.contains("\"min_hz\":1001"));
        assert!(!encoded.contains("khz"));
    }

    #[test]
    fn frame_hint_names_are_exact_and_round_trip() {
        for event in [
            FrameHintEvent::RenderStarted,
            FrameHintEvent::RenderIdle,
            FrameHintEvent::DeadlineMissed,
            FrameHintEvent::DisplayBlanked,
            FrameHintEvent::DisplayUnblanked,
        ] {
            assert_eq!(event.as_str().parse::<FrameHintEvent>(), Ok(event));
            assert_eq!(
                serde_json::from_str::<FrameHintEvent>(
                    &serde_json::to_string(&event).expect("serialize event")
                )
                .expect("deserialize event"),
                event
            );
        }
        assert!("render-idle-ish".parse::<FrameHintEvent>().is_err());
    }

    #[test]
    fn target_and_status_dbus_round_trips_preserve_exact_hertz() {
        let target = TargetCapability {
            id: "gpu.generic".into(),
            kind: "devfreq".into(),
            label: "Generic devfreq".into(),
            cpus: Vec::new(),
            minimum_hz: 1_001,
            maximum_hz: 1_003,
            available_hz: vec![1_001, 1_002, 1_003],
            can_override: true,
        };
        let encoded = to_bytes(Context::new_dbus(LE, 0), &target).expect("serialize target");
        let (decoded, _) = encoded
            .deserialize::<TargetCapability>()
            .expect("deserialize target");
        assert_eq!(decoded, target);

        let status = FrequencyStatus {
            target_id: "gpu.generic".into(),
            observed_available: true,
            observed_min_hz: 1_001,
            observed_max_hz: 1_003,
            desired_min_hz: 1_002,
            desired_max_hz: 1_003,
            desired_available: true,
            applied_min_hz: 1_001,
            applied_max_hz: 1_002,
            applied_verified: true,
            override_active: true,
            stale: false,
        };
        let encoded = to_bytes(Context::new_dbus(LE, 0), &status).expect("serialize status");
        let (decoded, _) = encoded
            .deserialize::<FrequencyStatus>()
            .expect("deserialize status");
        assert_eq!(decoded, status);
    }

    #[test]
    fn decision_trace_round_trips_over_dbus_encoding() {
        let governor = GovernorDiagnosticsStatus {
            available: true,
            elapsed_ms: 20,
            estimated_package_power_mw: 4_200,
            slow_limit_power_mw: 4_000,
            fast_limit_power_mw: 6_000,
            effective_budget_mw: 6_000,
            bucket_remaining_mj: -50,
            bypassed_power_budget: false,
            shared_ramp_basis_points: 7_500,
            targets: vec![GovernorTargetStatus {
                target_id: "cpu.policy0".into(),
                raw_load_basis_points: 8_000,
                ema_load_basis_points: 7_000,
                predicted_load_basis_points: 9_000,
                selected_load_basis_points: 9_000,
                effective_demand_basis_points: 9_500,
                prediction_bypassed_ramp: true,
                estimated_power_mw: 2_100,
                requested_floor_hz: 1_800_000_000,
                selected_floor_hz: 1_800_000_000,
                selected_cap_hz: 2_400_000_000,
                opp_reason: "prediction-bypass".into(),
            }],
        };
        let scalar = DecisionScalar {
            target_id: "scalar.ddr".into(),
            value_json: r#"{"kind":"integer","value":800}"#.into(),
        };
        let original = DecisionTraceEntry {
            decision_id: 7,
            reconcile_id: 6,
            monotonic_ms: 123,
            duration_us: 456,
            generation: 5,
            profile: "balance".into(),
            scene: "touch".into(),
            frequency_attempted: true,
            scheduler_attempted: false,
            desired_frequencies: vec![DecisionFrequency {
                target_id: "cpu.policy0".into(),
                min_hz: 400_000_000,
                max_hz: 1_800_000_000,
            }],
            applied_frequencies: vec![DecisionFrequency {
                target_id: "cpu.policy0".into(),
                min_hz: 400_000_000,
                max_hz: 1_800_000_000,
            }],
            success: true,
            error: String::new(),
            trigger_source: "desktop-input".into(),
            trigger_monotonic_ms: 100,
            verified_apply_latency_us: 2_500,
            governor: governor.clone(),
            desired_scalars: vec![scalar.clone()],
            applied_scalars: vec![scalar.clone()],
        };
        let encoded = to_bytes(Context::new_dbus(LE, 0), &original).expect("serialize trace");
        let (decoded, _) = encoded
            .deserialize::<DecisionTraceEntry>()
            .expect("deserialize trace");
        assert_eq!(decoded, original);

        let status = GovernorStatus {
            generation: 9,
            profile: "balance".into(),
            scene: "touch".into(),
            trigger_source: "desktop-input".into(),
            diagnostics: governor,
            desired_scalars: vec![scalar.clone()],
            applied_scalars: vec![scalar],
        };
        let encoded = to_bytes(Context::new_dbus(LE, 0), &status).expect("serialize governor");
        let (decoded, _) = encoded
            .deserialize::<GovernorStatus>()
            .expect("deserialize governor");
        assert_eq!(decoded, status);
    }

    #[test]
    fn capabilities_require_an_exact_feature_id() {
        let capabilities = Capabilities {
            features: vec![feature::THERMAL_GUARD.into()],
            ..Capabilities::default()
        };

        assert!(capabilities.supports(feature::THERMAL_GUARD));
        assert!(!capabilities.supports("thermal"));
        assert!(!capabilities.supports("thermal-guard-extra"));
    }

    #[test]
    fn composite_app_rule_round_trips_over_dbus_encoding() {
        assert_eq!(AppRule::SIGNATURE.to_string(), "(sbuasassi)");
        let original = AppRule {
            id: "game".into(),
            enabled: true,
            owner_uid: u32::MAX,
            executable: Some("/usr/bin/game".into()),
            comm_regex: Some("^Render.*".into()),
            mode: "performance".into(),
            priority: 10,
        };
        let encoded =
            to_bytes(Context::new_dbus(LE, 0), &original).expect("serialize D-Bus struct");
        let (decoded, _) = encoded
            .deserialize::<AppRule>()
            .expect("deserialize D-Bus struct");
        assert_eq!(decoded, original);
    }

    #[test]
    fn workload_request_wire_shape_contains_no_claimed_identity() {
        assert_eq!(WorkloadRequest::SIGNATURE.to_string(), "(uss)");
        let request = WorkloadRequest {
            pid: 42,
            mode: String::new(),
            reason: "test".into(),
        };
        let json = serde_json::to_value(request).expect("serialize request");
        assert_eq!(json["pid"], 42);
        assert!(json.get("uid").is_none());
        assert!(json.get("start_time_ticks").is_none());
    }

    #[test]
    fn running_workload_round_trips_without_executable_or_cmdline() {
        assert_eq!(
            RunningWorkload::SIGNATURE.to_string(),
            "((utu)ssb(bsuussbs))"
        );
        let original = RunningWorkload {
            identity: WorkloadIdentity {
                pid: 42,
                start_time_ticks: 100,
                uid: 1000,
            },
            name: "wine64".to_owned(),
            matched_pattern: "wine".to_owned(),
            active: true,
            scheduler: SchedulerStatus {
                enabled: true,
                matched_rule: "games".to_owned(),
                managed_tasks: 4,
                applied_tasks: 3,
                cgroup_class: "foreground".to_owned(),
                systemd_unit: "app-game.scope".to_owned(),
                cgroup_applied: true,
                warning: String::new(),
            },
        };
        let encoded =
            to_bytes(Context::new_dbus(LE, 0), &original).expect("serialize running workload");
        let (decoded, _) = encoded
            .deserialize::<RunningWorkload>()
            .expect("deserialize running workload");
        assert_eq!(decoded, original);
        let json = serde_json::to_value(original).expect("serialize running workload JSON");
        assert!(json.get("executable").is_none());
        assert!(json.get("cmdline").is_none());
    }
}
