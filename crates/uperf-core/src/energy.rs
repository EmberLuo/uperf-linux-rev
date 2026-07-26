//! Pure energy-model and dynamic-governor primitives.
//!
//! This module contains no Linux I/O.  It intentionally accepts explicit
//! elapsed time so recorded traces can be replayed deterministically.

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
    pub free_frequency_hz: Hertz,
    pub sweet_frequency_hz: Hertz,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum EnergyModelError {
    #[error("an energy model requires at least two available OPPs")]
    InsufficientOpps,
    #[error("energy-model value `{0}` must be finite and greater than zero")]
    InvalidValue(&'static str),
    #[error("energy-model frequencies must satisfy plain <= sweet <= typical and free <= typical")]
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
    /// Returns [`EnergyModelError`] when calibration data is invalid or cannot
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
        self.opps
            .iter()
            .find(|opp| opp.performance >= performance)
            .or_else(|| self.opps.last())
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
    if !(*plain_frequency_hz <= *sweet_frequency_hz && *sweet_frequency_hz <= *typical_frequency_hz)
        || *free_frequency_hz > *typical_frequency_hz
    {
        return Err(EnergyModelError::InvalidCurveOrdering);
    }
    let typical_frequency = hertz_as_f64(*typical_frequency_hz);
    let a3 = f64::from(*typical_power_mw_per_core) / typical_frequency.powi(3);
    let a2 = a3 * hertz_as_f64(*sweet_frequency_hz);
    let a1 = a2 * hertz_as_f64(*plain_frequency_hz);
    let performance_scale = f64::from(*relative_performance) / 100.0;
    let free_frequency_hz = snap_frequency_nearest(&frequencies, *free_frequency_hz);
    let opps = frequencies
        .into_iter()
        .map(|frequency_hz| {
            let frequency = hertz_as_f64(frequency_hz);
            let power_mw_per_core = if frequency_hz <= *plain_frequency_hz {
                a1 * frequency
            } else if frequency_hz <= *sweet_frequency_hz {
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
        .collect();
    Ok(EnergyModel {
        opps,
        free_frequency_hz,
        sweet_frequency_hz: *sweet_frequency_hz,
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
    Ok(EnergyModel {
        opps,
        free_frequency_hz,
        sweet_frequency_hz: free_frequency_hz,
    })
}

/// The exact documented Uperf v3 demand transformation.
#[must_use]
pub fn effective_demand(load: f64, margin: f64, burst: f64) -> f64 {
    let load = finite_unit(load);
    let headroom = finite_unit(margin + burst);
    load + (1.0 - load) * headroom
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
    previous_raw: Option<f64>,
    ema: Option<f64>,
}

impl DemandState {
    /// Consume one raw load sample with elapsed-time-weighted smoothing.
    #[must_use]
    pub fn update(
        &mut self,
        raw: f64,
        elapsed_ms: u64,
        config: &GovernorConfig,
    ) -> DemandDiagnostics {
        let raw = finite_unit(raw);
        let previous_raw = self.previous_raw.unwrap_or(raw);
        let ema = self.ema.map_or(raw, |previous| {
            elapsed_weighted_ema(previous, raw, elapsed_ms, config.ema_time_constant_ms)
        });
        let delta = raw - previous_raw;
        let predicted = finite_unit(raw + config.prediction_gain * delta);
        let prediction_bypassed_ramp = delta > config.predict_threshold;
        let selected = if prediction_bypassed_ramp {
            predicted
        } else {
            ema
        };
        self.previous_raw = Some(raw);
        self.ema = Some(ema);
        DemandDiagnostics {
            raw,
            ema,
            predicted,
            selected,
            prediction_bypassed_ramp,
        }
    }
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
    fast_budget_active: bool,
}

impl Default for EnergyBucketState {
    fn default() -> Self {
        Self {
            capacity_mj: 0.0,
            remaining_mj: 0.0,
            fast_budget_active: false,
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
            self.fast_budget_active = capacity > 0.0;
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
        if capacity == 0.0 {
            self.fast_budget_active = false;
        } else if self.fast_budget_active {
            // Expressed as used energy, this switches at 1.1 × capacity.
            if self.remaining_mj <= -0.1 * capacity {
                self.fast_budget_active = false;
            }
        } else if self.remaining_mj >= 0.1 * capacity {
            // Expressed as used energy, this switches back at 0.9 × capacity.
            self.fast_budget_active = true;
        }
        let selected_limit_power_mw = if self.fast_budget_active {
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct BudgetCluster<'a> {
    pub model: &'a EnergyModel,
    pub core_count: u32,
    pub demanded_performance_per_core: f64,
    /// Normal operating floor. A tighter safety cap may still force the
    /// selected OPP below it.
    pub minimum_hz: Hertz,
    pub safety_cap_hz: Hertz,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetedOpp {
    pub floor_hz: Hertz,
    pub cap_hz: Hertz,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct TargetGovernorState {
    demand: DemandState,
    last_limits: Option<FrequencyLimits>,
    last_change_at: Option<MonotonicMillis>,
}

struct PreparedTarget<'a> {
    id: &'a TargetId,
    target: &'a CpuTargetPolicy,
    model: &'a EnergyModel,
    safety_cap: Hertz,
    raw_load: f64,
    demand: DemandDiagnostics,
    effective_demand: f64,
    demanded_performance: f64,
    estimated_power_mw: f64,
}

/// All mutable state required by the energy governor.
///
/// Keeping this as a value returned from each transition makes policy replay
/// deterministic and lets reload validate a candidate before replacing live
/// state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GovernorState {
    targets: BTreeMap<TargetId, TargetGovernorState>,
    bucket: EnergyBucketState,
    previous_timestamp: Option<MonotonicMillis>,
    shared_ramp_elapsed_ms: u64,
}

pub struct GovernorInput<'a> {
    pub timestamp: MonotonicMillis,
    pub targets: &'a BTreeMap<TargetId, CpuTargetPolicy>,
    pub raw_loads: &'a BTreeMap<TargetId, f64>,
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
    #[error("target `{0}` has no calibrated energy model")]
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
            input
                .limit_efficiency
                .then_some(target.frequency.efficient_cap),
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
        let demand = target_state
            .demand
            .update(raw_load, elapsed_ms, input.config);
        any_prediction_bypass |= demand.prediction_bypassed_ramp;
        let effective_demand = effective_demand(demand.selected, input.margin, input.burst);
        let demanded_performance = maximum_opp.performance * effective_demand;
        let estimate_frequency = input
            .observed_frequencies
            .get(id)
            .copied()
            .or_else(|| target_state.last_limits.map(|limits| limits.min))
            .unwrap_or(target.frequency.floor);
        let estimated_power_mw = model
            .opp_at_or_below(estimate_frequency)
            .map_or(0.0, |opp| {
                opp.power_mw_per_core * raw_load * usize_as_f64(target.cpus.iter().count())
            });
        estimated_package_power_mw += estimated_power_mw;
        prepared.push(PreparedTarget {
            id,
            target,
            model,
            safety_cap,
            raw_load,
            demand,
            effective_demand,
            demanded_performance,
            estimated_power_mw,
        });
    }

    let bypassed_power_budget = input.burst > 0.0;
    let bucket = next_state.bucket.update(
        estimated_package_power_mw,
        elapsed_ms,
        input.power_budget,
        input.integrate_elapsed_time && !bypassed_power_budget,
    );
    let selected_package_budget_mw = if bypassed_power_budget {
        f64::INFINITY
    } else {
        bucket.selected_limit_power_mw
    };
    let budget_inputs = prepared
        .iter()
        .map(|prepared| BudgetCluster {
            model: prepared.model,
            core_count: u32::try_from(prepared.target.cpus.iter().count()).unwrap_or(u32::MAX),
            demanded_performance_per_core: prepared.demanded_performance,
            minimum_hz: prepared.target.frequency.floor,
            safety_cap_hz: prepared.safety_cap,
        })
        .collect::<Vec<_>>();
    let budgeted = allocate_package_budget(&budget_inputs, selected_package_budget_mw);

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

    let mut limits = BTreeMap::new();
    let mut target_diagnostics = BTreeMap::new();
    for (prepared, budgeted) in prepared.into_iter().zip(budgeted) {
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

        let target_state = next_state.targets.entry(prepared.id.clone()).or_default();
        let safety_reduction = target_state
            .last_limits
            .is_some_and(|previous| budgeted.cap_hz < previous.max);
        let dwell_active = target_state.last_change_at.is_some_and(|changed_at| {
            input.timestamp.saturating_duration_since(changed_at)
                < input.config.min_opp_residency_ms
        });
        let upward_change = target_state
            .last_limits
            .is_some_and(|previous| selected_floor_hz > previous.min);
        if dwell_active && upward_change && !safety_reduction && !bypassed_ramp {
            selected_floor_hz = target_state
                .last_limits
                .map_or(selected_floor_hz, |previous| {
                    previous.min.min(budgeted.cap_hz)
                });
        }
        selected_floor_hz = selected_floor_hz.min(budgeted.cap_hz);
        let selected = FrequencyLimits {
            min: selected_floor_hz,
            max: budgeted.cap_hz,
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
                ema_load: prepared.demand.ema,
                predicted_load: prepared.demand.predicted,
                selected_load: prepared.demand.selected,
                effective_demand: prepared.effective_demand,
                prediction_bypassed_ramp: prepared.demand.prediction_bypassed_ramp,
                estimated_power_mw: prepared.estimated_power_mw,
                requested_floor_hz,
                selected_floor_hz,
                selected_cap_hz: budgeted.cap_hz,
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

/// Allocate a package budget by globally ranking every next OPP by
/// `Δperformance / Δpower`.
///
/// The returned floor expresses demand, while the cap expresses the affordable
/// envelope. If the package budget cannot afford a demand floor, cap wins and
/// the floor is clamped down to preserve a safe `floor <= cap` pair.
#[must_use]
pub fn allocate_package_budget(
    clusters: &[BudgetCluster<'_>],
    package_budget_mw: f64,
) -> Vec<BudgetedOpp> {
    let eligible = clusters
        .iter()
        .map(eligible_budget_opps)
        .collect::<Vec<_>>();
    let mut selected = vec![0_usize; clusters.len()];
    let mut used_power = eligible
        .iter()
        .zip(clusters)
        .filter_map(|(opps, cluster)| {
            opps.first()
                .map(|opp| opp.power_mw_per_core * f64::from(cluster.core_count))
        })
        .sum::<f64>();

    loop {
        let best = eligible
            .iter()
            .enumerate()
            .filter_map(|(cluster_index, opps)| {
                let current_index = selected[cluster_index];
                let current = *opps.get(current_index)?;
                let next = *opps.get(current_index + 1)?;
                let cores = f64::from(clusters[cluster_index].core_count);
                let delta_power = (next.power_mw_per_core - current.power_mw_per_core) * cores;
                if delta_power <= 0.0 || used_power + delta_power > package_budget_mw {
                    return None;
                }
                let delta_performance = (next.performance - current.performance) * cores;
                Some((cluster_index, delta_performance / delta_power, delta_power))
            })
            .max_by(|left, right| left.1.partial_cmp(&right.1).unwrap_or(Ordering::Equal));
        let Some((cluster_index, _, delta_power)) = best else {
            break;
        };
        selected[cluster_index] += 1;
        used_power += delta_power;
    }

    clusters
        .iter()
        .zip(&eligible)
        .zip(selected)
        .map(|((cluster, opps), cap_index)| {
            let cap = opps
                .get(cap_index)
                .or_else(|| opps.last())
                .map_or(Hertz::ZERO, |opp| opp.frequency_hz);
            let demanded = cluster
                .model
                .first_opp_meeting_performance(cluster.demanded_performance_per_core)
                .map_or(cap, |opp| opp.frequency_hz)
                .min(cluster.safety_cap_hz)
                .max(opps.first().map_or(Hertz::ZERO, |opp| opp.frequency_hz));
            BudgetedOpp {
                floor_hz: demanded.min(cap),
                cap_hz: cap,
            }
        })
        .collect()
}

fn eligible_budget_opps<'model>(cluster: &BudgetCluster<'model>) -> Vec<&'model EnergyOpp> {
    let capped = cluster
        .model
        .opps()
        .iter()
        .take_while(|opp| opp.frequency_hz <= cluster.safety_cap_hz)
        .collect::<Vec<_>>();
    let first_at_floor = capped
        .iter()
        .position(|opp| opp.frequency_hz >= cluster.minimum_hz)
        // A safety cap below the normal floor wins immediately. Retain only
        // the highest affordable OPP instead of producing an empty range.
        .unwrap_or_else(|| capped.len().saturating_sub(1));
    capped.into_iter().skip(first_at_floor).collect()
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
        .opp_at_or_below(model.sweet_frequency_hz)
        .or_else(|| model.opps().first());
    let maximum = model
        .opp_at_or_below(cap_hz)
        .or_else(|| model.opps().last());
    let (Some(sweet), Some(maximum)) = (sweet, maximum) else {
        return requested_floor_hz.min(cap_hz);
    };
    let power_span = maximum.power_mw_per_core - sweet.power_mw_per_core;
    if power_span <= f64::EPSILON {
        return requested_floor_hz.min(cap_hz);
    }
    model
        .opps()
        .iter()
        .take_while(|opp| opp.frequency_hz <= requested_floor_hz.min(cap_hz))
        .filter(|opp| {
            opp.frequency_hz <= model.sweet_frequency_hz
                || (opp.power_mw_per_core - sweet.power_mw_per_core) / power_span <= progress
        })
        .last()
        .map_or(sweet.frequency_hz.min(cap_hz), |opp| opp.frequency_hz)
}

fn elapsed_weighted_ema(previous: f64, sample: f64, elapsed_ms: u64, tau_ms: u64) -> f64 {
    if tau_ms == 0 {
        return sample;
    }
    let alpha =
        1.0 - (-(duration_ms_as_seconds(elapsed_ms) / duration_ms_as_seconds(tau_ms))).exp();
    previous + alpha * (sample - previous)
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
        CpuEnergyModelConfig, CpuId, CpuSet, CpuTargetPolicy, FrequencyPolicy, GovernorRollout,
        MeasuredOppConfig, TargetId,
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
        GovernorConfig {
            rollout: GovernorRollout::Shadow,
            ..GovernorConfig::default()
        }
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
                plain_frequency_hz: Hertz(1_000),
                // `free` is a selection hint, not a curve boundary, and may
                // therefore lie above `plain`.
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
    fn documented_demand_formula_differs_from_legacy_multiplier() {
        assert!((effective_demand(0.5, 0.2, 0.1) - 0.65).abs() < f64::EPSILON);
        assert_close(effective_demand(1.0, 0.5, 0.5), 1.0);
    }

    #[test]
    fn prediction_uses_raw_delta_and_bypasses_ramp() {
        let mut state = DemandState::default();
        let config = governor();
        let _ = state.update(0.20, 20, &config);
        let result = state.update(0.50, 20, &config);
        assert!(result.prediction_bypassed_ramp);
        assert_close(result.predicted, 0.80);
        assert_close(result.selected, result.predicted);
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
    fn energy_bucket_switches_budgets_with_internal_point_nine_one_point_one_hysteresis() {
        let mut state = EnergyBucketState::default();
        let budget = PowerBudgetConfig {
            slow_limit_power_mw: 1_000,
            fast_limit_power_mw: 2_000,
            fast_limit_capacity_mj: 100,
            fast_limit_recover_scale: 1.0,
        };
        let inside_upper_band = state.update(2_050.0, 100, budget, true);
        assert_close(inside_upper_band.remaining_mj, -5.0);
        assert_close(inside_upper_band.selected_limit_power_mw, 2_000.0);

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
    fn package_allocator_prefers_the_best_increment_and_clamps_floor_to_cap() {
        let efficient = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 100,
                        power_mw_per_core: 100,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(2_000),
                        relative_capacity: 250,
                        power_mw_per_core: 200,
                    },
                ],
            },
            &[Hertz(1_000), Hertz(2_000)],
        )
        .expect("model");
        let inefficient = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 100,
                        power_mw_per_core: 100,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(2_000),
                        relative_capacity: 150,
                        power_mw_per_core: 300,
                    },
                ],
            },
            &[Hertz(1_000), Hertz(2_000)],
        )
        .expect("model");
        let plans = allocate_package_budget(
            &[
                BudgetCluster {
                    model: &efficient,
                    core_count: 1,
                    demanded_performance_per_core: 250.0,
                    minimum_hz: Hertz(1_000),
                    safety_cap_hz: Hertz(2_000),
                },
                BudgetCluster {
                    model: &inefficient,
                    core_count: 1,
                    demanded_performance_per_core: 150.0,
                    minimum_hz: Hertz(1_000),
                    safety_cap_hz: Hertz(2_000),
                },
            ],
            300.0,
        );
        assert_eq!(
            plans,
            vec![
                BudgetedOpp {
                    floor_hz: Hertz(2_000),
                    cap_hz: Hertz(2_000),
                },
                BudgetedOpp {
                    floor_hz: Hertz(1_000),
                    cap_hz: Hertz(1_000),
                },
            ]
        );
    }

    #[test]
    fn package_allocator_keeps_the_normal_floor_but_allows_a_lower_safety_cap() {
        let model = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(500),
                        relative_capacity: 50,
                        power_mw_per_core: 50,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 100,
                        power_mw_per_core: 100,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_500),
                        relative_capacity: 140,
                        power_mw_per_core: 200,
                    },
                ],
            },
            &[Hertz(500), Hertz(1_000), Hertz(1_500)],
        )
        .expect("model");
        let normal = allocate_package_budget(
            &[BudgetCluster {
                model: &model,
                core_count: 1,
                demanded_performance_per_core: 0.0,
                minimum_hz: Hertz(1_000),
                safety_cap_hz: Hertz(1_500),
            }],
            50.0,
        );
        assert_eq!(
            normal,
            vec![BudgetedOpp {
                floor_hz: Hertz(1_000),
                cap_hz: Hertz(1_000),
            }]
        );

        let safety_limited = allocate_package_budget(
            &[BudgetCluster {
                model: &model,
                core_count: 1,
                demanded_performance_per_core: 140.0,
                minimum_hz: Hertz(1_000),
                safety_cap_hz: Hertz(750),
            }],
            f64::INFINITY,
        );
        assert_eq!(
            safety_limited,
            vec![BudgetedOpp {
                floor_hz: Hertz(500),
                cap_hz: Hertz(500),
            }]
        );
    }

    #[test]
    fn package_allocator_matches_a_small_bruteforce_oracle() {
        let little = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 100,
                        power_mw_per_core: 100,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(2_000),
                        relative_capacity: 170,
                        power_mw_per_core: 200,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(3_000),
                        relative_capacity: 220,
                        power_mw_per_core: 400,
                    },
                ],
            },
            &[Hertz(1_000), Hertz(2_000), Hertz(3_000)],
        )
        .expect("little model");
        let big = EnergyModel::from_config(
            &CpuEnergyModelConfig::MeasuredOppV1 {
                points: vec![
                    MeasuredOppConfig {
                        frequency_hz: Hertz(1_000),
                        relative_capacity: 180,
                        power_mw_per_core: 200,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(2_000),
                        relative_capacity: 300,
                        power_mw_per_core: 400,
                    },
                    MeasuredOppConfig {
                        frequency_hz: Hertz(3_000),
                        relative_capacity: 380,
                        power_mw_per_core: 800,
                    },
                ],
            },
            &[Hertz(1_000), Hertz(2_000), Hertz(3_000)],
        )
        .expect("big model");
        let clusters = [
            BudgetCluster {
                model: &little,
                core_count: 2,
                demanded_performance_per_core: f64::INFINITY,
                minimum_hz: Hertz(1_000),
                safety_cap_hz: Hertz(3_000),
            },
            BudgetCluster {
                model: &big,
                core_count: 1,
                demanded_performance_per_core: f64::INFINITY,
                minimum_hz: Hertz(1_000),
                safety_cap_hz: Hertz(3_000),
            },
        ];

        for budget in [400.0, 600.0, 800.0, 1_000.0, 1_200.0, 1_600.0] {
            let selected = allocate_package_budget(&clusters, budget);
            let selected_performance = clusters
                .iter()
                .zip(&selected)
                .map(|(cluster, plan)| {
                    cluster
                        .model
                        .opps()
                        .iter()
                        .find(|opp| opp.frequency_hz == plan.cap_hz)
                        .expect("selected OPP")
                        .performance
                        * f64::from(cluster.core_count)
                })
                .sum::<f64>();
            let oracle = little
                .opps()
                .iter()
                .flat_map(|little_opp| {
                    big.opps().iter().map(move |big_opp| {
                        let power = little_opp.power_mw_per_core * 2.0 + big_opp.power_mw_per_core;
                        let performance = little_opp.performance * 2.0 + big_opp.performance;
                        (power, performance)
                    })
                })
                .filter(|(power, _)| *power <= budget)
                .map(|(_, performance)| performance)
                .fold(0.0_f64, f64::max);
            assert_close(selected_performance, oracle);
        }
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
    fn stateful_transition_enforces_budget_but_burst_never_bypasses_safety() {
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
        let loads = BTreeMap::from([(id.clone(), 1.0)]);
        let empty_frequencies = BTreeMap::new();
        let empty_caps = BTreeMap::new();
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
        let constrained = transition_governor(
            &GovernorState::default(),
            &GovernorInput {
                timestamp: MonotonicMillis(0),
                targets: &targets,
                raw_loads: &loads,
                observed_frequencies: &empty_frequencies,
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
        assert_eq!(constrained.limits[&id].max, Hertz(1_000));

        let thermal_caps = BTreeMap::from([(id.clone(), Hertz(2_000))]);
        let burst = transition_governor(
            &constrained.next_state,
            &GovernorInput {
                timestamp: MonotonicMillis(20),
                targets: &targets,
                raw_loads: &loads,
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
        .expect("burst transition");
        assert!(burst.diagnostics.bypassed_power_budget);
        assert_eq!(
            burst.limits[&id],
            FrequencyLimits {
                min: Hertz(2_000),
                max: Hertz(2_000),
            }
        );
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
            ema_time_constant_ms: 0,
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
