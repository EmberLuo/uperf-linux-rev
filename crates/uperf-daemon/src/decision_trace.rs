//! Bounded, process-local policy and reconciliation timeline.
//!
//! The trace is deliberately observational: publishing or reading it never
//! enters the runtime mutation lane and never performs filesystem I/O.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
    time::Duration,
};

use uperf_api::{
    DecisionFrequency, DecisionScalar, DecisionTraceEntry, DecisionTraceEntryV2,
    GovernorDiagnosticsStatus,
};
use uperf_core::{AppliedState, DesiredPlan, FrequencyLimits, ScalarSettingValue, TargetId};

pub(crate) const TRACE_CAPACITY: usize = 1_024;
pub(crate) const MAX_TRACE_PAGE: u32 = uperf_api::MAX_DECISION_TRACE_PAGE;

#[derive(Debug, Default)]
struct TraceState {
    next_decision_id: u64,
    next_reconcile_id: u64,
    entries: VecDeque<DecisionTraceEntry>,
    entries_v2: VecDeque<DecisionTraceEntryV2>,
}

/// Diagnostics frozen when a worker job is submitted. The reconciler combines
/// this policy-time snapshot with its later verified actuator result.
#[derive(Clone, Debug, Default)]
pub(crate) struct DecisionTraceContext {
    pub trigger_source: String,
    pub trigger_monotonic_ms: u64,
    pub governor: GovernorDiagnosticsStatus,
}

/// Thread-safe ring shared by the runtime, blocking reconciler, and D-Bus
/// service.
#[derive(Debug, Default)]
pub(crate) struct DecisionTraceStore {
    state: Mutex<TraceState>,
}

impl DecisionTraceStore {
    /// Record one completed reconciliation without performing durable I/O.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_reconcile(
        &self,
        monotonic_ms: u64,
        duration: Duration,
        desired: &DesiredPlan,
        applied: &AppliedState,
        frequency_attempted: bool,
        scheduler_attempted: bool,
        frequency_error: Option<&str>,
        scheduler_error: Option<&str>,
        context: &DecisionTraceContext,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_decision_id = state.next_decision_id.saturating_add(1);
        state.next_reconcile_id = state.next_reconcile_id.saturating_add(1);
        let error = format_errors(frequency_error, scheduler_error);
        let entry = DecisionTraceEntry {
            decision_id: state.next_decision_id,
            reconcile_id: state.next_reconcile_id,
            monotonic_ms,
            duration_us: u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            generation: desired.generation,
            profile: desired.effective_profile.to_string(),
            scene: desired.dominant_scene.to_string(),
            frequency_attempted,
            scheduler_attempted,
            desired_frequencies: frequency_snapshot(&desired.frequencies),
            applied_frequencies: frequency_snapshot(&applied.frequencies),
            success: error.is_empty(),
            error,
        };
        let entry_v2 = DecisionTraceEntryV2 {
            base: entry.clone(),
            trigger_source: context.trigger_source.clone(),
            trigger_monotonic_ms: context.trigger_monotonic_ms,
            verified_apply_latency_us: monotonic_ms
                .saturating_sub(context.trigger_monotonic_ms)
                .saturating_mul(1_000)
                .saturating_add(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)),
            governor: context.governor.clone(),
            desired_scalars: scalar_snapshot(&desired.scalars),
            applied_scalars: scalar_snapshot(&applied.scalars),
        };
        tracing::debug!(
            source = "reconciler",
            event = "decision-applied",
            decision_id = entry.decision_id,
            reconcile_id = entry.reconcile_id,
            generation = entry.generation,
            profile = %entry.profile,
            scene = %entry.scene,
            frequency_attempted = entry.frequency_attempted,
            scheduler_attempted = entry.scheduler_attempted,
            duration_us = entry.duration_us,
            success = entry.success,
            desired = ?entry.desired_frequencies,
            applied = ?entry.applied_frequencies,
            desired_scalars = ?desired.scalars,
            applied_scalars = ?applied.scalars,
            error = %entry.error,
            "policy decision reconciliation completed"
        );
        if state.entries.len() == TRACE_CAPACITY {
            state.entries.pop_front();
        }
        state.entries.push_back(entry);
        if state.entries_v2.len() == TRACE_CAPACITY {
            state.entries_v2.pop_front();
        }
        state.entries_v2.push_back(entry_v2);
    }

    /// Return entries with IDs strictly greater than `after_id`, oldest first.
    pub(crate) fn page(&self, after_id: u64, limit: u32) -> Vec<DecisionTraceEntry> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .entries
            .iter()
            .filter(|entry| entry.decision_id > after_id)
            .take(limit as usize)
            .cloned()
            .collect()
    }

    /// Return extended entries with the same IDs and retention as v1.
    pub(crate) fn page_v2(&self, after_id: u64, limit: u32) -> Vec<DecisionTraceEntryV2> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .entries_v2
            .iter()
            .filter(|entry| entry.base.decision_id > after_id)
            .take(limit as usize)
            .cloned()
            .collect()
    }
}

fn frequency_snapshot(frequencies: &BTreeMap<TargetId, FrequencyLimits>) -> Vec<DecisionFrequency> {
    frequencies
        .iter()
        .map(|(target, limits)| DecisionFrequency {
            target_id: target.to_string(),
            min_hz: limits.min.get(),
            max_hz: limits.max.get(),
        })
        .collect()
}

pub(crate) fn scalar_snapshot(
    scalars: &BTreeMap<TargetId, ScalarSettingValue>,
) -> Vec<DecisionScalar> {
    scalars
        .iter()
        .map(|(target, value)| DecisionScalar {
            target_id: target.to_string(),
            value_json: serde_json::to_string(value)
                .expect("validated scalar setting always serializes"),
        })
        .collect()
}

fn format_errors(frequency_error: Option<&str>, scheduler_error: Option<&str>) -> String {
    match (frequency_error, scheduler_error) {
        (None, None) => String::new(),
        (Some(error), None) => format!("frequency: {error}"),
        (None, Some(error)) => format!("scheduler: {error}"),
        (Some(frequency), Some(scheduler)) => {
            format!("frequency: {frequency}; scheduler: {scheduler}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use uperf_core::{
        AppliedState, DesiredPlan, FrequencyLimits, Hertz, ProfileId, ScalarSettingValue, Scene,
        TargetId,
    };

    use super::*;

    fn plan(generation: u64) -> DesiredPlan {
        DesiredPlan {
            generation,
            effective_profile: ProfileId::Balance,
            dominant_scene: Scene::Touch,
            frequencies: BTreeMap::from([(
                TargetId::new("cpu.policy0").expect("target"),
                FrequencyLimits::new(Hertz::new(300_000_000), Hertz::new(1_800_000_000))
                    .expect("limits"),
            )]),
            scalars: BTreeMap::new(),
            tasks: BTreeMap::new(),
        }
    }

    #[test]
    fn page_is_exclusive_ordered_and_bounded() {
        let store = DecisionTraceStore::default();
        for generation in 1..=6 {
            let desired = plan(generation);
            store.record_reconcile(
                generation * 10,
                Duration::from_micros(generation),
                &desired,
                &AppliedState {
                    generation,
                    frequencies: desired.frequencies.clone(),
                    scalars: BTreeMap::new(),
                    tasks: BTreeMap::new(),
                },
                true,
                false,
                None,
                None,
                &DecisionTraceContext::default(),
            );
        }

        let page = store.page(2, 2);
        assert_eq!(
            page.iter()
                .map(|entry| entry.decision_id)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        assert!(page.iter().all(|entry| entry.success));
        assert!(store.page(6, MAX_TRACE_PAGE).is_empty());
    }

    #[test]
    fn ring_drops_oldest_entries_and_keeps_ids_monotonic() {
        let store = DecisionTraceStore::default();
        for generation in 1..=u64::try_from(TRACE_CAPACITY + 3).expect("capacity") {
            store.record_reconcile(
                generation,
                Duration::ZERO,
                &plan(generation),
                &AppliedState::default(),
                true,
                true,
                Some("readback"),
                Some("permission"),
                &DecisionTraceContext::default(),
            );
        }

        let retained = store.page(0, u32::MAX);
        assert_eq!(retained.len(), TRACE_CAPACITY);
        assert_eq!(retained.first().expect("first").decision_id, 4);
        let last = retained.last().expect("last");
        assert_eq!(
            last.decision_id,
            u64::try_from(TRACE_CAPACITY + 3).expect("capacity")
        );
        assert!(!last.success);
        assert_eq!(last.error, "frequency: readback; scheduler: permission");
    }

    #[test]
    fn extended_trace_freezes_trigger_diagnostics_and_verified_latency() {
        let store = DecisionTraceStore::default();
        let mut desired = plan(7);
        desired.scalars.insert(
            TargetId::new("scalar.ddr").expect("target"),
            ScalarSettingValue::Integer(800),
        );
        let applied = AppliedState {
            generation: 7,
            frequencies: desired.frequencies.clone(),
            scalars: desired.scalars.clone(),
            tasks: BTreeMap::new(),
        };
        let context = DecisionTraceContext {
            trigger_source: "desktop-input".to_owned(),
            trigger_monotonic_ms: 10,
            governor: GovernorDiagnosticsStatus {
                available: true,
                effective_budget_mw: 4_000,
                ..GovernorDiagnosticsStatus::default()
            },
        };

        store.record_reconcile(
            25,
            Duration::from_micros(1_500),
            &desired,
            &applied,
            true,
            false,
            None,
            None,
            &context,
        );

        let page = store.page_v2(0, MAX_TRACE_PAGE);
        assert_eq!(page.len(), 1);
        let entry = &page[0];
        assert_eq!(entry.base.decision_id, store.page(0, 1)[0].decision_id);
        assert_eq!(entry.trigger_source, "desktop-input");
        assert_eq!(entry.trigger_monotonic_ms, 10);
        assert_eq!(entry.verified_apply_latency_us, 16_500);
        assert_eq!(entry.governor.effective_budget_mw, 4_000);
        assert_eq!(entry.desired_scalars, entry.applied_scalars);
        assert_eq!(
            entry.desired_scalars[0].value_json,
            r#"{"kind":"integer","value":800}"#
        );
    }
}
