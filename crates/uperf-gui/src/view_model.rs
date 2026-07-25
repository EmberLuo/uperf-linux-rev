//! Pure presentation logic for the capability-driven GTK client.

use std::collections::BTreeMap;

use uperf_api::{
    ActiveWorkload, ApiVersion, Capabilities, DaemonStatus, FrequencyOverride, FrequencyStatus,
    HealthStatus, ModeInfo, TargetCapability, ThermalStatus, feature,
};

use crate::i18n::{
    localized_mode_description, localized_mode_label, localized_protocol_value, tr, translate_known,
};

/// Stable health code emitted when the daemon refuses a focus report.
pub const FOCUS_REJECTED_CODE: &str = "focus.rejected";
/// Command that turns the bundled GNOME reporter on for the current user.
pub const FOCUS_REPORTER_COMMAND: &str = "gnome-extensions enable focus@uperflinux.org";
/// Policy key that gates the whole focus path inside the daemon.
pub const FOCUS_POLICY_KEY: &str = "scheduler.focus.enabled";
/// First API minor that carries the focus methods at all.
const FOCUS_API_MINOR: u32 = 2;

const _: () = assert!(ApiVersion::CURRENT.minor >= FOCUS_API_MINOR);

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
    pub focus: FocusView,
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

/// What the desktop knows about the compositor-side focus reporter.
///
/// The daemon cannot see whether a reporter exists, so this is probed
/// separately and stays [`ReporterState::Unknown`] outside GNOME Shell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReporterState {
    /// Not probed yet, or this desktop exposes no extension interface.
    #[default]
    Unknown,
    /// The bundled reporter is installed and enabled.
    Enabled,
    /// The bundled reporter is installed but switched off.
    Disabled,
    /// No bundled reporter is installed for this user.
    Missing,
}

/// Why focus scheduling is or is not steering the machine right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusState {
    /// The daemon does not advertise the focus feature at all.
    Unsupported,
    /// A live lease makes the focused application the effective workload.
    Following,
    /// Focus is available but an explicit selection outranks it.
    Overridden,
    /// The daemon refused the most recent report.
    Rejected,
    /// Focus is available and nothing is being reported.
    Waiting,
}

/// Everything the focus card needs, decided without any GTK type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusView {
    pub state: FocusState,
    /// Whether the daemon advertises focus support at all.
    pub supported: bool,
    /// Short localized status line.
    pub summary: String,
    /// Localized explanation, or the daemon's own rejection message.
    pub detail: String,
    /// Current focus holder, only when a lease is actually in effect.
    pub holder: Option<String>,
    /// Shell command that resolves the current obstacle, when one exists.
    pub command: Option<String>,
}

/// The single button the focus card may offer, chosen here so the widget layer
/// never has to re-derive it from a state and a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusAction {
    /// Nothing actionable: either focus already works, or only the
    /// administrator can change the situation.
    None,
    /// Ask GNOME Shell to switch the installed reporter on.
    EnableReporter,
    /// Drop the explicit selection so focus takes over again.
    ClearExplicit,
}

impl FocusView {
    /// The card before the daemon has been reached at all.
    ///
    /// Absent capabilities look exactly like a daemon that disabled focus, so
    /// deriving the card from them would send the user to edit a policy key that
    /// may already be correct. The reporter half is observed on the session bus
    /// and stays knowable, so its advice is kept.
    #[must_use]
    pub fn disconnected(reporter: ReporterState) -> Self {
        let (summary, detail) = reporter_obstacle(reporter).unwrap_or_else(|| {
            (
                tr("Waiting for the daemon").into(),
                tr("Focus state is unknown until the daemon connects").into(),
            )
        });
        Self {
            state: FocusState::Waiting,
            supported: false,
            summary,
            detail,
            holder: None,
            command: (reporter == ReporterState::Disabled)
                .then(|| FOCUS_REPORTER_COMMAND.to_owned()),
        }
    }

    /// Which action, if any, resolves the state the card is showing.
    #[must_use]
    pub fn action(&self) -> FocusAction {
        match self.state {
            FocusState::Overridden => FocusAction::ClearExplicit,
            // Only a reporter that exists can be switched on; a missing one
            // needs an install, which is not something the GUI should attempt.
            _ if self.command.is_some() => FocusAction::EnableReporter,
            _ => FocusAction::None,
        }
    }
}

impl ViewModel {
    /// Merge coherent status with dynamic capabilities.
    #[must_use]
    pub fn from_api(
        capabilities: &Capabilities,
        status: &DaemonStatus,
        reporter: ReporterState,
    ) -> Self {
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
            focus: focus_view(capabilities, status, reporter),
        }
    }
}

/// Decide the focus card from three independent facts: whether the daemon
/// enabled the feature, whether a reporter exists on this desktop, and what the
/// daemon currently reports about the effective workload.
fn focus_view(
    capabilities: &Capabilities,
    status: &DaemonStatus,
    reporter: ReporterState,
) -> FocusView {
    let supported = capabilities.supports(feature::FOREGROUND_FOCUS);
    let active = &status.active_workload;
    let holder = (active.present && active.source == "focus")
        .then(|| format!("{} · PID {}", active.name, active.identity.pid));
    let rejection = status
        .health
        .issues
        .iter()
        .find(|issue| issue.code == FOCUS_REJECTED_CODE);

    if !supported {
        // A daemon that predates the focus contract cannot be fixed by editing
        // policy, so the two causes need different advice.
        let detail = if capabilities.api_version.minor < FOCUS_API_MINOR {
            tr("This daemon is older than the focus contract; update uperf-linux").to_owned()
        } else {
            format!(
                "{} {FOCUS_POLICY_KEY}",
                tr("The daemon has focus scheduling disabled; enable")
            )
        };
        return FocusView {
            state: FocusState::Unsupported,
            supported,
            summary: tr("Off").into(),
            detail,
            holder: None,
            command: None,
        };
    }

    // A reporter obstacle is worth showing even while a stale lease survives,
    // but a live lease is the stronger signal and wins the headline.
    if holder.is_none()
        && let Some((summary, detail)) = reporter_obstacle(reporter)
    {
        return FocusView {
            state: FocusState::Waiting,
            supported,
            summary,
            detail,
            holder: None,
            command: (reporter == ReporterState::Disabled)
                .then(|| FOCUS_REPORTER_COMMAND.to_owned()),
        };
    }

    if let Some(holder) = holder {
        return FocusView {
            state: FocusState::Following,
            supported,
            summary: tr("Following the focused application").into(),
            detail: tr("The focused application is the effective workload").into(),
            holder: Some(holder),
            command: None,
        };
    }

    if active.present {
        return FocusView {
            state: FocusState::Overridden,
            supported,
            summary: tr("Paused by an explicit selection").into(),
            detail: tr("Clear the explicit workload to follow focus again").into(),
            holder: None,
            command: None,
        };
    }

    if let Some(issue) = rejection {
        return FocusView {
            state: FocusState::Rejected,
            supported,
            summary: tr("Last report refused").into(),
            detail: translate_known(&issue.message),
            holder: None,
            command: None,
        };
    }

    FocusView {
        state: FocusState::Waiting,
        supported,
        summary: tr("Waiting for a focus report").into(),
        detail: tr("No application is currently reported as focused").into(),
        holder: None,
        command: None,
    }
}

fn reporter_obstacle(reporter: ReporterState) -> Option<(String, String)> {
    match reporter {
        ReporterState::Disabled => Some((
            tr("Reporter is installed but off").into(),
            tr("Turn the GNOME focus reporter on to let focus steer scheduling").into(),
        )),
        ReporterState::Missing => Some((
            tr("No focus reporter found").into(),
            tr("Install the GNOME reporter, or report focus with uperfctl foreground").into(),
        )),
        ReporterState::Enabled | ReporterState::Unknown => None,
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

    use super::{
        FOCUS_REPORTER_COMMAND, FocusAction, FocusState, ReporterState, ViewModel,
        frequency_choices, frequency_override,
    };

    fn focus_capable() -> Capabilities {
        Capabilities {
            features: vec![
                feature::FOREGROUND_FOCUS.into(),
                feature::ACTIVE_WORKLOAD.into(),
            ],
            ..Capabilities::default()
        }
    }

    fn focus_view(status: &DaemonStatus, reporter: ReporterState) -> super::FocusView {
        ViewModel::from_api(&focus_capable(), status, reporter).focus
    }

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

        let view = ViewModel::from_api(&capabilities, &status, ReporterState::Unknown);
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

        let view = ViewModel::from_api(&Capabilities::default(), &status, ReporterState::Unknown);

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

    #[test]
    fn focus_is_off_when_the_daemon_does_not_advertise_the_feature() {
        let view = ViewModel::from_api(
            &Capabilities::default(),
            &DaemonStatus::default(),
            ReporterState::Enabled,
        );
        assert_eq!(view.focus.state, FocusState::Unsupported);
        assert!(!view.focus.supported);
        assert!(view.focus.detail.contains("scheduler.focus.enabled"));
        assert!(view.focus.command.is_none());
        assert_eq!(view.focus.action(), FocusAction::None);
    }

    #[test]
    fn an_old_daemon_is_told_to_update_rather_than_to_edit_policy() {
        let capabilities = Capabilities {
            api_version: uperf_api::ApiVersion { major: 1, minor: 1 },
            ..Capabilities::default()
        };

        let view = ViewModel::from_api(
            &capabilities,
            &DaemonStatus::default(),
            ReporterState::Enabled,
        );

        assert_eq!(view.focus.state, FocusState::Unsupported);
        assert!(
            !view.focus.detail.contains("scheduler.focus.enabled"),
            "editing a policy key that this daemon ignores would be bad advice"
        );
    }

    #[test]
    fn a_disabled_reporter_offers_the_command_that_enables_it() {
        let view = focus_view(&DaemonStatus::default(), ReporterState::Disabled);
        assert_eq!(view.state, FocusState::Waiting);
        assert_eq!(view.command.as_deref(), Some(FOCUS_REPORTER_COMMAND));
        assert_eq!(view.action(), FocusAction::EnableReporter);
    }

    #[test]
    fn a_missing_reporter_is_distinguished_from_a_disabled_one() {
        let view = focus_view(&DaemonStatus::default(), ReporterState::Missing);
        assert_eq!(view.state, FocusState::Waiting);
        assert!(
            view.command.is_none(),
            "enabling something that is not installed cannot be offered"
        );
        assert_eq!(view.action(), FocusAction::None);
    }

    #[test]
    fn a_disconnected_daemon_is_never_reported_as_having_focus_switched_off() {
        let view = super::FocusView::disconnected(ReporterState::Unknown);
        assert_eq!(view.state, FocusState::Waiting);
        assert!(!view.supported);
        assert!(view.holder.is_none());
        assert_eq!(
            view.action(),
            FocusAction::None,
            "no policy advice can be given before the daemon answers"
        );
    }

    #[test]
    fn a_disconnected_daemon_still_offers_to_switch_the_reporter_on() {
        let view = super::FocusView::disconnected(ReporterState::Disabled);
        assert_eq!(view.command.as_deref(), Some(FOCUS_REPORTER_COMMAND));
        assert_eq!(
            view.action(),
            FocusAction::EnableReporter,
            "the reporter lives on the session bus, so it stays fixable"
        );
    }

    #[test]
    fn a_live_lease_reports_its_holder_even_on_an_unprobed_desktop() {
        let status = DaemonStatus {
            active_workload: ActiveWorkload {
                present: true,
                identity: uperf_api::WorkloadIdentity {
                    pid: 4242,
                    ..uperf_api::WorkloadIdentity::default()
                },
                name: "blender".into(),
                source: "focus".into(),
                ..ActiveWorkload::default()
            },
            ..DaemonStatus::default()
        };

        let view = focus_view(&status, ReporterState::Unknown);

        assert_eq!(view.state, FocusState::Following);
        assert_eq!(view.holder.as_deref(), Some("blender · PID 4242"));
    }

    #[test]
    fn a_live_lease_outranks_a_stale_reporter_probe() {
        let status = DaemonStatus {
            active_workload: ActiveWorkload {
                present: true,
                name: "blender".into(),
                source: "focus".into(),
                ..ActiveWorkload::default()
            },
            ..DaemonStatus::default()
        };

        assert_eq!(
            focus_view(&status, ReporterState::Missing).state,
            FocusState::Following
        );
    }

    #[test]
    fn an_explicit_selection_explains_why_focus_is_paused() {
        let status = DaemonStatus {
            active_workload: ActiveWorkload {
                present: true,
                name: "game".into(),
                source: "explicit".into(),
                ..ActiveWorkload::default()
            },
            ..DaemonStatus::default()
        };

        let view = focus_view(&status, ReporterState::Enabled);

        assert_eq!(view.state, FocusState::Overridden);
        assert!(view.holder.is_none());
        assert_eq!(view.action(), FocusAction::ClearExplicit);
    }

    #[test]
    fn a_working_focus_path_offers_nothing_to_fix() {
        let status = DaemonStatus {
            active_workload: ActiveWorkload {
                present: true,
                name: "blender".into(),
                source: "focus".into(),
                ..ActiveWorkload::default()
            },
            ..DaemonStatus::default()
        };

        assert_eq!(
            focus_view(&status, ReporterState::Enabled).action(),
            FocusAction::None
        );
    }

    #[test]
    fn a_refused_report_surfaces_the_daemons_own_reason() {
        let status = DaemonStatus {
            health: HealthStatus {
                issues: vec![HealthIssue {
                    code: "focus.rejected".into(),
                    severity: "info".into(),
                    component: "focus".into(),
                    message: "process gnome-shell is protected from focus leasing".into(),
                }],
                ..HealthStatus::default()
            },
            ..DaemonStatus::default()
        };

        let view = focus_view(&status, ReporterState::Enabled);

        assert_eq!(view.state, FocusState::Rejected);
        assert_eq!(
            view.detail,
            "process gnome-shell is protected from focus leasing"
        );
    }

    #[test]
    fn an_enabled_reporter_with_no_report_yet_is_merely_waiting() {
        let view = focus_view(&DaemonStatus::default(), ReporterState::Enabled);
        assert_eq!(view.state, FocusState::Waiting);
        assert!(view.command.is_none());
    }
}
