//! Deterministic scene, load, frequency, and thermal policy.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    AppliedState, AppsConfig, CpuId, CpuSet, DesiredPlan, FrequencyLimits, Hertz, MilliCelsius,
    MonotonicMillis, ObservedState, PlanSource, PolicyConfig, ProcessIdentity, ProcessInfo,
    ProfileConfig, TargetFrequencyPlan, TargetId, TaskPlan, ThermalReading, ThermalZoneConfig,
    Validate, ValidationErrors, WorkloadMatcher,
};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileId {
    Powersave,
    Balance,
    Performance,
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Powersave => "powersave",
            Self::Balance => "balance",
            Self::Performance => "performance",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "profile", rename_all = "kebab-case")]
pub enum ModeSelection {
    Auto,
    Forced(ProfileId),
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Scene {
    Idle,
    Touch,
    Trigger,
    Gesture,
    Boost,
    Switch,
    Wake,
}

impl Scene {
    /// Higher values dominate lower values when several hints are active.
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Touch => 1,
            Self::Trigger => 2,
            Self::Gesture => 3,
            Self::Switch => 4,
            Self::Boost => 5,
            Self::Wake => 6,
        }
    }
}

impl fmt::Display for Scene {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Idle => "idle",
            Self::Touch => "touch",
            Self::Trigger => "trigger",
            Self::Gesture => "gesture",
            Self::Boost => "boost",
            Self::Switch => "switch",
            Self::Wake => "wake",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hint {
    pub scene: Scene,
    pub activated_at: MonotonicMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<MonotonicMillis>,
}

impl Hint {
    #[must_use]
    pub const fn persistent(scene: Scene, activated_at: MonotonicMillis) -> Self {
        Self {
            scene,
            activated_at,
            expires_at: None,
        }
    }

    #[must_use]
    pub const fn with_ttl(scene: Scene, activated_at: MonotonicMillis, duration_ms: u64) -> Self {
        Self {
            scene,
            activated_at,
            expires_at: Some(activated_at.saturating_add(duration_ms)),
        }
    }

    #[must_use]
    pub fn is_active_at(self, now: MonotonicMillis) -> bool {
        self.expires_at.is_none_or(|expires_at| now < expires_at)
    }
}

/// Active hints keyed by scene.  Re-activating a scene replaces its previous TTL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct HintSet(BTreeMap<Scene, Hint>);

impl HintSet {
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn activate(&mut self, hint: Hint) {
        if hint.scene != Scene::Idle {
            self.0.insert(hint.scene, hint);
        }
    }

    pub fn deactivate(&mut self, scene: Scene) -> Option<Hint> {
        self.0.remove(&scene)
    }

    /// Remove expired hints and report whether the set changed.
    pub fn expire(&mut self, now: MonotonicMillis) -> bool {
        let before = self.0.len();
        self.0.retain(|_, hint| hint.is_active_at(now));
        self.0.len() != before
    }

    #[must_use]
    pub fn contains_active(&self, scene: Scene, now: MonotonicMillis) -> bool {
        self.0
            .get(&scene)
            .is_some_and(|hint| hint.is_active_at(now))
    }

    #[must_use]
    pub fn dominant_at(&self, now: MonotonicMillis) -> Scene {
        self.dominant_at_excluding(now, &BTreeSet::new())
    }

    #[must_use]
    pub fn dominant_at_excluding(&self, now: MonotonicMillis, excluded: &BTreeSet<Scene>) -> Scene {
        self.0
            .values()
            .filter(|hint| hint.is_active_at(now) && !excluded.contains(&hint.scene))
            .max_by_key(|hint| hint.scene.priority())
            .map_or(Scene::Idle, |hint| hint.scene)
    }
}

#[must_use]
pub const fn resolve_effective_profile(
    mode: ModeSelection,
    app_profile: Option<ProfileId>,
    default_profile: ProfileId,
) -> ProfileId {
    match mode {
        ModeSelection::Forced(profile) => profile,
        ModeSelection::Auto => match app_profile {
            Some(profile) => profile,
            None => default_profile,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrequencyPolicy {
    pub hardware_limits: FrequencyLimits,
    pub floor: Hertz,
    pub reference: Hertz,
    pub efficient_cap: Hertz,
    /// Number of hertz represented by one integer in the kernel ABI.
    pub hertz_per_unit: u64,
    #[serde(default)]
    pub available_frequencies: Vec<Hertz>,
}

impl FrequencyPolicy {
    /// Validate model ordering, bounds, and discovered OPPs.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError`] if any frequency lies outside hardware bounds or
    /// the model is internally inconsistent.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if !self.hardware_limits.is_valid() {
            return Err(PolicyError::InvalidHardwareLimits(self.hardware_limits));
        }
        if !(self.hardware_limits.min <= self.floor
            && self.floor <= self.reference
            && self.reference <= self.hardware_limits.max
            && self.floor <= self.efficient_cap
            && self.efficient_cap <= self.hardware_limits.max)
        {
            return Err(PolicyError::InvalidFrequencyModel);
        }
        if self.hertz_per_unit == 0
            || [
                self.hardware_limits.min,
                self.hardware_limits.max,
                self.floor,
                self.reference,
                self.efficient_cap,
            ]
            .into_iter()
            .chain(self.available_frequencies.iter().copied())
            .any(|frequency| !frequency.get().is_multiple_of(self.hertz_per_unit))
        {
            return Err(PolicyError::InvalidFrequencyQuantum);
        }
        if self.available_frequencies.iter().any(|frequency| {
            *frequency < self.hardware_limits.min || *frequency > self.hardware_limits.max
        }) {
            return Err(PolicyError::OppOutsideHardwareLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadGovernorInput {
    pub demand: f64,
    pub burst: f64,
    pub margin: f64,
    pub limit_efficiency: bool,
    pub administrator_cap: Option<Hertz>,
    pub thermal_cap: Option<Hertz>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyError {
    #[error("hardware frequency limits are invalid: {0:?}")]
    InvalidHardwareLimits(FrequencyLimits),
    #[error("floor, reference, and efficient cap must each lie within the hardware limits")]
    InvalidFrequencyModel,
    #[error("an OPP lies outside the hardware limits")]
    OppOutsideHardwareLimits,
    #[error("frequency values are not representable in the kernel target unit")]
    InvalidFrequencyQuantum,
    #[error("no supported OPP is at or below safety cap {0}")]
    NoOppAtOrBelowCap(Hertz),
    #[error("requested frequency limits are reversed: {0:?}")]
    ReversedRequest(FrequencyLimits),
    #[error("load-policy input `{field}` must be finite and non-negative")]
    InvalidLoadInput { field: &'static str },
    #[error("policy configuration has no profile `{0}`")]
    MissingProfile(ProfileId),
    #[error("thermal-degraded target `{0}` has no sensor-failure cap")]
    MissingThermalCap(TargetId),
    #[error("scheduler rule references missing task profile `{0}`")]
    MissingTaskProfile(String),
    #[error("a validated workload matcher could not be compiled: {0}")]
    InvalidMatcherRegex(String),
    #[error("configuration is invalid: {0}")]
    InvalidConfiguration(#[from] ValidationErrors),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LoadGovernor;

impl LoadGovernor {
    /// Convert normalized demand and scene parameters into a safe OPP pair.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError`] for invalid model bounds or non-finite inputs.
    pub fn compute(
        policy: &FrequencyPolicy,
        input: LoadGovernorInput,
    ) -> Result<FrequencyLimits, PolicyError> {
        policy.validate()?;
        validate_non_negative_finite("demand", input.demand)?;
        validate_non_negative_finite("burst", input.burst)?;
        validate_non_negative_finite("margin", input.margin)?;

        let effective_demand = (input.demand + input.burst).clamp(0.0, 1.0);
        let requested = hertz_as_f64(policy.reference) * effective_demand * (1.0 + input.margin);
        let requested_min = Hertz(float_to_u64_ceil(requested).max(policy.floor.get()));
        let requested_max = if input.limit_efficiency {
            policy.efficient_cap
        } else {
            policy.hardware_limits.max
        };

        constrain_frequency_limits(
            FrequencyLimits {
                min: requested_min,
                max: requested_max,
            },
            policy.hardware_limits,
            input.administrator_cap,
            input.thermal_cap,
            &policy.available_frequencies,
            policy.hertz_per_unit,
        )
    }
}

/// Apply hardware/admin/thermal caps and snap the pair to supported OPPs.
///
/// The maximum is snapped downward before the minimum is snapped upward, which
/// guarantees the returned pair never has `min > max`.
///
/// # Errors
///
/// Returns [`PolicyError`] for inverted hardware or requested bounds.
pub fn constrain_frequency_limits(
    requested: FrequencyLimits,
    hardware: FrequencyLimits,
    administrator_cap: Option<Hertz>,
    thermal_cap: Option<Hertz>,
    available_frequencies: &[Hertz],
    hertz_per_unit: u64,
) -> Result<FrequencyLimits, PolicyError> {
    if !hardware.is_valid() {
        return Err(PolicyError::InvalidHardwareLimits(hardware));
    }
    if !requested.is_valid() {
        return Err(PolicyError::ReversedRequest(requested));
    }
    if hertz_per_unit == 0
        || !hardware.min.get().is_multiple_of(hertz_per_unit)
        || !hardware.max.get().is_multiple_of(hertz_per_unit)
    {
        return Err(PolicyError::InvalidFrequencyQuantum);
    }

    let hard_cap = [Some(hardware.max), administrator_cap, thermal_cap]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(hardware.max)
        .clamp(hardware.min, hardware.max);

    let mut opps = available_frequencies
        .iter()
        .copied()
        .filter(|frequency| *frequency >= hardware.min && *frequency <= hardware.max)
        .collect::<Vec<_>>();
    opps.sort_unstable();
    opps.dedup();

    let unsnapped_max = requested.max.clamp(hardware.min, hard_cap);
    let max = if opps.is_empty() {
        quantize_down(unsnapped_max, hertz_per_unit).max(hardware.min)
    } else {
        snap_down(&opps, unsnapped_max).ok_or(PolicyError::NoOppAtOrBelowCap(unsnapped_max))?
    };
    let unsnapped_min = requested.min.clamp(hardware.min, max);
    let min = if opps.is_empty() {
        quantize_up(unsnapped_min, hertz_per_unit).min(max)
    } else {
        snap_up(&opps, unsnapped_min)
            .filter(|frequency| *frequency <= max)
            .unwrap_or(max)
    };

    Ok(FrequencyLimits { min, max })
}

fn quantize_down(value: Hertz, quantum: u64) -> Hertz {
    Hertz::new(value.get() / quantum * quantum)
}

fn quantize_up(value: Hertz, quantum: u64) -> Hertz {
    let quotient = value.get() / quantum;
    let rounded = quotient.saturating_add(u64::from(!value.get().is_multiple_of(quantum)));
    Hertz::new(rounded.saturating_mul(quantum))
}

#[must_use]
pub fn snap_up(opps: &[Hertz], requested: Hertz) -> Option<Hertz> {
    opps.iter()
        .copied()
        .filter(|frequency| *frequency >= requested)
        .min()
}

#[must_use]
pub fn snap_down(opps: &[Hertz], requested: Hertz) -> Option<Hertz> {
    opps.iter()
        .copied()
        .filter(|frequency| *frequency <= requested)
        .max()
}

/// Compute an elapsed-time-weighted exponential moving average.
///
/// # Errors
///
/// Returns [`PolicyError`] for non-finite/negative samples or a zero time
/// constant.
pub fn time_weighted_ema(
    previous: f64,
    sample: f64,
    elapsed_ms: u64,
    time_constant_ms: u64,
) -> Result<f64, PolicyError> {
    validate_non_negative_finite("previous", previous)?;
    validate_non_negative_finite("sample", sample)?;
    if time_constant_ms == 0 {
        return Err(PolicyError::InvalidLoadInput {
            field: "time_constant_ms",
        });
    }
    let elapsed = std::time::Duration::from_millis(elapsed_ms).as_secs_f64();
    let time_constant = std::time::Duration::from_millis(time_constant_ms).as_secs_f64();
    let alpha = 1.0 - (-elapsed / time_constant).exp();
    Ok(previous + alpha * (sample - previous))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeavyLoadState {
    Idle,
    Heavy,
}

#[derive(Debug, Clone)]
pub struct HeavyLoadDetector {
    ema: Option<f64>,
    state: HeavyLoadState,
    below_exit_since: Option<MonotonicMillis>,
}

impl Default for HeavyLoadDetector {
    fn default() -> Self {
        Self {
            ema: None,
            state: HeavyLoadState::Idle,
            below_exit_since: None,
        }
    }
}

impl HeavyLoadDetector {
    #[must_use]
    pub fn state(&self) -> HeavyLoadState {
        self.state
    }

    #[must_use]
    pub fn ema(&self) -> Option<f64> {
        self.ema
    }

    /// Add one load sample and update the hysteretic heavy-load state.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError`] if a sample or threshold is non-finite/negative,
    /// or if the EMA time constant is zero.
    pub fn update(
        &mut self,
        sample: f64,
        elapsed_ms: u64,
        now: MonotonicMillis,
        config: &crate::LoadConfig,
    ) -> Result<HeavyLoadState, PolicyError> {
        let enter = config.heavy_enter;
        let exit = config.heavy_exit;
        let dwell_ms = config.heavy_dwell_ms;
        let time_constant_ms = config.ema_time_constant_ms;
        validate_non_negative_finite("sample", sample)?;
        validate_non_negative_finite("enter", enter)?;
        validate_non_negative_finite("exit", exit)?;
        let ema = match self.ema {
            None => sample,
            Some(previous) => time_weighted_ema(previous, sample, elapsed_ms, time_constant_ms)?,
        };
        self.ema = Some(ema);

        match self.state {
            HeavyLoadState::Idle if ema >= enter => {
                self.state = HeavyLoadState::Heavy;
                self.below_exit_since = None;
            }
            HeavyLoadState::Heavy if ema <= exit => {
                let below_since = *self.below_exit_since.get_or_insert(now);
                if now.saturating_duration_since(below_since) >= dwell_ms {
                    self.state = HeavyLoadState::Idle;
                    self.below_exit_since = None;
                }
            }
            HeavyLoadState::Heavy => self.below_exit_since = None,
            HeavyLoadState::Idle => {}
        }
        Ok(self.state)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ThermalState {
    Normal,
    Warning,
    Throttled,
    Critical,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThermalThresholds {
    pub warning: MilliCelsius,
    pub throttled: MilliCelsius,
    pub critical: MilliCelsius,
    pub hysteresis: MilliCelsius,
    pub dwell_ms: u64,
    pub stale_after_ms: u64,
}

impl From<&ThermalZoneConfig> for ThermalThresholds {
    fn from(value: &ThermalZoneConfig) -> Self {
        Self {
            warning: value.warning,
            throttled: value.throttled,
            critical: value.critical,
            hysteresis: value.hysteresis,
            dwell_ms: value.dwell_ms,
            stale_after_ms: value.stale_after_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThermalCaps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<Hertz>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttled: Option<Hertz>,
    pub critical: Hertz,
    pub sensor_failure: Hertz,
}

impl ThermalCaps {
    #[must_use]
    pub const fn for_state(self, state: ThermalState) -> Option<Hertz> {
        match state {
            ThermalState::Normal => None,
            ThermalState::Warning => self.warning,
            ThermalState::Throttled => match self.throttled {
                Some(cap) => Some(cap),
                None => self.warning,
            },
            ThermalState::Critical => Some(self.critical),
            ThermalState::Degraded => Some(self.sensor_failure),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThermalGuard {
    thresholds: ThermalThresholds,
    state: ThermalState,
    pending: Option<(ThermalState, MonotonicMillis)>,
}

impl ThermalGuard {
    #[must_use]
    pub const fn new(thresholds: ThermalThresholds) -> Self {
        Self {
            thresholds,
            state: ThermalState::Normal,
            pending: None,
        }
    }

    #[must_use]
    pub const fn state(&self) -> ThermalState {
        self.state
    }

    pub fn update(&mut self, now: MonotonicMillis, reading: &ThermalReading) -> ThermalState {
        let from_future = reading.sampled_at > now;
        let stale =
            now.saturating_duration_since(reading.sampled_at) > self.thresholds.stale_after_ms;
        if reading.health != crate::SensorHealth::Healthy
            || from_future
            || stale
            || reading.temperature.is_none()
        {
            self.state = ThermalState::Degraded;
            self.pending = None;
            return self.state;
        }

        let Some(temperature) = reading.temperature else {
            self.state = ThermalState::Degraded;
            self.pending = None;
            return self.state;
        };
        let candidate = self.candidate_state(temperature);
        // Match the C implementation's useful safety property: never debounce
        // entry into the emergency state.
        if candidate == ThermalState::Critical {
            self.state = candidate;
            self.pending = None;
            return self.state;
        }
        if candidate == self.state {
            self.pending = None;
            return self.state;
        }

        match self.pending {
            Some((pending, since)) if pending == candidate => {
                if now.saturating_duration_since(since) >= self.thresholds.dwell_ms {
                    self.state = candidate;
                    self.pending = None;
                }
            }
            _ if self.thresholds.dwell_ms == 0 => {
                self.state = candidate;
                self.pending = None;
            }
            _ => self.pending = Some((candidate, now)),
        }
        self.state
    }

    fn candidate_state(&self, temperature: MilliCelsius) -> ThermalState {
        let raw = classify_temperature(temperature, self.thresholds);
        if raw >= self.state || self.state == ThermalState::Degraded {
            return raw;
        }

        let recovery_threshold = match self.state {
            ThermalState::Critical => self.thresholds.critical,
            ThermalState::Throttled => self.thresholds.throttled,
            ThermalState::Warning => self.thresholds.warning,
            ThermalState::Normal | ThermalState::Degraded => return raw,
        };
        if temperature.get() < recovery_threshold.get() - self.thresholds.hysteresis.get() {
            raw
        } else {
            self.state
        }
    }
}

#[must_use]
pub const fn classify_temperature(
    temperature: MilliCelsius,
    thresholds: ThermalThresholds,
) -> ThermalState {
    if temperature.0 >= thresholds.critical.0 {
        ThermalState::Critical
    } else if temperature.0 >= thresholds.throttled.0 {
        ThermalState::Throttled
    } else if temperature.0 >= thresholds.warning.0 {
        ThermalState::Warning
    } else {
        ThermalState::Normal
    }
}

#[must_use]
pub fn worst_thermal_state(states: impl IntoIterator<Item = ThermalState>) -> ThermalState {
    states
        .into_iter()
        .max_by_key(|state| match state {
            ThermalState::Normal => 0,
            ThermalState::Warning => 1,
            ThermalState::Throttled => 2,
            ThermalState::Critical => 3,
            ThermalState::Degraded => 4,
        })
        .unwrap_or(ThermalState::Degraded)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuTargetPolicy {
    pub cpus: CpuSet,
    pub frequency: FrequencyPolicy,
}

pub struct PolicyInput<'a> {
    pub generation: u64,
    pub observed: &'a ObservedState,
    pub mode: ModeSelection,
    pub app_profile: Option<ProfileId>,
    pub hints: &'a HintSet,
    pub cpu_targets: &'a BTreeMap<TargetId, CpuTargetPolicy>,
    /// Additional manual-only targets, such as v1 GPU/devfreq support.
    pub manual_target_policies: &'a BTreeMap<TargetId, FrequencyPolicy>,
    pub manual_overrides: &'a BTreeMap<TargetId, FrequencyLimits>,
    pub administrator_caps: &'a BTreeMap<TargetId, Hertz>,
    pub thermal_caps: &'a BTreeMap<TargetId, Hertz>,
    pub thermal_degraded: bool,
}

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    config: PolicyConfig,
    scheduler_rules: Vec<CompiledSchedulerRule>,
}

#[derive(Debug, Clone)]
struct CompiledWorkloadMatcher {
    executable: Option<String>,
    desktop_id: Option<String>,
    comm_regex: Option<Regex>,
}

impl CompiledWorkloadMatcher {
    fn new(matcher: &WorkloadMatcher) -> Result<Self, PolicyError> {
        let comm_regex = matcher
            .comm_regex
            .as_deref()
            .map(Regex::new)
            .transpose()
            .map_err(|error| PolicyError::InvalidMatcherRegex(error.to_string()))?;
        Ok(Self {
            executable: matcher.executable.clone(),
            desktop_id: matcher.desktop_id.clone(),
            comm_regex,
        })
    }

    fn is_match(&self, process: &ProcessInfo) -> bool {
        if let Some(executable) = &self.executable
            && process.executable.as_deref() != Some(executable.as_str())
        {
            return false;
        }
        if let Some(desktop_id) = &self.desktop_id
            && process.desktop_id.as_deref() != Some(desktop_id.as_str())
        {
            return false;
        }
        self.comm_regex
            .as_ref()
            .is_none_or(|pattern| pattern.is_match(&process.comm))
    }
}

#[derive(Debug, Clone)]
struct CompiledSchedulerRule {
    matcher: CompiledWorkloadMatcher,
    thread_patterns: Vec<Regex>,
}

impl CompiledSchedulerRule {
    fn new(rule: &crate::ProcessRuleConfig) -> Result<Self, PolicyError> {
        let matcher = CompiledWorkloadMatcher::new(&rule.matcher)?;
        let thread_patterns = rule
            .threads
            .iter()
            .map(|thread| {
                Regex::new(&thread.comm_regex)
                    .map_err(|error| PolicyError::InvalidMatcherRegex(error.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            matcher,
            thread_patterns,
        })
    }
}

#[derive(Debug, Clone)]
struct CompiledAppRule {
    matcher: CompiledWorkloadMatcher,
    profile: ProfileId,
}

/// Deterministic, precompiled first-match evaluator for application rules.
///
/// Sorting and regular-expression compilation happen once when a configuration
/// candidate is built, never on the load, thermal, or frequency hot paths.
#[derive(Debug, Clone)]
pub struct AppRuleEngine {
    rules: Vec<CompiledAppRule>,
}

impl AppRuleEngine {
    /// Validate and compile an application-rule document.
    ///
    /// # Errors
    ///
    /// Returns a policy error when the document or one of its regular
    /// expressions is invalid.
    pub fn new(config: &AppsConfig) -> Result<Self, PolicyError> {
        config.validate()?;
        let rules = config
            .ordered_enabled_rules()
            .into_iter()
            .map(|rule| {
                Ok(CompiledAppRule {
                    matcher: CompiledWorkloadMatcher::new(&rule.matcher)?,
                    profile: rule.profile,
                })
            })
            .collect::<Result<_, PolicyError>>()?;
        Ok(Self { rules })
    }

    /// Return the profile selected by the first matching rule.
    #[must_use]
    pub fn match_profile(&self, process: &ProcessInfo) -> Option<ProfileId> {
        self.rules
            .iter()
            .find(|rule| rule.matcher.is_match(process))
            .map(|rule| rule.profile)
    }
}

/// Pure result of ordered process and thread rule evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SchedulerDecision {
    /// Task plans keyed by PID/TID identity. Unmatched threads are omitted and
    /// therefore retain their original state.
    pub tasks: BTreeMap<ProcessIdentity, TaskPlan>,
    /// Logical cgroup class selected by the first matching process rule.
    pub cgroup_class: Option<String>,
    /// Diagnostic name of the first matching process rule.
    pub matched_rule: Option<String>,
}

impl PolicyEngine {
    /// Build an engine from a fully validated policy configuration.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::InvalidConfiguration`] if semantic validation
    /// fails.
    pub fn new(config: PolicyConfig) -> Result<Self, PolicyError> {
        config.validate()?;
        let scheduler_rules = config
            .scheduler
            .process_rules
            .iter()
            .map(CompiledSchedulerRule::new)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            config,
            scheduler_rules,
        })
    }

    #[must_use]
    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    /// Evaluate one immutable observation snapshot into a desired plan.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError`] if a referenced profile or target model is
    /// invalid. No external state is mutated.
    #[allow(
        clippy::too_many_lines,
        reason = "the two target classes share one ordered safety-precedence evaluation"
    )]
    pub fn evaluate(&self, input: &PolicyInput<'_>) -> Result<DesiredPlan, PolicyError> {
        let effective_profile =
            resolve_effective_profile(input.mode, input.app_profile, self.config.default_profile);
        let profile = self
            .config
            .profile(effective_profile)
            .ok_or(PolicyError::MissingProfile(effective_profile))?;
        let dominant_scene = if input.thermal_degraded {
            input
                .hints
                .dominant_at_excluding(input.observed.timestamp, &BTreeSet::from([Scene::Boost]))
        } else {
            input.hints.dominant_at(input.observed.timestamp)
        };
        let parameters = effective_parameters(profile, dominant_scene);
        let mut frequencies = BTreeMap::new();

        for (id, target) in input.cpu_targets {
            let thermal_cap = required_thermal_cap(input, id)?;
            let demand = max_cpu_demand(&target.cpus, &input.observed.cpu_loads);
            let automatic = LoadGovernor::compute(
                &target.frequency,
                LoadGovernorInput {
                    demand,
                    burst: parameters.burst,
                    margin: parameters.margin,
                    limit_efficiency: parameters.limit_efficiency,
                    administrator_cap: input.administrator_caps.get(id).copied(),
                    thermal_cap,
                },
            )?;
            let (request, source) = input
                .manual_overrides
                .get(id)
                .map_or((automatic, PlanSource::Automatic), |manual| {
                    (*manual, PlanSource::ManualOverride)
                });
            let limits = constrain_frequency_limits(
                request,
                target.frequency.hardware_limits,
                input.administrator_caps.get(id).copied(),
                thermal_cap,
                &target.frequency.available_frequencies,
                target.frequency.hertz_per_unit,
            )?;
            frequencies.insert(
                id.clone(),
                TargetFrequencyPlan {
                    limits,
                    source: if input.thermal_degraded {
                        PlanSource::ThermalDegraded
                    } else {
                        source
                    },
                },
            );
        }

        for (id, target) in input.manual_target_policies {
            if frequencies.contains_key(id) {
                continue;
            }
            let thermal_cap = required_thermal_cap(input, id)?;
            let administrator_cap = input.administrator_caps.get(id).copied();
            let manual = input.manual_overrides.get(id).copied();
            // A manual-only target is normally left untouched.  Once an
            // administrator or thermal envelope is active it becomes a safety
            // target even without a client override, using readback (or the
            // hardware window when telemetry is unavailable) as its baseline.
            if manual.is_none() && thermal_cap.is_none() && administrator_cap.is_none() {
                continue;
            }
            target.validate()?;
            let requested = manual.unwrap_or_else(|| {
                input
                    .observed
                    .frequencies
                    .get(id)
                    .map_or(target.hardware_limits, |observed| observed.limits)
            });
            let limits = constrain_frequency_limits(
                requested,
                target.hardware_limits,
                administrator_cap,
                thermal_cap,
                &target.available_frequencies,
                target.hertz_per_unit,
            )?;
            frequencies.insert(
                id.clone(),
                TargetFrequencyPlan {
                    limits,
                    source: if input.thermal_degraded {
                        PlanSource::ThermalDegraded
                    } else if manual.is_some() {
                        PlanSource::ManualOverride
                    } else {
                        PlanSource::Automatic
                    },
                },
            );
        }

        Ok(DesiredPlan {
            generation: input.generation,
            effective_profile,
            dominant_scene,
            frequencies,
            tasks: BTreeMap::new(),
        })
    }

    /// Evaluate ordered scheduler rules for one explicitly registered workload.
    ///
    /// The first matching process rule wins. Its process task profile applies
    /// to the process leader, then the first matching thread rule may override
    /// the leader or select individual worker threads. Threads without a match
    /// are deliberately omitted and keep their original state.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::MissingTaskProfile`] if a supposedly validated
    /// configuration contains a broken task-profile reference.
    pub fn evaluate_scheduler(
        &self,
        workload: &ProcessInfo,
        threads: &[ProcessInfo],
    ) -> Result<SchedulerDecision, PolicyError> {
        let scheduler = &self.config.scheduler;
        if !scheduler.enabled {
            return Ok(SchedulerDecision::default());
        }
        let Some((rule, compiled_rule)) = scheduler
            .process_rules
            .iter()
            .zip(&self.scheduler_rules)
            .find(|(_, compiled)| compiled.matcher.is_match(workload))
        else {
            return Ok(SchedulerDecision::default());
        };

        let mut decision = SchedulerDecision {
            tasks: BTreeMap::new(),
            cgroup_class: rule.cgroup_class.clone(),
            matched_rule: Some(rule.name.clone()),
        };
        if let Some(profile_id) = &rule.task_profile {
            let profile = scheduler
                .task_profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
                .ok_or_else(|| PolicyError::MissingTaskProfile(profile_id.clone()))?;
            decision
                .tasks
                .insert(workload.identity, profile.plan.clone());
        }

        for thread in threads {
            if thread.identity.uid != workload.identity.uid {
                continue;
            }
            let Some((thread_rule, _)) = rule
                .threads
                .iter()
                .zip(&compiled_rule.thread_patterns)
                .find(|(_, pattern)| pattern.is_match(&thread.comm))
            else {
                continue;
            };
            let profile = scheduler
                .task_profiles
                .iter()
                .find(|profile| profile.id == thread_rule.task_profile)
                .ok_or_else(|| PolicyError::MissingTaskProfile(thread_rule.task_profile.clone()))?;
            decision.tasks.insert(thread.identity, profile.plan.clone());
        }
        Ok(decision)
    }

    /// True when no verified target state differs from the desired plan.
    #[must_use]
    pub fn is_reconciled(desired: &DesiredPlan, applied: &AppliedState) -> bool {
        desired.frequencies.len() == applied.frequencies.len()
            && desired.frequencies.iter().all(|(id, target)| {
                applied
                    .frequencies
                    .get(id)
                    .is_some_and(|actual| actual.limits == target.limits)
            })
            && desired.tasks == applied.tasks
    }
}

#[derive(Debug, Clone, Copy)]
struct EffectiveParameters {
    margin: f64,
    burst: f64,
    limit_efficiency: bool,
}

fn effective_parameters(profile: &ProfileConfig, scene: Scene) -> EffectiveParameters {
    let patch = profile.scenes.get(&scene);
    EffectiveParameters {
        margin: patch
            .and_then(|value| value.margin)
            .unwrap_or(profile.margin),
        burst: patch.and_then(|value| value.burst).unwrap_or(profile.burst),
        limit_efficiency: patch
            .and_then(|value| value.limit_efficiency)
            .unwrap_or(profile.limit_efficiency),
    }
}

fn max_cpu_demand(cpus: &CpuSet, loads: &BTreeMap<CpuId, f64>) -> f64 {
    cpus.iter()
        .filter_map(|cpu| loads.get(cpu).copied())
        .filter(|load| load.is_finite())
        .map(|load| load.clamp(0.0, 1.0))
        .fold(0.0, f64::max)
}

fn required_thermal_cap(
    input: &PolicyInput<'_>,
    id: &TargetId,
) -> Result<Option<Hertz>, PolicyError> {
    let cap = input.thermal_caps.get(id).copied();
    if input.thermal_degraded && cap.is_none() {
        Err(PolicyError::MissingThermalCap(id.clone()))
    } else {
        Ok(cap)
    }
}

fn validate_non_negative_finite(field: &'static str, value: f64) -> Result<(), PolicyError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(PolicyError::InvalidLoadInput { field })
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn float_to_u64_ceil(value: f64) -> u64 {
    if value >= u64::MAX as f64 {
        u64::MAX
    } else {
        value.ceil() as u64
    }
}

#[allow(clippy::cast_precision_loss)]
fn hertz_as_f64(value: Hertz) -> f64 {
    // Linux cpufreq/devfreq values are several orders of magnitude below 2^53,
    // so conversion is exact for every supported target.
    value.get() as f64
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        AppRule, AppliedTargetState, CONFIG_SCHEMA_VERSION, CpuId, InputConfig, LoadConfig,
        ObservedFrequency, ProcessId, ProcessIdentity, ProcessRuleConfig, ScenePatch,
        SchedulerConfig, SchedulingClass, SensorHealth, TaskPlan, TaskProfileConfig,
        ThermalPolicyConfig, ThreadRuleConfig, UserId, WorkloadMatcher,
    };

    fn target(value: &str) -> TargetId {
        TargetId::new(value).expect("target id")
    }

    fn frequency_policy() -> FrequencyPolicy {
        FrequencyPolicy {
            hardware_limits: FrequencyLimits {
                min: Hertz(300),
                max: Hertz(3_000),
            },
            floor: Hertz(500),
            reference: Hertz(2_000),
            efficient_cap: Hertz(2_400),
            hertz_per_unit: 1,
            available_frequencies: vec![
                Hertz(3_000),
                Hertz(500),
                Hertz(1_000),
                Hertz(1_500),
                Hertz(2_000),
                Hertz(2_400),
                Hertz(1_500),
            ],
        }
    }

    fn policy_config() -> PolicyConfig {
        PolicyConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            default_profile: ProfileId::Balance,
            profiles: vec![
                ProfileConfig {
                    id: ProfileId::Powersave,
                    margin: 0.1,
                    burst: 0.0,
                    limit_efficiency: true,
                    scenes: BTreeMap::new(),
                },
                ProfileConfig {
                    id: ProfileId::Balance,
                    margin: 0.2,
                    burst: 0.0,
                    limit_efficiency: false,
                    scenes: BTreeMap::from([(
                        Scene::Boost,
                        ScenePatch {
                            margin: Some(0.5),
                            burst: Some(0.2),
                            limit_efficiency: None,
                        },
                    )]),
                },
                ProfileConfig {
                    id: ProfileId::Performance,
                    margin: 0.4,
                    burst: 0.2,
                    limit_efficiency: false,
                    scenes: BTreeMap::new(),
                },
            ],
            load: LoadConfig::default(),
            thermal: ThermalPolicyConfig::default(),
            input: InputConfig::default(),
            scheduler: SchedulerConfig::default(),
        }
    }

    #[test]
    fn mode_precedence_is_forced_then_app_then_default() {
        assert_eq!(
            resolve_effective_profile(
                ModeSelection::Forced(ProfileId::Powersave),
                Some(ProfileId::Performance),
                ProfileId::Balance,
            ),
            ProfileId::Powersave
        );
        assert_eq!(
            resolve_effective_profile(
                ModeSelection::Auto,
                Some(ProfileId::Performance),
                ProfileId::Balance,
            ),
            ProfileId::Performance
        );
        assert_eq!(
            resolve_effective_profile(ModeSelection::Auto, None, ProfileId::Balance),
            ProfileId::Balance
        );
    }

    #[test]
    fn app_rule_engine_preserves_priority_and_compiled_regex_matching() {
        let engine = AppRuleEngine::new(&AppsConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            rules: vec![
                AppRule {
                    id: "fallback".to_owned(),
                    enabled: true,
                    priority: 0,
                    matcher: WorkloadMatcher {
                        executable: None,
                        desktop_id: None,
                        comm_regex: Some("^game$".to_owned()),
                    },
                    profile: ProfileId::Balance,
                },
                AppRule {
                    id: "preferred".to_owned(),
                    enabled: true,
                    priority: 10,
                    matcher: WorkloadMatcher {
                        executable: Some("/usr/bin/game".to_owned()),
                        desktop_id: None,
                        comm_regex: Some("^game$".to_owned()),
                    },
                    profile: ProfileId::Performance,
                },
            ],
        })
        .expect("valid application rules");

        assert_eq!(
            engine.match_profile(&process(10, 100, "game", Some("/usr/bin/game"))),
            Some(ProfileId::Performance)
        );
        assert_eq!(
            engine.match_profile(&process(11, 101, "game", Some("/opt/game"))),
            Some(ProfileId::Balance)
        );
    }

    #[test]
    fn scheduler_uses_first_process_and_first_thread_match() {
        let mut config = policy_config();
        config.scheduler = SchedulerConfig {
            enabled: true,
            task_profiles: vec![
                TaskProfileConfig {
                    id: "leader".to_owned(),
                    plan: TaskPlan {
                        nice: Some(-2),
                        scheduling_class: Some(SchedulingClass::Other),
                        ..TaskPlan::default()
                    },
                },
                TaskProfileConfig {
                    id: "render".to_owned(),
                    plan: TaskPlan {
                        nice: Some(-5),
                        ..TaskPlan::default()
                    },
                },
                TaskProfileConfig {
                    id: "later".to_owned(),
                    plan: TaskPlan {
                        nice: Some(10),
                        ..TaskPlan::default()
                    },
                },
            ],
            process_rules: vec![
                ProcessRuleConfig {
                    name: "first".to_owned(),
                    matcher: WorkloadMatcher {
                        executable: Some("/usr/bin/game".to_owned()),
                        desktop_id: None,
                        comm_regex: None,
                    },
                    task_profile: Some("leader".to_owned()),
                    cgroup_class: Some("foreground".to_owned()),
                    threads: vec![
                        ThreadRuleConfig {
                            comm_regex: "^Render".to_owned(),
                            task_profile: "render".to_owned(),
                        },
                        ThreadRuleConfig {
                            comm_regex: "Worker$".to_owned(),
                            task_profile: "later".to_owned(),
                        },
                    ],
                },
                ProcessRuleConfig {
                    name: "shadowed".to_owned(),
                    matcher: WorkloadMatcher {
                        executable: None,
                        desktop_id: None,
                        comm_regex: Some("game".to_owned()),
                    },
                    task_profile: Some("later".to_owned()),
                    cgroup_class: None,
                    threads: Vec::new(),
                },
            ],
            cgroup_classes: vec![crate::CgroupClassConfig {
                id: "foreground".to_owned(),
                allowed_cpus: CpuSet::from(vec![CpuId(0)]),
                cpu_weight: 500,
            }],
        };
        let engine = PolicyEngine::new(config).expect("valid policy");
        let workload = process(10, 100, "game", Some("/usr/bin/game"));
        let render = process(11, 101, "RenderWorker", None);
        let unmatched = process(12, 102, "Audio", None);

        let decision = engine
            .evaluate_scheduler(&workload, &[render.clone(), unmatched])
            .expect("scheduler decision");

        assert_eq!(decision.matched_rule.as_deref(), Some("first"));
        assert_eq!(decision.cgroup_class.as_deref(), Some("foreground"));
        assert_eq!(decision.tasks[&workload.identity].nice, Some(-2));
        assert_eq!(decision.tasks[&render.identity].nice, Some(-5));
        assert_eq!(decision.tasks.len(), 2);
    }

    fn process(
        pid: u32,
        start_time_ticks: u64,
        comm: &str,
        executable: Option<&str>,
    ) -> ProcessInfo {
        ProcessInfo {
            identity: ProcessIdentity {
                pid: ProcessId(pid),
                start_time_ticks,
                uid: UserId(1_000),
            },
            owner_control_safe: true,
            comm: comm.to_owned(),
            executable: executable.map(str::to_owned),
            desktop_id: None,
        }
    }

    #[test]
    fn hint_priority_and_ttl_are_deterministic() {
        let mut hints = HintSet::new();
        hints.activate(Hint::persistent(Scene::Touch, MonotonicMillis(10)));
        hints.activate(Hint::with_ttl(Scene::Gesture, MonotonicMillis(20), 100));
        hints.activate(Hint::with_ttl(Scene::Wake, MonotonicMillis(30), 10));

        assert_eq!(hints.dominant_at(MonotonicMillis(35)), Scene::Wake);
        assert_eq!(hints.dominant_at(MonotonicMillis(40)), Scene::Gesture);
        assert_eq!(hints.dominant_at(MonotonicMillis(120)), Scene::Touch);
    }

    #[test]
    fn governor_snaps_min_up_max_down_and_respects_thermal() {
        let result = LoadGovernor::compute(
            &frequency_policy(),
            LoadGovernorInput {
                demand: 0.75,
                burst: 0.0,
                margin: 0.2,
                limit_efficiency: false,
                administrator_cap: None,
                thermal_cap: Some(Hertz(2_100)),
            },
        )
        .expect("governor");
        assert_eq!(
            result,
            FrequencyLimits {
                min: Hertz(2_000),
                max: Hertz(2_000)
            }
        );
    }

    #[test]
    fn cap_below_requested_min_never_inverts_pair() {
        let result = constrain_frequency_limits(
            FrequencyLimits {
                min: Hertz(2_400),
                max: Hertz(3_000),
            },
            frequency_policy().hardware_limits,
            None,
            Some(Hertz(1_200)),
            &frequency_policy().available_frequencies,
            frequency_policy().hertz_per_unit,
        )
        .expect("valid limits");
        assert_eq!(result.min, Hertz(1_000));
        assert_eq!(result.max, Hertz(1_000));
        assert!(result.is_valid());
    }

    #[test]
    fn safety_cap_below_lowest_reported_opp_fails_closed() {
        let error = constrain_frequency_limits(
            FrequencyLimits {
                min: Hertz(500),
                max: Hertz(3_000),
            },
            frequency_policy().hardware_limits,
            None,
            Some(Hertz(400)),
            &frequency_policy().available_frequencies,
            frequency_policy().hertz_per_unit,
        )
        .expect_err("no safe OPP");
        assert_eq!(error, PolicyError::NoOppAtOrBelowCap(Hertz(400)));
    }

    #[test]
    fn continuous_range_is_quantized_inward_without_jumping_to_hardware_max() {
        let limits = constrain_frequency_limits(
            FrequencyLimits {
                min: Hertz(1_234),
                max: Hertz(2_876),
            },
            FrequencyLimits {
                min: Hertz(300),
                max: Hertz(3_000),
            },
            None,
            None,
            &[],
            100,
        )
        .expect("continuous kernel range");
        assert_eq!(
            limits,
            FrequencyLimits {
                min: Hertz(1_300),
                max: Hertz(2_800),
            }
        );
    }

    #[test]
    fn ema_is_independent_of_sample_period() {
        let one_step = time_weighted_ema(0.0, 1.0, 100, 1_000).expect("ema");
        let first = time_weighted_ema(0.0, 1.0, 50, 1_000).expect("ema");
        let two_steps = time_weighted_ema(first, 1.0, 50, 1_000).expect("ema");
        assert!((one_step - two_steps).abs() < 1e-12);
    }

    #[test]
    fn heavy_load_exit_requires_dwell() {
        let mut detector = HeavyLoadDetector::default();
        let config = LoadConfig {
            heavy_enter: 0.6,
            heavy_exit: 0.2,
            heavy_dwell_ms: 100,
            ema_time_constant_ms: 1,
            ..LoadConfig::default()
        };
        assert_eq!(
            detector
                .update(1.0, 10, MonotonicMillis(0), &config)
                .expect("update"),
            HeavyLoadState::Heavy
        );
        assert_eq!(
            detector
                .update(0.0, 10, MonotonicMillis(10), &config)
                .expect("update"),
            HeavyLoadState::Heavy
        );
        assert_eq!(
            detector
                .update(0.0, 100, MonotonicMillis(110), &config)
                .expect("update"),
            HeavyLoadState::Idle
        );
    }

    fn thresholds() -> ThermalThresholds {
        ThermalThresholds {
            warning: MilliCelsius(70_000),
            throttled: MilliCelsius(80_000),
            critical: MilliCelsius(95_000),
            hysteresis: MilliCelsius(5_000),
            dwell_ms: 100,
            stale_after_ms: 500,
        }
    }

    fn reading(temp: i64, at: u64) -> ThermalReading {
        ThermalReading {
            temperature: Some(MilliCelsius(temp)),
            sampled_at: MonotonicMillis(at),
            health: SensorHealth::Healthy,
        }
    }

    #[test]
    fn thermal_guard_uses_dwell_hysteresis_and_staleness() {
        let mut guard = ThermalGuard::new(thresholds());
        assert_eq!(
            guard.update(MonotonicMillis(0), &reading(81_000, 0)),
            ThermalState::Normal
        );
        assert_eq!(
            guard.update(MonotonicMillis(100), &reading(81_000, 100)),
            ThermalState::Throttled
        );
        // Above throttled recovery threshold (75 C), so hysteresis holds.
        assert_eq!(
            guard.update(MonotonicMillis(200), &reading(76_000, 200)),
            ThermalState::Throttled
        );
        assert_eq!(
            guard.update(MonotonicMillis(300), &reading(60_000, 300)),
            ThermalState::Throttled
        );
        assert_eq!(
            guard.update(MonotonicMillis(400), &reading(60_000, 400)),
            ThermalState::Normal
        );
        assert_eq!(
            guard.update(MonotonicMillis(1_000), &reading(60_000, 400)),
            ThermalState::Degraded
        );
    }

    #[test]
    fn critical_is_immediate_and_future_readings_fail_closed() {
        let mut guard = ThermalGuard::new(thresholds());
        assert_eq!(
            guard.update(MonotonicMillis(10), &reading(96_000, 10)),
            ThermalState::Critical
        );
        assert_eq!(
            guard.update(MonotonicMillis(20), &reading(60_000, 21)),
            ThermalState::Degraded
        );
    }

    #[test]
    fn policy_engine_uses_max_policy_cpu_and_thermal_overrides_manual() {
        let engine = PolicyEngine::new(policy_config()).expect("engine");
        let id = target("cpu.prime");
        let observed = ObservedState {
            timestamp: MonotonicMillis(50),
            cpu_loads: BTreeMap::from([(CpuId(3), 0.1), (CpuId(7), 0.8), (CpuId(128), 1.0)]),
            frequencies: BTreeMap::from([(
                id.clone(),
                ObservedFrequency {
                    limits: FrequencyLimits {
                        min: Hertz(300),
                        max: Hertz(3_000),
                    },
                    current: Some(Hertz(1_500)),
                },
            )]),
            thermal: BTreeMap::new(),
            active_workload: None,
        };
        let mut hints = HintSet::new();
        hints.activate(Hint::with_ttl(Scene::Boost, MonotonicMillis(0), 100));
        let cpu_targets = BTreeMap::from([(
            id.clone(),
            CpuTargetPolicy {
                cpus: CpuSet::from_ids([CpuId(3), CpuId(7)]),
                frequency: frequency_policy(),
            },
        )]);
        let plan = engine
            .evaluate(&PolicyInput {
                generation: 7,
                observed: &observed,
                mode: ModeSelection::Auto,
                app_profile: None,
                hints: &hints,
                cpu_targets: &cpu_targets,
                manual_target_policies: &BTreeMap::new(),
                manual_overrides: &BTreeMap::from([(
                    id.clone(),
                    FrequencyLimits {
                        min: Hertz(3_000),
                        max: Hertz(3_000),
                    },
                )]),
                administrator_caps: &BTreeMap::new(),
                thermal_caps: &BTreeMap::from([(id.clone(), Hertz(1_500))]),
                thermal_degraded: false,
            })
            .expect("evaluate");
        assert_eq!(plan.dominant_scene, Scene::Boost);
        assert_eq!(
            plan.frequencies[&id].limits,
            FrequencyLimits {
                min: Hertz(1_500),
                max: Hertz(1_500)
            }
        );
        assert_eq!(plan.frequencies[&id].source, PlanSource::ManualOverride);
    }

    #[test]
    fn degraded_thermal_suppresses_boost_and_marks_source() {
        let engine = PolicyEngine::new(policy_config()).expect("engine");
        let id = target("cpu.0");
        let observed = ObservedState {
            timestamp: MonotonicMillis(10),
            cpu_loads: BTreeMap::from([(CpuId(0), 0.1)]),
            frequencies: BTreeMap::new(),
            thermal: BTreeMap::new(),
            active_workload: None,
        };
        let mut hints = HintSet::new();
        hints.activate(Hint::persistent(Scene::Touch, MonotonicMillis(0)));
        hints.activate(Hint::persistent(Scene::Boost, MonotonicMillis(0)));
        let targets = BTreeMap::from([(
            id.clone(),
            CpuTargetPolicy {
                cpus: CpuSet::from_ids([CpuId(0)]),
                frequency: frequency_policy(),
            },
        )]);
        let plan = engine
            .evaluate(&PolicyInput {
                generation: 1,
                observed: &observed,
                mode: ModeSelection::Auto,
                app_profile: None,
                hints: &hints,
                cpu_targets: &targets,
                manual_target_policies: &BTreeMap::new(),
                manual_overrides: &BTreeMap::new(),
                administrator_caps: &BTreeMap::new(),
                thermal_caps: &BTreeMap::from([(id.clone(), Hertz(1_000))]),
                thermal_degraded: true,
            })
            .expect("evaluate");
        assert_eq!(plan.dominant_scene, Scene::Touch);
        assert_eq!(plan.frequencies[&id].source, PlanSource::ThermalDegraded);
    }

    #[test]
    fn thermal_envelope_controls_manual_only_target_without_override() {
        let engine = PolicyEngine::new(policy_config()).expect("engine");
        let id = target("gpu.0");
        let observed = ObservedState {
            timestamp: MonotonicMillis(10),
            cpu_loads: BTreeMap::new(),
            frequencies: BTreeMap::from([(
                id.clone(),
                ObservedFrequency {
                    limits: FrequencyLimits {
                        min: Hertz(1_000),
                        max: Hertz(3_000),
                    },
                    current: Some(Hertz(2_000)),
                },
            )]),
            thermal: BTreeMap::new(),
            active_workload: None,
        };
        let manual_targets = BTreeMap::from([(id.clone(), frequency_policy())]);
        let plan = engine
            .evaluate(&PolicyInput {
                generation: 1,
                observed: &observed,
                mode: ModeSelection::Auto,
                app_profile: None,
                hints: &HintSet::new(),
                cpu_targets: &BTreeMap::new(),
                manual_target_policies: &manual_targets,
                manual_overrides: &BTreeMap::new(),
                administrator_caps: &BTreeMap::new(),
                thermal_caps: &BTreeMap::from([(id.clone(), Hertz(1_500))]),
                thermal_degraded: false,
            })
            .expect("evaluate");
        assert_eq!(
            plan.frequencies[&id].limits,
            FrequencyLimits {
                min: Hertz(1_000),
                max: Hertz(1_500),
            }
        );
        assert_eq!(plan.frequencies[&id].source, PlanSource::Automatic);
    }

    #[test]
    fn degraded_thermal_without_a_failure_cap_fails_closed() {
        let engine = PolicyEngine::new(policy_config()).expect("engine");
        let id = target("cpu.0");
        let observed = ObservedState {
            timestamp: MonotonicMillis(10),
            cpu_loads: BTreeMap::new(),
            frequencies: BTreeMap::new(),
            thermal: BTreeMap::new(),
            active_workload: None,
        };
        let targets = BTreeMap::from([(
            id.clone(),
            CpuTargetPolicy {
                cpus: CpuSet::from_ids([CpuId(0)]),
                frequency: frequency_policy(),
            },
        )]);
        let error = engine
            .evaluate(&PolicyInput {
                generation: 1,
                observed: &observed,
                mode: ModeSelection::Auto,
                app_profile: None,
                hints: &HintSet::new(),
                cpu_targets: &targets,
                manual_target_policies: &BTreeMap::new(),
                manual_overrides: &BTreeMap::new(),
                administrator_caps: &BTreeMap::new(),
                thermal_caps: &BTreeMap::new(),
                thermal_degraded: true,
            })
            .expect_err("missing cap");
        assert_eq!(error, PolicyError::MissingThermalCap(id));
    }

    #[test]
    fn reconciliation_compares_verified_values_not_generation() {
        let id = target("cpu.0");
        let desired = DesiredPlan {
            generation: 5,
            effective_profile: ProfileId::Balance,
            dominant_scene: Scene::Idle,
            frequencies: BTreeMap::from([(
                id.clone(),
                TargetFrequencyPlan {
                    limits: FrequencyLimits {
                        min: Hertz(500),
                        max: Hertz(1_000),
                    },
                    source: PlanSource::Automatic,
                },
            )]),
            tasks: BTreeMap::new(),
        };
        let applied = AppliedState {
            generation: 4,
            frequencies: BTreeMap::from([(
                id,
                AppliedTargetState {
                    limits: FrequencyLimits {
                        min: Hertz(500),
                        max: Hertz(1_000),
                    },
                    generation: 4,
                    verified_at: MonotonicMillis(1),
                },
            )]),
            tasks: BTreeMap::new(),
            degraded: false,
            degraded_reason: None,
        };
        assert!(PolicyEngine::is_reconciled(&desired, &applied));
    }
}
