//! Pure presentation logic for the capability-driven GTK client.

use std::collections::BTreeMap;

use uperf_api::{
    ActiveWorkload, Capabilities, DaemonStatus, FrequencyOverride, FrequencyStatus, HealthStatus,
    ModeInfo, TargetCapability, ThermalStatus, feature,
};

use crate::i18n::{
    localized_mode_description, localized_mode_label, localized_protocol_value, tr, translate_known,
};

/// Presentation state derived only from versioned API DTOs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewModel {
    pub daemon_state: String,
    pub health: HealthView,
    pub profile: String,
    pub scene: String,
    pub modes: Vec<ModeView>,
    pub targets: Vec<TargetView>,
    pub thermal: Option<ThermalView>,
    pub workload: Option<WorkloadView>,
}

/// Aggregate health summary and every structured daemon finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthView {
    pub summary: String,
    pub issues: Vec<HealthIssueView>,
}

/// One human-readable structured health finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthIssueView {
    pub message: String,
    pub detail: String,
}

/// One daemon-advertised mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeView {
    pub id: String,
    pub label: String,
    pub description: String,
    pub selected: bool,
}

/// One daemon-advertised frequency target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetView {
    pub capability: TargetCapability,
    pub status: Option<FrequencyStatus>,
    pub choices_hz: Vec<u64>,
}

/// Aggregate thermal information exposed by D-Bus v1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThermalView {
    pub state: String,
    pub temperature: String,
    pub detail: String,
}

/// Redacted active-workload information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadView {
    pub active: ActiveWorkload,
}

impl ViewModel {
    /// Merge coherent status with dynamic capabilities.
    #[must_use]
    pub fn from_api(capabilities: &Capabilities, status: &DaemonStatus) -> Self {
        let frequency_by_id: BTreeMap<&str, &FrequencyStatus> = status
            .frequencies
            .iter()
            .map(|frequency| (frequency.target_id.as_str(), frequency))
            .collect();

        let modes = capabilities
            .modes
            .iter()
            .map(|mode| mode_view(mode, &status.mode))
            .collect();
        let targets = capabilities
            .targets
            .iter()
            .cloned()
            .map(|capability| TargetView {
                choices_hz: frequency_choices(&capability),
                status: frequency_by_id
                    .get(capability.id.as_str())
                    .map(|value| (*value).clone()),
                capability,
            })
            .collect();

        let thermal = capabilities
            .supports(feature::THERMAL_GUARD)
            .then(|| thermal_view(&status.thermal))
            .or_else(|| (!status.thermal.state.is_empty()).then(|| thermal_view(&status.thermal)));
        let workload = (capabilities.supports(feature::ACTIVE_WORKLOAD)
            || status.active_workload.present)
            .then(|| WorkloadView {
                active: status.active_workload.clone(),
            });

        Self {
            daemon_state: localized_protocol_value(&status.state),
            health: health_view(&status.health),
            profile: localized_protocol_value(&status.effective_profile),
            scene: localized_protocol_value(&status.dominant_scene),
            modes,
            targets,
            thermal,
            workload,
        }
    }
}

fn health_view(health: &HealthStatus) -> HealthView {
    let issues = health
        .issues
        .iter()
        .map(|issue| HealthIssueView {
            message: translate_known(&issue.message),
            detail: format!(
                "{} · {} · {}",
                localized_protocol_value(&issue.severity),
                localized_protocol_value(&issue.component),
                issue.code
            ),
        })
        .collect();
    HealthView {
        summary: localized_protocol_value(&health.summary),
        issues,
    }
}

fn mode_view(mode: &ModeInfo, selected: &str) -> ModeView {
    ModeView {
        id: mode.id.clone(),
        label: localized_mode_label(&mode.id, &mode.display_name),
        description: localized_mode_description(&mode.id, &mode.description),
        selected: mode.id == selected,
    }
}

/// Produce sorted choices without assuming an OPP count or target kind.
#[must_use]
pub fn frequency_choices(target: &TargetCapability) -> Vec<u64> {
    if target.minimum_hz == 0 || target.maximum_hz < target.minimum_hz {
        return Vec::new();
    }

    let mut choices: Vec<u64> = target
        .available_hz
        .iter()
        .copied()
        .filter(|frequency| *frequency >= target.minimum_hz && *frequency <= target.maximum_hz)
        .collect();
    choices.push(target.minimum_hz);
    choices.push(target.maximum_hz);
    choices.sort_unstable();
    choices.dedup();
    choices
}

/// Validate and construct the typed request used by the confirmation dialog.
pub fn frequency_override(
    target: &TargetCapability,
    minimum_hz: u64,
    maximum_hz: u64,
) -> Result<FrequencyOverride, String> {
    if !target.can_override {
        return Err(format!("{} does not allow manual overrides", target.label));
    }
    if minimum_hz > maximum_hz {
        return Err("minimum frequency exceeds maximum frequency".into());
    }
    if minimum_hz < target.minimum_hz || maximum_hz > target.maximum_hz {
        return Err("requested frequencies exceed the advertised hardware bounds".into());
    }
    if !target.available_hz.is_empty()
        && (!target.available_hz.contains(&minimum_hz)
            || !target.available_hz.contains(&maximum_hz))
    {
        return Err("requested frequency is not an advertised operating point".into());
    }

    Ok(FrequencyOverride {
        target_id: target.id.clone(),
        min_hz: minimum_hz,
        max_hz: maximum_hz,
        ttl_ms: 0,
        reason: "confirmed in uperf-gui".into(),
    })
}

/// Convert a telemetry CPU load (hundredths of one percent) to a percentage.
#[must_use]
pub fn cpu_load_percent(load: uperf_api::CpuLoad) -> f64 {
    f64::from(load.utilization_basis_points) / 100.0
}

fn thermal_view(thermal: &ThermalStatus) -> ThermalView {
    let temperature = if thermal.max_temperature_millicelsius == 0
        && matches!(thermal.state.as_str(), "unavailable" | "stale" | "")
    {
        tr("Unavailable").into()
    } else {
        format!(
            "{:.1} °C",
            f64::from(thermal.max_temperature_millicelsius) / 1_000.0
        )
    };
    let mut conditions = Vec::new();
    if thermal.cap_active {
        conditions.push(tr("safety cap active"));
    }
    if thermal.sensors_stale {
        conditions.push(tr("sensor data stale"));
    }
    let detail = if conditions.is_empty() {
        tr("Sensors healthy").into()
    } else {
        conditions.join(", ")
    };
    ThermalView {
        state: localized_protocol_value(&thermal.state),
        temperature,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use uperf_api::{
        ActiveWorkload, Capabilities, DaemonStatus, FrequencyStatus, HealthIssue, HealthStatus,
        ModeInfo, TargetCapability, feature,
    };

    use super::{ViewModel, frequency_choices, frequency_override};

    fn target() -> TargetCapability {
        TargetCapability {
            id: "cpu.policy7".into(),
            kind: "cpufreq".into(),
            label: "CPUs 4, 8".into(),
            cpus: vec![4, 8],
            minimum_hz: 300_000_000,
            maximum_hz: 2_000_000_000,
            available_hz: vec![2_000_000_000, 900_000_000, 300_000_000, 900_000_000],
            can_override: true,
        }
    }

    #[test]
    fn choices_are_dynamic_sorted_and_deduplicated() {
        assert_eq!(
            frequency_choices(&target()),
            vec![300_000_000, 900_000_000, 2_000_000_000]
        );
    }

    #[test]
    fn continuous_targets_still_offer_bounds() {
        let mut continuous = target();
        continuous.available_hz.clear();
        assert_eq!(
            frequency_choices(&continuous),
            vec![300_000_000, 2_000_000_000]
        );
    }

    #[test]
    fn override_rejects_non_opp_and_inverted_ranges() {
        assert!(frequency_override(&target(), 900_000_000, 2_000_000_000).is_ok());
        assert!(frequency_override(&target(), 800_000_000, 2_000_000_000).is_err());
        assert!(frequency_override(&target(), 2_000_000_000, 900_000_000).is_err());
    }

    #[test]
    fn generic_devfreq_opps_preserve_exact_hertz() {
        let target = TargetCapability {
            id: "gpu.generic".into(),
            kind: "devfreq".into(),
            label: "Generic devfreq".into(),
            cpus: Vec::new(),
            minimum_hz: 1_001,
            maximum_hz: 1_003,
            available_hz: vec![1_003, 1_002, 1_001],
            can_override: true,
        };
        assert_eq!(frequency_choices(&target), vec![1_001, 1_002, 1_003]);
        let request = frequency_override(&target, 1_001, 1_002).expect("exact OPPs");
        assert_eq!(request.min_hz, 1_001);
        assert_eq!(request.max_hz, 1_002);
    }

    #[test]
    fn api_state_is_joined_by_stable_target_id() {
        let capabilities = Capabilities {
            features: vec![
                feature::THERMAL_GUARD.into(),
                feature::ACTIVE_WORKLOAD.into(),
            ],
            modes: vec![ModeInfo {
                id: "auto".into(),
                display_name: "Automatic".into(),
                description: "Follow policy".into(),
            }],
            targets: vec![target()],
            ..Capabilities::default()
        };
        let status = DaemonStatus {
            mode: "auto".into(),
            frequencies: vec![FrequencyStatus {
                target_id: "cpu.policy7".into(),
                applied_min_hz: 300_000_000,
                applied_max_hz: 900_000_000,
                applied_verified: true,
                ..FrequencyStatus::default()
            }],
            active_workload: ActiveWorkload {
                present: true,
                name: "game".into(),
                ..ActiveWorkload::default()
            },
            ..DaemonStatus::default()
        };

        let view = ViewModel::from_api(&capabilities, &status);
        assert_eq!(view.targets[0].capability.cpus, vec![4, 8]);
        assert_eq!(
            view.targets[0]
                .status
                .as_ref()
                .expect("status")
                .applied_max_hz,
            900_000_000
        );
        assert!(view.thermal.is_some());
        assert!(view.workload.expect("workload").active.present);
    }

    #[test]
    fn informational_health_issues_are_preserved_even_when_health_is_healthy() {
        let status = DaemonStatus {
            health: HealthStatus {
                state: "healthy".into(),
                summary: "all mandatory components are healthy".into(),
                issues: vec![HealthIssue {
                    code: "focus.rejected".into(),
                    severity: "info".into(),
                    component: "focus".into(),
                    message: "focused process is protected".into(),
                }],
                ..HealthStatus::default()
            },
            ..DaemonStatus::default()
        };

        let view = ViewModel::from_api(&Capabilities::default(), &status);

        assert_eq!(view.health.summary, "all mandatory components are healthy");
        assert_eq!(view.health.issues.len(), 1);
        assert_eq!(
            view.health.issues[0].message,
            "focused process is protected"
        );
        assert_eq!(
            view.health.issues[0].detail,
            "info · focus · focus.rejected"
        );
    }
}
