use std::collections::BTreeSet;

use zbus::Connection;

use crate::{
    ActiveWorkload, ApiVersion, AppRule, Capabilities, ClientError, DaemonStatus, DiagnosticCheck,
    DiagnosticReport, FrequencyOverride, MutationReceipt, ReloadReport, RunningWorkload,
    TelemetrySnapshot, WorkloadRequest,
};

#[zbus::proxy(
    default_service = "org.uperflinux.Daemon1",
    default_path = "/org/uperflinux/Daemon1",
    interface = "org.uperflinux.Daemon1",
    gen_blocking = false
)]
pub trait Daemon1 {
    /// Breaking and compatible contract generation.
    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<ApiVersion>;

    /// Daemon package version.
    #[zbus(property)]
    fn daemon_version(&self) -> zbus::Result<String>;

    /// Successfully loaded configuration generation.
    #[zbus(property)]
    fn config_generation(&self) -> zbus::Result<u64>;

    /// Requested global mode.
    #[zbus(property)]
    fn mode(&self) -> zbus::Result<String>;

    /// Effective profile after workload and safety constraints.
    #[zbus(property)]
    fn effective_profile(&self) -> zbus::Result<String>;

    /// Dominant scene currently influencing policy.
    #[zbus(property)]
    fn dominant_scene(&self) -> zbus::Result<String>;

    /// Structured aggregate health.
    #[zbus(property, name = "Health")]
    fn health_property(&self) -> zbus::Result<crate::HealthStatus>;

    /// Convenience property for clients that only need a safety gate.
    #[zbus(property)]
    fn degraded(&self) -> zbus::Result<bool>;

    /// Return one coherent observed/desired/applied snapshot.
    fn get_status(&self) -> zbus::Result<DaemonStatus>;

    /// Return modes, stable target IDs, and feature support.
    fn get_capabilities(&self) -> zbus::Result<Capabilities>;

    /// Discover running game-like processes without selecting one.
    fn list_running_workloads(&self) -> zbus::Result<Vec<RunningWorkload>>;

    /// Select a global policy mode.
    fn set_mode(&self, mode: &str) -> zbus::Result<MutationReceipt>;

    /// Select a PID. The daemon resolves and owns the stable process identity.
    fn set_active_workload(&self, request: WorkloadRequest) -> zbus::Result<MutationReceipt>;

    /// Clear the daemon's currently selected stable workload identity.
    fn clear_active_workload(&self) -> zbus::Result<MutationReceipt>;

    /// Report the compositor's focused PID as an implicit workload source.
    fn set_foreground_process(&self, pid: u32, reason: &str) -> zbus::Result<MutationReceipt>;

    /// Release the focus lease held for the caller.
    fn clear_foreground_process(&self) -> zbus::Result<MutationReceipt>;

    /// Atomically replace overrides for the listed stable targets.
    fn set_frequency_overrides(
        &self,
        overrides: Vec<FrequencyOverride>,
    ) -> zbus::Result<MutationReceipt>;

    /// Clear listed target overrides; an empty list means all targets.
    fn clear_frequency_overrides(&self, target_ids: Vec<String>) -> zbus::Result<MutationReceipt>;

    /// Parse, validate, instantiate, and atomically swap configuration.
    fn reload_config(&self) -> zbus::Result<ReloadReport>;

    /// List persistent rules from the daemon's read-only snapshot.
    fn list_app_rules(&self) -> zbus::Result<Vec<AppRule>>;

    /// Create or replace one persistent rule.
    fn set_app_rule(&self, rule: AppRule) -> zbus::Result<MutationReceipt>;

    /// Remove a persistent rule by stable ID.
    fn remove_app_rule(&self, rule_id: &str) -> zbus::Result<MutationReceipt>;

    /// Emitted after a desired-state mutation is accepted.
    #[zbus(signal)]
    fn state_changed(&self, generation: u64) -> zbus::Result<()>;

    /// Emitted when topology or feature support changes.
    #[zbus(signal)]
    fn capabilities_changed(&self) -> zbus::Result<()>;

    /// Emitted whenever aggregate health changes.
    #[zbus(signal)]
    fn health_changed(&self, health: crate::HealthStatus) -> zbus::Result<()>;

    /// Observational telemetry, emitted by the daemon at no more than 4 Hz.
    #[zbus(signal)]
    fn telemetry_updated(&self, snapshot: TelemetrySnapshot) -> zbus::Result<()>;

    /// Emitted when the read-only running candidate or scheduler snapshot changes.
    #[zbus(signal)]
    fn running_workloads_changed(&self) -> zbus::Result<()>;
}

/// Thin typed client around the version-1 system-bus interface.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    connection: Connection,
}

impl DaemonClient {
    /// Connect to the production system bus.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Transport`] when the bus connection fails.
    pub async fn system() -> Result<Self, ClientError> {
        Ok(Self::from_connection(Connection::system().await?))
    }

    /// Connect to the session bus, intended for tests and development.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Transport`] when the bus connection fails.
    pub async fn session() -> Result<Self, ClientError> {
        Ok(Self::from_connection(Connection::session().await?))
    }

    /// Build a client over an existing connection.
    #[must_use]
    pub const fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// Build a raw typed proxy for property and signal subscriptions.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Transport`] if proxy construction fails.
    pub async fn proxy(&self) -> Result<Daemon1Proxy<'_>, ClientError> {
        Daemon1Proxy::new(&self.connection)
            .await
            .map_err(ClientError::from)
    }

    /// Fetch a coherent daemon status and verify its API major version.
    ///
    /// # Errors
    ///
    /// Returns an error when the D-Bus call fails or the API major is
    /// incompatible.
    pub async fn status(&self) -> Result<DaemonStatus, ClientError> {
        let status = self
            .proxy()
            .await?
            .get_status()
            .await
            .map_err(ClientError::from)?;
        ensure_compatible(status.api_version)?;
        Ok(status)
    }

    /// Fetch dynamic capabilities and verify their API major version.
    ///
    /// # Errors
    ///
    /// Returns an error when the D-Bus call fails or the API major is
    /// incompatible.
    pub async fn capabilities(&self) -> Result<Capabilities, ClientError> {
        let capabilities = self
            .proxy()
            .await?
            .get_capabilities()
            .await
            .map_err(ClientError::from)?;
        ensure_compatible(capabilities.api_version)?;
        Ok(capabilities)
    }

    /// Discover running game-like processes and read active scheduler state.
    ///
    /// This method is observational only; use [`Self::set_active_workload`] to
    /// explicitly select one of the returned PIDs.
    ///
    /// # Errors
    ///
    /// Returns an error when the D-Bus call or procfs scan fails.
    pub async fn running_workloads(&self) -> Result<Vec<RunningWorkload>, ClientError> {
        self.proxy()
            .await?
            .list_running_workloads()
            .await
            .map_err(ClientError::from)
    }

    /// Select a global policy mode.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mode identifier or a rejected D-Bus
    /// call.
    pub async fn set_mode(&self, mode: &str) -> Result<MutationReceipt, ClientError> {
        validate_identifier("mode", mode)?;
        self.proxy()
            .await?
            .set_mode(mode)
            .await
            .map_err(ClientError::from)
    }

    /// Ask the daemon to resolve and select an active workload PID.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid request or a rejected D-Bus call.
    pub async fn set_active_workload(
        &self,
        request: WorkloadRequest,
    ) -> Result<MutationReceipt, ClientError> {
        validate_workload_pid(request.pid)?;
        if !request.mode.is_empty() {
            validate_identifier("mode", &request.mode)?;
        }
        if request.reason.len() > 256 {
            return Err(ClientError::InvalidRequest(
                "workload reason exceeds 256 bytes".into(),
            ));
        }
        self.proxy()
            .await?
            .set_active_workload(request)
            .await
            .map_err(ClientError::from)
    }

    /// Clear the daemon's currently selected workload.
    ///
    /// # Errors
    ///
    /// Returns an error when the D-Bus call is rejected.
    pub async fn clear_active_workload(&self) -> Result<MutationReceipt, ClientError> {
        self.proxy()
            .await?
            .clear_active_workload()
            .await
            .map_err(ClientError::from)
    }

    /// Report the currently focused PID.
    ///
    /// The receipt confirms only that the report was accepted for resolution.
    /// A PID the daemon later refuses surfaces as a `focus.rejected` health
    /// issue, which keeps rapid window switching off the control lane.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid PID or a rejected D-Bus call.
    pub async fn set_foreground_process(
        &self,
        pid: u32,
        reason: &str,
    ) -> Result<MutationReceipt, ClientError> {
        validate_workload_pid(pid)?;
        if reason.len() > 256 {
            return Err(ClientError::InvalidRequest(
                "focus reason exceeds 256 bytes".into(),
            ));
        }
        self.proxy()
            .await?
            .set_foreground_process(pid, reason)
            .await
            .map_err(ClientError::from)
    }

    /// Release the focus lease held for this caller.
    ///
    /// # Errors
    ///
    /// Returns an error when the D-Bus call is rejected.
    pub async fn clear_foreground_process(&self) -> Result<MutationReceipt, ClientError> {
        self.proxy()
            .await?
            .clear_foreground_process()
            .await
            .map_err(ClientError::from)
    }

    /// Atomically install one or more bounded frequency overrides.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, duplicate, or inverted overrides, or
    /// when the daemon rejects the D-Bus call.
    pub async fn set_frequency_overrides(
        &self,
        overrides: Vec<FrequencyOverride>,
    ) -> Result<MutationReceipt, ClientError> {
        validate_frequency_overrides(&overrides)?;
        self.proxy()
            .await?
            .set_frequency_overrides(overrides)
            .await
            .map_err(ClientError::from)
    }

    /// Clear listed target overrides. An empty list clears every override.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or duplicate IDs, or when the daemon
    /// rejects the D-Bus call.
    pub async fn clear_frequency_overrides(
        &self,
        target_ids: Vec<String>,
    ) -> Result<MutationReceipt, ClientError> {
        let mut unique = BTreeSet::new();
        for target_id in &target_ids {
            validate_identifier("target ID", target_id)?;
            if !unique.insert(target_id.as_str()) {
                return Err(ClientError::InvalidRequest(format!(
                    "duplicate target '{target_id}'"
                )));
            }
        }
        self.proxy()
            .await?
            .clear_frequency_overrides(target_ids)
            .await
            .map_err(ClientError::from)
    }

    /// Request a transactional configuration reload.
    ///
    /// # Errors
    ///
    /// Returns an error when the D-Bus call or daemon-side reload fails.
    pub async fn reload_config(&self) -> Result<ReloadReport, ClientError> {
        self.proxy()
            .await?
            .reload_config()
            .await
            .map_err(ClientError::from)
    }

    /// List persistent application rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the D-Bus call is unavailable.
    pub async fn app_rules(&self) -> Result<Vec<AppRule>, ClientError> {
        self.proxy()
            .await?
            .list_app_rules()
            .await
            .map_err(ClientError::from)
    }

    /// Create or replace an administrator-owned global application rule.
    ///
    /// API v1 matches an exact executable path, a kernel-name regex, or both.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid rule or a rejected D-Bus call.
    pub async fn set_app_rule(&self, rule: AppRule) -> Result<MutationReceipt, ClientError> {
        validate_rule(&rule)?;
        self.proxy()
            .await?
            .set_app_rule(rule)
            .await
            .map_err(ClientError::from)
    }

    /// Remove a persistent application rule.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid rule ID or a rejected D-Bus call.
    pub async fn remove_app_rule(&self, rule_id: &str) -> Result<MutationReceipt, ClientError> {
        validate_identifier("rule ID", rule_id)?;
        self.proxy()
            .await?
            .remove_app_rule(rule_id)
            .await
            .map_err(ClientError::from)
    }

    /// Compose a deterministic diagnostic report without exposing raw paths.
    ///
    /// # Errors
    ///
    /// Returns an error when status or capability retrieval fails.
    pub async fn diagnose(&self) -> Result<DiagnosticReport, ClientError> {
        let status = self.status().await?;
        let capabilities = self.capabilities().await?;
        let mut checks = Vec::with_capacity(5);

        checks.push(DiagnosticCheck {
            id: "api.compatible".into(),
            passed: true,
            message: format!("daemon API {} is compatible", status.api_version),
        });
        checks.push(DiagnosticCheck {
            id: "daemon.health".into(),
            passed: status.health.state == "healthy",
            message: status.health.summary.clone(),
        });
        checks.push(DiagnosticCheck {
            id: "actuator.writable".into(),
            passed: !status.health.read_only,
            message: if status.health.read_only {
                "actuator is in read-only degraded mode".into()
            } else {
                "actuator accepts authorized mutations".into()
            },
        });
        checks.push(DiagnosticCheck {
            id: "recovery.complete".into(),
            passed: !status.health.recovery_pending,
            message: if status.health.recovery_pending {
                "crash recovery is still pending".into()
            } else {
                "no unfinished recovery journal".into()
            },
        });
        checks.push(DiagnosticCheck {
            id: "targets.discovered".into(),
            passed: !capabilities.targets.is_empty(),
            message: format!("{} target(s) discovered", capabilities.targets.len()),
        });

        let healthy = checks.iter().all(|check| check.passed);
        Ok(DiagnosticReport {
            api_version: status.api_version,
            healthy,
            checks,
        })
    }

    /// Return the active workload from a fresh coherent status snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when status retrieval fails.
    pub async fn active_workload(&self) -> Result<ActiveWorkload, ClientError> {
        Ok(self.status().await?.active_workload)
    }
}

fn ensure_compatible(server: ApiVersion) -> Result<(), ClientError> {
    if ApiVersion::CURRENT.is_compatible_with(server) {
        Ok(())
    } else {
        Err(ClientError::IncompatibleApi {
            client: ApiVersion::CURRENT,
            server,
        })
    }
}

fn validate_workload_pid(pid: u32) -> Result<(), ClientError> {
    if pid == 0 {
        return Err(ClientError::InvalidRequest(
            "PID zero is not a workload".into(),
        ));
    }
    Ok(())
}

fn validate_frequency_overrides(overrides: &[FrequencyOverride]) -> Result<(), ClientError> {
    if overrides.is_empty() {
        return Err(ClientError::InvalidRequest(
            "at least one frequency override is required".into(),
        ));
    }

    let mut target_ids = BTreeSet::new();
    for request in overrides {
        validate_identifier("target ID", &request.target_id)?;
        if request.min_hz == 0 || request.max_hz == 0 {
            return Err(ClientError::InvalidRequest(format!(
                "target '{}' has a zero frequency bound",
                request.target_id
            )));
        }
        if request.min_hz > request.max_hz {
            return Err(ClientError::InvalidRequest(format!(
                "target '{}' minimum exceeds maximum",
                request.target_id
            )));
        }
        if request.reason.len() > 256 {
            return Err(ClientError::InvalidRequest(format!(
                "target '{}' reason exceeds 256 bytes",
                request.target_id
            )));
        }
        if !target_ids.insert(request.target_id.as_str()) {
            return Err(ClientError::InvalidRequest(format!(
                "duplicate target '{}'",
                request.target_id
            )));
        }
    }
    Ok(())
}

fn validate_identifier(kind: &str, value: &str) -> Result<(), ClientError> {
    if value.is_empty() {
        return Err(ClientError::InvalidRequest(format!(
            "{kind} must not be empty"
        )));
    }
    if value.len() > 64 {
        return Err(ClientError::InvalidRequest(format!(
            "{kind} exceeds 64 bytes"
        )));
    }
    let mut bytes = value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(ClientError::InvalidRequest(format!(
            "{kind} contains unsupported characters"
        )));
    }
    Ok(())
}

fn validate_rule(rule: &AppRule) -> Result<(), ClientError> {
    validate_identifier("rule ID", &rule.id)?;
    if rule.owner_uid != u32::MAX {
        return Err(ClientError::InvalidRequest(
            "D-Bus API v1 only supports administrator-owned global rules".into(),
        ));
    }
    validate_identifier("mode", &rule.mode)?;
    if rule.executable.is_none() && rule.comm_regex.is_none() {
        return Err(ClientError::InvalidRequest(
            "an executable or comm regex matcher is required".into(),
        ));
    }
    for (name, value) in [
        ("executable", rule.executable.as_deref()),
        ("comm regex", rule.comm_regex.as_deref()),
    ] {
        if let Some(value) = value {
            if value.is_empty() {
                return Err(ClientError::InvalidRequest(format!(
                    "rule {name} must not be empty"
                )));
            }
            if value.len() > 1024 {
                return Err(ClientError::InvalidRequest(format!(
                    "rule {name} exceeds 1024 bytes"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        validate_frequency_overrides, validate_identifier, validate_rule, validate_workload_pid,
    };
    use crate::{AppRule, ClientError, FrequencyOverride};

    #[test]
    fn identifiers_reject_paths_and_whitespace() {
        assert!(validate_identifier("target", "cpu.policy0").is_ok());
        assert!(matches!(
            validate_identifier("target", "/sys/policy0"),
            Err(ClientError::InvalidRequest(_))
        ));
        assert!(validate_identifier("target", "policy zero").is_err());
    }

    #[test]
    fn workload_requests_only_validate_the_untrusted_pid() {
        assert!(validate_workload_pid(42).is_ok());
        assert!(validate_workload_pid(0).is_err());
    }

    #[test]
    fn client_preserves_non_kilohertz_frequency_bounds() {
        let request = FrequencyOverride {
            target_id: "gpu.generic".into(),
            min_hz: 1_001,
            max_hz: 1_003,
            ttl_ms: 0,
            reason: "unit test".into(),
        };
        validate_frequency_overrides(&[request]).expect("exact hertz are valid");
    }

    fn app_rule() -> AppRule {
        AppRule {
            id: "game".into(),
            enabled: true,
            owner_uid: u32::MAX,
            executable: Some("/usr/bin/game".into()),
            comm_regex: None,
            mode: "performance".into(),
            priority: 0,
        }
    }

    #[test]
    fn app_rules_preserve_single_and_composite_matchers() {
        assert!(validate_rule(&app_rule()).is_ok());

        let mut regex_only = app_rule();
        regex_only.executable = None;
        regex_only.comm_regex = Some("^game$".into());
        assert!(validate_rule(&regex_only).is_ok());

        let mut composite = app_rule();
        composite.comm_regex = Some("^game$".into());
        assert!(validate_rule(&composite).is_ok());

        let mut empty = app_rule();
        empty.executable = None;
        assert!(validate_rule(&empty).is_err());
    }

    #[test]
    fn app_rules_are_global_in_api_v1() {
        let mut rule = app_rule();
        rule.owner_uid = 1_000;
        assert!(matches!(
            validate_rule(&rule),
            Err(ClientError::InvalidRequest(message))
                if message.contains("administrator-owned global")
        ));
    }
}
