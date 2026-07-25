//! Version-1 D-Bus service backed by the runtime actor.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use uperf_api::{
    ApiVersion, AppRule, Capabilities, DaemonStatus, FrequencyOverride, HealthStatus,
    MutationReceipt, ReloadReport, RunningWorkload, SchedulerStatus, ServiceError,
    TelemetrySnapshot, WorkloadIdentity, WorkloadRequest,
};
use uperf_core::ProcessInfo;
use uperf_platform::{PlatformError, ProcReader};
use zbus::{Connection, message::Header, object_server::SignalEmitter};

use crate::{
    auth::{ADMIN_ACTION, Authorizer, CONTROL_ACTION},
    runtime::{RuntimeError, RuntimeEvent, RuntimeHandle, TELEMETRY_INTERVAL},
};

const RUNNING_WORKLOAD_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_RUNNING_WORKLOADS: usize = 128;

const GAME_PROCESS_PATTERNS: &[&str] = &[
    "unitymain",
    "gamethread",
    "renderthread",
    "glthread",
    "dolphin",
    "ppsspp",
    "retroarch",
    "wine",
    "proton",
    "mihoyo",
    "hoyoverse",
    "minecraft",
    "gameloft",
    "supercell",
    "niantic",
    "rovio",
    "ea.games",
    "playdead",
    "half-life",
    "steam",
    "gta",
    "pubg",
    "fortnite",
    "callofduty",
    "genshin",
    "honkai",
    "arknights",
    "yuzu",
    "ryujinx",
];

/// Five-second, read-only procfs candidate cache shared by the D-Bus method
/// and signal pump.
pub struct RunningWorkloadScanner {
    source: Option<Arc<dyn ProcReader>>,
    candidates: Mutex<Vec<RunningWorkload>>,
}

impl RunningWorkloadScanner {
    #[must_use]
    pub fn new(source: Arc<dyn ProcReader>) -> Self {
        Self {
            source: Some(source),
            candidates: Mutex::new(Vec::new()),
        }
    }

    #[cfg(test)]
    fn unavailable() -> Self {
        Self {
            source: None,
            candidates: Mutex::new(Vec::new()),
        }
    }

    async fn refresh(&self) -> Result<(), String> {
        let Some(source) = self.source.clone() else {
            return Ok(());
        };
        let result = tokio::task::spawn_blocking(move || scan_running_workloads(source.as_ref()))
            .await
            .map_err(|error| format!("running-workload scan worker failed: {error}"))
            .and_then(std::convert::identity);
        let mut cached = self
            .candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match result {
            Ok(candidates) => {
                *cached = candidates;
                Ok(())
            }
            Err(error) => {
                cached.clear();
                Err(error)
            }
        }
    }

    fn snapshot(&self, runtime: &RuntimeHandle, caller_uid: u32) -> Vec<RunningWorkload> {
        let published = runtime.snapshot();
        let active = &published.status.active_workload;
        let explicit_active = active.present && active.source == "explicit";
        let scheduler = &published.scheduler;
        let mut candidates = self
            .candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if explicit_active {
            // A cached row for a reused PID must not sit beside the daemon's
            // newer stable active identity.
            candidates.retain(|candidate| {
                candidate.identity.pid != active.identity.pid
                    || candidate.identity == active.identity
            });
        }
        if explicit_active
            && !candidates
                .iter()
                .any(|candidate| candidate.identity == active.identity)
        {
            candidates.push(RunningWorkload {
                identity: active.identity,
                name: active.name.clone(),
                matched_pattern: "active".to_owned(),
                active: true,
                scheduler: scheduler.clone(),
            });
        }
        for candidate in &mut candidates {
            candidate.active = explicit_active && candidate.identity == active.identity;
            candidate.scheduler = if candidate.active {
                scheduler.clone()
            } else {
                SchedulerStatus::default()
            };
        }
        if caller_uid != 0 {
            candidates.retain(|candidate| candidate.identity.uid == caller_uid);
        }
        candidates.sort_by_key(|candidate| candidate.identity.pid);
        candidates
    }
}

/// Exported `org.uperflinux.Daemon1` object.
pub struct DaemonService {
    runtime: RuntimeHandle,
    authorizer: Authorizer,
    running_workloads: Arc<RunningWorkloadScanner>,
}

impl DaemonService {
    #[must_use]
    pub fn new(
        runtime: RuntimeHandle,
        authorizer: Authorizer,
        running_workloads: Arc<RunningWorkloadScanner>,
    ) -> Self {
        Self {
            runtime,
            authorizer,
            running_workloads,
        }
    }
}

#[zbus::interface(name = "org.uperflinux.Daemon1", introspection_docs = false)]
impl DaemonService {
    fn get_status(&self) -> DaemonStatus {
        self.runtime.snapshot().status.clone()
    }

    fn get_capabilities(&self) -> Capabilities {
        self.runtime.snapshot().capabilities.clone()
    }

    async fn list_running_workloads(
        &self,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<Vec<RunningWorkload>, ServiceError> {
        let caller_uid = self.authorizer.caller_uid(connection, &header).await?;
        Ok(self.running_workloads.snapshot(&self.runtime, caller_uid))
    }

    async fn set_mode(
        &self,
        mode: &str,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        self.authorizer
            .require_action(connection, &header, CONTROL_ACTION)
            .await?;
        self.runtime
            .set_mode(mode.to_owned())
            .await
            .map_err(map_runtime_error)
    }

    async fn set_active_workload(
        &self,
        request: WorkloadRequest,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        let caller = self.authorizer.caller_uid(connection, &header).await?;
        if !request.mode.is_empty() {
            self.authorizer
                .require_action(connection, &header, CONTROL_ACTION)
                .await?;
        }
        self.runtime
            .set_active_workload(request, caller)
            .await
            .map_err(map_runtime_error)
    }

    async fn clear_active_workload(
        &self,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        let caller = self.authorizer.caller_uid(connection, &header).await?;
        self.runtime
            .clear_active_workload(caller)
            .await
            .map_err(map_runtime_error)
    }

    async fn set_foreground_process(
        &self,
        pid: u32,
        reason: &str,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        let caller = self
            .authorizer
            .require_active_local_session(connection, &header)
            .await?;
        let peer = header.sender().map(ToString::to_string);
        self.runtime
            .set_foreground_process(pid, reason.to_owned(), caller, peer)
            .await
            .map_err(map_runtime_error)
    }

    async fn clear_foreground_process(
        &self,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        let caller = self.authorizer.caller_uid(connection, &header).await?;
        self.runtime
            .clear_foreground_process(caller)
            .await
            .map_err(map_runtime_error)
    }

    async fn set_frequency_overrides(
        &self,
        overrides: Vec<FrequencyOverride>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        self.authorizer
            .require_action(connection, &header, ADMIN_ACTION)
            .await?;
        self.runtime
            .set_frequency_overrides(overrides)
            .await
            .map_err(map_runtime_error)
    }

    async fn clear_frequency_overrides(
        &self,
        target_ids: Vec<String>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        self.authorizer
            .require_action(connection, &header, ADMIN_ACTION)
            .await?;
        self.runtime
            .clear_frequency_overrides(target_ids)
            .await
            .map_err(map_runtime_error)
    }

    async fn reload_config(
        &self,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<ReloadReport, ServiceError> {
        self.authorizer
            .require_action(connection, &header, ADMIN_ACTION)
            .await?;
        self.runtime.reload().await.map_err(map_runtime_error)
    }

    fn list_app_rules(&self) -> Vec<AppRule> {
        self.runtime.snapshot().app_rules.clone()
    }

    async fn set_app_rule(
        &self,
        rule: AppRule,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        self.authorizer
            .require_action(connection, &header, ADMIN_ACTION)
            .await?;
        self.runtime
            .set_app_rule(rule)
            .await
            .map_err(map_runtime_error)
    }

    async fn remove_app_rule(
        &self,
        rule_id: &str,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<MutationReceipt, ServiceError> {
        self.authorizer
            .require_action(connection, &header, ADMIN_ACTION)
            .await?;
        self.runtime
            .remove_app_rule(rule_id.to_owned())
            .await
            .map_err(map_runtime_error)
    }

    #[zbus(property, name = "ApiVersion")]
    #[allow(
        clippy::unused_self,
        reason = "zbus properties are instance methods even when their value is compile-time metadata"
    )]
    fn api_version_property(&self) -> ApiVersion {
        ApiVersion::CURRENT
    }

    #[zbus(property, name = "DaemonVersion")]
    #[allow(
        clippy::unused_self,
        reason = "zbus properties are instance methods even when their value is compile-time metadata"
    )]
    fn daemon_version_property(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    #[zbus(property, name = "ConfigGeneration")]
    fn config_generation_property(&self) -> u64 {
        self.runtime.snapshot().status.config_generation
    }

    #[zbus(property, name = "Mode")]
    fn mode_property(&self) -> String {
        self.runtime.snapshot().status.mode.clone()
    }

    #[zbus(property, name = "EffectiveProfile")]
    fn effective_profile_property(&self) -> String {
        self.runtime.snapshot().status.effective_profile.clone()
    }

    #[zbus(property, name = "DominantScene")]
    fn dominant_scene_property(&self) -> String {
        self.runtime.snapshot().status.dominant_scene.clone()
    }

    #[zbus(property(emits_changed_signal = "invalidates"), name = "Health")]
    fn health_property(&self) -> HealthStatus {
        self.runtime.snapshot().status.health.clone()
    }

    #[zbus(property, name = "Degraded")]
    fn degraded_property(&self) -> bool {
        self.runtime.snapshot().status.health.state != "healthy"
    }

    #[zbus(signal, name = "StateChanged")]
    async fn state_changed(emitter: &SignalEmitter<'_>, generation: u64) -> zbus::Result<()>;

    #[zbus(signal, name = "HealthChanged")]
    async fn health_changed(emitter: &SignalEmitter<'_>, health: HealthStatus) -> zbus::Result<()>;

    #[zbus(signal, name = "TelemetryUpdated")]
    async fn telemetry_updated(
        emitter: &SignalEmitter<'_>,
        telemetry: TelemetrySnapshot,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "CapabilitiesChanged")]
    async fn capabilities_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal, name = "RunningWorkloadsChanged")]
    async fn running_workloads_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[derive(Debug, Eq, PartialEq)]
struct StateProperties {
    config_generation: u64,
    mode: String,
    effective_profile: String,
    dominant_scene: String,
}

impl StateProperties {
    fn capture(runtime: &RuntimeHandle) -> Self {
        let snapshot = runtime.snapshot();
        Self {
            config_generation: snapshot.status.config_generation,
            mode: snapshot.status.mode.clone(),
            effective_profile: snapshot.status.effective_profile.clone(),
            dominant_scene: snapshot.status.dominant_scene.clone(),
        }
    }
}

/// Forward runtime notifications to D-Bus signals and standard properties.
///
/// # Errors
///
/// Returns a D-Bus error when the exported interface cannot be acquired or a
/// signal/property notification cannot be emitted.
#[allow(
    clippy::too_many_lines,
    reason = "one select loop keeps D-Bus signals ordered against coherent runtime snapshots"
)]
pub async fn run_signal_pump(
    connection: Connection,
    runtime: RuntimeHandle,
    running_workloads: Arc<RunningWorkloadScanner>,
    mut shutdown: watch::Receiver<bool>,
) -> zbus::Result<()> {
    let interface = connection
        .object_server()
        .interface::<_, DaemonService>(uperf_api::OBJECT_PATH)
        .await?;
    let mut events = runtime.subscribe_events();
    let mut telemetry = tokio::time::interval(TELEMETRY_INTERVAL);
    telemetry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (workload_scan_tx, mut workload_scan_rx) = tokio::sync::mpsc::channel(1);
    let scanner_for_worker = running_workloads.clone();
    let mut scanner_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RUNNING_WORKLOAD_SCAN_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let result = scanner_for_worker.refresh().await;
                    if workload_scan_tx.send(result).await.is_err() {
                        break;
                    }
                }
                changed = scanner_shutdown.changed() => {
                    if changed.is_err() || *scanner_shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
    let mut last_telemetry_sequence = None;
    let mut last_properties = StateProperties::capture(&runtime);
    let mut last_running_workloads = Vec::new();

    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(RuntimeEvent::StateChanged(generation)) => {
                        DaemonService::state_changed(
                            interface.signal_emitter(),
                            generation,
                        ).await?;
                        let current = StateProperties::capture(&runtime);
                        if current != last_properties {
                            let service = interface.get().await;
                            if current.config_generation != last_properties.config_generation {
                                service.config_generation_changed(interface.signal_emitter()).await?;
                            }
                            if current.mode != last_properties.mode {
                                service.mode_changed(interface.signal_emitter()).await?;
                            }
                            if current.effective_profile != last_properties.effective_profile {
                                service.effective_profile_changed(interface.signal_emitter()).await?;
                            }
                            if current.dominant_scene != last_properties.dominant_scene {
                                service.dominant_scene_changed(interface.signal_emitter()).await?;
                            }
                            last_properties = current;
                        }
                        emit_running_workloads_if_changed(
                            interface.signal_emitter(),
                            &runtime,
                            running_workloads.as_ref(),
                            &mut last_running_workloads,
                        ).await?;
                    }
                    Ok(RuntimeEvent::HealthChanged(health)) => {
                        DaemonService::health_changed(
                            interface.signal_emitter(),
                            health,
                        ).await?;
                        let service = interface.get().await;
                        service.health_invalidate(interface.signal_emitter()).await?;
                        service.degraded_changed(interface.signal_emitter()).await?;
                    }
                    Ok(RuntimeEvent::CapabilitiesChanged) => {
                        DaemonService::capabilities_changed(interface.signal_emitter()).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Broadcast delivery is intentionally bounded.  A slow
                        // D-Bus peer must receive a coherent current snapshot
                        // after lag instead of silently missing a transition.
                        let snapshot = runtime.snapshot();
                        DaemonService::state_changed(
                            interface.signal_emitter(),
                            snapshot.state_revision,
                        ).await?;
                        DaemonService::health_changed(
                            interface.signal_emitter(),
                            snapshot.status.health.clone(),
                        ).await?;
                        DaemonService::capabilities_changed(
                            interface.signal_emitter(),
                        ).await?;
                        let service = interface.get().await;
                        service.config_generation_changed(interface.signal_emitter()).await?;
                        service.mode_changed(interface.signal_emitter()).await?;
                        service.effective_profile_changed(interface.signal_emitter()).await?;
                        service.dominant_scene_changed(interface.signal_emitter()).await?;
                        service.health_invalidate(interface.signal_emitter()).await?;
                        service.degraded_changed(interface.signal_emitter()).await?;
                        last_properties = StateProperties::capture(&runtime);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = telemetry.tick() => {
                let snapshot = runtime.snapshot().telemetry.clone();
                if last_telemetry_sequence != Some(snapshot.sequence) {
                    last_telemetry_sequence = Some(snapshot.sequence);
                    DaemonService::telemetry_updated(
                        interface.signal_emitter(),
                        snapshot,
                    ).await?;
                }
            }
            scan = workload_scan_rx.recv() => {
                if let Some(result) = scan {
                    if let Err(error) = &result {
                        // The cache was cleared before this notification. Keep
                        // a human-readable journal record in addition to
                        // structured daemon health.
                        eprintln!("uperf-linux: running-workload observer degraded: {error}");
                    }
                    let _ = runtime.report_running_workload_health(result).await;
                }
                emit_running_workloads_if_changed(
                    interface.signal_emitter(),
                    &runtime,
                    running_workloads.as_ref(),
                    &mut last_running_workloads,
                ).await?;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn emit_running_workloads_if_changed(
    emitter: &SignalEmitter<'_>,
    runtime: &RuntimeHandle,
    scanner: &RunningWorkloadScanner,
    previous: &mut Vec<RunningWorkload>,
) -> zbus::Result<()> {
    let current = scanner.snapshot(runtime, 0);
    if current != *previous {
        DaemonService::running_workloads_changed(emitter).await?;
        *previous = current;
    }
    Ok(())
}

fn scan_running_workloads(source: &dyn ProcReader) -> Result<Vec<RunningWorkload>, String> {
    let processes = source
        .list_processes()
        .map_err(|error| format!("list procfs processes: {error}"))?;
    let mut candidates = Vec::new();
    for pid in processes {
        if pid.get() < 2 {
            continue;
        }
        let process = match source.process_identity(pid) {
            Ok(process) => process,
            Err(
                PlatformError::Disappeared(_)
                | PlatformError::AccessDenied { .. }
                | PlatformError::Io { .. },
            ) => continue,
            Err(error) => return Err(format!("read process {}: {error}", pid.get())),
        };
        let Some(pattern) = matched_game_pattern(&process) else {
            continue;
        };
        candidates.push(RunningWorkload {
            identity: api_identity(&process),
            name: process.comm,
            matched_pattern: pattern.to_owned(),
            active: false,
            scheduler: SchedulerStatus::default(),
        });
        if candidates.len() == MAX_RUNNING_WORKLOADS {
            break;
        }
    }
    candidates.sort_by_key(|candidate| candidate.identity.pid);
    Ok(candidates)
}

fn matched_game_pattern(process: &ProcessInfo) -> Option<&'static str> {
    let comm = process.comm.to_ascii_lowercase();
    let executable = process
        .executable
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    GAME_PROCESS_PATTERNS
        .iter()
        .copied()
        .find(|pattern| comm.contains(pattern) || executable.contains(pattern))
}

fn api_identity(process: &ProcessInfo) -> WorkloadIdentity {
    WorkloadIdentity {
        pid: process.identity.pid.get(),
        start_time_ticks: process.identity.start_time_ticks,
        uid: process.identity.uid.get(),
    }
}

fn map_runtime_error(error: RuntimeError) -> ServiceError {
    match error {
        RuntimeError::InvalidArgument(message) => ServiceError::InvalidArgument(message),
        RuntimeError::NotFound(message) => ServiceError::NotFound(message),
        RuntimeError::NotAuthorized(message) => ServiceError::NotAuthorized(message),
        RuntimeError::Conflict(message) => ServiceError::Conflict(message),
        RuntimeError::Degraded(message) => ServiceError::Degraded(message),
        RuntimeError::Validation(message) => ServiceError::ValidationFailed(message),
        RuntimeError::Internal(message) => ServiceError::Internal(message),
    }
}

#[cfg(test)]
mod tests {
    use uperf_core::{ProcessIdentity, UserId};
    use uperf_testkit::FakeProc;
    use zbus::object_server::Interface;

    use super::*;
    use crate::auth::AuthorizationMode;

    #[test]
    fn daemon1_introspection_matches_the_versioned_snapshot() {
        let service = DaemonService::new(
            RuntimeHandle::snapshot_only(),
            Authorizer::new(AuthorizationMode::DevelopmentSession),
            Arc::new(RunningWorkloadScanner::unavailable()),
        );
        let mut xml = String::new();
        service.introspect_to_writer(&mut xml, 0);
        assert_eq!(xml, include_str!("daemon1-introspection.xml"));
    }

    #[test]
    fn listing_app_rules_is_a_read_only_snapshot_without_authorization() {
        let service = DaemonService::new(
            RuntimeHandle::snapshot_only(),
            Authorizer::new(AuthorizationMode::PolicyKit),
            Arc::new(RunningWorkloadScanner::unavailable()),
        );

        assert!(service.list_app_rules().is_empty());
    }

    fn process(pid: u32, uid: u32, comm: &str, executable: &str) -> ProcessInfo {
        ProcessInfo {
            identity: ProcessIdentity {
                pid: uperf_core::ProcessId::new(pid),
                start_time_ticks: u64::from(pid) * 10,
                uid: UserId::new(uid),
            },
            owner_control_safe: true,
            comm: comm.to_owned(),
            executable: Some(executable.to_owned()),
            desktop_id: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broad_game_patterns_are_observational_and_uid_filtered() {
        let procfs = FakeProc::default();
        procfs.insert_process(process(10, 1000, "wine64-preloader", "/usr/bin/wine64"));
        procfs.insert_process(process(11, 1000, "pressure-vessel", "/opt/Proton/run"));
        procfs.insert_process(process(12, 1001, "steamwebhelper", "/usr/lib/steam/steam"));
        procfs.insert_process(process(13, 1000, "ordinary", "/usr/bin/ordinary"));
        let scanner = RunningWorkloadScanner::new(Arc::new(procfs));
        let runtime = RuntimeHandle::snapshot_only();
        let mode_before = runtime.snapshot().status.mode.clone();

        scanner.refresh().await.expect("refresh candidates");

        let own = scanner.snapshot(&runtime, 1000);
        assert_eq!(
            own.iter()
                .map(|candidate| candidate.matched_pattern.as_str())
                .collect::<Vec<_>>(),
            ["wine", "proton"]
        );
        assert_eq!(scanner.snapshot(&runtime, 0).len(), 3);
        assert_eq!(runtime.snapshot().status.mode, mode_before);
        assert!(!runtime.snapshot().status.active_workload.present);
    }

    #[test]
    fn explicit_active_workload_is_visible_without_a_broad_match() {
        let identity = WorkloadIdentity {
            pid: 77,
            start_time_ticks: 1234,
            uid: 1000,
        };
        let scheduler = SchedulerStatus {
            enabled: true,
            matched_rule: "game-process".to_owned(),
            managed_tasks: 3,
            applied_tasks: 3,
            ..SchedulerStatus::default()
        };
        let runtime = RuntimeHandle::snapshot_only_with(
            DaemonStatus {
                active_workload: uperf_api::ActiveWorkload {
                    present: true,
                    identity,
                    name: "custom-engine".to_owned(),
                    source: "explicit".to_owned(),
                    ..uperf_api::ActiveWorkload::default()
                },
                ..DaemonStatus::default()
            },
            scheduler.clone(),
        );
        let scanner = RunningWorkloadScanner::unavailable();

        let candidates = scanner.snapshot(&runtime, 1000);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].identity, identity);
        assert_eq!(candidates[0].matched_pattern, "active");
        assert!(candidates[0].active);
        assert_eq!(candidates[0].scheduler, scheduler);
    }

    #[test]
    fn focus_only_workload_is_not_synthesized_as_an_explicit_candidate() {
        let runtime = RuntimeHandle::snapshot_only_with(
            DaemonStatus {
                active_workload: uperf_api::ActiveWorkload {
                    present: true,
                    identity: WorkloadIdentity {
                        pid: 77,
                        start_time_ticks: 1234,
                        uid: 1000,
                    },
                    name: "ordinary".to_owned(),
                    source: "focus".to_owned(),
                    ..uperf_api::ActiveWorkload::default()
                },
                ..DaemonStatus::default()
            },
            SchedulerStatus {
                enabled: true,
                matched_rule: "focus-default".to_owned(),
                applied_tasks: 1,
                ..SchedulerStatus::default()
            },
        );

        assert!(
            RunningWorkloadScanner::unavailable()
                .snapshot(&runtime, 1000)
                .is_empty()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focused_game_candidate_remains_inactive_and_has_no_scheduler_snapshot() {
        let focused = process(77, 1000, "wine64-preloader", "/usr/bin/wine64");
        let identity = api_identity(&focused);
        let procfs = FakeProc::default();
        procfs.insert_process(focused);
        let scanner = RunningWorkloadScanner::new(Arc::new(procfs));
        scanner.refresh().await.expect("refresh candidate");
        let runtime = RuntimeHandle::snapshot_only_with(
            DaemonStatus {
                active_workload: uperf_api::ActiveWorkload {
                    present: true,
                    identity,
                    name: "wine64-preloader".to_owned(),
                    source: "focus".to_owned(),
                    ..uperf_api::ActiveWorkload::default()
                },
                ..DaemonStatus::default()
            },
            SchedulerStatus {
                enabled: true,
                matched_rule: "focus-default".to_owned(),
                applied_tasks: 1,
                ..SchedulerStatus::default()
            },
        );

        let candidates = scanner.snapshot(&runtime, 1000);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].identity, identity);
        assert_eq!(candidates[0].matched_pattern, "wine");
        assert!(!candidates[0].active);
        assert_eq!(candidates[0].scheduler, SchedulerStatus::default());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_workload_replaces_a_scanned_identity_that_reused_the_same_pid() {
        let procfs = FakeProc::default();
        procfs.insert_process(process(77, 1000, "wine64-preloader", "/usr/bin/wine64"));
        let scanner = RunningWorkloadScanner::new(Arc::new(procfs));
        scanner.refresh().await.expect("refresh stale candidate");
        let identity = WorkloadIdentity {
            pid: 77,
            start_time_ticks: 1234,
            uid: 1000,
        };
        let scheduler = SchedulerStatus {
            enabled: true,
            matched_rule: "game-process".to_owned(),
            applied_tasks: 1,
            ..SchedulerStatus::default()
        };
        let runtime = RuntimeHandle::snapshot_only_with(
            DaemonStatus {
                active_workload: uperf_api::ActiveWorkload {
                    present: true,
                    identity,
                    name: "replacement".to_owned(),
                    source: "explicit".to_owned(),
                    ..uperf_api::ActiveWorkload::default()
                },
                ..DaemonStatus::default()
            },
            scheduler.clone(),
        );

        let candidates = scanner.snapshot(&runtime, 1000);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].identity, identity);
        assert_eq!(candidates[0].name, "replacement");
        assert_eq!(candidates[0].matched_pattern, "active");
        assert!(candidates[0].active);
        assert_eq!(candidates[0].scheduler, scheduler);
    }
}
