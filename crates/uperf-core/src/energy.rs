//! Pure energy-model and dynamic-governor primitives.
//!
//! This module contains no Linux I/O.  It intentionally accepts explicit
//! elapsed time so identical input streams are deterministic.

use std::{cmp::Ordering, collections::BTreeMap};

use thiserror::Error;

use crate::{
    CpuEnergyModelConfig, CpuTargetPolicy, FrequencyLimits, GovernorConfig, Hertz, MonotonicMillis,
    PolicyError, PowerBudgetConfig, TargetId,
};

#[derive(Debug, Clone, PartialEq)]
pub struct EnergyOpp {
    pub frequency_hz: Hertz,
    /// Relative single-core performance in arbitrary, model-local units.
    pub performance: f64,
    pub power_mw_per_core: f64,
}

impl EnergyOpp {
    #[must_use]
    pub fn cost_mw_per_performance(&self) -> f64 {
        self.power_mw_per_core / self.performance
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnergyModel {
    opps: Vec<EnergyOpp>,
    control_opps: Vec<EnergyOpp>,
    performance_per_hz: f64,
    pub free_frequency_hz: Hertz,
    pub sweet_frequency_hz: Hertz,
    pub plain_frequency_hz: Hertz,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum EnergyModelError {
    #[error("an energy model requires at least two available OPPs")]
    InsufficientOpps,
    #[error("energy-model value `{0}` must be finite and greater than zero")]
    InvalidValue(&'static str),
    #[error("energy-model frequencies must satisfy free <= plain <= sweet <= typical")]
    InvalidCurveOrdering,
    #[error("measured energy model has no point for available OPP {0}")]
    MissingMeasuredOpp(Hertz),
}

impl EnergyModel {
    /// Expand a configured model onto the OPPs exposed by the running kernel.
    ///
    /// Reference curves use their documented cubic segment above
    /// `typical_frequency_hz`; typical is a calibration point, not a maximum.
    /// Measured models require an exact point for every exposed OPP.
    ///
    /// # Errors
    ///
    /// Returns [`EnergyModelError`] when model data is invalid or cannot
    /// describe all available OPPs.
    pub fn from_config(
        config: &CpuEnergyModelConfig,
        available_opps: &[Hertz],
    ) -> Result<Self, EnergyModelError> {
        let mut frequencies = available_opps.to_vec();
        frequencies.sort_unstable();
        frequencies.dedup();
        if frequencies.len() < 2 {
            return Err(EnergyModelError::InsufficientOpps);
        }

        match config {
            CpuEnergyModelConfig::ReferenceCurveV1 { .. } => {
                expand_reference_curve(config, frequencies)
            }
            CpuEnergyModelConfig::MeasuredOppV1 { .. } => expand_measured_opps(config, frequencies),
        }
    }

    #[must_use]
    pub fn opps(&self) -> &[EnergyOpp] {
        &self.opps
    }

    #[must_use]
    pub fn opp_at_or_below(&self, cap: Hertz) -> Option<&EnergyOpp> {
        self.opps.iter().rev().find(|opp| opp.frequency_hz <= cap)
    }

    #[must_use]
    pub fn first_opp_meeting_performance(&self, performance: f64) -> Option<&EnergyOpp> {
        self.control_opps
            .iter()
            .find(|opp| opp.performance >= performance)
            .or_else(|| self.control_opps.last())
    }

    /// Sparse OPP set used by Uperf v3 for demand and package-cap control.
    #[must_use]
    pub fn control_opps(&self) -> &[EnergyOpp] {
        &self.control_opps
    }
}

fn expand_reference_curve(
    config: &CpuEnergyModelConfig,
    frequencies: Vec<Hertz>,
) -> Result<EnergyModel, EnergyModelError> {
    let CpuEnergyModelConfig::ReferenceCurveV1 {
        relative_performance,
        typical_power_mw_per_core,
        typical_frequency_hz,
        sweet_frequency_hz,
        plain_frequency_hz,
        free_frequency_hz,
    } = config
    else {
        unreachable!("called only for a reference curve");
    };
    if *relative_performance == 0 {
        return Err(EnergyModelError::InvalidValue("relative_performance"));
    }
    if *typical_power_mw_per_core == 0 {
        return Err(EnergyModelError::InvalidValue("typical_power_mw_per_core"));
    }
    if !(*free_frequency_hz <= *plain_frequency_hz
        && *plain_frequency_hz <= *sweet_frequency_hz
        && *sweet_frequency_hz <= *typical_frequency_hz)
    {
        return Err(EnergyModelError::InvalidCurveOrdering);
    }
    let typical_frequency = hertz_as_f64(*typical_frequency_hz);
    let a3 = f64::from(*typical_power_mw_per_core) / typical_frequency.powi(3);
    let a2 = a3 * hertz_as_f64(*sweet_frequency_hz);
    let a1 = a2 * hertz_as_f64(*plain_frequency_hz);
    let performance_scale = f64::from(*relative_performance) / 100.0;
    let curve_plain_frequency_hz = *plain_frequency_hz;
    let curve_sweet_frequency_hz = *sweet_frequency_hz;
    let free_frequency_hz = snap_frequency_nearest(&frequencies, *free_frequency_hz);
    let plain_frequency_hz = snap_frequency_nearest(&frequencies, *plain_frequency_hz);
    let sweet_frequency_hz = snap_frequency_nearest(&frequencies, *sweet_frequency_hz);
    let opps = frequencies
        .into_iter()
        .map(|frequency_hz| {
            let frequency = hertz_as_f64(frequency_hz);
            let power_mw_per_core = if frequency_hz <= curve_plain_frequency_hz {
                a1 * frequency
            } else if frequency_hz <= curve_sweet_frequency_hz {
                a2 * frequency.powi(2)
            } else {
                a3 * frequency.powi(3)
            };
            EnergyOpp {
                frequency_hz,
                performance: frequency * performance_scale,
                power_mw_per_core,
            }
        })
        .collect::<Vec<_>>();
    let control_opps = sparse_reference_control_opps(
        &opps,
        free_frequency_hz,
        plain_frequency_hz,
        sweet_frequency_hz,
    );
    Ok(EnergyModel {
        opps,
        control_opps,
        performance_per_hz: performance_scale,
        free_frequency_hz,
        sweet_frequency_hz,
        plain_frequency_hz,
    })
}

fn snap_frequency_nearest(opps: &[Hertz], requested: Hertz) -> Hertz {
    opps.iter()
        .copied()
        .min_by_key(|frequency| {
            frequency
                .get()
                .abs_diff(requested.get())
                .saturating_mul(2)
                .saturating_add(u64::from(*frequency > requested))
        })
        .unwrap_or(requested)
}

fn sparse_reference_control_opps(
    opps: &[EnergyOpp],
    free: Hertz,
    plain: Hertz,
    sweet: Hertz,
) -> Vec<EnergyOpp> {
    let maximum = opps.last().map_or(sweet, |opp| opp.frequency_hz);
    let mut requested = vec![free, plain];
    requested.extend((1..=3).map(|part| interpolate_hertz(plain, sweet, part, 3)));
    requested.extend((1..=4).map(|part| interpolate_hertz(sweet, maximum, part, 4)));

    let available = opps.iter().map(|opp| opp.frequency_hz).collect::<Vec<_>>();
    let mut selected = Vec::with_capacity(requested.len());
    for target in requested {
        let snapped = snap_frequency_nearest(&available, target);
        if selected
            .last()
            .is_none_or(|previous: &EnergyOpp| previous.frequency_hz < snapped)
            && let Some(opp) = opps.iter().find(|opp| opp.frequency_hz == snapped)
        {
            selected.push(opp.clone());
        }
    }
    selected
}

fn interpolate_hertz(lower: Hertz, upper: Hertz, part: u64, total: u64) -> Hertz {
    Hertz(lower.get() + upper.get().saturating_sub(lower.get()) * part / total)
}

fn expand_measured_opps(
    config: &CpuEnergyModelConfig,
    frequencies: Vec<Hertz>,
) -> Result<EnergyModel, EnergyModelError> {
    let CpuEnergyModelConfig::MeasuredOppV1 { points } = config else {
        unreachable!("called only for measured OPPs");
    };
    let mut points = points.clone();
    points.sort_unstable_by_key(|point| point.frequency_hz);
    let opps = frequencies
        .into_iter()
        .map(|frequency_hz| {
            let point = points
                .binary_search_by_key(&frequency_hz, |point| point.frequency_hz)
                .ok()
                .map(|index| &points[index])
                .ok_or(EnergyModelError::MissingMeasuredOpp(frequency_hz))?;
            if point.relative_capacity == 0 {
                return Err(EnergyModelError::InvalidValue("relative_capacity"));
            }
            if point.power_mw_per_core == 0 {
                return Err(EnergyModelError::InvalidValue("power_mw_per_core"));
            }
            Ok(EnergyOpp {
                frequency_hz,
                performance: f64::from(point.relative_capacity),
                power_mw_per_core: f64::from(point.power_mw_per_core),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let free_frequency_hz = opps
        .iter()
        .min_by(|left, right| {
            left.cost_mw_per_performance()
                .partial_cmp(&right.cost_mw_per_performance())
                .unwrap_or(Ordering::Equal)
        })
        .map_or(Hertz::ZERO, |opp| opp.frequency_hz);
    let control_opps = opps.clone();
    let performance_per_hz = opps
        .last()
        .map_or(0.0, |opp| opp.performance / hertz_as_f64(opp.frequency_hz));
    Ok(EnergyModel {
        opps,
        control_opps,
        performance_per_hz,
        free_frequency_hz,
        sweet_frequency_hz: free_frequency_hz,
        plain_frequency_hz: free_frequency_hz,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DemandDiagnostics {
    pub raw: f64,
    pub ema: f64,
    pub predicted: f64,
    pub selected: f64,
    pub prediction_bypassed_ramp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DemandState {
    previous_raw: f64,
    predicted_performance: f64,
}

impl DemandState {
    fn update_performance(
        &mut self,
        raw: f64,
        previous_final_performance: f64,
        maximum_performance: f64,
        config: &GovernorConfig,
    ) -> DemandDiagnostics {
        let raw = finite_unit(raw);
        let maximum_performance = if maximum_performance.is_finite() {
            maximum_performance.max(0.0)
        } else {
            0.0
        };
        let previous_final_performance = if previous_final_performance.is_finite() {
            previous_final_performance.clamp(0.0, maximum_performance)
        } else {
            maximum_performance
        };
        let performance_load = raw * previous_final_performance;
        let alpha: f64 = if performance_load > self.predicted_performance {
            0.98
        } else {
            0.97
        };
        self.predicted_performance =
            alpha.mul_add(self.predicted_performance, (1.0 - alpha) * performance_load);
        let delta = raw - self.previous_raw;
        let prediction_bypassed_ramp = delta > config.predict_threshold;
        let selected_performance =
            if prediction_bypassed_ramp && self.predicted_performance > performance_load {
                self.predicted_performance
            } else {
                performance_load
            }
            .clamp(0.0, maximum_performance);
        self.previous_raw = raw;

        let normalize = |performance: f64| {
            if maximum_performance > 0.0 {
                finite_unit(performance / maximum_performance)
            } else {
                0.0
            }
        };
        let predicted = normalize(self.predicted_performance);
        DemandDiagnostics {
            raw,
            // Uperf v3 has one asymmetric predicted-performance accumulator,
            // not a separate time-weighted EMA plus linear extrapolator. Keep
            // both normalized diagnostic fields populated for API stability.
            ema: predicted,
            predicted,
            selected: normalize(selected_performance),
            prediction_bypassed_ramp,
        }
    }
}

fn reference_normal_demand_performance(
    selected_performance: f64,
    maximum_performance: f64,
    margin: f64,
) -> f64 {
    let selected = selected_performance.clamp(0.0, maximum_performance);
    let margin = finite_unit(margin);
    let residual_headroom = selected + (maximum_performance - selected) * margin;
    let growth_limit = selected * (8.0 * margin).max(1.1);
    residual_headroom.min(growth_limit).min(maximum_performance)
}

fn reference_burst_demand_performance(
    selected_performance: f64,
    maximum_performance: f64,
    margin: f64,
    burst: f64,
) -> f64 {
    let selected = selected_performance.clamp(0.0, maximum_performance);
    let margin = finite_unit(margin);
    let burst = finite_unit(burst);
    let residual_headroom = selected + (maximum_performance - selected) * (margin + burst);
    let growth_limit = selected * (8.0 * (margin + 4.0 * burst)).max(1.1);
    residual_headroom.min(growth_limit).min(maximum_performance)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SamplingMode {
    Active,
    #[default]
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdaptiveSampler {
    mode: SamplingMode,
}

impl AdaptiveSampler {
    #[must_use]
    pub fn update(&mut self, maximum_load: f64, config: &GovernorConfig) -> u64 {
        let load = finite_unit(maximum_load);
        match self.mode {
            SamplingMode::Idle if load > config.active_load_threshold => {
                self.mode = SamplingMode::Active;
            }
            SamplingMode::Active if load < config.idle_load_threshold => {
                self.mode = SamplingMode::Idle;
            }
            SamplingMode::Active | SamplingMode::Idle => {}
        }
        self.interval_ms(config)
    }

    #[must_use]
    pub const fn mode(self) -> SamplingMode {
        self.mode
    }

    #[must_use]
    pub const fn interval_ms(self, config: &GovernorConfig) -> u64 {
        match self.mode {
            SamplingMode::Active => config.active_sample_ms,
            SamplingMode::Idle => config.idle_sample_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergyBucketState {
    capacity_mj: f64,
    remaining_mj: f64,
}

impl Default for EnergyBucketState {
    fn default() -> Self {
        Self {
            capacity_mj: 0.0,
            remaining_mj: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergyBucketDiagnostics {
    pub remaining_mj: f64,
    pub delta_mj: f64,
    pub selected_limit_power_mw: f64,
}

impl EnergyBucketState {
    /// Integrate actual elapsed time into the PL1/PL2-style energy bucket.
    ///
    /// Increasing capacity starts a fresh full bucket. Decreasing capacity only
    /// clamps the prior balance. A suspended interval must be passed with
    /// `integrate=false` so sleep time is never treated as recovery.
    #[must_use]
    pub fn update(
        &mut self,
        estimated_power_mw: f64,
        actual_dt_ms: u64,
        budget: PowerBudgetConfig,
        integrate: bool,
    ) -> EnergyBucketDiagnostics {
        let capacity = f64::from(budget.fast_limit_capacity_mj);
        if capacity > self.capacity_mj {
            self.remaining_mj = capacity;
        } else {
            self.remaining_mj = self.remaining_mj.clamp(-capacity, capacity);
        }
        self.capacity_mj = capacity;

        let mut delta_mj = 0.0;
        if integrate {
            delta_mj = (estimated_power_mw - f64::from(budget.slow_limit_power_mw))
                * duration_ms_as_seconds(actual_dt_ms);
            if delta_mj < 0.0 {
                delta_mj *= budget.fast_limit_recover_scale;
            }
            self.remaining_mj = (self.remaining_mj - delta_mj).clamp(-capacity, capacity);
        }
        // Android Uperf v3 selects PL2 while the bucket is strictly positive
        // and PL1 otherwise. The 0.9/1.1 hysteresis belongs to the package-cap
        // feedback loop, not to this fast/slow selection.
        let selected_limit_power_mw = if self.remaining_mj > 0.0 {
            f64::from(budget.fast_limit_power_mw)
        } else {
            f64::from(budget.slow_limit_power_mw)
        };
        EnergyBucketDiagnostics {
            remaining_mj: self.remaining_mj,
            delta_mj,
            selected_limit_power_mw,
        }
    }

    #[must_use]
    pub const fn remaining_mj(self) -> f64 {
        self.remaining_mj
    }

    fn diagnostics_without_update(self, budget: PowerBudgetConfig) -> EnergyBucketDiagnostics {
        EnergyBucketDiagnostics {
            remaining_mj: self.remaining_mj,
            delta_mj: 0.0,
            selected_limit_power_mw: if self.remaining_mj > 0.0 {
                f64::from(budget.fast_limit_power_mw)
            } else {
                f64::from(budget.slow_limit_power_mw)
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetedOpp {
    pub floor_hz: Hertz,
    pub cap_hz: Hertz,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct TargetGovernorState {
    demand: DemandState,
    budget_cap_hz: Option<Hertz>,
    last_limits: Option<FrequencyLimits>,
    last_change_at: Option<MonotonicMillis>,
}

struct PreparedTarget<'a> {
    id: &'a TargetId,
    target: &'a CpuTargetPolicy,
    model: &'a EnergyModel,
    safety_cap: Hertz,
    raw_load: f64,
    active_load_sum: f64,
    demand: DemandDiagnostics,
    maximum_performance: f64,
    effective_demand: f64,
    demanded_performance: f64,
    estimated_power_mw: f64,
}

/// All mutable state required by the energy governor.
///
/// Keeping this as a value returned from each transition makes evaluation
/// deterministic and lets reload validate a candidate before replacing live
/// state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GovernorState {
    targets: BTreeMap<TargetId, TargetGovernorState>,
    bucket: EnergyBucketState,
    previous_timestamp: Option<MonotonicMillis>,
    shared_ramp_elapsed_ms: u64,
    budget_cursor: Option<usize>,
    budget_path_len: usize,
}

pub struct GovernorInput<'a> {
    pub timestamp: MonotonicMillis,
    pub targets: &'a BTreeMap<TargetId, CpuTargetPolicy>,
    /// Highest per-CPU load in each cluster. Drives frequency demand.
    pub raw_loads: &'a BTreeMap<TargetId, f64>,
    /// Sum of the per-CPU loads in each cluster, in units of busy cores.
    ///
    /// Package power integration uses this instead of `raw_loads`, because one
    /// busy thread on a four-core cluster costs one core of power, not four.
    /// Missing entries fall back to `raw_loads × core_count`, preserving the
    /// previous behaviour for callers that cannot observe per-CPU loads.
    pub active_load_sums: &'a BTreeMap<TargetId, f64>,
    pub observed_frequencies: &'a BTreeMap<TargetId, Hertz>,
    pub administrator_caps: &'a BTreeMap<TargetId, Hertz>,
    pub thermal_caps: &'a BTreeMap<TargetId, Hertz>,
    pub config: &'a GovernorConfig,
    pub power_budget: PowerBudgetConfig,
    pub margin: f64,
    pub burst: f64,
    pub limit_efficiency: bool,
    /// False across suspend/resume or another known non-running interval.
    pub integrate_elapsed_time: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TargetGovernorDiagnostics {
    pub raw_load: f64,
    /// Busy cores used for power integration, in units of cores.
    pub active_load_sum: f64,
    pub ema_load: f64,
    pub predicted_load: f64,
    pub selected_load: f64,
    pub effective_demand: f64,
    pub prediction_bypassed_ramp: bool,
    pub estimated_power_mw: f64,
    pub requested_floor_hz: Hertz,
    pub selected_floor_hz: Hertz,
    pub selected_cap_hz: Hertz,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GovernorDiagnostics {
    pub elapsed_ms: u64,
    pub estimated_package_power_mw: f64,
    pub bucket_remaining_mj: f64,
    pub selected_package_budget_mw: f64,
    pub bypassed_power_budget: bool,
    pub shared_ramp_progress: f64,
    pub targets: BTreeMap<TargetId, TargetGovernorDiagnostics>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GovernorTransition {
    pub limits: BTreeMap<TargetId, FrequencyLimits>,
    pub next_state: GovernorState,
    pub diagnostics: GovernorDiagnostics,
}

#[derive(Debug, Error)]
pub enum GovernorError {
    #[error("target `{0}` has no energy model")]
    MissingEnergyModel(TargetId),
    #[error("target `{0}` has no model OPP at or below its safety cap")]
    NoOppBelowSafetyCap(TargetId),
    #[error("target `{target}` has an invalid frequency policy: {source}")]
    InvalidFrequencyPolicy {
        target: TargetId,
        #[source]
        source: PolicyError,
    },
}

/// Evaluate all CPU clusters as one power-constrained package.
///
/// Safety caps are applied before demand or power planning and are never
/// delayed by prediction, ramp, dwell, or burst.
///
/// # Errors
///
/// Returns [`GovernorError`] if a target lacks an energy model, has an invalid
/// frequency policy, or exposes no OPP below a safety cap.
#[allow(
    clippy::too_many_lines,
    reason = "one pure transition keeps safety, prediction, package allocation, ramp, dwell, and diagnostics in an auditable order"
)]
pub fn transition_governor(
    state: &GovernorState,
    input: &GovernorInput<'_>,
) -> Result<GovernorTransition, GovernorError> {
    let mut next_state = state.clone();
    let elapsed_ms = state.previous_timestamp.map_or(0, |previous| {
        input.timestamp.saturating_duration_since(previous)
    });
    next_state.previous_timestamp = Some(input.timestamp);

    let mut prepared = Vec::with_capacity(input.targets.len());
    let mut estimated_package_power_mw = 0.0;
    let mut any_prediction_bypass = false;
    for (id, target) in input.targets {
        target
            .frequency
            .validate()
            .map_err(|source| GovernorError::InvalidFrequencyPolicy {
                target: id.clone(),
                source,
            })?;
        let model = target
            .energy_model
            .as_ref()
            .ok_or_else(|| GovernorError::MissingEnergyModel(id.clone()))?;
        let safety_cap = [
            Some(target.frequency.hardware_limits.max),
            input.administrator_caps.get(id).copied(),
            input.thermal_caps.get(id).copied(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(target.frequency.hardware_limits.max);
        let maximum_opp = model
            .opp_at_or_below(safety_cap)
            .ok_or_else(|| GovernorError::NoOppBelowSafetyCap(id.clone()))?;
        let target_state = next_state.targets.entry(id.clone()).or_default();
        let raw_load = finite_unit(input.raw_loads.get(id).copied().unwrap_or(0.0));
        // The stripped controller initializes every cluster's previous final
        // OPP to its maximum. Afterwards load is capacity-scaled by the OPP
        // selected in the preceding cycle, rather than treated as a direct
        // percentage of the current maximum.
        let previous_final_performance = target_state
            .last_limits
            .and_then(|limits| model.opp_at_or_below(limits.min))
            .map_or(maximum_opp.performance, |opp| opp.performance)
            .min(maximum_opp.performance);
        let demand = target_state.demand.update_performance(
            raw_load,
            previous_final_performance,
            maximum_opp.performance,
            input.config,
        );
        any_prediction_bypass |= demand.prediction_bypassed_ramp;
        let selected_performance = maximum_opp.performance * demand.selected;
        let demanded_performance = reference_normal_demand_performance(
            selected_performance,
            maximum_opp.performance,
            input.margin,
        );
        let effective_demand = if maximum_opp.performance > 0.0 {
            demanded_performance / maximum_opp.performance
        } else {
            0.0
        };
        let estimate_frequency = input
            .observed_frequencies
            .get(id)
            .copied()
            .or_else(|| target_state.last_limits.map(|limits| limits.min))
            .unwrap_or(target.frequency.floor);
        let core_count = usize_as_f64(target.cpus.iter().count());
        let active_load_sum = input
            .active_load_sums
            .get(id)
            .copied()
            .filter(|value| value.is_finite())
            .map_or(raw_load * core_count, |value| value.clamp(0.0, core_count));
        let estimated_power_mw = model
            .opp_at_or_below(estimate_frequency)
            .map_or(0.0, |opp| {
                opp.power_mw_per_core * active_load_sum * reference_concurrency_scale(core_count)
            });
        estimated_package_power_mw += estimated_power_mw;
        prepared.push(PreparedTarget {
            id,
            target,
            model,
            safety_cap,
            raw_load,
            active_load_sum,
            demand,
            maximum_performance: maximum_opp.performance,
            effective_demand,
            demanded_performance,
            estimated_power_mw,
        });
    }

    if input.burst > 0.0 {
        let mut performance_order = (0..prepared.len()).collect::<Vec<_>>();
        performance_order.sort_by(|left, right| {
            prepared[*left]
                .model
                .performance_per_hz
                .total_cmp(&prepared[*right].model.performance_per_hz)
                .then_with(|| left.cmp(right))
        });

        // In Uperf v3 the burst calculation is stored in the per-cluster +24
        // guide slot only for clusters 1..N-1. Its consumer replaces those
        // clusters' generated floor while cluster 0 retains normal margin
        // demand. Model ordering supplies the Linux equivalent of that
        // little-to-big cluster index; safety caps remain absolute below.
        for index in performance_order.into_iter().skip(1) {
            let prepared = &mut prepared[index];
            let selected_performance = prepared.maximum_performance * prepared.demand.selected;
            prepared.demanded_performance = reference_burst_demand_performance(
                selected_performance,
                prepared.maximum_performance,
                input.margin,
                input.burst,
            );
            prepared.effective_demand = if prepared.maximum_performance > 0.0 {
                prepared.demanded_performance / prepared.maximum_performance
            } else {
                0.0
            };
        }
    }

    // Uperf v3 treats every non-zero burst as a temporary PL1/PL2 waiver in
    // addition to the high-cluster floor override above. Hardware,
    // administrator, and thermal caps were already applied and remain
    // absolute.
    let bypassed_power_budget = input.burst > 0.0;
    let path = reference_budget_path(&prepared);
    if next_state.budget_path_len != path.len() {
        next_state.budget_path_len = path.len();
        next_state.budget_cursor = path.len().checked_sub(1);
        for prepared in &prepared {
            next_state
                .targets
                .entry(prepared.id.clone())
                .or_default()
                .budget_cap_hz = prepared
                .model
                .control_opps()
                .last()
                .map(|opp| opp.frequency_hz);
        }
    }
    let bucket = if bypassed_power_budget {
        // The reference burst path freezes both the bucket and the global cap
        // cursor. It resumes from the same state after burst returns to zero.
        next_state
            .bucket
            .diagnostics_without_update(input.power_budget)
    } else {
        next_state.bucket.update(
            estimated_package_power_mw,
            elapsed_ms,
            input.power_budget,
            input.integrate_elapsed_time,
        )
    };
    let selected_package_budget_mw = if bypassed_power_budget {
        f64::INFINITY
    } else {
        bucket.selected_limit_power_mw
    };
    if !bypassed_power_budget {
        adjust_reference_budget_cap(
            &mut next_state,
            &prepared,
            &path,
            estimated_package_power_mw,
            selected_package_budget_mw,
        );
    }
    let budgeted = prepared
        .iter()
        .map(|prepared| {
            let demand_floor = prepared
                .model
                .first_opp_meeting_performance(prepared.demanded_performance)
                .map_or(prepared.target.frequency.floor, |opp| opp.frequency_hz);
            let feedback_cap = next_state
                .targets
                .get(prepared.id)
                .and_then(|state| state.budget_cap_hz)
                .unwrap_or(prepared.safety_cap);
            let cap_hz = if bypassed_power_budget {
                prepared.safety_cap
            } else {
                feedback_cap.min(prepared.safety_cap)
            };
            BudgetedOpp {
                floor_hz: demand_floor.min(cap_hz),
                cap_hz,
            }
        })
        .collect::<Vec<_>>();
    let bypassed_ramp = bypassed_power_budget || any_prediction_bypass;
    let high_cost_increase = prepared.iter().zip(&budgeted).any(|(prepared, plan)| {
        let previous = next_state
            .targets
            .get(prepared.id)
            .and_then(|state| state.last_limits)
            .map_or(prepared.target.frequency.floor, |limits| limits.min);
        plan.floor_hz > prepared.model.sweet_frequency_hz && plan.floor_hz > previous
    });
    if bypassed_ramp {
        next_state.shared_ramp_elapsed_ms = input.config.ramp_latency_ms;
    } else if high_cost_increase {
        next_state.shared_ramp_elapsed_ms = next_state
            .shared_ramp_elapsed_ms
            .saturating_add(elapsed_ms)
            .min(input.config.ramp_latency_ms);
    } else {
        next_state.shared_ramp_elapsed_ms = 0;
    }
    let shared_ramp_progress = if input.config.ramp_latency_ms == 0 {
        1.0
    } else {
        u64_as_f64(next_state.shared_ramp_elapsed_ms) / u64_as_f64(input.config.ramp_latency_ms)
    }
    .clamp(0.0, 1.0);

    let requested_floors = budgeted
        .iter()
        .map(|plan| plan.floor_hz)
        .collect::<Vec<_>>();
    let mut selected_plans = prepared
        .iter()
        .zip(&budgeted)
        .map(|(prepared, budgeted)| {
            let requested_floor_hz = budgeted.floor_hz;
            let mut selected_floor_hz =
                if bypassed_ramp || requested_floor_hz <= prepared.model.sweet_frequency_hz {
                    requested_floor_hz
                } else {
                    ramp_limited_floor(
                        prepared.model,
                        requested_floor_hz,
                        budgeted.cap_hz,
                        shared_ramp_progress,
                    )
                };

            let target_state = next_state.targets.get(prepared.id);
            let safety_reduction = target_state
                .and_then(|state| state.last_limits)
                .is_some_and(|previous| budgeted.cap_hz < previous.max);
            let dwell_active = target_state
                .and_then(|state| state.last_change_at)
                .is_some_and(|changed_at| {
                    input.timestamp.saturating_duration_since(changed_at)
                        < input.config.min_opp_residency_ms
                });
            let upward_change = target_state
                .and_then(|state| state.last_limits)
                .is_some_and(|previous| selected_floor_hz > previous.min);
            if dwell_active && upward_change && !safety_reduction && !bypassed_ramp {
                selected_floor_hz = target_state
                    .and_then(|state| state.last_limits)
                    .map_or(selected_floor_hz, |previous| {
                        previous.min.min(budgeted.cap_hz)
                    });
            }
            selected_floor_hz = selected_floor_hz.min(budgeted.cap_hz);
            BudgetedOpp {
                floor_hz: selected_floor_hz,
                cap_hz: budgeted.cap_hz,
            }
        })
        .collect::<Vec<_>>();
    if input.limit_efficiency {
        let models = prepared
            .iter()
            .map(|prepared| prepared.model)
            .collect::<Vec<_>>();
        apply_reference_efficiency_limits(&models, &mut selected_plans);
    }

    let mut limits = BTreeMap::new();
    let mut target_diagnostics = BTreeMap::new();
    for ((prepared, selected_plan), requested_floor_hz) in prepared
        .into_iter()
        .zip(selected_plans)
        .zip(requested_floors)
    {
        let target_state = next_state.targets.entry(prepared.id.clone()).or_default();
        let selected = FrequencyLimits {
            min: selected_plan.floor_hz,
            max: selected_plan.cap_hz,
        };
        if target_state.last_limits != Some(selected) {
            target_state.last_change_at = Some(input.timestamp);
        }
        target_state.last_limits = Some(selected);
        limits.insert(prepared.id.clone(), selected);
        target_diagnostics.insert(
            prepared.id.clone(),
            TargetGovernorDiagnostics {
                raw_load: prepared.raw_load,
                active_load_sum: prepared.active_load_sum,
                ema_load: prepared.demand.ema,
                predicted_load: prepared.demand.predicted,
                selected_load: prepared.demand.selected,
                effective_demand: prepared.effective_demand,
                prediction_bypassed_ramp: prepared.demand.prediction_bypassed_ramp,
                estimated_power_mw: prepared.estimated_power_mw,
                requested_floor_hz,
                selected_floor_hz: selected_plan.floor_hz,
                selected_cap_hz: selected_plan.cap_hz,
            },
        );
    }

    Ok(GovernorTransition {
        limits,
        next_state,
        diagnostics: GovernorDiagnostics {
            elapsed_ms,
            estimated_package_power_mw,
            bucket_remaining_mj: bucket.remaining_mj,
            selected_package_budget_mw,
            bypassed_power_budget,
            shared_ramp_progress,
            targets: target_diagnostics,
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BudgetStep {
    key: f64,
    cluster_index: usize,
    lower_hz: Hertz,
    upper_hz: Hertz,
}

/// Build the Android v3 global cap path. Each step represents one sparse
/// control-OPP transition, ordered by the lower OPP's absolute energy cost.
fn reference_budget_path(prepared: &[PreparedTarget<'_>]) -> Vec<BudgetStep> {
    let mut path = Vec::new();
    for (cluster_index, prepared) in prepared.iter().enumerate() {
        let controls = prepared.model.control_opps();
        let first = controls
            .iter()
            .position(|opp| opp.frequency_hz >= prepared.model.plain_frequency_hz)
            .unwrap_or_else(|| controls.len().saturating_sub(1));
        let penalty = reference_nr_cost_penalty(usize_as_f64(prepared.target.cpus.iter().count()));
        for pair in controls[first..].windows(2) {
            path.push(BudgetStep {
                key: pair[0].power_mw_per_core * penalty,
                cluster_index,
                lower_hz: pair[0].frequency_hz,
                upper_hz: pair[1].frequency_hz,
            });
        }
    }
    path.sort_by(|left, right| {
        left.key
            .total_cmp(&right.key)
            .then_with(|| left.cluster_index.cmp(&right.cluster_index))
            .then_with(|| left.lower_hz.cmp(&right.lower_hz))
    });
    path
}

/// Apply at most one entry from the global cap path, preserving the reference
/// controller's direction-change delay at the shared cursor.
fn adjust_reference_budget_cap(
    state: &mut GovernorState,
    prepared: &[PreparedTarget<'_>],
    path: &[BudgetStep],
    estimated_package_power_mw: f64,
    selected_package_budget_mw: f64,
) {
    let Some(cursor) = state.budget_cursor else {
        return;
    };
    let Some(step) = path.get(cursor) else {
        return;
    };
    let target = prepared[step.cluster_index].id;
    if estimated_package_power_mw > selected_package_budget_mw * 1.1 {
        state
            .targets
            .entry(target.clone())
            .or_default()
            .budget_cap_hz = Some(step.lower_hz);
        if cursor > 0 {
            state.budget_cursor = Some(cursor - 1);
        }
    } else if estimated_package_power_mw < selected_package_budget_mw * 0.9 {
        state
            .targets
            .entry(target.clone())
            .or_default()
            .budget_cap_hz = Some(step.upper_hz);
        if cursor + 1 < path.len() {
            state.budget_cursor = Some(cursor + 1);
        }
    }
}

/// Enforce Android v3's cross-cluster efficiency ceiling after ramp and
/// package-cap selection. The highest-performance cluster is never capped by
/// this rule; each lower cluster is compared with the already-finalized
/// adjacent higher cluster, so the result cascades from high to low.
fn apply_reference_efficiency_limits(models: &[&EnergyModel], selected: &mut [BudgetedOpp]) {
    let mut order = (0..models.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| {
        models[*left]
            .performance_per_hz
            .total_cmp(&models[*right].performance_per_hz)
            .then_with(|| left.cmp(right))
    });

    for pair in order.windows(2).rev() {
        let low_index = pair[0];
        let high_index = pair[1];
        let high_model = models[high_index];
        let Some(high_opp) = high_model
            .opp_at_or_below(selected[high_index].floor_hz)
            .or_else(|| high_model.opps().first())
        else {
            continue;
        };
        let high_cost = high_opp.cost_mw_per_performance();
        let low_model = models[low_index];
        let Some(mut efficiency_cap) = low_model
            .control_opps()
            .iter()
            // The binary uses the first strictly-more-expensive OPP as an
            // inclusive cap, a deliberate one-OPP tolerance over README prose.
            .find(|opp| opp.cost_mw_per_performance() > high_cost)
            .or_else(|| low_model.control_opps().last())
            .map(|opp| opp.frequency_hz)
        else {
            continue;
        };
        efficiency_cap = efficiency_cap.max(low_model.plain_frequency_hz);
        selected[low_index].cap_hz = selected[low_index].cap_hz.min(efficiency_cap);
        selected[low_index].floor_hz = selected[low_index].floor_hz.min(selected[low_index].cap_hz);
    }
}

fn reference_concurrency_scale(core_count: f64) -> f64 {
    if core_count <= 0.0 {
        return 0.0;
    }
    (1.0 + 0.8 * (core_count - 1.0)) / core_count
}

fn reference_nr_cost_penalty(core_count: f64) -> f64 {
    (1.0 + 0.1 * (core_count - 1.0)).clamp(1.0, 1.2)
}

fn finite_unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn ramp_limited_floor(
    model: &EnergyModel,
    requested_floor_hz: Hertz,
    cap_hz: Hertz,
    progress: f64,
) -> Hertz {
    let sweet = model
        .control_opps()
        .iter()
        .rev()
        .find(|opp| opp.frequency_hz <= model.sweet_frequency_hz)
        .or_else(|| model.control_opps().first());
    let maximum = model
        .control_opps()
        .iter()
        .rev()
        .find(|opp| opp.frequency_hz <= cap_hz)
        .or_else(|| model.control_opps().last());
    let (Some(sweet), Some(maximum)) = (sweet, maximum) else {
        return requested_floor_hz.min(cap_hz);
    };
    let power_span = maximum.power_mw_per_core - sweet.power_mw_per_core;
    if power_span <= f64::EPSILON {
        return requested_floor_hz.min(cap_hz);
    }
    model
        .control_opps()
        .iter()
        .take_while(|opp| opp.frequency_hz <= requested_floor_hz.min(cap_hz))
        .filter(|opp| {
            opp.frequency_hz <= model.sweet_frequency_hz
                || (opp.power_mw_per_core - sweet.power_mw_per_core) / power_span <= progress
        })
        .last()
        .map_or(sweet.frequency_hz.min(cap_hz), |opp| opp.frequency_hz)
}

#[allow(clippy::cast_precision_loss)]
fn duration_ms_as_seconds(value: u64) -> f64 {
    std::time::Duration::from_millis(value).as_secs_f64()
}

#[allow(clippy::cast_precision_loss)]
fn hertz_as_f64(value: Hertz) -> f64 {
    value.get() as f64
}

#[allow(clippy::cast_precision_loss)]
fn usize_as_f64(value: usize) -> f64 {
    value as f64
}

#[allow(clippy::cast_precision_loss)]
fn u64_as_f64(value: u64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CpuEnergyModelConfig, CpuId, CpuSet, CpuTargetPolicy, FrequencyPolicy, MeasuredOppConfig,
        TargetId,
    };

    fn reference_model() -> CpuEnergyModelConfig {
        CpuEnergyModelConfig::ReferenceCurveV1 {
            relative_performance: 100,
            typical_power_mw_per_core: 1_000,
            typical_frequency_hz: Hertz(3_000),
            sweet_frequency_hz: Hertz(2_000),
            plain_frequency_hz: Hertz(1_000),
            free_frequency_hz: Hertz(500),
        }
    }

    fn governor() -> GovernorConfig {
        GovernorConfig::default()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn reference_curve_is_continuous_and_restores_typical_power() {
        let model = EnergyModel::from_config(
            &reference_model(),
            &[
                Hertz(500),
                Hertz(999),
                Hertz(1_000),
                Hertz(1_001),
                Hertz(1_999),
                Hertz(2_000),
                Hertz(2_001),
                Hertz(3_000),
            ],
        )
        .expect("valid curve");
        let power = |frequency| {
            model
                .opps()
                .iter()
                .find(|opp| opp.frequency_hz == Hertz(frequency))
                .expect("OPP")
                .power_mw_per_core
        };
        assert!((power(3_000) - 1_000.0).abs() < 1e-9);
        assert!((power(999) - power(1_000)).abs() < 1.0);
        assert!((power(1_999) - power(2_000)).abs() < 1.0);
        assert!(
            model
                .opps()
                .windows(2)
                .all(|pair| pair[0].power_mw_per_core <= pair[1].power_mw_per_core)
        );
    }

    #[test]
    fn free_frequency_is_independently_snapped_to_a_real_opp() {
        let model = EnergyModel::from_config(
            &CpuEnergyModelConfig::ReferenceCurveV1 {
                relative_performance: 100,
                typical_power_mw_per_core: 1_000,
                typical_frequency_hz: Hertz(3_000),
                sweet_frequency_hz: Hertz(2_000),
                plain_frequency_hz: Hertz(2_000),
                // `free` is independently snapped even when it is not itself
                // a kernel OPP.
                free_frequency_hz: Hertz(1_600),
            },
            &[Hertz(500), Hertz(1_500), Hertz(2_000), Hertz(3_000)],
        )
        .expect("valid independently snapped free frequency");
        assert_eq!(model.free_frequency_hz, Hertz(1_500));
    }

    #[test]
    fn reference_curve_extrapolates_above_the_typical_calibration_point() {
        let model = EnergyModel::from_config(
            &reference_model(),
            &[Hertz(500), Hertz(3_000), Hertz(3_500)],
        )
        .expect("typical frequency is not a maximum");
        let typical = model
            .opps()
            .iter()
            .find(|opp| opp.frequency_hz == Hertz(3_000))
            .expect("typical OPP");
        let extrapolated = model
            .opps()
            .iter()
            .find(|opp| opp.frequency_hz == Hertz(3_500))
            .expect("extrapolated OPP");
        assert_close(typical.power_mw_per_core, 1_000.0);
        assert!(
            extrapolated.power_mw_per_core > typical.power_mw_per_core,
            "the cubic segment must continue above typical"
        );
    }

    #[test]
    fn predictor_uses_capacity_scaled_load_and_raw_delta_bypass() {
        let mut state = DemandState::default();
        let config = GovernorConfig {
            predict_threshold: 1.0,
            ..governor()
        };
        let scaled = state.update_performance(0.50, 40.0, 100.0, &config);
        assert_close(scaled.selected, 0.20);
        assert_close(scaled.predicted, 0.004);

        let mut first_cycle = DemandState::default();
        let first = first_cycle.update_performance(1.0, 100.0, 100.0, &governor());
        assert!(first.prediction_bypassed_ramp);
        assert_close(first.selected, 1.0);
    }

    #[test]
    fn predictor_uses_fixed_asymmetric_rise_and_fall_coefficients() {
        let mut state = DemandState::default();
        let config = GovernorConfig {
            predict_threshold: 1.0,
            ..governor()
        };
        let first = state.update_performance(0.50, 100.0, 100.0, &config);
        assert_close(first.predicted, 0.01);
        let second = state.update_performance(0.50, 100.0, 100.0, &config);
        assert_close(second.predicted, 0.0198);
        let falling = state.update_performance(0.0, 100.0, 100.0, &config);
        assert_close(falling.predicted, 0.019_206);
        assert_close(falling.ema, falling.predicted);
    }

    #[test]
    fn reference_normal_demand_applies_residual_and_growth_limits() {
        assert_close(reference_normal_demand_performance(10.0, 100.0, 0.1), 11.0);
        assert_close(reference_normal_demand_performance(50.0, 100.0, 0.2), 60.0);
        assert_close(reference_normal_demand_performance(0.0, 100.0, 1.0), 0.0);
    }

    #[test]
    fn adaptive_sampling_is_hysteretic() {
        let mut sampler = AdaptiveSampler::default();
        let config = governor();
        assert_eq!(sampler.update(0.20, &config), config.idle_sample_ms);
        assert_eq!(sampler.update(0.31, &config), config.active_sample_ms);
        assert_eq!(sampler.update(0.20, &config), config.active_sample_ms);
        assert_eq!(sampler.update(0.14, &config), config.idle_sample_ms);
    }

    #[test]
    fn energy_bucket_discharge_recovery_resize_and_suspend_are_exact() {
        let mut state = EnergyBucketState::default();
        let mut budget = PowerBudgetConfig {
            slow_limit_power_mw: 1_000,
            fast_limit_power_mw: 2_000,
            fast_limit_capacity_mj: 100,
            fast_limit_recover_scale: 2.0,
        };
        let first = state.update(2_000.0, 50, budget, true);
        assert_close(first.remaining_mj, 50.0);
        let recovered = state.update(500.0, 20, budget, true);
        assert_close(recovered.remaining_mj, 70.0);
        let suspended = state.update(0.0, 10_000, budget, false);
        assert_close(suspended.remaining_mj, 70.0);
        budget.fast_limit_capacity_mj = 200;
        assert_close(state.update(1_000.0, 0, budget, true).remaining_mj, 200.0);
        budget.fast_limit_capacity_mj = 20;
        assert_close(state.update(1_000.0, 0, budget, true).remaining_mj, 20.0);
    }

    #[test]
    fn energy_bucket_switches_budgets_at_zero_without_hysteresis() {
        let mut state = EnergyBucketState::default();
        let budget = PowerBudgetConfig {
            slow_limit_power_mw: 1_000,
            fast_limit_power_mw: 2_000,
            fast_limit_capacity_mj: 100,
            fast_limit_recover_scale: 1.0,
        };
        let inside_upper_band = state.update(2_050.0, 100, budget, true);
        assert_close(inside_upper_band.remaining_mj, -5.0);
        assert_close(inside_upper_band.selected_limit_power_mw, 1_000.0);

        let exhausted = state.update(1_100.0, 50, budget, true);
        assert_close(exhausted.remaining_mj, -10.0);
        assert_close(exhausted.selected_limit_power_mw, 1_000.0);

        let inside_lower_band = state.update(900.0, 100, budget, true);
        assert_close(inside_lower_band.remaining_mj, 0.0);
        assert_close(inside_lower_band.selected_limit_power_mw, 1_000.0);

        let recovered = state.update(900.0, 100, budget, true);
        assert_close(recovered.remaining_mj, 10.0);
        assert_close(recovered.selected_limit_power_mw, 2_000.0);
    }

    #[test]
    fn measured_models_require_exact_kernel_opps() {
        let config = CpuEnergyModelConfig::MeasuredOppV1 {
            points: vec![
                MeasuredOppConfig {
                    frequency_hz: Hertz(1_000),
                    relative_capacity: 100,
                    power_mw_per_core: 100,
                },
                MeasuredOppConfig {
                    frequency_hz: Hertz(2_000),
                    relative_capacity: 200,
                    power_mw_per_core: 300,
                },
            ],
        };
        assert!(matches!(
            EnergyModel::from_config(&config, &[Hertz(1_000), Hertz(1_500)]),
            Err(EnergyModelError::MissingMeasuredOpp(Hertz(1_500)))
        ));
    }

    #[test]
    fn efficiency_limit_is_dynamic_adjacent_and_never_caps_the_top_cluster() {
        let frequencies = [Hertz(1_000), Hertz(2_000), Hertz(3_000)];
        let low = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 100,
                        power_mw_per_core: 100,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(2_000),
                        relative_capacity: 200,
                        power_mw_per_core: 300,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(3_000),
                        relative_capacity: 300,
                        power_mw_per_core: 900,
                    },
                ],
            },
            &frequencies,
        )
        .expect("low cluster");
        let high = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 200,
                        power_mw_per_core: 100,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(2_000),
                        relative_capacity: 400,
                        power_mw_per_core: 400,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(3_000),
                        relative_capacity: 600,
                        power_mw_per_core: 1_200,
                    },
                ],
            },
            &frequencies,
        )
        .expect("high cluster");
        // Deliberately pass high before low: model performance, not target id
        // or map order, defines the cluster hierarchy.
        let mut selected = [
            BudgetedOpp {
                floor_hz: Hertz(2_000),
                cap_hz: Hertz(3_000),
            },
            BudgetedOpp {
                floor_hz: Hertz(3_000),
                cap_hz: Hertz(3_000),
            },
        ];
        apply_reference_efficiency_limits(&[&high, &low], &mut selected);
        assert_eq!(selected[0].cap_hz, Hertz(3_000));
        assert_eq!(selected[1].cap_hz, Hertz(2_000));
        assert_eq!(selected[1].floor_hz, Hertz(2_000));
    }

    #[test]
    fn synthetic_reference_curve_has_stable_golden_vectors() {
        let model = EnergyModel::from_config(
            &CpuEnergyModelConfig::ReferenceCurveV1 {
                relative_performance: 160,
                typical_power_mw_per_core: 512,
                typical_frequency_hz: Hertz(4_000_000_000),
                sweet_frequency_hz: Hertz(2_000_000_000),
                plain_frequency_hz: Hertz(1_000_000_000),
                free_frequency_hz: Hertz(500_000_000),
            },
            &[
                Hertz(500_000_000),
                Hertz(1_000_000_000),
                Hertz(2_000_000_000),
                Hertz(4_000_000_000),
            ],
        )
        .expect("synthetic reference curve");
        let powers = model
            .opps()
            .iter()
            .map(|opp| opp.power_mw_per_core)
            .collect::<Vec<_>>();
        for (actual, expected) in powers.iter().zip([8.0, 16.0, 64.0, 512.0]) {
            assert!((actual - expected).abs() < 1.0e-9);
        }
    }

    #[test]
    fn reference_control_opps_use_android_thirds_and_quarters() {
        let model = EnergyModel::from_config(
            &CpuEnergyModelConfig::ReferenceCurveV1 {
                relative_performance: 160,
                typical_power_mw_per_core: 512,
                typical_frequency_hz: Hertz(4_000),
                sweet_frequency_hz: Hertz(2_000),
                plain_frequency_hz: Hertz(1_000),
                free_frequency_hz: Hertz(500),
            },
            &[
                Hertz(500),
                Hertz(750),
                Hertz(1_000),
                Hertz(1_250),
                Hertz(1_500),
                Hertz(1_750),
                Hertz(2_000),
                Hertz(2_500),
                Hertz(3_000),
                Hertz(3_500),
                Hertz(4_000),
            ],
        )
        .expect("reference controls");
        assert_eq!(
            model
                .control_opps()
                .iter()
                .map(|opp| opp.frequency_hz)
                .collect::<Vec<_>>(),
            [500, 1_000, 1_250, 1_750, 2_000, 2_500, 3_000, 3_500, 4_000].map(Hertz)
        );
    }

    /// A single-CPU target whose model spans four OPPs, used by the budget and
    /// bypass tests below.
    fn budget_fixture() -> (TargetId, BTreeMap<TargetId, CpuTargetPolicy>) {
        let id = TargetId::new("cpu.test").expect("target");
        let model = EnergyModel::from_config(
            &reference_model(),
            &[Hertz(500), Hertz(1_000), Hertz(2_000), Hertz(3_000)],
        )
        .expect("model");
        let targets = BTreeMap::from([(
            id.clone(),
            CpuTargetPolicy {
                cpus: CpuSet::from_ids([CpuId(0)]),
                frequency: FrequencyPolicy {
                    hardware_limits: FrequencyLimits {
                        min: Hertz(500),
                        max: Hertz(3_000),
                    },
                    floor: Hertz(500),
                    reference: Hertz(3_000),
                    efficient_cap: Hertz(3_000),
                    hertz_per_unit: 1,
                    available_frequencies: vec![
                        Hertz(500),
                        Hertz(1_000),
                        Hertz(2_000),
                        Hertz(3_000),
                    ],
                },
                energy_model: Some(model),
            },
        )]);
        (id, targets)
    }

    fn budget_test_config() -> (GovernorConfig, PowerBudgetConfig) {
        let config = GovernorConfig {
            predict_threshold: 1.0,
            min_opp_residency_ms: 0,
            ..governor()
        };
        let budget = PowerBudgetConfig {
            slow_limit_power_mw: 100,
            fast_limit_power_mw: 150,
            fast_limit_capacity_mj: 10,
            fast_limit_recover_scale: 1.0,
        };
        (config, budget)
    }

    fn burst_cluster_fixture() -> (
        TargetId,
        TargetId,
        BTreeMap<TargetId, CpuTargetPolicy>,
        GovernorConfig,
        PowerBudgetConfig,
    ) {
        // Deliberately put the high-performance cluster first in BTree order:
        // burst cluster 0 is defined by model performance, not target ID.
        let low_id = TargetId::new("cpu.z-low").expect("low target");
        let high_id = TargetId::new("cpu.a-high").expect("high target");
        let frequencies = [Hertz(500), Hertz(1_000), Hertz(2_000), Hertz(3_000)];
        let make_target = |cpu, relative_performance| {
            let model = EnergyModel::from_config(
                &CpuEnergyModelConfig::ReferenceCurveV1 {
                    relative_performance,
                    typical_power_mw_per_core: 1_000,
                    typical_frequency_hz: Hertz(3_000),
                    sweet_frequency_hz: Hertz(2_000),
                    plain_frequency_hz: Hertz(1_000),
                    free_frequency_hz: Hertz(500),
                },
                &frequencies,
            )
            .expect("cluster model");
            CpuTargetPolicy {
                cpus: CpuSet::from_ids([CpuId(cpu)]),
                frequency: FrequencyPolicy {
                    hardware_limits: FrequencyLimits {
                        min: Hertz(500),
                        max: Hertz(3_000),
                    },
                    floor: Hertz(500),
                    reference: Hertz(3_000),
                    efficient_cap: Hertz(3_000),
                    hertz_per_unit: 1,
                    available_frequencies: frequencies.to_vec(),
                },
                energy_model: Some(model),
            }
        };
        let targets = BTreeMap::from([
            (low_id.clone(), make_target(0, 100)),
            (high_id.clone(), make_target(1, 200)),
        ]);
        let config = GovernorConfig {
            predict_threshold: 1.0,
            ramp_latency_ms: 0,
            min_opp_residency_ms: 0,
            ..governor()
        };
        let budget = PowerBudgetConfig {
            slow_limit_power_mw: 1_000_000,
            fast_limit_power_mw: 1_000_000,
            fast_limit_capacity_mj: 1_000,
            fast_limit_recover_scale: 1.0,
        };
        (low_id, high_id, targets, config, budget)
    }

    #[test]
    fn burst_leaves_low_cluster_normal_and_raises_high_cluster_floor() {
        let (low_id, high_id, targets, config, budget) = burst_cluster_fixture();
        let loads = BTreeMap::from([(low_id.clone(), 0.25), (high_id.clone(), 0.25)]);
        let empty = BTreeMap::new();
        let evaluate = |burst| {
            transition_governor(
                &GovernorState::default(),
                &GovernorInput {
                    timestamp: MonotonicMillis(0),
                    targets: &targets,
                    raw_loads: &loads,
                    active_load_sums: &loads,
                    observed_frequencies: &empty,
                    administrator_caps: &empty,
                    thermal_caps: &empty,
                    config: &config,
                    power_budget: budget,
                    margin: 0.0,
                    burst,
                    limit_efficiency: false,
                    integrate_elapsed_time: true,
                },
            )
            .expect("cluster transition")
        };
        let normal = evaluate(0.0);
        let burst = evaluate(0.2);

        assert_eq!(normal.limits[&low_id].min, Hertz(1_000));
        assert_eq!(burst.limits[&low_id].min, normal.limits[&low_id].min);
        assert_eq!(normal.limits[&high_id].min, Hertz(1_000));
        assert_eq!(burst.limits[&high_id].min, Hertz(2_000));
    }

    #[test]
    fn burst_high_cluster_override_never_exceeds_safety_cap() {
        let (low_id, high_id, targets, config, budget) = burst_cluster_fixture();
        let loads = BTreeMap::from([(low_id, 0.25), (high_id.clone(), 1.0)]);
        let empty = BTreeMap::new();
        let thermal_caps = BTreeMap::from([(high_id.clone(), Hertz(1_000))]);
        let transition = transition_governor(
            &GovernorState::default(),
            &GovernorInput {
                timestamp: MonotonicMillis(0),
                targets: &targets,
                raw_loads: &loads,
                active_load_sums: &loads,
                observed_frequencies: &empty,
                administrator_caps: &empty,
                thermal_caps: &thermal_caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 1.0,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("safety-capped burst");

        assert_eq!(
            transition.limits[&high_id],
            FrequencyLimits {
                min: Hertz(1_000),
                max: Hertz(1_000),
            }
        );
    }

    #[test]
    fn stateful_transition_enforces_budget_and_burst_waives_it() {
        let (id, targets) = budget_fixture();
        let loads = BTreeMap::from([(id.clone(), 1.0)]);
        let observed_frequencies = BTreeMap::from([(id.clone(), Hertz(3_000))]);
        let empty_caps = BTreeMap::new();
        let (config, budget) = budget_test_config();
        let constrained = transition_governor(
            &GovernorState::default(),
            &GovernorInput {
                timestamp: MonotonicMillis(0),
                targets: &targets,
                raw_loads: &loads,
                active_load_sums: &loads,
                observed_frequencies: &observed_frequencies,
                administrator_caps: &empty_caps,
                thermal_caps: &empty_caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.0,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("budgeted transition");
        // Android feedback sheds exactly one global control-OPP transition per
        // cycle instead of jumping directly to a budget-derived static cap.
        assert_eq!(constrained.limits[&id].max, Hertz(2_000));

        // This is the reference v3 contract: any non-zero burst temporarily
        // ignores the slow and fast power limits.
        let bursty = transition_governor(
            &constrained.next_state,
            &GovernorInput {
                timestamp: MonotonicMillis(20),
                targets: &targets,
                raw_loads: &loads,
                active_load_sums: &loads,
                observed_frequencies: &observed_frequencies,
                administrator_caps: &empty_caps,
                thermal_caps: &empty_caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.2,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("bursty transition");
        assert!(bursty.diagnostics.bypassed_power_budget);
        assert!(bursty.diagnostics.selected_package_budget_mw.is_infinite());
        assert_eq!(bursty.limits[&id].max, Hertz(3_000));
    }

    #[test]
    fn reference_budget_cursor_reverses_with_a_one_cycle_delay() {
        let (id, targets) = budget_fixture();
        let high_load = BTreeMap::from([(id.clone(), 1.0)]);
        let low_load = BTreeMap::from([(id.clone(), 0.0)]);
        let high_frequency = BTreeMap::from([(id.clone(), Hertz(3_000))]);
        let low_frequency = BTreeMap::from([(id.clone(), Hertz(500))]);
        let caps = BTreeMap::new();
        let (config, budget) = budget_test_config();
        let high = transition_governor(
            &GovernorState::default(),
            &GovernorInput {
                timestamp: MonotonicMillis(0),
                targets: &targets,
                raw_loads: &high_load,
                active_load_sums: &high_load,
                observed_frequencies: &high_frequency,
                administrator_caps: &caps,
                thermal_caps: &caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.0,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("one downward step");
        assert_eq!(high.limits[&id].max, Hertz(2_000));

        let first_low = transition_governor(
            &high.next_state,
            &GovernorInput {
                timestamp: MonotonicMillis(20),
                targets: &targets,
                raw_loads: &low_load,
                active_load_sums: &low_load,
                observed_frequencies: &low_frequency,
                administrator_caps: &caps,
                thermal_caps: &caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.0,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("direction-change step");
        assert_eq!(first_low.limits[&id].max, Hertz(2_000));

        let second_low = transition_governor(
            &first_low.next_state,
            &GovernorInput {
                timestamp: MonotonicMillis(40),
                targets: &targets,
                raw_loads: &low_load,
                active_load_sums: &low_load,
                observed_frequencies: &low_frequency,
                administrator_caps: &caps,
                thermal_caps: &caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.0,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("upward step after cursor catches up");
        assert_eq!(second_low.limits[&id].max, Hertz(3_000));
    }

    #[test]
    fn reference_budget_path_orders_absolute_power_not_perf_per_watt() {
        let frequencies = [Hertz(500), Hertz(1_000), Hertz(2_000), Hertz(3_000)];
        let make_model = |relative_performance, typical_power_mw_per_core| {
            EnergyModel::from_config(
                &CpuEnergyModelConfig::ReferenceCurveV1 {
                    relative_performance,
                    typical_power_mw_per_core,
                    typical_frequency_hz: Hertz(3_000),
                    sweet_frequency_hz: Hertz(2_000),
                    plain_frequency_hz: Hertz(1_000),
                    free_frequency_hz: Hertz(500),
                },
                &frequencies,
            )
            .expect("reference model")
        };
        let low_power_id = TargetId::new("cpu.low-power").expect("target");
        let high_efficiency_id = TargetId::new("cpu.high-efficiency").expect("target");
        let low_power_model = make_model(100, 100);
        let high_efficiency_model = make_model(1_000, 200);
        let make_target = |cpu, model| CpuTargetPolicy {
            cpus: CpuSet::from_ids([CpuId(cpu)]),
            frequency: FrequencyPolicy {
                hardware_limits: FrequencyLimits {
                    min: Hertz(500),
                    max: Hertz(3_000),
                },
                floor: Hertz(500),
                reference: Hertz(3_000),
                efficient_cap: Hertz(3_000),
                hertz_per_unit: 1,
                available_frequencies: frequencies.to_vec(),
            },
            energy_model: Some(model),
        };
        let low_power_target = make_target(0, low_power_model);
        let high_efficiency_target = make_target(1, high_efficiency_model);
        let zero_demand = DemandDiagnostics {
            raw: 0.0,
            ema: 0.0,
            predicted: 0.0,
            selected: 0.0,
            prediction_bypassed_ramp: false,
        };
        let low_power_model = low_power_target.energy_model.as_ref().expect("model");
        let high_efficiency_model = high_efficiency_target.energy_model.as_ref().expect("model");
        let prepared = [
            PreparedTarget {
                id: &low_power_id,
                target: &low_power_target,
                model: low_power_model,
                safety_cap: Hertz(3_000),
                raw_load: 0.0,
                active_load_sum: 0.0,
                demand: zero_demand,
                maximum_performance: 0.0,
                effective_demand: 0.0,
                demanded_performance: 0.0,
                estimated_power_mw: 0.0,
            },
            PreparedTarget {
                id: &high_efficiency_id,
                target: &high_efficiency_target,
                model: high_efficiency_model,
                safety_cap: Hertz(3_000),
                raw_load: 0.0,
                active_load_sum: 0.0,
                demand: zero_demand,
                maximum_performance: 0.0,
                effective_demand: 0.0,
                demanded_performance: 0.0,
                estimated_power_mw: 0.0,
            },
        ];

        let low_plain = &low_power_model.control_opps()[1];
        let high_plain = &high_efficiency_model.control_opps()[1];
        assert!(low_plain.power_mw_per_core < high_plain.power_mw_per_core);
        assert!(
            low_plain.cost_mw_per_performance() > high_plain.cost_mw_per_performance(),
            "fixture must distinguish absolute power from Perf/W ordering"
        );

        let path = reference_budget_path(&prepared);
        assert_eq!(path.first().map(|step| step.cluster_index), Some(0));
        assert_close(
            path.first().expect("budget step").key,
            low_plain.power_mw_per_core,
        );
    }

    /// Burst lifts the power budget only: a thermal cap still binds.
    #[test]
    fn bypassing_the_budget_never_bypasses_a_thermal_cap() {
        let (id, targets) = budget_fixture();
        let loads = BTreeMap::from([(id.clone(), 1.0)]);
        let empty_frequencies = BTreeMap::new();
        let empty_caps = BTreeMap::new();
        let (config, budget) = budget_test_config();
        let thermal_caps = BTreeMap::from([(id.clone(), Hertz(2_000))]);
        let bypassed = transition_governor(
            &GovernorState::default(),
            &GovernorInput {
                timestamp: MonotonicMillis(20),
                targets: &targets,
                raw_loads: &loads,
                active_load_sums: &loads,
                observed_frequencies: &empty_frequencies,
                administrator_caps: &empty_caps,
                thermal_caps: &thermal_caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.2,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("bypassed transition");
        assert!(bypassed.diagnostics.bypassed_power_budget);
        assert_eq!(
            bypassed.limits[&id],
            FrequencyLimits {
                min: Hertz(2_000),
                max: Hertz(2_000),
            }
        );
    }

    #[test]
    fn power_integration_charges_busy_cores_not_the_whole_cluster() {
        let id = TargetId::new("cpu.quad").expect("target");
        let opps = [Hertz(1_000), Hertz(2_000)];
        let model = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 100,
                        power_mw_per_core: 100,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(2_000),
                        relative_capacity: 200,
                        power_mw_per_core: 400,
                    },
                ],
            },
            &opps,
        )
        .expect("model");
        let targets = BTreeMap::from([(
            id.clone(),
            CpuTargetPolicy {
                cpus: CpuSet::from_ids([CpuId(0), CpuId(1), CpuId(2), CpuId(3)]),
                frequency: FrequencyPolicy {
                    hardware_limits: FrequencyLimits {
                        min: Hertz(1_000),
                        max: Hertz(2_000),
                    },
                    floor: Hertz(1_000),
                    reference: Hertz(2_000),
                    efficient_cap: Hertz(2_000),
                    hertz_per_unit: 1,
                    available_frequencies: opps.to_vec(),
                },
                energy_model: Some(model),
            },
        )]);
        // One saturated CPU out of four: frequency demand is 1.0, but only one
        // core's worth of power is being drawn.
        let raw_loads = BTreeMap::from([(id.clone(), 1.0)]);
        let observed = BTreeMap::from([(id.clone(), Hertz(1_000))]);
        let empty_caps = BTreeMap::new();
        let budget = PowerBudgetConfig {
            slow_limit_power_mw: 10_000,
            fast_limit_power_mw: 10_000,
            fast_limit_capacity_mj: 10,
            fast_limit_recover_scale: 1.0,
        };
        let input = |sums: &BTreeMap<TargetId, f64>| {
            transition_governor(
                &GovernorState::default(),
                &GovernorInput {
                    timestamp: MonotonicMillis(0),
                    targets: &targets,
                    raw_loads: &raw_loads,
                    active_load_sums: sums,
                    observed_frequencies: &observed,
                    administrator_caps: &empty_caps,
                    thermal_caps: &empty_caps,
                    config: &governor(),
                    power_budget: budget,
                    margin: 0.0,
                    burst: 0.0,
                    limit_efficiency: false,
                    integrate_elapsed_time: true,
                },
            )
            .expect("transition")
        };

        let sparse = input(&BTreeMap::from([(id.clone(), 1.0)]));
        // Uperf v3 applies its shared-power discount: four cores scale by
        // ((4-1)*0.8+1)/4 = 0.85.
        assert!((sparse.diagnostics.estimated_package_power_mw - 85.0).abs() < 1.0e-9);
        assert!((sparse.diagnostics.targets[&id].active_load_sum - 1.0).abs() < 1.0e-9);

        let saturated = input(&BTreeMap::from([(id.clone(), 4.0)]));
        assert!((saturated.diagnostics.estimated_package_power_mw - 340.0).abs() < 1.0e-9);

        // Absent per-CPU data the old `max × cores` estimate is retained.
        let fallback = input(&BTreeMap::new());
        assert!((fallback.diagnostics.estimated_package_power_mw - 340.0).abs() < 1.0e-9);

        // A sum beyond the cluster width cannot inflate the estimate.
        let clamped = input(&BTreeMap::from([(id.clone(), 9.0)]));
        assert!((clamped.diagnostics.estimated_package_power_mw - 340.0).abs() < 1.0e-9);
    }

    #[test]
    fn minimum_residency_never_delays_a_downfrequency() {
        let id = TargetId::new("cpu.test").expect("target");
        let model = EnergyModel::from_config(
            &reference_model(),
            &[Hertz(500), Hertz(1_000), Hertz(2_000), Hertz(3_000)],
        )
        .expect("model");
        let targets = BTreeMap::from([(
            id.clone(),
            CpuTargetPolicy {
                cpus: CpuSet::from_ids([CpuId(0)]),
                frequency: FrequencyPolicy {
                    hardware_limits: FrequencyLimits {
                        min: Hertz(500),
                        max: Hertz(3_000),
                    },
                    floor: Hertz(500),
                    reference: Hertz(3_000),
                    efficient_cap: Hertz(3_000),
                    hertz_per_unit: 1,
                    available_frequencies: vec![
                        Hertz(500),
                        Hertz(1_000),
                        Hertz(2_000),
                        Hertz(3_000),
                    ],
                },
                energy_model: Some(model),
            },
        )]);
        let config = GovernorConfig {
            predict_threshold: 1.0,
            ramp_latency_ms: 0,
            min_opp_residency_ms: 100,
            ..governor()
        };
        let budget = PowerBudgetConfig {
            slow_limit_power_mw: 10_000,
            fast_limit_power_mw: 10_000,
            fast_limit_capacity_mj: 100,
            fast_limit_recover_scale: 1.0,
        };
        let frequencies = BTreeMap::new();
        let caps = BTreeMap::new();
        let high_load = BTreeMap::from([(id.clone(), 1.0)]);
        let high = transition_governor(
            &GovernorState::default(),
            &GovernorInput {
                timestamp: MonotonicMillis(0),
                targets: &targets,
                raw_loads: &high_load,
                active_load_sums: &high_load,
                observed_frequencies: &frequencies,
                administrator_caps: &caps,
                thermal_caps: &caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.0,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("high transition");
        let low_load = BTreeMap::from([(id.clone(), 0.0)]);
        let low = transition_governor(
            &high.next_state,
            &GovernorInput {
                timestamp: MonotonicMillis(10),
                targets: &targets,
                raw_loads: &low_load,
                active_load_sums: &low_load,
                observed_frequencies: &frequencies,
                administrator_caps: &caps,
                thermal_caps: &caps,
                config: &config,
                power_budget: budget,
                margin: 0.0,
                burst: 0.0,
                limit_efficiency: false,
                integrate_elapsed_time: true,
            },
        )
        .expect("low transition");
        assert!(
            low.limits[&id].min < high.limits[&id].min,
            "minimum residency applies only to upward OPP changes"
        );
    }
}
