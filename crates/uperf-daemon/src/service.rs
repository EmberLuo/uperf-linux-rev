//! Version-1 D-Bus service backed by the runtime actor.

use tokio::sync::watch;
use uperf_api::{
    ApiVersion, AppRule, Capabilities, DaemonStatus, FrequencyOverride, HealthStatus,
    MutationReceipt, ReloadReport, ServiceError, TelemetrySnapshot, WorkloadRequest,
};
use zbus::{Connection, message::Header, object_server::SignalEmitter};

use crate::{
    auth::{ADMIN_ACTION, Authorizer, CONTROL_ACTION},
    runtime::{RuntimeError, RuntimeEvent, RuntimeHandle, TELEMETRY_INTERVAL},
};

/// Exported `org.uperflinux.Daemon1` object.
pub struct DaemonService {
    runtime: RuntimeHandle,
    authorizer: Authorizer,
}

impl DaemonService {
    #[must_use]
    pub const fn new(runtime: RuntimeHandle, authorizer: Authorizer) -> Self {
        Self {
            runtime,
            authorizer,
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

    async fn list_app_rules(
        &self,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<Vec<AppRule>, ServiceError> {
        self.authorizer
            .require_action(connection, &header, ADMIN_ACTION)
            .await?;
        Ok(self.runtime.snapshot().app_rules.clone())
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
pub async fn run_signal_pump(
    connection: Connection,
    runtime: RuntimeHandle,
    mut shutdown: watch::Receiver<bool>,
) -> zbus::Result<()> {
    let interface = connection
        .object_server()
        .interface::<_, DaemonService>(uperf_api::OBJECT_PATH)
        .await?;
    let mut events = runtime.subscribe_events();
    let mut telemetry = tokio::time::interval(TELEMETRY_INTERVAL);
    telemetry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_telemetry_sequence = None;
    let mut last_properties = StateProperties::capture(&runtime);

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
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    Ok(())
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
    use zbus::object_server::Interface;

    use super::*;
    use crate::auth::AuthorizationMode;

    #[test]
    fn daemon1_introspection_matches_the_versioned_snapshot() {
        let service = DaemonService::new(
            RuntimeHandle::snapshot_only(),
            Authorizer::new(AuthorizationMode::DevelopmentSession),
        );
        let mut xml = String::new();
        service.introspect_to_writer(&mut xml, 0);
        assert_eq!(xml, include_str!("daemon1-introspection.xml"));
    }
}
