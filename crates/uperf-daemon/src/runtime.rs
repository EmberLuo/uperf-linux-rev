//! Single-owner runtime state, observer reduction, policy and reconciliation.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};
use uperf_actuator::{ActuatorMode, FileStateStore, FrequencyActuator};
use uperf_api::{
    ActiveWorkload, ApiVersion, AppRule as ApiAppRule, Capabilities, CpuLoad, DaemonStatus,
    FrequencyOverride, FrequencyStatus, HealthIssue, HealthStatus, MutationReceipt, ReloadReport,
    TelemetrySnapshot, ThermalStatus, WorkloadIdentity, WorkloadRequest, feature,
};
use uperf_core::{
    AppliedState, AppsConfig, DesiredPlan, FrequencyLimits, HeavyLoadDetector, HeavyLoadState,
    Hertz, Hint, HintSet, InputConfig, MilliCelsius, ModeSelection, MonotonicMillis,
    ObservedFrequency, ObservedState, ProcessId, ProcessIdentity, ProcessInfo, ProfileId, Scene,
    SensorHealth, TargetId, TaskPlan, ThermalGuard, ThermalReading, ThermalState,
    ThermalThresholds, worst_thermal_state,
};
use uperf_linux::{LinuxDiscovery, LinuxEnvironment};
use uperf_platform::{
    CpuTimeSnapshot, InputEvent, ProcReader, RuntimePlatform, StateStore, SysfsIo,
    SystemdUnitProperties, ThermalSample, TouchContactId,
};

use crate::{
    config::{ConfigurationPaths, ResolvedConfiguration},
    reconcile::{FrequencySafetyFence, ReconcileJob, ReconcileOutcome},
};

const COMMAND_CAPACITY: usize = 64;
const HINT_CAPACITY: usize = 64;
pub const TELEMETRY_INTERVAL: Duration = Duration::from_millis(250);
const FREQUENCY_OBSERVER_INTERVAL: Duration = Duration::from_secs(1);
const REDUCER_TICK: Duration = Duration::from_millis(100);
const WORKLOAD_CHECK_INTERVAL_MS: u64 = 1_000;
const FREQUENCY_STALE_AFTER_MS: u64 = 3_000;
const APP_PERSISTENCE_IN_FLIGHT: &str = "an application-rule update is still being persisted";
const RELOAD_IN_FLIGHT: &str = "configuration reload is still being prepared";

/// Errors produced by the state owner before D-Bus translation.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("not authorized: {0}")]
    NotAuthorized(String),
    #[error("state conflict: {0}")]
    Conflict(String),
    #[error("mutation unavailable in degraded mode: {0}")]
    Degraded(String),
    #[error("configuration validation failed: {0}")]
    Validation(String),
    #[error("runtime internal error: {0}")]
    Internal(String),
}

/// Coherent data published to API readers.
#[derive(Clone, Debug)]
pub struct PublishedState {
    /// Monotonic revision of externally visible control state.
    pub state_revision: u64,
    pub status: DaemonStatus,
    pub capabilities: Capabilities,
    pub telemetry: TelemetrySnapshot,
    pub app_rules: Vec<ApiAppRule>,
}

/// Events used by the D-Bus layer to emit signals without polling.
#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    StateChanged(u64),
    HealthChanged(HealthStatus),
    CapabilitiesChanged,
}

#[derive(Clone, Debug)]
struct TimedOverride {
    limits: FrequencyLimits,
    expires_at: Option<MonotonicMillis>,
}

#[derive(Default)]
struct ActiveTouchContacts {
    contacts: BTreeSet<TouchContactId>,
}

impl ActiveTouchContacts {
    /// Returns true only when this is the first active contact.
    fn press(&mut self, contact: TouchContactId) -> bool {
        let was_empty = self.contacts.is_empty();
        self.contacts.insert(contact) && was_empty
    }

    /// Returns true only when a known contact was the final active contact.
    fn release(&mut self, contact: TouchContactId) -> bool {
        self.contacts.remove(&contact) && self.contacts.is_empty()
    }

    /// Discard one device or all devices, returning true when Touch ended.
    fn resync(&mut self, device: Option<uperf_platform::InputDeviceId>) -> bool {
        let had_contacts = !self.contacts.is_empty();
        if let Some(device) = device {
            self.contacts.retain(|contact| contact.device != device);
        } else {
            self.contacts.clear();
        }
        had_contacts && self.contacts.is_empty()
    }

    fn clear(&mut self) {
        self.contacts.clear();
    }
}

#[derive(Debug)]
enum Command {
    SetMode {
        mode: String,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    },
    SetActiveWorkload {
        request: WorkloadRequest,
        caller_uid: u32,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    },
    ClearActiveWorkload {
        caller_uid: u32,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    },
    SetFrequencyOverrides {
        overrides: Vec<FrequencyOverride>,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    },
    ClearFrequencyOverrides {
        target_ids: Vec<String>,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    },
    Reload {
        reply: oneshot::Sender<Result<ReloadReport, RuntimeError>>,
    },
    SetAppRule {
        rule: ApiAppRule,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    },
    RemoveAppRule {
        id: String,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    },
    BeginShutdown {
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
}

/// Cloneable control-plane endpoint. It never exposes mutable runtime state.
#[derive(Clone)]
pub struct RuntimeHandle {
    commands: mpsc::Sender<Command>,
    published: watch::Receiver<Arc<PublishedState>>,
    events: broadcast::Sender<RuntimeEvent>,
}

impl RuntimeHandle {
    #[cfg(test)]
    pub(crate) fn snapshot_only() -> Self {
        let (commands, _) = mpsc::channel(1);
        let (published, published_rx) = watch::channel(Arc::new(PublishedState {
            state_revision: 0,
            status: DaemonStatus::default(),
            capabilities: Capabilities::default(),
            telemetry: TelemetrySnapshot::default(),
            app_rules: Vec::new(),
        }));
        drop(published);
        let (events, _) = broadcast::channel(1);
        Self {
            commands,
            published: published_rx,
            events,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<PublishedState> {
        Arc::clone(&self.published.borrow())
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<PublishedState>> {
        self.published.clone()
    }

    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    /// Select the global automatic or forced profile mode.
    ///
    /// # Errors
    ///
    /// Returns a runtime error when validation fails or the state task stops.
    pub async fn set_mode(&self, mode: String) -> Result<MutationReceipt, RuntimeError> {
        self.request(|reply| Command::SetMode { mode, reply }).await
    }

    /// Register one stable process identity as the active workload.
    ///
    /// # Errors
    ///
    /// Returns a runtime error for stale identity, invalid mode, or task loss.
    pub async fn set_active_workload(
        &self,
        request: WorkloadRequest,
        caller_uid: u32,
    ) -> Result<MutationReceipt, RuntimeError> {
        self.request(|reply| Command::SetActiveWorkload {
            request,
            caller_uid,
            reply,
        })
        .await
    }

    /// Clear the matching active workload identity.
    ///
    /// # Errors
    ///
    /// Returns a runtime error when the identity conflicts or the task stops.
    pub async fn clear_active_workload(
        &self,
        caller_uid: u32,
    ) -> Result<MutationReceipt, RuntimeError> {
        self.request(|reply| Command::ClearActiveWorkload { caller_uid, reply })
            .await
    }

    /// Submit one atomic batch of logical frequency overrides.
    ///
    /// # Errors
    ///
    /// Returns a runtime error for invalid limits, degraded safety, or task loss.
    pub async fn set_frequency_overrides(
        &self,
        overrides: Vec<FrequencyOverride>,
    ) -> Result<MutationReceipt, RuntimeError> {
        self.request(|reply| Command::SetFrequencyOverrides { overrides, reply })
            .await
    }

    /// Clear selected overrides, or every override for an empty list.
    ///
    /// # Errors
    ///
    /// Returns a runtime error for invalid targets, degraded safety, or task loss.
    pub async fn clear_frequency_overrides(
        &self,
        target_ids: Vec<String>,
    ) -> Result<MutationReceipt, RuntimeError> {
        self.request(|reply| Command::ClearFrequencyOverrides { target_ids, reply })
            .await
    }

    /// Parse, resolve, and atomically install a new configuration generation.
    ///
    /// # Errors
    ///
    /// Returns a validation or runtime error while retaining the old generation.
    pub async fn reload(&self) -> Result<ReloadReport, RuntimeError> {
        self.request(|reply| Command::Reload { reply }).await
    }

    /// Persist or replace an application rule.
    ///
    /// # Errors
    ///
    /// Returns a validation, persistence, or runtime communication error.
    pub async fn set_app_rule(&self, rule: ApiAppRule) -> Result<MutationReceipt, RuntimeError> {
        self.request(|reply| Command::SetAppRule { rule, reply })
            .await
    }

    /// Remove a persistent application rule by logical ID.
    ///
    /// # Errors
    ///
    /// Returns `NotFound`, a persistence error, or a runtime communication error.
    pub async fn remove_app_rule(&self, id: String) -> Result<MutationReceipt, RuntimeError> {
        self.request(|reply| Command::RemoveAppRule { id, reply })
            .await
    }

    /// Close the control plane without restoring resources yet.
    ///
    /// Commands already ordered before this barrier complete normally; every
    /// later mutation request is rejected.
    ///
    /// # Errors
    ///
    /// Returns an error if the state task has already stopped.
    pub async fn begin_shutdown(&self) -> Result<(), RuntimeError> {
        self.request(|reply| Command::BeginShutdown { reply }).await
    }

    /// Restore owned resources and stop the state task.
    ///
    /// # Errors
    ///
    /// Returns an error when verified restoration or task communication fails.
    pub async fn stop(&self) -> Result<(), RuntimeError> {
        loop {
            match self.request(|reply| Command::Stop { reply }).await {
                Err(RuntimeError::Conflict(message))
                    if message == APP_PERSISTENCE_IN_FLIGHT || message == RELOAD_IN_FLIGHT =>
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                result => return result,
            }
        }
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, RuntimeError>>) -> Command,
    ) -> Result<T, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| RuntimeError::Internal("state task has stopped".to_owned()))?;
        response
            .await
            .map_err(|_| RuntimeError::Internal("state task dropped its reply".to_owned()))?
    }
}

type LoadObservation = Option<(u64, Result<CpuTimeSnapshot, String>)>;
type ThermalObservation = Option<(u64, Result<Vec<ThermalSample>, String>)>;
type FrequencyObservation = Option<(u64, Result<FrequencyObservationBatch, String>)>;
type LogindHealthObservation = Option<Result<(), String>>;
type InputHealthObservation = Option<Result<(), String>>;
type ReconcileResult = Result<ReconcileOutcome, String>;

#[derive(Clone, Debug)]
struct FrequencyObservationBatch {
    readings: BTreeMap<TargetId, ObservedFrequency>,
    errors: BTreeMap<TargetId, String>,
}

struct AppPersistenceOutcome {
    candidate: AppsConfig,
    changed_id: String,
    message: &'static str,
    reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    result: Result<(), RuntimeError>,
}

struct ReloadOutcome {
    reply: oneshot::Sender<Result<ReloadReport, RuntimeError>>,
    result: Result<ResolvedConfiguration, RuntimeError>,
}

/// Congestion-resistant observer inputs.
///
/// Load and thermal use watch channels, so a slow reducer keeps the latest
/// sample instead of accumulating unbounded stale work.
#[derive(Clone)]
pub struct ObserverIngress {
    load: watch::Sender<LoadObservation>,
    thermal: watch::Sender<ThermalObservation>,
    frequency: watch::Sender<FrequencyObservation>,
    logind_health: watch::Sender<LogindHealthObservation>,
    input_health: watch::Sender<InputHealthObservation>,
    runtime_events: mpsc::Sender<RuntimeInput>,
    settings: watch::Receiver<ObserverSettings>,
    frequency_targets: Arc<BTreeMap<TargetId, crate::config::ResolvedTarget>>,
}

impl ObserverIngress {
    pub fn observe_load(&self, sequence: u64, sample: Result<CpuTimeSnapshot, String>) {
        self.load.send_replace(Some((sequence, sample)));
    }

    pub fn observe_thermal(&self, sequence: u64, sample: Result<Vec<ThermalSample>, String>) {
        self.thermal.send_replace(Some((sequence, sample)));
    }

    fn observe_frequencies(
        &self,
        sequence: u64,
        sample: Result<FrequencyObservationBatch, String>,
    ) {
        self.frequency.send_replace(Some((sequence, sample)));
    }

    #[must_use]
    pub fn try_hint(&self, scene: Scene) -> bool {
        self.runtime_events
            .try_send(RuntimeInput::Hint(scene))
            .is_ok()
    }

    /// Forward an input transition without silently dropping it under load.
    ///
    /// The dedicated evdev thread waits on the bounded runtime channel. It may
    /// abandon a pending event only during explicit observer shutdown.
    #[must_use]
    pub fn send_input_with_backpressure(&self, event: InputEvent, cancelled: &AtomicBool) -> bool {
        let mut pending = RuntimeInput::Input(event);
        loop {
            if cancelled.load(Ordering::Acquire) {
                return false;
            }
            match self.runtime_events.try_send(pending) {
                Ok(()) => return true,
                Err(mpsc::error::TrySendError::Full(event)) => pending = event,
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Deliver a sleep transition and wait for the state owner to finish its
    /// safety work.
    ///
    /// The same absolute deadline covers both bounded-channel admission and
    /// actuator restore. This lets the logind observer release its delay
    /// inhibitor before logind's own maximum delay expires.
    ///
    /// # Errors
    ///
    /// Returns an error when the reducer has stopped, the deadline expires, or
    /// pre-sleep restoration fails.
    pub async fn prepare_for_sleep(&self, sleeping: bool, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let (completion, completed) = oneshot::channel();
        tokio::time::timeout_at(
            deadline,
            self.runtime_events.send(RuntimeInput::PrepareForSleep {
                sleeping,
                completion,
            }),
        )
        .await
        .map_err(|_| "timed out delivering the logind sleep transition".to_owned())?
        .map_err(|_| "state task stopped before the logind sleep transition".to_owned())?;
        tokio::time::timeout_at(deadline, completed)
            .await
            .map_err(|_| "timed out waiting for the logind sleep transition".to_owned())?
            .map_err(|_| "state task dropped the logind sleep acknowledgement".to_owned())?
    }

    /// Publish logind observer health without competing with input/hint traffic.
    pub fn report_logind_health(&self, result: Result<(), String>) {
        self.logind_health.send_replace(Some(result));
    }

    /// Publish evdev health independently from bounded touch transitions.
    pub fn report_input_health(&self, result: Result<(), String>) {
        self.input_health.send_replace(Some(result));
    }

    /// Subscribe to atomically swapped observer configuration.
    #[must_use]
    pub fn settings(&self) -> watch::Receiver<ObserverSettings> {
        self.settings.clone()
    }
}

/// Reloadable observer cadences and input policy.
#[derive(Clone, Debug, PartialEq)]
pub struct ObserverSettings {
    pub load_interval: Duration,
    pub thermal_interval: Duration,
    pub thermal_paths: Vec<PathBuf>,
    pub input: InputConfig,
}

impl ObserverSettings {
    fn from_configuration(configuration: &ResolvedConfiguration) -> Self {
        Self {
            load_interval: Duration::from_millis(configuration.policy.load.sample_interval_ms),
            thermal_interval: Duration::from_millis(
                configuration.policy.thermal.sample_interval_ms,
            ),
            thermal_paths: configuration
                .thermal_zones
                .iter()
                .filter_map(|zone| zone.sysfs_path.as_ref().map(PathBuf::from))
                .collect(),
            input: configuration.policy.input.clone(),
        }
    }
}

#[derive(Debug)]
enum RuntimeInput {
    Hint(Scene),
    Input(InputEvent),
    PrepareForSleep {
        sleeping: bool,
        completion: oneshot::Sender<Result<(), String>>,
    },
}

/// Inputs required to start the single state-owning task.
pub struct RuntimeParts {
    pub environment: Arc<dyn RuntimePlatform>,
    pub discovery: LinuxDiscovery,
    pub configuration: ResolvedConfiguration,
    pub configuration_paths: ConfigurationPaths,
    pub actuator: Option<Arc<FrequencyActuator>>,
    pub started_at: Instant,
}

/// Start the reducer and return API, observer, and join handles.
#[must_use]
pub fn spawn_runtime(
    parts: RuntimeParts,
) -> (
    RuntimeHandle,
    ObserverIngress,
    JoinHandle<Result<(), RuntimeError>>,
) {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (load_tx, load_rx) = watch::channel(None);
    let (thermal_tx, thermal_rx) = watch::channel(None);
    let (frequency_tx, frequency_rx) = watch::channel(None);
    let (logind_health_tx, logind_health_rx) = watch::channel(None);
    let (input_health_tx, input_health_rx) = watch::channel(None);
    let (runtime_event_tx, runtime_event_rx) = mpsc::channel(HINT_CAPACITY);
    let (reconcile_tx, reconcile_rx) = mpsc::channel::<ReconcileResult>(1);
    let (app_persistence_tx, app_persistence_rx) = mpsc::channel(1);
    let (reload_tx, reload_rx) = mpsc::channel(1);
    let (settings_tx, settings_rx) =
        watch::channel(ObserverSettings::from_configuration(&parts.configuration));
    let frequency_targets = Arc::new(parts.configuration.targets.clone());
    let (event_tx, _) = broadcast::channel(64);
    let actor = RuntimeActor::new(
        parts,
        settings_tx,
        reconcile_tx,
        app_persistence_tx,
        reload_tx,
    );
    let initial = actor.published();
    let (published_tx, published_rx) = watch::channel(Arc::new(initial));
    let handle = RuntimeHandle {
        commands: command_tx,
        published: published_rx,
        events: event_tx.clone(),
    };
    let ingress = ObserverIngress {
        load: load_tx,
        thermal: thermal_tx,
        frequency: frequency_tx,
        logind_health: logind_health_tx,
        input_health: input_health_tx,
        runtime_events: runtime_event_tx,
        settings: settings_rx,
        frequency_targets,
    };
    let task = tokio::spawn(actor.run(
        command_rx,
        load_rx,
        thermal_rx,
        frequency_rx,
        logind_health_rx,
        input_health_rx,
        runtime_event_rx,
        reconcile_rx,
        app_persistence_rx,
        reload_rx,
        published_tx,
        event_tx,
    ));
    (handle, ingress, task)
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "orthogonal safety gates are intentionally explicit in the single-owner state machine"
)]
struct RuntimeActor {
    environment: Arc<dyn RuntimePlatform>,
    discovery: LinuxDiscovery,
    configuration: ResolvedConfiguration,
    configuration_paths: ConfigurationPaths,
    observer_settings: watch::Sender<ObserverSettings>,
    actuator: Option<Arc<FrequencyActuator>>,
    reconcile_tx: mpsc::Sender<ReconcileResult>,
    mutation_gate: Arc<Mutex<()>>,
    frequency_safety: Arc<FrequencySafetyFence>,
    app_persistence_tx: mpsc::Sender<AppPersistenceOutcome>,
    app_persist_in_flight: bool,
    reload_tx: mpsc::Sender<ReloadOutcome>,
    reload_in_flight: bool,
    started_at: Instant,
    mode: ModeSelection,
    active_workload: Option<ProcessInfo>,
    requested_workload_profile: Option<ProfileId>,
    observed: ObservedState,
    desired: Option<DesiredPlan>,
    applied: AppliedState,
    hints: HintSet,
    active_touch_contacts: ActiveTouchContacts,
    overrides: BTreeMap<TargetId, TimedOverride>,
    thermal_guards: BTreeMap<String, ThermalGuard>,
    thermal_state: ThermalState,
    thermal_caps: BTreeMap<TargetId, Hertz>,
    maximum_temperature: Option<MilliCelsius>,
    thermal_ready: bool,
    previous_cpu_times: Option<CpuTimeSnapshot>,
    last_load_success: Option<MonotonicMillis>,
    last_frequency_success: Option<MonotonicMillis>,
    heavy_load: HeavyLoadDetector,
    generation: u64,
    state_revision: u64,
    config_generation: u64,
    telemetry_sequence: u64,
    health_issues: BTreeMap<String, HealthIssue>,
    last_workload_check: MonotonicMillis,
    last_scheduler_scan: MonotonicMillis,
    scheduler_dirty: bool,
    applied_units: BTreeMap<String, SystemdUnitProperties>,
    reconcile_in_flight: bool,
    reconcile_pending: bool,
    inflight_frequency: bool,
    inflight_scheduler: bool,
    frequency_failures: u32,
    frequency_retry_not_before: MonotonicMillis,
    scheduler_failures: u32,
    scheduler_retry_not_before: MonotonicMillis,
    actuator_read_only: bool,
    startup_recovery_pending: bool,
    suspended: bool,
    accepting_control: bool,
    stop_requested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeStateSignature {
    control_generation: u64,
    config_generation: u64,
    mode: ModeSelection,
    active_workload: Option<ProcessIdentity>,
    effective_profile: ProfileId,
    dominant_scene: Scene,
    thermal_state: ThermalState,
    desired_frequencies: BTreeMap<TargetId, FrequencyLimits>,
    applied_frequencies: BTreeMap<TargetId, FrequencyLimits>,
    desired_tasks: BTreeMap<ProcessIdentity, TaskPlan>,
    applied_tasks: BTreeMap<ProcessIdentity, TaskPlan>,
    applied_units: BTreeMap<String, SystemdUnitProperties>,
    suspended: bool,
}

impl RuntimeActor {
    fn new(
        parts: RuntimeParts,
        observer_settings: watch::Sender<ObserverSettings>,
        reconcile_tx: mpsc::Sender<ReconcileResult>,
        app_persistence_tx: mpsc::Sender<AppPersistenceOutcome>,
        reload_tx: mpsc::Sender<ReloadOutcome>,
    ) -> Self {
        let thermal_guards = parts
            .configuration
            .thermal_zones
            .iter()
            .map(|zone| {
                (
                    zone.id.clone(),
                    ThermalGuard::new(ThermalThresholds::from(zone)),
                )
            })
            .collect();
        let (actuator_read_only, startup_recovery_pending) =
            actuator_health_snapshot(parts.actuator.as_deref());
        let mut actor = Self {
            environment: parts.environment,
            discovery: parts.discovery,
            configuration: parts.configuration,
            configuration_paths: parts.configuration_paths,
            observer_settings,
            actuator: parts.actuator,
            reconcile_tx,
            mutation_gate: Arc::new(Mutex::new(())),
            frequency_safety: Arc::new(FrequencySafetyFence::default()),
            app_persistence_tx,
            app_persist_in_flight: false,
            reload_tx,
            reload_in_flight: false,
            started_at: parts.started_at,
            mode: ModeSelection::Auto,
            active_workload: None,
            requested_workload_profile: None,
            observed: ObservedState {
                timestamp: MonotonicMillis::new(0),
                cpu_loads: BTreeMap::new(),
                frequencies: BTreeMap::new(),
                thermal: BTreeMap::new(),
                active_workload: None,
            },
            desired: None,
            applied: AppliedState::default(),
            hints: HintSet::new(),
            active_touch_contacts: ActiveTouchContacts::default(),
            overrides: BTreeMap::new(),
            thermal_guards,
            thermal_state: ThermalState::Degraded,
            thermal_caps: BTreeMap::new(),
            maximum_temperature: None,
            // Until the first trusted sample arrives the sensor-failure
            // envelope is active; startup must not create an unbounded window.
            thermal_ready: true,
            previous_cpu_times: None,
            last_load_success: None,
            last_frequency_success: None,
            heavy_load: HeavyLoadDetector::default(),
            generation: 0,
            state_revision: 0,
            config_generation: 1,
            telemetry_sequence: 0,
            health_issues: BTreeMap::new(),
            last_workload_check: MonotonicMillis::new(0),
            last_scheduler_scan: MonotonicMillis::new(0),
            scheduler_dirty: false,
            applied_units: BTreeMap::new(),
            reconcile_in_flight: false,
            reconcile_pending: false,
            inflight_frequency: false,
            inflight_scheduler: false,
            frequency_failures: 0,
            frequency_retry_not_before: MonotonicMillis::new(0),
            scheduler_failures: 0,
            scheduler_retry_not_before: MonotonicMillis::new(0),
            actuator_read_only,
            startup_recovery_pending,
            suspended: false,
            accepting_control: true,
            stop_requested: false,
        };
        actor.update_thermal_caps();
        actor.seed_health();
        actor
    }

    fn seed_health(&mut self) {
        self.health_issues.remove("scheduler.process_backend");
        self.health_issues.remove("scheduler.systemd_backend");
        for (index, warning) in self.configuration.warnings.iter().enumerate() {
            self.health_issues.insert(
                format!("discovery.warning.{index}"),
                issue(
                    format!("discovery.warning.{index}"),
                    "warning",
                    "discovery",
                    warning.clone(),
                ),
            );
        }
        if self.actuator.is_none() {
            self.health_issues.insert(
                "actuator.read_only".to_owned(),
                issue(
                    "actuator.read_only",
                    "warning",
                    "actuator",
                    "daemon was started without a mutation actuator",
                ),
            );
        }
        if self.configuration.policy.scheduler.enabled {
            let process_available = self
                .actuator
                .as_deref()
                .is_some_and(FrequencyActuator::has_process_backend);
            if !process_available {
                self.health_issues.insert(
                    "scheduler.process_backend".to_owned(),
                    issue(
                        "scheduler.process_backend",
                        "warning",
                        "scheduler",
                        "task scheduling is configured but its process backend is unavailable",
                    ),
                );
            }
            let systemd_available = self
                .actuator
                .as_deref()
                .is_some_and(FrequencyActuator::has_systemd_backend);
            if !self
                .configuration
                .policy
                .scheduler
                .cgroup_classes
                .is_empty()
                && !systemd_available
            {
                self.health_issues.insert(
                    "scheduler.systemd_backend".to_owned(),
                    issue(
                        "scheduler.systemd_backend",
                        "warning",
                        "scheduler",
                        "cgroup classes are configured but the systemd backend is unavailable",
                    ),
                );
            }
        }
        self.refresh_actuator_health();
    }

    fn state_signature(&self) -> RuntimeStateSignature {
        let effective_profile = self
            .desired
            .as_ref()
            .map_or(self.configuration.policy.default_profile, |plan| {
                plan.effective_profile
            });
        let dominant_scene = self
            .desired
            .as_ref()
            .map_or(Scene::Idle, |plan| plan.dominant_scene);
        RuntimeStateSignature {
            control_generation: self.generation,
            config_generation: self.config_generation,
            mode: self.mode,
            active_workload: self.active_workload.as_ref().map(|info| info.identity),
            effective_profile,
            dominant_scene,
            thermal_state: self.thermal_state,
            desired_frequencies: self.desired.as_ref().map_or_else(BTreeMap::new, |plan| {
                plan.frequencies
                    .iter()
                    .map(|(id, target)| (id.clone(), target.limits))
                    .collect()
            }),
            applied_frequencies: self
                .applied
                .frequencies
                .iter()
                .map(|(id, target)| (id.clone(), target.limits))
                .collect(),
            desired_tasks: self
                .desired
                .as_ref()
                .map_or_else(BTreeMap::new, |plan| plan.tasks.clone()),
            applied_tasks: self.applied.tasks.clone(),
            applied_units: self.applied_units.clone(),
            suspended: self.suspended,
        }
    }

    fn refresh_actuator_health(&mut self) {
        self.health_issues.remove("actuator.degraded");
        let Some(actuator) = &self.actuator else {
            self.actuator_read_only = true;
            self.startup_recovery_pending = false;
            return;
        };
        self.startup_recovery_pending = actuator.startup_recovery_required().unwrap_or(true);
        match actuator.mode() {
            Ok(ActuatorMode::ReadWrite) => {
                self.actuator_read_only = false;
            }
            Ok(ActuatorMode::ReadOnlyDegraded { reason }) => {
                self.actuator_read_only = true;
                self.health_issues.insert(
                    "actuator.degraded".to_owned(),
                    issue("actuator.degraded", "critical", "actuator", reason),
                );
            }
            Err(error) => {
                self.actuator_read_only = true;
                self.health_issues.insert(
                    "actuator.degraded".to_owned(),
                    issue(
                        "actuator.degraded",
                        "critical",
                        "actuator",
                        error.to_string(),
                    ),
                );
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "independent bounded/watch channels preserve observer failure isolation"
    )]
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut load: watch::Receiver<LoadObservation>,
        mut thermal: watch::Receiver<ThermalObservation>,
        mut frequency: watch::Receiver<FrequencyObservation>,
        mut logind_health: watch::Receiver<LogindHealthObservation>,
        mut input_health: watch::Receiver<InputHealthObservation>,
        mut runtime_events: mpsc::Receiver<RuntimeInput>,
        mut reconcile_results: mpsc::Receiver<ReconcileResult>,
        mut app_persistence_results: mpsc::Receiver<AppPersistenceOutcome>,
        mut reload_results: mpsc::Receiver<ReloadOutcome>,
        published: watch::Sender<Arc<PublishedState>>,
        events: broadcast::Sender<RuntimeEvent>,
    ) -> Result<(), RuntimeError> {
        let mut ticker = tokio::time::interval(REDUCER_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous_state = self.state_signature();
        let mut previous_health = self.health();
        let mut previous_actuator_read_only = self.actuator_read_only;
        let mut published_telemetry_sequence = self.telemetry_sequence;
        let mut published_uptime_seconds = self.started_at.elapsed().as_secs();

        while !self.stop_requested {
            let mut capabilities_changed = false;
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        self.stop_requested = true;
                        continue;
                    };
                    capabilities_changed = self.handle_command(command);
                }
                changed = load.changed() => {
                    if changed.is_ok()
                        && let Some((_, observation)) = load.borrow_and_update().clone()
                    {
                        self.reduce_load(observation);
                    }
                }
                changed = thermal.changed() => {
                    if changed.is_ok()
                        && let Some((_, observation)) = thermal.borrow_and_update().clone()
                    {
                        self.reduce_thermal(observation);
                    }
                }
                changed = frequency.changed() => {
                    if changed.is_ok()
                        && let Some((_, observation)) = frequency.borrow_and_update().clone()
                    {
                        self.reduce_frequencies(observation);
                    }
                }
                changed = logind_health.changed() => {
                    if changed.is_ok()
                        && let Some(observation) = logind_health.borrow_and_update().clone()
                    {
                        self.reduce_logind_health(observation);
                    }
                }
                changed = input_health.changed() => {
                    if changed.is_ok()
                        && let Some(observation) = input_health.borrow_and_update().clone()
                    {
                        self.reduce_input_health(observation);
                    }
                }
                event = runtime_events.recv() => {
                    if let Some(event) = event {
                        self.reduce_runtime_input(event);
                    }
                }
                result = reconcile_results.recv() => {
                    if let Some(result) = result {
                        self.finish_reconcile(result);
                    }
                }
                result = app_persistence_results.recv() => {
                    if let Some(result) = result {
                        self.finish_app_persistence(result);
                    }
                }
                result = reload_results.recv() => {
                    if let Some(result) = result {
                        capabilities_changed = self.finish_reload(result);
                    }
                }
                _ = ticker.tick() => {
                    self.on_tick();
                }
            }

            capabilities_changed |= self.actuator_read_only != previous_actuator_read_only;
            previous_actuator_read_only = self.actuator_read_only;
            let new_state = self.state_signature();
            let new_health = self.health();
            let state_changed = new_state != previous_state;
            let health_changed = new_health != previous_health;
            if state_changed {
                self.state_revision = self.state_revision.saturating_add(1);
                previous_state = new_state;
            }
            let uptime_seconds = self.started_at.elapsed().as_secs();
            let publish = state_changed
                || health_changed
                || capabilities_changed
                || self.telemetry_sequence != published_telemetry_sequence
                || uptime_seconds != published_uptime_seconds;
            if publish {
                published.send_replace(Arc::new(self.published()));
                published_telemetry_sequence = self.telemetry_sequence;
                published_uptime_seconds = uptime_seconds;
            }

            // The coherent snapshot must become visible before any signal
            // advertising its revision.  Otherwise a fast D-Bus consumer can
            // observe the previous property values after StateChanged.
            if state_changed {
                let _ = events.send(RuntimeEvent::StateChanged(self.state_revision));
            }
            if health_changed {
                let _ = events.send(RuntimeEvent::HealthChanged(new_health.clone()));
            }
            if capabilities_changed {
                let _ = events.send(RuntimeEvent::CapabilitiesChanged);
            }
            previous_health = new_health;
        }
        Ok(())
    }

    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::SetMode { mode, reply } => {
                let result = self
                    .require_accepting_control()
                    .and_then(|()| self.command_set_mode(&mode));
                let _ = reply.send(result);
            }
            Command::SetActiveWorkload {
                request,
                caller_uid,
                reply,
            } => {
                let result = self
                    .require_accepting_control()
                    .and_then(|()| self.command_set_workload(&request, caller_uid));
                let _ = reply.send(result);
            }
            Command::ClearActiveWorkload { caller_uid, reply } => {
                let result = self
                    .require_accepting_control()
                    .and_then(|()| self.command_clear_workload(caller_uid));
                let _ = reply.send(result);
            }
            Command::SetFrequencyOverrides { overrides, reply } => {
                let result = self
                    .require_accepting_control()
                    .and_then(|()| self.command_set_overrides(overrides));
                let _ = reply.send(result);
            }
            Command::ClearFrequencyOverrides { target_ids, reply } => {
                let result = self
                    .require_accepting_control()
                    .and_then(|()| self.command_clear_overrides(target_ids));
                let _ = reply.send(result);
            }
            Command::Reload { reply } => {
                if let Err(error) = self.require_accepting_control() {
                    let _ = reply.send(Err(error));
                } else {
                    self.start_reload(reply);
                }
            }
            Command::SetAppRule { rule, reply } => {
                if let Err(error) = self.require_accepting_control() {
                    let _ = reply.send(Err(error));
                } else {
                    self.start_set_app_rule(&rule, reply);
                }
            }
            Command::RemoveAppRule { id, reply } => {
                if let Err(error) = self.require_accepting_control() {
                    let _ = reply.send(Err(error));
                } else {
                    self.start_remove_app_rule(&id, reply);
                }
            }
            Command::BeginShutdown { reply } => {
                self.accepting_control = false;
                self.reconcile_pending = false;
                let _ = reply.send(Ok(()));
            }
            Command::Stop { reply } => {
                if self.app_persist_in_flight || self.reload_in_flight {
                    let _ = reply.send(Err(RuntimeError::Conflict(
                        if self.app_persist_in_flight {
                            APP_PERSISTENCE_IN_FLIGHT
                        } else {
                            RELOAD_IN_FLIGHT
                        }
                        .to_owned(),
                    )));
                    return false;
                }
                let result = self.restore_on_shutdown();
                self.stop_requested = true;
                let _ = reply.send(result);
            }
        }
        false
    }

    fn require_accepting_control(&self) -> Result<(), RuntimeError> {
        if self.accepting_control {
            Ok(())
        } else {
            Err(RuntimeError::Conflict(
                "daemon shutdown has started; mutation requests are closed".to_owned(),
            ))
        }
    }

    fn reduce_load(&mut self, observation: Result<CpuTimeSnapshot, String>) {
        let sample = match observation {
            Ok(sample) => {
                self.health_issues.remove("observer.load");
                self.health_issues.remove("observer.load_stale");
                sample
            }
            Err(message) => {
                self.health_issues.insert(
                    "observer.load".to_owned(),
                    issue("observer.load", "error", "load", message),
                );
                self.invalidate_load_state();
                self.evaluate_and_reconcile();
                return;
            }
        };

        self.last_load_success = Some(sample.observed_at);
        let previous = self.previous_cpu_times.replace(sample.clone());
        self.observed.timestamp = sample.observed_at;
        let Some(previous) = previous else {
            return;
        };

        self.observed.cpu_loads = sample
            .cpus
            .iter()
            .filter_map(|(cpu, current)| {
                previous
                    .cpus
                    .get(cpu)
                    .and_then(|old| current.utilization_since(*old))
                    .map(|utilization| (*cpu, utilization.clamp(0.0, 1.0)))
            })
            .collect();

        let elapsed_ms = sample
            .observed_at
            .saturating_duration_since(previous.observed_at);
        if self.configuration.policy.load.enabled {
            if let Some(utilization) = sample
                .aggregate
                .utilization_since(previous.aggregate)
                .map(|value| value.clamp(0.0, 1.0))
            {
                self.health_issues.remove("observer.load_counters");
                match self.heavy_load.update(
                    utilization,
                    elapsed_ms,
                    sample.observed_at,
                    &self.configuration.policy.load,
                ) {
                    Ok(HeavyLoadState::Heavy) => {
                        self.hints
                            .activate(Hint::persistent(Scene::Boost, sample.observed_at));
                    }
                    Ok(HeavyLoadState::Idle) => {
                        self.hints.deactivate(Scene::Boost);
                    }
                    Err(error) => {
                        self.health_issues.insert(
                            "policy.heavy_load".to_owned(),
                            issue("policy.heavy_load", "error", "policy", error.to_string()),
                        );
                    }
                }
            } else {
                self.health_issues.insert(
                    "observer.load_counters".to_owned(),
                    issue(
                        "observer.load_counters",
                        "warning",
                        "load",
                        "aggregate CPU counters moved backwards or had no elapsed time",
                    ),
                );
                self.invalidate_load_state();
            }
        } else {
            self.hints.deactivate(Scene::Boost);
            self.heavy_load = HeavyLoadDetector::default();
        }
        self.telemetry_sequence = self.telemetry_sequence.saturating_add(1);
        self.evaluate_and_reconcile();
    }

    fn invalidate_load_state(&mut self) {
        self.observed.cpu_loads.clear();
        self.previous_cpu_times = None;
        self.heavy_load = HeavyLoadDetector::default();
        self.hints.deactivate(Scene::Boost);
        self.telemetry_sequence = self.telemetry_sequence.saturating_add(1);
    }

    fn reduce_frequencies(&mut self, observation: Result<FrequencyObservationBatch, String>) {
        self.health_issues
            .retain(|code, _| !code.starts_with("observer.frequency."));
        match observation {
            Ok(batch) => {
                if !batch.readings.is_empty() {
                    self.last_frequency_success = Some(self.environment.monotonic_millis());
                }
                for (id, message) in batch.errors {
                    let code = format!("observer.frequency.{id}");
                    self.health_issues
                        .insert(code.clone(), issue(code, "error", "frequency", message));
                }
                self.observed.frequencies = batch.readings;
            }
            Err(message) => {
                self.observed.frequencies.clear();
                self.health_issues.insert(
                    "observer.frequency.worker".to_owned(),
                    issue("observer.frequency.worker", "error", "frequency", message),
                );
            }
        }
        self.telemetry_sequence = self.telemetry_sequence.saturating_add(1);
        self.evaluate_and_reconcile();
    }

    fn reduce_thermal(&mut self, observation: Result<Vec<ThermalSample>, String>) {
        let now = self.environment.monotonic_millis();
        self.observed.timestamp = now;
        self.thermal_ready = true;
        let samples = match observation {
            Ok(samples) => {
                self.health_issues.remove("observer.thermal");
                samples
            }
            Err(message) => {
                self.health_issues.insert(
                    "observer.thermal".to_owned(),
                    issue("observer.thermal", "critical", "thermal", message),
                );
                self.mark_all_thermal_unavailable(now);
                self.update_thermal_caps();
                self.evaluate_and_reconcile();
                return;
            }
        };

        let mut states = Vec::new();
        let mut maximum = None;
        for configured in &self.configuration.thermal_zones {
            let matched = samples.iter().find(|sample| {
                sample.zone_type == configured.zone_type
                    && configured.sysfs_path.as_ref().map_or_else(
                        || sample.zone_type == configured.zone_type,
                        |path| std::path::Path::new(path) == sample.path,
                    )
            });
            let reading = matched.map_or(
                ThermalReading {
                    temperature: None,
                    sampled_at: now,
                    health: SensorHealth::Unavailable,
                },
                |sample| sample.reading.clone(),
            );
            if reading.health == SensorHealth::Healthy
                && let Some(temperature) = reading.temperature
            {
                maximum =
                    Some(maximum.map_or(temperature, |old: MilliCelsius| old.max(temperature)));
            }
            self.observed
                .thermal
                .insert(configured.id.clone(), reading.clone());
            if let Some(guard) = self.thermal_guards.get_mut(&configured.id) {
                states.push(guard.update(now, &reading));
            }
        }
        self.maximum_temperature = maximum;
        self.thermal_state = worst_thermal_state(states);
        self.update_thermal_health();
        self.update_thermal_caps();
        self.telemetry_sequence = self.telemetry_sequence.saturating_add(1);
        self.evaluate_and_reconcile();
    }

    fn mark_all_thermal_unavailable(&mut self, now: MonotonicMillis) {
        for configured in &self.configuration.thermal_zones {
            let reading = ThermalReading {
                temperature: None,
                sampled_at: now,
                health: SensorHealth::Unavailable,
            };
            self.observed
                .thermal
                .insert(configured.id.clone(), reading.clone());
            if let Some(guard) = self.thermal_guards.get_mut(&configured.id) {
                guard.update(now, &reading);
            }
        }
        self.maximum_temperature = None;
        self.thermal_state = ThermalState::Degraded;
        self.update_thermal_health();
    }

    fn update_thermal_health(&mut self) {
        self.health_issues.remove("thermal.degraded");
        if self.thermal_state == ThermalState::Degraded {
            self.health_issues.insert(
                "thermal.degraded".to_owned(),
                issue(
                    "thermal.degraded",
                    "critical",
                    "thermal",
                    "a trusted thermal sensor is stale or unavailable; boost and new overrides are inhibited",
                ),
            );
        }
    }

    fn update_thermal_caps(&mut self) {
        let previous = std::mem::take(&mut self.thermal_caps);
        for (id, target) in &self.configuration.targets {
            let cap = match self.thermal_state {
                ThermalState::Normal => None,
                ThermalState::Warning => target
                    .automatic_policy
                    .as_ref()
                    .map(|policy| policy.efficient_cap),
                ThermalState::Throttled => target.automatic_policy.as_ref().map_or_else(
                    || middle_opp(target),
                    |policy| Some(policy.reference.min(policy.efficient_cap)),
                ),
                ThermalState::Critical => Some(target.critical_cap),
                ThermalState::Degraded => Some(target.sensor_failure_cap),
            };
            if let Some(cap) = cap {
                self.thermal_caps.insert(id.clone(), cap);
            }
        }
        let mut upper_caps = self.configuration.administrator_caps();
        for (id, thermal_cap) in &self.thermal_caps {
            upper_caps
                .entry(id.clone())
                .and_modify(|cap| *cap = (*cap).min(*thermal_cap))
                .or_insert(*thermal_cap);
        }
        match self.frequency_safety.replace_upper_caps(upper_caps) {
            Ok(_) => {
                self.health_issues.remove("safety.frequency_fence");
            }
            Err(message) => {
                self.actuator_read_only = true;
                self.health_issues.insert(
                    "safety.frequency_fence".to_owned(),
                    issue(
                        "safety.frequency_fence",
                        "critical",
                        "thermal",
                        format!("{message}; all further mutations are disabled"),
                    ),
                );
            }
        }
        let tightened = self.thermal_caps.iter().any(|(id, cap)| {
            previous
                .get(id)
                .is_none_or(|previous_cap| cap < previous_cap)
        });
        if tightened {
            // Safety envelopes are a separate failure domain. A scheduler or
            // earlier frequency failure must never postpone a stricter cap.
            self.clear_frequency_failure();
        }
    }

    fn reduce_runtime_input(&mut self, event: RuntimeInput) {
        match event {
            RuntimeInput::Hint(scene) => self.activate_external_hint(scene),
            RuntimeInput::Input(InputEvent::TouchDown { contact, .. }) => {
                if self.active_touch_contacts.press(contact) {
                    let now = self.environment.monotonic_millis();
                    self.hints.activate(Hint::persistent(Scene::Touch, now));
                    self.evaluate_and_reconcile();
                }
            }
            RuntimeInput::Input(InputEvent::TouchUp { contact, .. }) => {
                if self.active_touch_contacts.release(contact) {
                    self.hints.deactivate(Scene::Touch);
                    self.activate_external_hint(Scene::Trigger);
                }
            }
            RuntimeInput::Input(InputEvent::Gesture { .. }) => {
                self.activate_external_hint(Scene::Gesture);
            }
            RuntimeInput::Input(InputEvent::Resync { device }) => {
                if self.active_touch_contacts.resync(device) {
                    self.hints.deactivate(Scene::Touch);
                    self.evaluate_and_reconcile();
                }
            }
            RuntimeInput::PrepareForSleep {
                sleeping,
                completion,
            } => {
                let result = if sleeping {
                    self.prepare_to_sleep()
                } else {
                    self.resume_from_sleep();
                    Ok(())
                };
                let _ = completion.send(result);
            }
        }
    }

    fn reduce_logind_health(&mut self, observation: Result<(), String>) {
        match observation {
            Ok(()) => {
                self.health_issues.remove("observer.logind");
            }
            Err(message) => {
                self.health_issues.insert(
                    "observer.logind".to_owned(),
                    issue("observer.logind", "error", "logind", message),
                );
            }
        }
    }

    fn reduce_input_health(&mut self, observation: Result<(), String>) {
        match observation {
            Ok(()) => {
                self.health_issues.remove("observer.input");
            }
            Err(message) => {
                self.health_issues.insert(
                    "observer.input".to_owned(),
                    issue("observer.input", "warning", "input", message),
                );
            }
        }
    }

    fn prepare_to_sleep(&mut self) -> Result<(), String> {
        self.suspended = true;
        self.hints = HintSet::new();
        self.active_touch_contacts.clear();
        let Some(actuator) = self.actuator.clone() else {
            return Ok(());
        };
        let mutation_gate = self.mutation_gate.clone();
        let _gate = mutation_gate
            .lock()
            .map_err(|_| "runtime mutation gate was poisoned".to_owned())?;
        match actuator.restore_all() {
            Ok(()) => {
                self.applied = AppliedState::default();
                self.applied_units.clear();
                self.observed.frequencies.clear();
                Ok(())
            }
            Err(error) => {
                let message = format!("failed to restore resources before sleep: {error}");
                self.health_issues.insert(
                    "actuator.sleep_restore".to_owned(),
                    issue(
                        "actuator.sleep_restore",
                        "critical",
                        "actuator",
                        message.clone(),
                    ),
                );
                self.refresh_actuator_health();
                Err(message)
            }
        }
    }

    fn resume_from_sleep(&mut self) {
        self.suspended = false;
        self.thermal_ready = true;
        self.thermal_state = ThermalState::Degraded;
        self.mark_all_thermal_unavailable(self.environment.monotonic_millis());
        self.update_thermal_caps();
        self.scheduler_dirty = true;
        self.activate_external_hint(Scene::Wake);
    }

    fn activate_external_hint(&mut self, scene: Scene) {
        let now = self.environment.monotonic_millis();
        let input = &self.configuration.policy.input;
        let duration = match scene {
            Scene::Trigger => input.trigger_duration_ms,
            Scene::Gesture => input.gesture_duration_ms,
            Scene::Switch => input.switch_duration_ms,
            Scene::Wake => input.wake_duration_ms,
            Scene::Touch | Scene::Boost => 0,
            Scene::Idle => return,
        };
        if duration == 0 {
            self.hints.activate(Hint::persistent(scene, now));
        } else {
            self.hints.activate(Hint::with_ttl(scene, now, duration));
        }
        self.evaluate_and_reconcile();
    }

    fn on_tick(&mut self) {
        let now = self.environment.monotonic_millis();
        let thermal_changed = self.refresh_thermal_staleness(now);
        let load_changed = self.refresh_load_staleness(now);
        self.refresh_frequency_staleness(now);
        let hints_expired = self.hints.expire(now);
        let expired_overrides = self
            .overrides
            .iter()
            .filter(|(_, request)| request.expires_at.is_some_and(|expires| now >= expires))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let had_expired_overrides = !expired_overrides.is_empty();
        for id in &expired_overrides {
            self.overrides.remove(id);
        }
        if had_expired_overrides {
            self.generation = self.generation.saturating_add(1);
        }
        if now.saturating_duration_since(self.last_workload_check) >= WORKLOAD_CHECK_INTERVAL_MS {
            self.last_workload_check = now;
            self.check_workload_identity();
        }
        if self.active_workload.is_some()
            && self.configuration.policy.scheduler.enabled
            && now.saturating_duration_since(self.last_scheduler_scan) >= WORKLOAD_CHECK_INTERVAL_MS
        {
            self.last_scheduler_scan = now;
            self.scheduler_dirty = true;
        }
        if hints_expired
            || thermal_changed
            || load_changed
            || had_expired_overrides
            || self.desired.is_none()
            || self.scheduler_dirty && self.thermal_ready && !self.suspended
            || self.frequency_failures != 0 && now >= self.frequency_retry_not_before
            || self.scheduler_failures != 0 && now >= self.scheduler_retry_not_before
        {
            self.evaluate_and_reconcile();
        }
    }

    fn refresh_load_staleness(&mut self, now: MonotonicMillis) -> bool {
        let Some(last) = self.last_load_success else {
            return false;
        };
        let stale_after_ms = self
            .configuration
            .policy
            .load
            .sample_interval_ms
            .saturating_mul(5)
            .max(1_000);
        if last <= now && now.saturating_duration_since(last) <= stale_after_ms {
            return false;
        }
        self.health_issues.insert(
            "observer.load_stale".to_owned(),
            issue(
                "observer.load_stale",
                "warning",
                "load",
                "CPU load telemetry is stale; demand and persistent boost were cleared",
            ),
        );
        let changed =
            !self.observed.cpu_loads.is_empty() || self.hints.contains_active(Scene::Boost, now);
        if changed {
            self.invalidate_load_state();
        }
        changed
    }

    fn refresh_frequency_staleness(&mut self, now: MonotonicMillis) {
        if self.frequency_observations_stale(now) {
            self.health_issues.insert(
                "observer.frequency.stale".to_owned(),
                issue(
                    "observer.frequency.stale",
                    "warning",
                    "frequency",
                    "frequency limit telemetry is stale; reported observations may no longer match the kernel",
                ),
            );
        } else {
            self.health_issues.remove("observer.frequency.stale");
        }
    }

    fn frequency_observations_stale(&self, now: MonotonicMillis) -> bool {
        if self.configuration.targets.is_empty() {
            return false;
        }
        frequency_sample_is_stale(self.last_frequency_success, now)
    }

    fn refresh_thermal_staleness(&mut self, now: MonotonicMillis) -> bool {
        if !self.thermal_ready {
            return false;
        }
        let previous = self.thermal_state;
        let mut reading_changed = false;
        let mut states = Vec::with_capacity(self.configuration.thermal_zones.len());
        let mut maximum = None;
        for configured in &self.configuration.thermal_zones {
            let reading = self
                .observed
                .thermal
                .entry(configured.id.clone())
                .or_insert(ThermalReading {
                    temperature: None,
                    sampled_at: now,
                    health: SensorHealth::Unavailable,
                });
            if reading.health == SensorHealth::Healthy
                && (reading.sampled_at > now
                    || now.saturating_duration_since(reading.sampled_at)
                        > configured.stale_after_ms)
            {
                reading.health = SensorHealth::Stale;
                reading_changed = true;
            }
            if reading.health == SensorHealth::Healthy
                && let Some(temperature) = reading.temperature
            {
                maximum =
                    Some(maximum.map_or(temperature, |old: MilliCelsius| old.max(temperature)));
            }
            if let Some(guard) = self.thermal_guards.get_mut(&configured.id) {
                states.push(guard.update(now, reading));
            }
        }
        self.maximum_temperature = maximum;
        self.thermal_state = worst_thermal_state(states);
        self.update_thermal_health();
        self.update_thermal_caps();
        reading_changed || self.thermal_state != previous
    }

    fn command_set_mode(&mut self, value: &str) -> Result<MutationReceipt, RuntimeError> {
        let mode = parse_mode(value)?;
        if self.mode == mode {
            return Ok(receipt(
                self.generation,
                Vec::new(),
                "mode was already selected",
            ));
        }
        self.mode = mode;
        self.generation = self.generation.saturating_add(1);
        self.evaluate_and_reconcile();
        Ok(receipt(
            self.generation,
            vec!["mode".to_owned()],
            format!("mode changed to {}", mode_name(mode)),
        ))
    }

    fn command_set_workload(
        &mut self,
        request: &WorkloadRequest,
        caller_uid: u32,
    ) -> Result<MutationReceipt, RuntimeError> {
        if request.pid == 0 {
            return Err(RuntimeError::InvalidArgument(
                "workload PID must be non-zero".to_owned(),
            ));
        }
        if request.reason.len() > 256 {
            return Err(RuntimeError::InvalidArgument(
                "workload audit reason exceeds 256 bytes".to_owned(),
            ));
        }
        let observed = self
            .environment
            .process_identity(ProcessId::new(request.pid))
            .map_err(|error| RuntimeError::NotFound(error.to_string()))?;
        if caller_uid != 0
            && (!observed.owner_control_safe || observed.identity.uid.get() != caller_uid)
        {
            return Err(RuntimeError::NotAuthorized(
                "non-root callers may only select a process whose real/effective/saved/fs UIDs all equal their UID".to_owned(),
            ));
        }
        let requested_profile = if request.mode.is_empty() {
            None
        } else {
            Some(parse_profile(&request.mode)?)
        };
        let changed = self.active_workload.as_ref().map(|info| info.identity)
            != Some(observed.identity)
            || self.requested_workload_profile != requested_profile;
        if !changed {
            return Ok(receipt(
                self.generation,
                Vec::new(),
                "workload was already active",
            ));
        }
        let observed_identity = observed.identity;
        self.active_workload = Some(observed);
        self.requested_workload_profile = requested_profile;
        self.observed.active_workload = Some(observed_identity);
        self.scheduler_dirty = true;
        let now = self.environment.monotonic_millis();
        self.hints.activate(Hint::with_ttl(
            Scene::Switch,
            now,
            self.configuration.policy.input.switch_duration_ms,
        ));
        self.generation = self.generation.saturating_add(1);
        self.evaluate_and_reconcile();
        Ok(receipt(
            self.generation,
            vec![format!("workload:{}", request.pid)],
            "active workload selected",
        ))
    }

    fn command_clear_workload(&mut self, caller_uid: u32) -> Result<MutationReceipt, RuntimeError> {
        let Some(active_identity) = self.active_workload.as_ref().map(|active| active.identity)
        else {
            return Ok(receipt(
                self.generation,
                Vec::new(),
                "no workload was active",
            ));
        };
        let current = self
            .environment
            .process_identity(active_identity.pid)
            .map_err(|error| RuntimeError::Conflict(error.to_string()))?;
        if current.identity != active_identity {
            return Err(RuntimeError::Conflict(
                "active workload exited or its PID was reused".to_owned(),
            ));
        }
        if caller_uid != 0
            && (!current.owner_control_safe || current.identity.uid.get() != caller_uid)
        {
            return Err(RuntimeError::NotAuthorized(
                "non-root callers may only clear a workload owned by their UID".to_owned(),
            ));
        }
        let pid = active_identity.pid;
        self.active_workload = None;
        self.requested_workload_profile = None;
        self.observed.active_workload = None;
        self.scheduler_dirty = true;
        self.generation = self.generation.saturating_add(1);
        self.evaluate_and_reconcile();
        Ok(receipt(
            self.generation,
            vec![format!("workload:{}", pid.get())],
            "active workload cleared",
        ))
    }

    fn command_set_overrides(
        &mut self,
        requests: Vec<FrequencyOverride>,
    ) -> Result<MutationReceipt, RuntimeError> {
        if !self.thermal_ready || self.thermal_state == ThermalState::Degraded {
            return Err(RuntimeError::Degraded(
                "trusted thermal data is not currently healthy".to_owned(),
            ));
        }
        if requests.is_empty() {
            return Err(RuntimeError::InvalidArgument(
                "at least one override is required".to_owned(),
            ));
        }
        self.require_writable_actuator()?;
        let now = self.environment.monotonic_millis();
        let mut parsed = Vec::with_capacity(requests.len());
        let mut seen = BTreeSet::new();
        for request in requests {
            let id = TargetId::new(request.target_id.clone())
                .map_err(|error| RuntimeError::InvalidArgument(error.to_string()))?;
            if !seen.insert(id.clone()) {
                return Err(RuntimeError::InvalidArgument(format!(
                    "duplicate target {id}"
                )));
            }
            let target = self
                .configuration
                .targets
                .get(&id)
                .ok_or_else(|| RuntimeError::NotFound(id.to_string()))?;
            let minimum = Hertz::new(request.min_hz);
            let maximum = Hertz::new(request.max_hz);
            let limits = FrequencyLimits::new(minimum, maximum)
                .map_err(|error| RuntimeError::InvalidArgument(error.to_string()))?;
            if limits.min < target.hardware_limits.min || limits.max > target.hardware_limits.max {
                return Err(RuntimeError::InvalidArgument(format!(
                    "{id}: override lies outside advertised hardware limits"
                )));
            }
            let expires_at = (request.ttl_ms != 0).then(|| now.saturating_add(request.ttl_ms));
            parsed.push((id, TimedOverride { limits, expires_at }));
        }
        let changed_ids = parsed
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>();
        for (id, request) in parsed {
            self.overrides.insert(id, request);
        }
        self.generation = self.generation.saturating_add(1);
        self.evaluate_and_reconcile();
        Ok(receipt(
            self.generation,
            changed_ids,
            "frequency override batch accepted",
        ))
    }

    fn command_clear_overrides(
        &mut self,
        requested_ids: Vec<String>,
    ) -> Result<MutationReceipt, RuntimeError> {
        self.require_writable_actuator()?;
        let ids = if requested_ids.is_empty() {
            self.overrides.keys().cloned().collect::<Vec<_>>()
        } else {
            requested_ids
                .into_iter()
                .map(|value| {
                    TargetId::new(value)
                        .map_err(|error| RuntimeError::InvalidArgument(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let changed = ids
            .iter()
            .filter(|id| self.overrides.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in &changed {
            self.overrides.remove(id);
        }
        if changed.is_empty() {
            return Ok(receipt(
                self.generation,
                Vec::new(),
                "no matching overrides were active",
            ));
        }
        self.generation = self.generation.saturating_add(1);
        self.evaluate_and_reconcile();
        Ok(receipt(
            self.generation,
            changed.iter().map(ToString::to_string).collect(),
            "frequency overrides cleared",
        ))
    }

    fn start_reload(&mut self, reply: oneshot::Sender<Result<ReloadReport, RuntimeError>>) {
        if self.app_persist_in_flight || self.reload_in_flight {
            let _ = reply.send(Err(RuntimeError::Conflict(
                if self.app_persist_in_flight {
                    APP_PERSISTENCE_IN_FLIGHT
                } else {
                    RELOAD_IN_FLIGHT
                }
                .to_owned(),
            )));
            return;
        }
        let paths = self.configuration_paths.clone();
        let discovery = self.discovery.clone();
        let sender = self.reload_tx.clone();
        self.reload_in_flight = true;
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let candidate = ResolvedConfiguration::load(&paths, &discovery)
                    .map_err(|error| RuntimeError::Validation(format!("{error:#}")))?;
                candidate
                    .actuator_registry()
                    .map_err(|error| RuntimeError::Validation(format!("{error:#}")))?;
                Ok(candidate)
            })
            .await
            .map_err(|error| {
                RuntimeError::Internal(format!("blocking configuration reload failed: {error}"))
            })
            .and_then(std::convert::identity);
            let _ = sender.send(ReloadOutcome { reply, result }).await;
        });
    }

    fn finish_reload(&mut self, outcome: ReloadOutcome) -> bool {
        self.reload_in_flight = false;
        let result = outcome
            .result
            .and_then(|candidate| self.apply_reload_candidate(candidate));
        let changed = result.is_ok();
        let _ = outcome.reply.send(result);
        changed
    }

    fn apply_reload_candidate(
        &mut self,
        candidate: ResolvedConfiguration,
    ) -> Result<ReloadReport, RuntimeError> {
        if target_signature(&candidate) != target_signature(&self.configuration) {
            return Err(RuntimeError::Validation(
                "device target mapping changed; safely restore and restart the daemon to adopt it"
                    .to_owned(),
            ));
        }
        // A successful reload is a mutation barrier, not just an in-memory
        // pointer swap. Waiting for the gate here guarantees that no task,
        // cgroup or frequency transaction built from the previous generation
        // can write after the new generation has been published.
        let mutation_gate = self.mutation_gate.clone();
        let barrier = mutation_gate
            .lock()
            .map_err(|_| RuntimeError::Internal("runtime mutation gate was poisoned".to_owned()))?;
        let warnings = candidate.warnings.clone();
        let observer_settings = ObserverSettings::from_configuration(&candidate);
        self.configuration = candidate;
        self.observer_settings.send_replace(observer_settings);
        self.scheduler_dirty = true;
        self.thermal_guards = self
            .configuration
            .thermal_zones
            .iter()
            .map(|zone| {
                (
                    zone.id.clone(),
                    ThermalGuard::new(ThermalThresholds::from(zone)),
                )
            })
            .collect();
        self.thermal_ready = true;
        self.thermal_state = ThermalState::Degraded;
        self.mark_all_thermal_unavailable(self.environment.monotonic_millis());
        self.update_thermal_caps();
        self.config_generation = self.config_generation.saturating_add(1);
        self.generation = self.generation.saturating_add(1);
        self.health_issues
            .retain(|code, _| !code.starts_with("discovery.warning."));
        self.seed_health();
        drop(barrier);
        self.evaluate_and_reconcile();
        Ok(ReloadReport {
            config_generation: self.config_generation,
            warnings,
            message:
                "configuration generation swapped under the sensor-failure envelope; awaiting a fresh thermal sample"
                    .to_owned(),
        })
    }

    fn start_set_app_rule(
        &mut self,
        rule: &ApiAppRule,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    ) {
        if self.app_persist_in_flight || self.reload_in_flight {
            let _ = reply.send(Err(RuntimeError::Conflict(
                if self.app_persist_in_flight {
                    APP_PERSISTENCE_IN_FLIGHT
                } else {
                    RELOAD_IN_FLIGHT
                }
                .to_owned(),
            )));
            return;
        }
        let converted = match api_rule_to_core(rule) {
            Ok(converted) => converted,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let mut candidate = self.configuration.apps.clone();
        if let Some(existing) = candidate
            .rules
            .iter_mut()
            .find(|item| item.id == converted.id)
        {
            *existing = converted;
        } else {
            candidate.rules.push(converted);
        }
        self.start_app_persistence(
            candidate,
            format!("app-rule:{}", rule.id),
            "application rule stored",
            reply,
        );
    }

    fn start_remove_app_rule(
        &mut self,
        id: &str,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    ) {
        if self.app_persist_in_flight || self.reload_in_flight {
            let _ = reply.send(Err(RuntimeError::Conflict(
                if self.app_persist_in_flight {
                    APP_PERSISTENCE_IN_FLIGHT
                } else {
                    RELOAD_IN_FLIGHT
                }
                .to_owned(),
            )));
            return;
        }
        let mut candidate = self.configuration.apps.clone();
        let before = candidate.rules.len();
        candidate.rules.retain(|rule| rule.id != id);
        if candidate.rules.len() == before {
            let _ = reply.send(Err(RuntimeError::NotFound(format!(
                "application rule {id}"
            ))));
            return;
        }
        self.start_app_persistence(
            candidate,
            format!("app-rule:{id}"),
            "application rule removed",
            reply,
        );
    }

    fn start_app_persistence(
        &mut self,
        candidate: AppsConfig,
        changed_id: String,
        message: &'static str,
        reply: oneshot::Sender<Result<MutationReceipt, RuntimeError>>,
    ) {
        let persisted = candidate.clone();
        let paths = self.configuration_paths.clone();
        let sender = self.app_persistence_tx.clone();
        self.app_persist_in_flight = true;
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || persist_apps(&paths, &persisted))
                .await
                .map_err(|error| {
                    RuntimeError::Internal(format!(
                        "blocking application persistence task failed: {error}"
                    ))
                })
                .and_then(std::convert::identity);
            let _ = sender
                .send(AppPersistenceOutcome {
                    candidate,
                    changed_id,
                    message,
                    reply,
                    result,
                })
                .await;
        });
    }

    fn finish_app_persistence(&mut self, outcome: AppPersistenceOutcome) {
        self.app_persist_in_flight = false;
        let result = outcome.result.map(|()| {
            self.configuration.apps = outcome.candidate;
            self.config_generation = self.config_generation.saturating_add(1);
            self.generation = self.generation.saturating_add(1);
            self.evaluate_and_reconcile();
            receipt(self.generation, vec![outcome.changed_id], outcome.message)
        });
        let _ = outcome.reply.send(result);
    }

    fn evaluate_and_reconcile(&mut self) {
        let now = self.environment.monotonic_millis();
        self.hints.expire(now);
        let manual_overrides = self
            .overrides
            .iter()
            .map(|(id, request)| (id.clone(), request.limits))
            .collect::<BTreeMap<_, _>>();
        let app_profile = self.requested_workload_profile.or_else(|| {
            self.active_workload
                .as_ref()
                .and_then(|info| self.match_app_profile(info))
        });
        let cpu_targets = self.configuration.cpu_target_policies();
        let manual_targets = self.configuration.manual_target_policies();
        let administrator_caps = self.configuration.administrator_caps();
        let input = uperf_core::PolicyInput {
            generation: self.generation,
            observed: &self.observed,
            mode: self.mode,
            app_profile,
            hints: &self.hints,
            cpu_targets: &cpu_targets,
            manual_target_policies: &manual_targets,
            manual_overrides: &manual_overrides,
            administrator_caps: &administrator_caps,
            thermal_caps: &self.thermal_caps,
            thermal_degraded: self.thermal_state == ThermalState::Degraded,
        };
        let mut desired = match self.configuration.policy_engine.evaluate(&input) {
            Ok(desired) => {
                self.health_issues.remove("policy.evaluate");
                desired
            }
            Err(error) => {
                self.health_issues.insert(
                    "policy.evaluate".to_owned(),
                    issue("policy.evaluate", "error", "policy", error.to_string()),
                );
                return;
            }
        };
        if let Some(previous) = &self.desired {
            desired.tasks.clone_from(&previous.tasks);
        }
        self.desired = Some(desired.clone());

        if !self.thermal_ready || self.suspended || !self.accepting_control {
            return;
        }
        let Some(actuator) = self.actuator.clone() else {
            self.scheduler_dirty = false;
            return;
        };
        if self.actuator_read_only {
            return;
        }
        if self.reconcile_in_flight {
            self.reconcile_pending = true;
            return;
        }

        let reconcile_frequencies =
            self.frequency_needs_reconcile(&desired) && now >= self.frequency_retry_not_before;
        let reconcile_scheduler = self.scheduler_dirty && now >= self.scheduler_retry_not_before;
        if !reconcile_frequencies && !reconcile_scheduler {
            return;
        }

        let job = ReconcileJob {
            actuator,
            environment: self.environment.clone(),
            policy_engine: self.configuration.policy_engine.clone(),
            active_workload: self.active_workload.clone(),
            desired,
            applied: self.applied.clone(),
            applied_units: self.applied_units.clone(),
            reconcile_frequencies,
            reconcile_scheduler,
            mutation_gate: self.mutation_gate.clone(),
            frequency_safety: self.frequency_safety.clone(),
        };
        let sender = self.reconcile_tx.clone();
        self.reconcile_in_flight = true;
        self.reconcile_pending = false;
        self.inflight_frequency = reconcile_frequencies;
        self.inflight_scheduler = reconcile_scheduler;
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || crate::reconcile::run(&job))
                .await
                .map_err(|error| format!("blocking reconciler task failed: {error}"));
            let _ = sender.send(result).await;
        });
    }

    fn frequency_needs_reconcile(&self, desired: &DesiredPlan) -> bool {
        desired.frequencies.iter().any(|(id, target)| {
            self.applied
                .frequencies
                .get(id)
                .is_none_or(|applied| applied.limits != target.limits)
                || self
                    .observed
                    .frequencies
                    .get(id)
                    .is_none_or(|observed| observed.limits != target.limits)
        }) || self
            .applied
            .frequencies
            .keys()
            .any(|id| !desired.frequencies.contains_key(id))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one reducer transition atomically consumes every reconciler failure domain"
    )]
    fn finish_reconcile(&mut self, result: ReconcileResult) {
        let was_pending = self.reconcile_pending;
        let attempted_frequency = self.inflight_frequency;
        let attempted_scheduler = self.inflight_scheduler;
        self.reconcile_in_flight = false;
        self.reconcile_pending = false;
        self.inflight_frequency = false;
        self.inflight_scheduler = false;

        match result {
            Ok(outcome) => {
                let frequency_succeeded =
                    outcome.frequency_attempted && outcome.frequency_error.is_none();
                if !self.suspended {
                    let removed = self
                        .applied
                        .frequencies
                        .keys()
                        .filter(|id| !outcome.applied.frequencies.contains_key(*id))
                        .cloned()
                        .collect::<Vec<_>>();
                    self.applied = outcome.applied;
                    self.applied_units = outcome.applied_units;
                    if frequency_succeeded {
                        for id in removed {
                            self.observed.frequencies.remove(&id);
                        }
                        for (id, applied) in &self.applied.frequencies {
                            self.observed.frequencies.insert(
                                id.clone(),
                                ObservedFrequency {
                                    limits: applied.limits,
                                    // The actuator verifies the two limit
                                    // nodes, but it does not read the
                                    // instantaneous clock node.
                                    current: None,
                                },
                            );
                        }
                        self.last_frequency_success = Some(self.environment.monotonic_millis());
                        self.health_issues.remove("observer.frequency.stale");
                    }
                    if !was_pending
                        && outcome.scheduler_attempted
                        && outcome.scheduler_error.is_none()
                    {
                        if let Some(desired) = &mut self.desired {
                            desired.tasks = outcome.desired.tasks;
                        }
                        self.scheduler_dirty = false;
                    }
                }
                if outcome.scheduler_attempted {
                    match outcome.scheduler_warning {
                        Some(message) => {
                            self.health_issues.insert(
                                "scheduler.cgroup_ownership".to_owned(),
                                issue(
                                    "scheduler.cgroup_ownership",
                                    "warning",
                                    "scheduler",
                                    message,
                                ),
                            );
                        }
                        None => {
                            self.health_issues.remove("scheduler.cgroup_ownership");
                        }
                    }
                }
                if outcome.frequency_attempted {
                    if let Some(error) = outcome.frequency_error {
                        self.record_frequency_failure(&error);
                    } else {
                        self.clear_frequency_failure();
                    }
                }
                if outcome.scheduler_attempted {
                    if let Some(error) = outcome.scheduler_error {
                        self.record_scheduler_failure(&error);
                    } else {
                        self.clear_scheduler_failure();
                    }
                }
            }
            Err(error) => {
                if attempted_frequency {
                    self.record_frequency_failure(&error);
                }
                if attempted_scheduler {
                    self.record_scheduler_failure(&error);
                }
            }
        }

        self.refresh_actuator_health();
        if !self.suspended
            && !self.scheduler_dirty
            && let Some(desired) = &self.desired
            && !self.frequency_needs_reconcile(desired)
        {
            self.applied.generation = desired.generation;
        }
        if was_pending
            || self.scheduler_dirty
            || self
                .desired
                .as_ref()
                .is_some_and(|desired| self.frequency_needs_reconcile(desired))
        {
            self.evaluate_and_reconcile();
        }
    }

    fn record_frequency_failure(&mut self, message: &str) {
        self.frequency_failures = self.frequency_failures.saturating_add(1);
        let delay_ms = retry_delay_ms(self.frequency_failures);
        self.frequency_retry_not_before =
            self.environment.monotonic_millis().saturating_add(delay_ms);
        self.health_issues.insert(
            "reconciler.frequency_backoff".to_owned(),
            issue(
                "reconciler.frequency_backoff",
                "error",
                "reconciler",
                format!("{message}; retrying frequency control in {delay_ms} ms"),
            ),
        );
    }

    fn clear_frequency_failure(&mut self) {
        self.frequency_failures = 0;
        self.frequency_retry_not_before = MonotonicMillis::new(0);
        self.health_issues.remove("reconciler.frequency_backoff");
    }

    fn record_scheduler_failure(&mut self, message: &str) {
        self.scheduler_failures = self.scheduler_failures.saturating_add(1);
        let delay_ms = retry_delay_ms(self.scheduler_failures);
        self.scheduler_retry_not_before =
            self.environment.monotonic_millis().saturating_add(delay_ms);
        self.health_issues.insert(
            "reconciler.scheduler_backoff".to_owned(),
            issue(
                "reconciler.scheduler_backoff",
                "error",
                "scheduler",
                format!("{message}; retrying scheduler control in {delay_ms} ms"),
            ),
        );
    }

    fn clear_scheduler_failure(&mut self) {
        self.scheduler_failures = 0;
        self.scheduler_retry_not_before = MonotonicMillis::new(0);
        self.health_issues.remove("reconciler.scheduler_backoff");
    }

    fn require_writable_actuator(&self) -> Result<(), RuntimeError> {
        if self.actuator.is_none() {
            return Err(RuntimeError::Degraded(
                "daemon is running read-only".to_owned(),
            ));
        }
        if self.actuator_read_only {
            let reason = self
                .health_issues
                .get("actuator.degraded")
                .map_or("actuator is read-only degraded", |issue| {
                    issue.message.as_str()
                });
            Err(RuntimeError::Degraded(reason.to_owned()))
        } else {
            Ok(())
        }
    }

    fn check_workload_identity(&mut self) {
        let Some(active) = self.active_workload.clone() else {
            return;
        };
        if let Ok(current) = self.environment.process_identity(active.identity.pid)
            && current.identity == active.identity
        {
            if current != active {
                self.active_workload = Some(current);
                self.scheduler_dirty = true;
                self.generation = self.generation.saturating_add(1);
                self.evaluate_and_reconcile();
            }
            return;
        }
        self.active_workload = None;
        self.requested_workload_profile = None;
        self.observed.active_workload = None;
        self.scheduler_dirty = true;
        self.generation = self.generation.saturating_add(1);
        self.health_issues.insert(
            "workload.exited".to_owned(),
            issue(
                "workload.exited",
                "info",
                "workload",
                "the active workload exited or its PID was reused",
            ),
        );
        self.evaluate_and_reconcile();
    }

    fn match_app_profile(&self, process: &ProcessInfo) -> Option<ProfileId> {
        self.configuration.app_rule_engine.match_profile(process)
    }

    fn restore_on_shutdown(&mut self) -> Result<(), RuntimeError> {
        let Some(actuator) = &self.actuator else {
            return Ok(());
        };
        let mutation_gate = self.mutation_gate.clone();
        let _gate = mutation_gate
            .lock()
            .map_err(|_| RuntimeError::Internal("runtime mutation gate was poisoned".to_owned()))?;
        actuator
            .restore_all()
            .map_err(|error| RuntimeError::Degraded(error.to_string()))?;
        self.applied = AppliedState::default();
        self.applied_units.clear();
        self.observed.frequencies.clear();
        Ok(())
    }

    fn health(&self) -> HealthStatus {
        let read_only = self.actuator_read_only;
        let recovery_pending = self.startup_recovery_pending;
        let issues = self.health_issues.values().cloned().collect::<Vec<_>>();
        let failed = issues
            .iter()
            .any(|issue| issue.severity == "critical" && issue.component != "thermal");
        let degraded = read_only
            || issues
                .iter()
                .any(|issue| matches!(issue.severity.as_str(), "warning" | "error" | "critical"));
        HealthStatus {
            state: if failed {
                "failed"
            } else if degraded {
                "degraded"
            } else {
                "healthy"
            }
            .to_owned(),
            read_only,
            recovery_pending,
            summary: if failed {
                "one or more mandatory components failed"
            } else if degraded {
                "running with one or more safety or capability restrictions"
            } else {
                "all mandatory components are healthy"
            }
            .to_owned(),
            issues,
        }
    }

    fn published(&self) -> PublishedState {
        let health = self.health();
        let effective_profile = self.desired.as_ref().map_or_else(
            || self.configuration.policy.default_profile.to_string(),
            |plan| plan.effective_profile.to_string(),
        );
        let dominant_scene = self
            .desired
            .as_ref()
            .map_or(Scene::Idle, |plan| plan.dominant_scene);
        let frequencies = self.frequency_statuses();
        let thermal = self.thermal_status();
        let status = DaemonStatus {
            api_version: ApiVersion::CURRENT,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            state: if self.stop_requested {
                "stopping"
            } else if health.state == "healthy" {
                "running"
            } else {
                "degraded"
            }
            .to_owned(),
            health,
            mode: mode_name(self.mode).to_owned(),
            effective_profile: effective_profile.clone(),
            dominant_scene: dominant_scene.to_string(),
            active_workload: self.api_active_workload(&effective_profile),
            thermal: thermal.clone(),
            frequencies: frequencies.clone(),
            config_generation: self.config_generation,
            reconcile_generation: self.applied.generation,
            uptime_ms: u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let cpu_loads = self
            .observed
            .cpu_loads
            .iter()
            .map(|(cpu, utilization)| CpuLoad {
                cpu_id: cpu.get(),
                utilization_basis_points: utilization_to_basis_points(*utilization),
            })
            .collect();
        PublishedState {
            state_revision: self.state_revision,
            capabilities: self.runtime_capabilities(),
            app_rules: self
                .configuration
                .apps
                .rules
                .iter()
                .map(core_rule_to_api)
                .collect(),
            telemetry: TelemetrySnapshot {
                sequence: self.telemetry_sequence,
                monotonic_ms: self.observed.timestamp.get(),
                cpu_loads,
                thermal,
                frequencies,
            },
            status,
        }
    }

    fn runtime_capabilities(&self) -> Capabilities {
        let mut capabilities = self.configuration.capabilities();
        let writable = self.actuator.is_some() && !self.actuator_read_only;
        for target in &mut capabilities.targets {
            target.can_override = writable;
        }
        if writable {
            capabilities
                .features
                .push(feature::FREQUENCY_TRANSACTIONS.to_owned());
            if self.configuration.policy.scheduler.enabled
                && self
                    .actuator
                    .as_deref()
                    .is_some_and(FrequencyActuator::has_process_backend)
            {
                capabilities
                    .features
                    .push(feature::TASK_SCHEDULER.to_owned());
            }
            if self.configuration.policy.scheduler.enabled
                && !self
                    .configuration
                    .policy
                    .scheduler
                    .cgroup_classes
                    .is_empty()
                && self
                    .actuator
                    .as_deref()
                    .is_some_and(FrequencyActuator::has_systemd_backend)
            {
                capabilities
                    .features
                    .push(feature::SYSTEMD_CGROUP.to_owned());
            }
        }
        capabilities
    }

    fn frequency_statuses(&self) -> Vec<FrequencyStatus> {
        let observations_stale =
            self.frequency_observations_stale(self.environment.monotonic_millis());
        self.configuration
            .targets
            .keys()
            .map(|id| {
                let observed = self.observed.frequencies.get(id).map(|state| state.limits);
                let desired = self
                    .desired
                    .as_ref()
                    .and_then(|plan| plan.frequencies.get(id))
                    .map(|plan| plan.limits);
                let applied = self.applied.frequencies.get(id).map(|state| state.limits);
                let observed_values = observed.unwrap_or_default();
                let desired_values = desired.unwrap_or_default();
                let applied_values = applied.unwrap_or_default();
                FrequencyStatus {
                    target_id: id.to_string(),
                    observed_available: observed.is_some(),
                    observed_min_hz: observed_values.min.get(),
                    observed_max_hz: observed_values.max.get(),
                    desired_min_hz: desired_values.min.get(),
                    desired_max_hz: desired_values.max.get(),
                    desired_available: desired.is_some(),
                    applied_min_hz: applied_values.min.get(),
                    applied_max_hz: applied_values.max.get(),
                    applied_verified: applied.is_some(),
                    override_active: self.overrides.contains_key(id),
                    stale: observations_stale || observed.is_none(),
                }
            })
            .collect()
    }

    fn thermal_status(&self) -> ThermalStatus {
        let maximum = self.maximum_temperature.map_or(0, |temperature| {
            i32::try_from(temperature.get()).unwrap_or_else(|_| {
                if temperature.get().is_negative() {
                    i32::MIN
                } else {
                    i32::MAX
                }
            })
        });
        ThermalStatus {
            state: match self.thermal_state {
                ThermalState::Normal => "normal",
                ThermalState::Warning => "warning",
                ThermalState::Throttled => "throttled",
                ThermalState::Critical => "critical",
                ThermalState::Degraded => "stale",
            }
            .to_owned(),
            max_temperature_millicelsius: maximum,
            cap_active: !self.thermal_caps.is_empty(),
            sensors_stale: self.thermal_state == ThermalState::Degraded,
        }
    }

    fn api_active_workload(&self, effective_profile: &str) -> ActiveWorkload {
        let Some(process) = &self.active_workload else {
            return ActiveWorkload::default();
        };
        ActiveWorkload {
            present: true,
            identity: core_identity_to_api(process.identity),
            name: process.comm.clone(),
            requested_mode: self
                .requested_workload_profile
                .map_or_else(String::new, |profile| profile.to_string()),
            effective_mode: effective_profile.to_owned(),
            source: "explicit".to_owned(),
        }
    }
}

/// Independently scheduled Linux observer tasks.
pub struct ObserverTasks {
    shutdown: watch::Sender<bool>,
    joins: Vec<JoinHandle<()>>,
}

impl ObserverTasks {
    pub async fn stop(self) {
        self.shutdown.send_replace(true);
        for join in self.joins {
            let _ = join.await;
        }
    }
}

/// Start independent load and thermal cadences.
#[must_use]
pub fn spawn_linux_observers(
    environment: Arc<LinuxEnvironment>,
    ingress: &ObserverIngress,
) -> ObserverTasks {
    let (shutdown, shutdown_rx) = watch::channel(false);
    let load = spawn_load_observer(environment.clone(), ingress.clone(), shutdown_rx.clone());
    let thermal = spawn_thermal_observer(environment.clone(), ingress.clone(), shutdown_rx.clone());
    let frequency = spawn_frequency_observer(environment, ingress.clone(), shutdown_rx);
    ObserverTasks {
        shutdown,
        joins: vec![load, thermal, frequency],
    }
}

fn spawn_load_observer(
    environment: Arc<LinuxEnvironment>,
    ingress: ObserverIngress,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let mut load_settings = ingress.settings();
    tokio::spawn(async move {
        let mut sequence = 0_u64;
        let mut ticker =
            tokio::time::interval(nonzero_interval(load_settings.borrow().load_interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    sequence = sequence.saturating_add(1);
                    let sample = environment.cpu_times().map_err(|error| error.to_string());
                    ingress.observe_load(
                        sequence,
                        sample,
                    );
                }
                changed = load_settings.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    ticker = tokio::time::interval(nonzero_interval(
                        load_settings.borrow_and_update().load_interval,
                    ));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

fn spawn_thermal_observer(
    environment: Arc<LinuxEnvironment>,
    ingress: ObserverIngress,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let mut thermal_settings = ingress.settings();
    tokio::spawn(async move {
        let mut sequence = 0_u64;
        let mut ticker =
            tokio::time::interval(nonzero_interval(thermal_settings.borrow().thermal_interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    sequence = sequence.saturating_add(1);
                    let paths = thermal_settings.borrow().thermal_paths.clone();
                    let sample = environment.read_thermal_paths(&paths);
                    ingress.observe_thermal(
                        sequence,
                        Ok(sample),
                    );
                }
                changed = thermal_settings.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    ticker = tokio::time::interval(nonzero_interval(
                        thermal_settings.borrow_and_update().thermal_interval,
                    ));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

fn spawn_frequency_observer(
    environment: Arc<LinuxEnvironment>,
    ingress: ObserverIngress,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let frequency_targets = ingress.frequency_targets.clone();
    tokio::spawn(async move {
        let mut sequence = 0_u64;
        let mut ticker = tokio::time::interval(FREQUENCY_OBSERVER_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    sequence = sequence.saturating_add(1);
                    let sample = read_frequency_observation(
                        environment.as_ref(),
                        frequency_targets.as_ref(),
                    );
                    ingress.observe_frequencies(sequence, Ok(sample));
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

fn nonzero_interval(interval: Duration) -> Duration {
    interval.max(Duration::from_millis(1))
}

fn actuator_health_snapshot(actuator: Option<&FrequencyActuator>) -> (bool, bool) {
    let Some(actuator) = actuator else {
        return (true, false);
    };
    let read_only = !matches!(actuator.mode(), Ok(ActuatorMode::ReadWrite));
    let recovery_pending = actuator.startup_recovery_required().unwrap_or(true);
    (read_only, recovery_pending)
}

fn retry_delay_ms(failures: u32) -> u64 {
    let exponent = failures.saturating_sub(1).min(8);
    100_u64.saturating_mul(1_u64 << exponent).min(30_000)
}

fn frequency_sample_is_stale(last_success: Option<MonotonicMillis>, now: MonotonicMillis) -> bool {
    last_success.is_none_or(|last| {
        last > now || now.saturating_duration_since(last) > FREQUENCY_STALE_AFTER_MS
    })
}

fn read_frequency_observation(
    environment: &LinuxEnvironment,
    targets: &BTreeMap<TargetId, crate::config::ResolvedTarget>,
) -> FrequencyObservationBatch {
    let mut readings = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for (id, target) in targets {
        match read_frequency_limits(environment, target) {
            Ok(limits) => {
                let current = read_current_frequency(environment, target);
                readings.insert(id.clone(), ObservedFrequency { limits, current });
            }
            Err(error) => {
                errors.insert(id.clone(), error);
            }
        }
    }
    FrequencyObservationBatch { readings, errors }
}

fn read_current_frequency(
    environment: &LinuxEnvironment,
    target: &crate::config::ResolvedTarget,
) -> Option<Hertz> {
    environment
        .sysfs()
        .read_string(&target.paths.current)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(target.paths.hertz_per_unit)
        .map(Hertz::new)
}

fn read_frequency_limits(
    environment: &LinuxEnvironment,
    target: &crate::config::ResolvedTarget,
) -> Result<FrequencyLimits, String> {
    let read = |path: &std::path::Path| {
        environment
            .sysfs()
            .read_string(path)
            .map_err(|error| error.to_string())?
            .trim()
            .parse::<u64>()
            .map_err(|error| format!("{}: {error}", path.display()))?
            .checked_mul(target.paths.hertz_per_unit)
            .map(Hertz::new)
            .ok_or_else(|| format!("{}: frequency overflow", path.display()))
    };
    let minimum = read(&target.paths.minimum)?;
    let maximum = read(&target.paths.maximum)?;
    FrequencyLimits::new(minimum, maximum).map_err(|error| error.to_string())
}

fn api_rule_to_core(rule: &ApiAppRule) -> Result<uperf_core::AppRule, RuntimeError> {
    if rule.owner_uid != u32::MAX {
        return Err(RuntimeError::InvalidArgument(
            "D-Bus API v1 only supports administrator-owned global application rules".to_owned(),
        ));
    }
    if rule.executable.is_none() && rule.comm_regex.is_none() {
        return Err(RuntimeError::InvalidArgument(
            "an application rule requires executable and/or comm_regex".to_owned(),
        ));
    }
    Ok(uperf_core::AppRule {
        id: rule.id.clone(),
        enabled: rule.enabled,
        priority: rule.priority,
        matcher: uperf_core::WorkloadMatcher {
            executable: rule.executable.clone(),
            desktop_id: None,
            comm_regex: rule.comm_regex.clone(),
        },
        profile: parse_profile(&rule.mode)?,
    })
}

fn core_rule_to_api(rule: &uperf_core::AppRule) -> ApiAppRule {
    debug_assert!(
        rule.matcher.desktop_id.is_none(),
        "validated v2 rules cannot expose an unsupported desktop ID"
    );
    ApiAppRule {
        id: rule.id.clone(),
        enabled: rule.enabled,
        owner_uid: u32::MAX,
        executable: rule.matcher.executable.clone(),
        comm_regex: rule.matcher.comm_regex.clone(),
        mode: rule.profile.to_string(),
        priority: rule.priority,
    }
}

fn persist_apps(paths: &ConfigurationPaths, apps: &AppsConfig) -> Result<(), RuntimeError> {
    uperf_core::Validate::validate(apps)
        .map_err(|error| RuntimeError::Validation(error.to_string()))?;
    let bytes = serde_json::to_vec_pretty(apps)
        .map_err(|error| RuntimeError::Internal(error.to_string()))?;
    FileStateStore::new(&paths.apps)
        .store_durable(&bytes)
        .map_err(|error| RuntimeError::Internal(error.to_string()))
}

fn parse_mode(value: &str) -> Result<ModeSelection, RuntimeError> {
    if value == "auto" {
        Ok(ModeSelection::Auto)
    } else {
        parse_profile(value).map(ModeSelection::Forced)
    }
}

fn parse_profile(value: &str) -> Result<ProfileId, RuntimeError> {
    match value {
        "powersave" => Ok(ProfileId::Powersave),
        "balance" => Ok(ProfileId::Balance),
        "performance" => Ok(ProfileId::Performance),
        _ => Err(RuntimeError::InvalidArgument(format!(
            "unknown profile {value}"
        ))),
    }
}

fn mode_name(mode: ModeSelection) -> &'static str {
    match mode {
        ModeSelection::Auto => "auto",
        ModeSelection::Forced(ProfileId::Powersave) => "powersave",
        ModeSelection::Forced(ProfileId::Balance) => "balance",
        ModeSelection::Forced(ProfileId::Performance) => "performance",
    }
}

fn core_identity_to_api(identity: uperf_core::ProcessIdentity) -> WorkloadIdentity {
    WorkloadIdentity {
        pid: identity.pid.get(),
        start_time_ticks: identity.start_time_ticks,
        uid: identity.uid.get(),
    }
}

fn receipt(
    generation: u64,
    changed_ids: Vec<String>,
    message: impl Into<String>,
) -> MutationReceipt {
    MutationReceipt {
        generation,
        changed_ids,
        message: message.into(),
    }
}

fn issue(
    code: impl Into<String>,
    severity: impl Into<String>,
    component: impl Into<String>,
    message: impl Into<String>,
) -> HealthIssue {
    HealthIssue {
        code: code.into(),
        severity: severity.into(),
        component: component.into(),
        message: message.into(),
    }
}

fn middle_opp(target: &crate::config::ResolvedTarget) -> Option<Hertz> {
    if target.available_frequencies.is_empty() {
        return Some(Hertz::new(
            target
                .hardware_limits
                .min
                .get()
                .saturating_add(target.hardware_limits.max.get())
                / 2,
        ));
    }
    let mut frequencies = target.available_frequencies.clone();
    frequencies.sort_unstable();
    frequencies.get(frequencies.len() / 2).copied()
}

fn utilization_to_basis_points(value: f64) -> u16 {
    let scaled = (value.clamp(0.0, 1.0) * 10_000.0).round();
    if scaled >= f64::from(u16::MAX) {
        u16::MAX
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            scaled as u16
        }
    }
}

fn target_signature(
    configuration: &ResolvedConfiguration,
) -> Vec<(TargetId, std::path::PathBuf, std::path::PathBuf, u64)> {
    configuration
        .targets
        .iter()
        .map(|(id, target)| {
            (
                id.clone(),
                target.paths.minimum.clone(),
                target.paths.maximum.clone(),
                target.paths.hertz_per_unit,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uperf_core::{
        AppRuleEngine, CpuId, CpuSet, DeviceCapabilities, DeviceConfig, PolicyConfig, PolicyEngine,
    };
    use uperf_platform::{InputDeviceId, TouchContactId};
    use uperf_testkit::{FakeClock, FakeProc, FakeRuntime};

    fn contact(device: u64, tracking_id: u32) -> TouchContactId {
        TouchContactId::new(InputDeviceId::new(device), tracking_id)
    }

    #[test]
    fn frequency_sample_staleness_has_a_bounded_deadline() {
        let last = MonotonicMillis::new(100);
        assert!(!frequency_sample_is_stale(
            Some(last),
            last.saturating_add(FREQUENCY_STALE_AFTER_MS)
        ));
        assert!(frequency_sample_is_stale(
            Some(last),
            last.saturating_add(FREQUENCY_STALE_AFTER_MS + 1)
        ));
        assert!(frequency_sample_is_stale(
            Some(last.saturating_add(1)),
            last
        ));
        assert!(frequency_sample_is_stale(None, last));
    }

    #[tokio::test]
    async fn fake_clock_drives_hint_expiry_through_the_complete_actor() {
        let device = DeviceConfig::from_json(include_str!("../../../config/devices/sm8550.json"))
            .expect("device configuration");
        let policy = PolicyConfig::from_json(include_str!("../../../config/policy.json"))
            .expect("policy configuration");
        let apps = AppsConfig::from_json(include_str!("../../../config/apps.json"))
            .expect("application rules");
        let thermal_zones = device.thermal_zones.clone();
        let configuration = ResolvedConfiguration {
            device,
            app_rule_engine: AppRuleEngine::new(&apps).expect("application rule engine"),
            policy_engine: PolicyEngine::new(policy.clone()).expect("policy engine"),
            policy,
            apps,
            targets: BTreeMap::new(),
            thermal_zones,
            warnings: Vec::new(),
        };
        let discovery = LinuxDiscovery {
            capabilities: DeviceCapabilities {
                device_name: Some("fake SM8550".to_owned()),
                compatible: vec!["qcom,sm8550".to_owned()],
                matched_profile: Some("qcom-sm8550".to_owned()),
                cpu_policies: Vec::new(),
                devfreq_targets: Vec::new(),
                thermal_zones: Vec::new(),
                input_devices: Vec::new(),
            },
            frequency_targets: BTreeMap::new(),
            thermal_zone_paths: BTreeMap::new(),
            warnings: Vec::new(),
        };
        let clock = FakeClock::new(MonotonicMillis::new(10));
        let platform = Arc::new(FakeRuntime::new(
            clock.clone(),
            FakeProc::default(),
            CpuSet::from_ids([CpuId::new(0)]),
        ));
        let temporary = tempfile::tempdir().expect("temporary configuration roots");
        let (runtime, ingress, state_task) = spawn_runtime(RuntimeParts {
            environment: platform,
            discovery,
            configuration,
            configuration_paths: ConfigurationPaths::below(temporary.path(), temporary.path()),
            actuator: None,
            started_at: Instant::now(),
        });
        let mut states = runtime.subscribe();

        assert!(ingress.try_hint(Scene::Wake));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if states.borrow().status.dominant_scene == "wake" {
                    break;
                }
                states.changed().await.expect("runtime state update");
            }
        })
        .await
        .expect("wake scene publication");

        let _ = clock.advance(501);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if states.borrow().status.dominant_scene == "idle" {
                    break;
                }
                states.changed().await.expect("runtime state update");
            }
        })
        .await
        .expect("hint expiry publication");

        runtime.stop().await.expect("stop runtime");
        state_task
            .await
            .expect("join runtime")
            .expect("runtime result");
    }

    #[test]
    fn touch_remains_active_until_the_last_contact_is_released() {
        let mut contacts = ActiveTouchContacts::default();
        assert!(contacts.press(contact(1, 7)));
        assert!(!contacts.press(contact(1, 8)));
        assert!(!contacts.press(contact(2, 7)));

        assert!(!contacts.release(contact(1, 7)));
        assert!(!contacts.release(contact(99, 7)));
        assert!(!contacts.release(contact(1, 8)));
        assert!(contacts.release(contact(2, 7)));
    }

    #[test]
    fn device_resync_preserves_contacts_from_other_devices() {
        let mut contacts = ActiveTouchContacts::default();
        assert!(contacts.press(contact(10, 1)));
        assert!(!contacts.press(contact(20, 1)));

        assert!(!contacts.resync(Some(InputDeviceId::new(10))));
        assert!(contacts.release(contact(20, 1)));

        assert!(contacts.press(contact(10, 2)));
        assert!(!contacts.press(contact(20, 2)));
        assert!(contacts.resync(None));
    }

    #[test]
    fn input_sender_applies_bounded_backpressure() {
        let (load, _) = watch::channel(None);
        let (thermal, _) = watch::channel(None);
        let (frequency, _) = watch::channel(None);
        let (logind_health, _) = watch::channel(None);
        let (input_health, _) = watch::channel(None);
        let (runtime_events, mut queue_rx) = mpsc::channel(1);
        let (_, settings) = watch::channel(ObserverSettings {
            load_interval: Duration::from_millis(1),
            thermal_interval: Duration::from_millis(1),
            thermal_paths: Vec::new(),
            input: InputConfig::default(),
        });
        runtime_events
            .try_send(RuntimeInput::Hint(Scene::Wake))
            .unwrap();
        let ingress = ObserverIngress {
            load,
            thermal,
            frequency,
            logind_health,
            input_health,
            runtime_events,
            settings,
            frequency_targets: Arc::new(BTreeMap::new()),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = cancelled.clone();
        let sender = thread::spawn(move || {
            ingress.send_input_with_backpressure(
                InputEvent::TouchDown {
                    contact: contact(1, 5),
                    x: 0.25,
                    y: 0.75,
                },
                &thread_cancelled,
            )
        });

        thread::sleep(Duration::from_millis(10));
        assert!(
            !sender.is_finished(),
            "full bounded channel must backpressure"
        );
        assert!(matches!(
            queue_rx.blocking_recv(),
            Some(RuntimeInput::Hint(Scene::Wake))
        ));
        assert!(sender.join().unwrap());
        let queued_event = queue_rx.blocking_recv();
        let Some(RuntimeInput::Input(InputEvent::TouchDown {
            contact: contact_id,
            ..
        })) = queued_event
        else {
            panic!("expected the queued touch-down transition");
        };
        assert_eq!(contact_id, contact(1, 5));
    }

    #[tokio::test]
    async fn sleep_transition_waits_for_reducer_acknowledgement() {
        let (load, _) = watch::channel(None);
        let (thermal, _) = watch::channel(None);
        let (frequency, _) = watch::channel(None);
        let (logind_health, _) = watch::channel(None);
        let (input_health, _) = watch::channel(None);
        let (runtime_events, mut queue_rx) = mpsc::channel(1);
        let (_, settings) = watch::channel(ObserverSettings {
            load_interval: Duration::from_millis(1),
            thermal_interval: Duration::from_millis(1),
            thermal_paths: Vec::new(),
            input: InputConfig::default(),
        });
        let ingress = ObserverIngress {
            load,
            thermal,
            frequency,
            logind_health,
            input_health,
            runtime_events,
            settings,
            frequency_targets: Arc::new(BTreeMap::new()),
        };

        let sender = tokio::spawn(async move {
            ingress
                .prepare_for_sleep(true, Duration::from_secs(1))
                .await
        });
        let Some(RuntimeInput::PrepareForSleep {
            sleeping,
            completion,
        }) = queue_rx.recv().await
        else {
            panic!("expected a sleep transition");
        };
        assert!(sleeping);
        assert!(!sender.is_finished());
        completion.send(Ok(())).unwrap();
        assert_eq!(sender.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn sleep_transition_has_a_single_bounded_deadline() {
        let (load, _) = watch::channel(None);
        let (thermal, _) = watch::channel(None);
        let (frequency, _) = watch::channel(None);
        let (logind_health, _) = watch::channel(None);
        let (input_health, _) = watch::channel(None);
        let (runtime_events, _queue_rx) = mpsc::channel(1);
        let (_, settings) = watch::channel(ObserverSettings {
            load_interval: Duration::from_millis(1),
            thermal_interval: Duration::from_millis(1),
            thermal_paths: Vec::new(),
            input: InputConfig::default(),
        });
        let ingress = ObserverIngress {
            load,
            thermal,
            frequency,
            logind_health,
            input_health,
            runtime_events,
            settings,
            frequency_targets: Arc::new(BTreeMap::new()),
        };

        let started = tokio::time::Instant::now();
        let error = ingress
            .prepare_for_sleep(true, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
