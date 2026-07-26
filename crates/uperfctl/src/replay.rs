//! Deterministic offline comparison of the legacy and energy governors.

use std::{collections::BTreeMap, fmt, path::Path, str::FromStr};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uperf_core::{
    CpuEnergyModelConfig, CpuId, CpuSet, CpuTargetPolicy, EnergyModel, FrequencyLimits,
    FrequencyPolicy, GovernorDiagnostics, GovernorRollout, GovernorState, Hertz, Hint, HintSet,
    MAX_CONFIG_FILE_BYTES, MAX_CPU_POLICIES, ModeSelection, MonotonicMillis, ObservedFrequency,
    ObservedState, PolicyConfig, PolicyEngine, PolicyInput, ProfileId, Scene,
    TargetGovernorDiagnostics, TargetId,
};

use crate::config;

const REPLAY_SCHEMA_VERSION: u32 = 1;
const REPLAY_REPORT_FORMAT: &str = "uperf-governor-replay-v1";
const MAX_REPLAY_STEPS: usize = MAX_CONFIG_FILE_BYTES / 32;

/// Whether the candidate planner is being evaluated as observe-only shadow
/// output or as the active fail-closed energy rollout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateRollout {
    Shadow,
    Energy,
}

impl CandidateRollout {
    const fn governor_rollout(self) -> GovernorRollout {
        match self {
            Self::Shadow => GovernorRollout::Shadow,
            Self::Energy => GovernorRollout::Energy,
        }
    }
}

impl fmt::Display for CandidateRollout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Shadow => "shadow",
            Self::Energy => "energy",
        })
    }
}

impl FromStr for CandidateRollout {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "shadow" => Ok(Self::Shadow),
            "energy" => Ok(Self::Energy),
            _ => Err(format!(
                "unknown replay rollout '{value}'; expected shadow or energy"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayTrace {
    schema_version: u32,
    targets: Vec<ReplayTarget>,
    steps: Vec<ReplayStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayTarget {
    id: TargetId,
    cpus: CpuSet,
    frequency: FrequencyPolicy,
    energy_model: CpuEnergyModelConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayStep {
    timestamp_ms: u64,
    profile: ProfileId,
    scene: Scene,
    #[serde(default = "default_true")]
    integrate_elapsed_time: bool,
    #[serde(default)]
    thermal_degraded: bool,
    #[serde(default)]
    administrator_caps_hz: BTreeMap<TargetId, Hertz>,
    #[serde(default)]
    thermal_caps_hz: BTreeMap<TargetId, Hertz>,
    targets: BTreeMap<TargetId, ReplaySample>,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaySample {
    raw_load: f64,
    observed_frequency_hz: Hertz,
}

/// Machine-readable replay result. The format name is versioned separately
/// from the input schema so consumers can reject incompatible output.
#[derive(Debug, Serialize)]
pub struct ReplayReport {
    format: &'static str,
    input_schema_version: u32,
    candidate_rollout: CandidateRollout,
    steps: Vec<ReplayStepReport>,
    summary: ReplaySummary,
}

impl ReplayReport {
    pub fn print_human(&self) {
        println!(
            "governor replay: {} steps, candidate rollout {}",
            self.steps.len(),
            self.candidate_rollout
        );
        for step in &self.steps {
            let budget = step
                .diagnostics
                .enforced_package_budget_mw
                .map_or_else(|| "bypassed".to_owned(), |value| format!("{value:.3} mW"));
            println!(
                "[{}] {}ms profile={} scene={} package_power={:.3} mW budget={}",
                step.index,
                step.timestamp_ms,
                step.profile,
                step.scene,
                step.diagnostics.estimated_package_power_mw,
                budget
            );
            for (id, target) in &step.targets {
                println!(
                    "  {id}: observed={} Hz legacy={}..{} Hz candidate={}..{} Hz delta={:+}/{:+} Hz",
                    target.observed_frequency_hz.get(),
                    target.legacy.min.get(),
                    target.legacy.max.get(),
                    target.candidate.min.get(),
                    target.candidate.max.get(),
                    target.minimum_delta_hz,
                    target.maximum_delta_hz
                );
            }
        }
        println!(
            "summary: {} target comparisons, {} changed, max |min delta|={} Hz, max |max delta|={} Hz",
            self.summary.target_comparisons,
            self.summary.changed_comparisons,
            self.summary.maximum_absolute_minimum_delta_hz,
            self.summary.maximum_absolute_maximum_delta_hz
        );
    }
}

#[derive(Debug, Serialize)]
struct ReplayStepReport {
    index: usize,
    timestamp_ms: u64,
    profile: ProfileId,
    scene: Scene,
    integrate_elapsed_time: bool,
    thermal_degraded: bool,
    targets: BTreeMap<TargetId, TargetComparison>,
    diagnostics: ReplayGovernorDiagnostics,
}

#[derive(Debug, Serialize)]
struct TargetComparison {
    raw_load: f64,
    observed_frequency_hz: Hertz,
    legacy: FrequencyLimits,
    candidate: FrequencyLimits,
    minimum_delta_hz: i64,
    maximum_delta_hz: i64,
    changed: bool,
    governor: ReplayTargetDiagnostics,
}

#[derive(Debug, Serialize)]
struct ReplayGovernorDiagnostics {
    elapsed_ms: u64,
    estimated_package_power_mw: f64,
    bucket_remaining_mj: f64,
    enforced_package_budget_mw: Option<f64>,
    bypassed_power_budget: bool,
    shared_ramp_progress: f64,
}

impl From<&GovernorDiagnostics> for ReplayGovernorDiagnostics {
    fn from(diagnostics: &GovernorDiagnostics) -> Self {
        Self {
            elapsed_ms: diagnostics.elapsed_ms,
            estimated_package_power_mw: report_float(diagnostics.estimated_package_power_mw),
            bucket_remaining_mj: report_float(diagnostics.bucket_remaining_mj),
            enforced_package_budget_mw: diagnostics
                .selected_package_budget_mw
                .is_finite()
                .then(|| report_float(diagnostics.selected_package_budget_mw)),
            bypassed_power_budget: diagnostics.bypassed_power_budget,
            shared_ramp_progress: report_float(diagnostics.shared_ramp_progress),
        }
    }
}

#[derive(Debug, Serialize)]
struct ReplayTargetDiagnostics {
    raw_load: f64,
    ema_load: f64,
    predicted_load: f64,
    selected_load: f64,
    effective_demand: f64,
    prediction_bypassed_ramp: bool,
    estimated_power_mw: f64,
    requested_floor_hz: Hertz,
    selected_floor_hz: Hertz,
    selected_cap_hz: Hertz,
}

impl From<&TargetGovernorDiagnostics> for ReplayTargetDiagnostics {
    fn from(diagnostics: &TargetGovernorDiagnostics) -> Self {
        Self {
            raw_load: report_float(diagnostics.raw_load),
            ema_load: report_float(diagnostics.ema_load),
            predicted_load: report_float(diagnostics.predicted_load),
            selected_load: report_float(diagnostics.selected_load),
            effective_demand: report_float(diagnostics.effective_demand),
            prediction_bypassed_ramp: diagnostics.prediction_bypassed_ramp,
            estimated_power_mw: report_float(diagnostics.estimated_power_mw),
            requested_floor_hz: diagnostics.requested_floor_hz,
            selected_floor_hz: diagnostics.selected_floor_hz,
            selected_cap_hz: diagnostics.selected_cap_hz,
        }
    }
}

fn report_float(value: f64) -> f64 {
    const PRECISION: f64 = 1_000_000.0;
    (value * PRECISION).round() / PRECISION
}

#[derive(Debug, Default, Serialize)]
struct ReplaySummary {
    steps: usize,
    target_comparisons: usize,
    changed_comparisons: usize,
    minimum_delta_direction: DirectionCounts,
    maximum_delta_direction: DirectionCounts,
    maximum_absolute_minimum_delta_hz: u64,
    maximum_absolute_maximum_delta_hz: u64,
    targets: BTreeMap<TargetId, TargetSummary>,
}

#[derive(Debug, Default, Serialize)]
struct TargetSummary {
    comparisons: usize,
    changed_comparisons: usize,
    minimum_delta_direction: DirectionCounts,
    maximum_delta_direction: DirectionCounts,
    maximum_absolute_minimum_delta_hz: u64,
    maximum_absolute_maximum_delta_hz: u64,
}

#[derive(Debug, Default, Serialize)]
struct DirectionCounts {
    candidate_lower: usize,
    equal: usize,
    candidate_higher: usize,
}

impl DirectionCounts {
    fn observe(&mut self, delta: i64) {
        match delta.cmp(&0) {
            std::cmp::Ordering::Less => self.candidate_lower += 1,
            std::cmp::Ordering::Equal => self.equal += 1,
            std::cmp::Ordering::Greater => self.candidate_higher += 1,
        }
    }
}

/// Load a bounded replay trace and policy document, then compare both planners
/// without connecting to D-Bus or touching hardware.
///
/// # Errors
///
/// Returns an error for invalid input, policy/model inconsistencies, or a
/// governor transition that cannot produce a complete candidate plan.
pub fn replay_path(
    trace_path: &Path,
    policy_path: &Path,
    rollout: CandidateRollout,
) -> Result<ReplayReport> {
    let trace_text = config::read_config_text(trace_path)?;
    let trace = serde_json::from_str::<ReplayTrace>(&trace_text)
        .with_context(|| format!("parse replay trace {}", trace_path.display()))?;
    let policy_text = config::read_config_text(policy_path)?;
    let policy = PolicyConfig::from_json(&policy_text)
        .with_context(|| format!("parse replay policy {}", policy_path.display()))?;
    replay(&trace, policy, rollout)
}

fn replay(
    trace: &ReplayTrace,
    mut policy: PolicyConfig,
    rollout: CandidateRollout,
) -> Result<ReplayReport> {
    validate_trace_shape(trace)?;
    let targets = expand_targets(&trace.targets)?;
    validate_steps(&trace.steps, &targets, &policy)?;
    policy.governor.rollout = rollout.governor_rollout();
    let engine = PolicyEngine::new(policy).context("construct replay policy engine")?;
    let empty_frequency_policies = BTreeMap::new();
    let empty_limits = BTreeMap::new();
    let mut governor_state = GovernorState::default();
    let mut reports = Vec::with_capacity(trace.steps.len());
    let mut summary = ReplaySummary {
        steps: trace.steps.len(),
        ..ReplaySummary::default()
    };

    for (index, step) in trace.steps.iter().enumerate() {
        let observed = observed_state(step, &targets);
        let hints = hints_for(step);
        let input = PolicyInput {
            generation: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
            observed: &observed,
            mode: ModeSelection::Forced(step.profile),
            app_profile: None,
            hints: &hints,
            cpu_targets: &targets,
            manual_target_policies: &empty_frequency_policies,
            manual_overrides: &empty_limits,
            administrator_caps: &step.administrator_caps_hz,
            thermal_caps: &step.thermal_caps_hz,
            thermal_degraded: step.thermal_degraded,
        };
        let legacy = engine
            .evaluate(&input)
            .with_context(|| format!("evaluate legacy planner at step {index}"))?;
        let candidate = engine
            .evaluate_stateful(&input, &governor_state, step.integrate_elapsed_time)
            .with_context(|| format!("evaluate candidate planner at step {index}"))?;
        if let Some(error) = candidate.governor_error {
            bail!("candidate planner failed at step {index}: {error}");
        }
        let candidate_frequencies = candidate
            .shadow_frequencies
            .context("candidate planner returned no frequency plan")?;
        let diagnostics = candidate
            .governor_diagnostics
            .context("candidate planner returned no diagnostics")?;
        governor_state = candidate.next_governor_state;
        let comparisons = compare_targets(
            step,
            &legacy.frequencies,
            &candidate_frequencies,
            &diagnostics,
            &mut summary,
        )
        .with_context(|| format!("compare planner output at step {index}"))?;
        reports.push(ReplayStepReport {
            index,
            timestamp_ms: step.timestamp_ms,
            profile: step.profile,
            scene: step.scene,
            integrate_elapsed_time: step.integrate_elapsed_time,
            thermal_degraded: step.thermal_degraded,
            targets: comparisons,
            diagnostics: ReplayGovernorDiagnostics::from(&diagnostics),
        });
    }

    Ok(ReplayReport {
        format: REPLAY_REPORT_FORMAT,
        input_schema_version: trace.schema_version,
        candidate_rollout: rollout,
        steps: reports,
        summary,
    })
}

fn validate_trace_shape(trace: &ReplayTrace) -> Result<()> {
    if trace.schema_version != REPLAY_SCHEMA_VERSION {
        bail!(
            "unsupported replay schema version {}; expected {}",
            trace.schema_version,
            REPLAY_SCHEMA_VERSION
        );
    }
    if trace.targets.is_empty() {
        bail!("replay trace must define at least one target");
    }
    if trace.targets.len() > MAX_CPU_POLICIES {
        bail!(
            "replay trace has {} targets; maximum is {MAX_CPU_POLICIES}",
            trace.targets.len()
        );
    }
    if trace.steps.is_empty() {
        bail!("replay trace must contain at least one step");
    }
    if trace.steps.len() > MAX_REPLAY_STEPS {
        bail!(
            "replay trace has {} steps; maximum is {MAX_REPLAY_STEPS}",
            trace.steps.len()
        );
    }
    Ok(())
}

fn expand_targets(targets: &[ReplayTarget]) -> Result<BTreeMap<TargetId, CpuTargetPolicy>> {
    let mut expanded = BTreeMap::new();
    let mut claimed_cpus = BTreeMap::<CpuId, TargetId>::new();
    for target in targets {
        if target.cpus.is_empty() {
            bail!("replay target `{}` has an empty CPU set", target.id);
        }
        target
            .frequency
            .validate()
            .with_context(|| format!("validate replay target `{}`", target.id))?;
        if target.frequency.hardware_limits.max.get() > i64::MAX as u64 {
            bail!(
                "replay target `{}` exceeds the signed delta reporting range",
                target.id
            );
        }
        for cpu in &target.cpus {
            if let Some(owner) = claimed_cpus.insert(*cpu, target.id.clone()) {
                bail!(
                    "CPU {} is assigned to both replay targets `{owner}` and `{}`",
                    cpu,
                    target.id
                );
            }
        }
        let energy_model = EnergyModel::from_config(
            &target.energy_model,
            &target.frequency.available_frequencies,
        )
        .with_context(|| format!("expand energy model for replay target `{}`", target.id))?;
        let previous = expanded.insert(
            target.id.clone(),
            CpuTargetPolicy {
                cpus: target.cpus.clone(),
                frequency: target.frequency.clone(),
                energy_model: Some(energy_model),
            },
        );
        if previous.is_some() {
            bail!("duplicate replay target `{}`", target.id);
        }
    }
    Ok(expanded)
}

fn validate_steps(
    steps: &[ReplayStep],
    targets: &BTreeMap<TargetId, CpuTargetPolicy>,
    policy: &PolicyConfig,
) -> Result<()> {
    let mut previous_timestamp = None;
    for (index, step) in steps.iter().enumerate() {
        if previous_timestamp.is_some_and(|previous| step.timestamp_ms <= previous) {
            bail!("replay step {index} timestamp must be strictly increasing");
        }
        previous_timestamp = Some(step.timestamp_ms);
        if policy.profile(step.profile).is_none() {
            bail!(
                "replay step {index} references missing profile `{}`",
                step.profile
            );
        }
        validate_step_targets(index, step, targets)?;
        validate_caps(index, "administrator", &step.administrator_caps_hz, targets)?;
        validate_caps(index, "thermal", &step.thermal_caps_hz, targets)?;
        if step.thermal_degraded {
            for id in targets.keys() {
                if !step.thermal_caps_hz.contains_key(id) {
                    bail!(
                        "replay step {index} is thermal-degraded but has no thermal cap for `{id}`"
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_step_targets(
    index: usize,
    step: &ReplayStep,
    targets: &BTreeMap<TargetId, CpuTargetPolicy>,
) -> Result<()> {
    for id in step.targets.keys() {
        if !targets.contains_key(id) {
            bail!("replay step {index} has a sample for unknown target `{id}`");
        }
    }
    for (id, target) in targets {
        let sample = step
            .targets
            .get(id)
            .with_context(|| format!("replay step {index} has no sample for target `{id}`"))?;
        if !sample.raw_load.is_finite() || !(0.0..=1.0).contains(&sample.raw_load) {
            bail!("replay step {index} target `{id}` raw_load must be finite and in 0..=1");
        }
        if sample.observed_frequency_hz < target.frequency.hardware_limits.min
            || sample.observed_frequency_hz > target.frequency.hardware_limits.max
        {
            bail!(
                "replay step {index} target `{id}` observed frequency is outside hardware limits"
            );
        }
    }
    Ok(())
}

fn validate_caps(
    index: usize,
    kind: &str,
    caps: &BTreeMap<TargetId, Hertz>,
    targets: &BTreeMap<TargetId, CpuTargetPolicy>,
) -> Result<()> {
    for (id, cap) in caps {
        let target = targets
            .get(id)
            .with_context(|| format!("replay step {index} has a {kind} cap for unknown `{id}`"))?;
        if *cap < target.frequency.hardware_limits.min
            || *cap > target.frequency.hardware_limits.max
        {
            bail!("replay step {index} target `{id}` {kind} cap is outside hardware limits");
        }
    }
    Ok(())
}

fn observed_state(
    step: &ReplayStep,
    targets: &BTreeMap<TargetId, CpuTargetPolicy>,
) -> ObservedState {
    let mut cpu_loads = BTreeMap::new();
    let mut frequencies = BTreeMap::new();
    for (id, target) in targets {
        let sample = step
            .targets
            .get(id)
            .expect("trace was validated before replay");
        for cpu in &target.cpus {
            cpu_loads.insert(*cpu, sample.raw_load);
        }
        frequencies.insert(
            id.clone(),
            ObservedFrequency {
                limits: target.frequency.hardware_limits,
                current: Some(sample.observed_frequency_hz),
            },
        );
    }
    ObservedState {
        timestamp: MonotonicMillis::new(step.timestamp_ms),
        cpu_loads,
        frequencies,
        thermal: BTreeMap::new(),
    }
}

fn hints_for(step: &ReplayStep) -> HintSet {
    let mut hints = HintSet::new();
    if step.scene != Scene::Idle {
        hints.activate(Hint::persistent(
            step.scene,
            MonotonicMillis::new(step.timestamp_ms),
        ));
    }
    hints
}

fn compare_targets(
    step: &ReplayStep,
    legacy: &BTreeMap<TargetId, FrequencyLimits>,
    candidate: &BTreeMap<TargetId, FrequencyLimits>,
    diagnostics: &GovernorDiagnostics,
    summary: &mut ReplaySummary,
) -> Result<BTreeMap<TargetId, TargetComparison>> {
    let mut comparisons = BTreeMap::new();
    for (id, sample) in &step.targets {
        let legacy = legacy
            .get(id)
            .with_context(|| format!("legacy planner omitted target `{id}`"))?;
        let candidate = candidate
            .get(id)
            .with_context(|| format!("candidate planner omitted target `{id}`"))?;
        let minimum_delta_hz = signed_delta(candidate.min, legacy.min)?;
        let maximum_delta_hz = signed_delta(candidate.max, legacy.max)?;
        let changed = candidate != legacy;
        observe_summary(summary, id, minimum_delta_hz, maximum_delta_hz, changed);
        let target_diagnostics = diagnostics
            .targets
            .get(id)
            .with_context(|| format!("candidate diagnostics omitted target `{id}`"))?;
        comparisons.insert(
            id.clone(),
            TargetComparison {
                raw_load: sample.raw_load,
                observed_frequency_hz: sample.observed_frequency_hz,
                legacy: *legacy,
                candidate: *candidate,
                minimum_delta_hz,
                maximum_delta_hz,
                changed,
                governor: ReplayTargetDiagnostics::from(target_diagnostics),
            },
        );
    }
    Ok(comparisons)
}

fn signed_delta(candidate: Hertz, legacy: Hertz) -> Result<i64> {
    let candidate = i64::try_from(candidate.get()).context("candidate frequency exceeds i64")?;
    let legacy = i64::try_from(legacy.get()).context("legacy frequency exceeds i64")?;
    Ok(candidate - legacy)
}

fn observe_summary(
    summary: &mut ReplaySummary,
    id: &TargetId,
    minimum_delta_hz: i64,
    maximum_delta_hz: i64,
    changed: bool,
) {
    summary.target_comparisons += 1;
    summary.changed_comparisons += usize::from(changed);
    summary.minimum_delta_direction.observe(minimum_delta_hz);
    summary.maximum_delta_direction.observe(maximum_delta_hz);
    summary.maximum_absolute_minimum_delta_hz = summary
        .maximum_absolute_minimum_delta_hz
        .max(minimum_delta_hz.unsigned_abs());
    summary.maximum_absolute_maximum_delta_hz = summary
        .maximum_absolute_maximum_delta_hz
        .max(maximum_delta_hz.unsigned_abs());

    let target = summary.targets.entry(id.clone()).or_default();
    target.comparisons += 1;
    target.changed_comparisons += usize::from(changed);
    target.minimum_delta_direction.observe(minimum_delta_hz);
    target.maximum_delta_direction.observe(maximum_delta_hz);
    target.maximum_absolute_minimum_delta_hz = target
        .maximum_absolute_minimum_delta_hz
        .max(minimum_delta_hz.unsigned_abs());
    target.maximum_absolute_maximum_delta_hz = target
        .maximum_absolute_maximum_delta_hz
        .max(maximum_delta_hz.unsigned_abs());
}

#[cfg(test)]
mod tests {
    use super::{CandidateRollout, ReplayTrace, replay};
    use uperf_core::PolicyConfig;

    const POLICY: &str = include_str!("../tests/fixtures/governor-replay-policy.json");
    const TRACE: &str = include_str!("../tests/fixtures/governor-replay-trace.json");

    fn inputs() -> (ReplayTrace, PolicyConfig) {
        (
            serde_json::from_str(TRACE).expect("fixture trace"),
            PolicyConfig::from_json(POLICY).expect("fixture policy"),
        )
    }

    #[test]
    fn fixture_replays_all_steps_and_targets() {
        let (trace, policy) = inputs();
        let report = replay(&trace, policy, CandidateRollout::Shadow).expect("replay");
        let value = serde_json::to_value(report).expect("serialize report");
        assert_eq!(value["format"], "uperf-governor-replay-v1");
        assert_eq!(value["summary"]["steps"], 4);
        assert_eq!(value["summary"]["target_comparisons"], 8);
        assert_eq!(
            value["steps"][1]["targets"]["cpu.big"]["governor"]["raw_load"],
            0.7
        );
    }

    #[test]
    fn non_increasing_timestamps_are_rejected() {
        let (mut trace, policy) = inputs();
        trace.steps[1].timestamp_ms = trace.steps[0].timestamp_ms;
        let error = replay(&trace, policy, CandidateRollout::Shadow).unwrap_err();
        assert!(error.to_string().contains("strictly increasing"));
    }

    #[test]
    fn missing_target_sample_is_rejected() {
        let (mut trace, policy) = inputs();
        let id = trace.targets[0].id.clone();
        trace.steps[0].targets.remove(&id);
        let error = replay(&trace, policy, CandidateRollout::Shadow).unwrap_err();
        assert!(error.to_string().contains("has no sample"));
    }
}
