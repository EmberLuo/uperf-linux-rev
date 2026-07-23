//! Blocking reconciliation worker.
//!
//! Every operation in this module may perform filesystem I/O, durable `fsync`,
//! process scheduler syscalls, or a blocking systemd D-Bus call.  The runtime
//! therefore executes [`run`] with `tokio::task::spawn_blocking`; the single
//! state reducer only consumes the resulting snapshot.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use uperf_actuator::{
    ActuatorError, FrequencyActuator, FrequencyRequest, TaskRequest, UnitRequest,
};
use uperf_core::{
    AppliedState, AppliedTargetState, DesiredPlan, Hertz, MonotonicMillis, PolicyEngine,
    ProcessIdentity, ProcessInfo, SchedulingClass, TargetId, TaskPlan,
};
use uperf_platform::{
    PlatformError, ProcessSchedulingState, RuntimePlatform, SchedulingPolicy, SystemdUnitProperties,
};

/// One immutable state snapshot submitted to the serialized blocking worker.
pub(crate) struct ReconcileJob {
    pub actuator: Arc<FrequencyActuator>,
    pub environment: Arc<dyn RuntimePlatform>,
    pub policy_engine: PolicyEngine,
    pub active_workload: Option<ProcessInfo>,
    pub desired: DesiredPlan,
    pub applied: AppliedState,
    pub applied_units: BTreeMap<String, SystemdUnitProperties>,
    pub reconcile_frequencies: bool,
    pub reconcile_scheduler: bool,
    pub mutation_gate: Arc<Mutex<()>>,
    pub frequency_safety: Arc<FrequencySafetyFence>,
}

/// Actual state and independent failure domains returned to the reducer.
pub(crate) struct ReconcileOutcome {
    pub desired: DesiredPlan,
    pub applied: AppliedState,
    pub applied_units: BTreeMap<String, SystemdUnitProperties>,
    pub frequency_attempted: bool,
    pub frequency_error: Option<String>,
    pub scheduler_attempted: bool,
    pub scheduler_error: Option<String>,
    pub scheduler_warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesiredCgroup {
    unit: String,
    properties: SystemdUnitProperties,
}

struct SchedulerResolution {
    tasks: BTreeMap<ProcessIdentity, TaskPlan>,
    cgroup: Option<DesiredCgroup>,
    warning: Option<String>,
}

#[derive(Debug, Default)]
struct FrequencySafetyEnvelope {
    generation: u64,
    upper_caps: BTreeMap<TargetId, Hertz>,
}

/// Linearization fence between safety-envelope changes and frequency writes.
///
/// A worker retains this lock across durable journaling, mutation, rollback and
/// readback. An envelope update therefore either happens before a transaction,
/// in which case that transaction is clamped to the new cap, or after the
/// transaction has completely finished. There is no point at which a newly
/// published safety generation can be followed by an old-generation write.
#[derive(Debug, Default)]
pub(crate) struct FrequencySafetyFence {
    envelope: Mutex<FrequencySafetyEnvelope>,
}

impl FrequencySafetyFence {
    pub(crate) fn replace_upper_caps(
        &self,
        upper_caps: BTreeMap<TargetId, Hertz>,
    ) -> Result<u64, String> {
        let mut envelope = self
            .envelope
            .lock()
            .map_err(|_| "frequency safety fence was poisoned".to_owned())?;
        envelope.generation = envelope.generation.saturating_add(1);
        envelope.upper_caps = upper_caps;
        Ok(envelope.generation)
    }

    fn with_upper_caps<T>(
        &self,
        operation: impl FnOnce(&BTreeMap<TargetId, Hertz>) -> T,
    ) -> Result<T, String> {
        let envelope = self
            .envelope
            .lock()
            .map_err(|_| "frequency safety fence was poisoned".to_owned())?;
        Ok(operation(&envelope.upper_caps))
    }
}

/// Execute one reconciliation snapshot.
///
/// A daemon-owned gate spans frequency, task and unit transactions so a
/// suspend/shutdown restoration cannot interleave between those independently
/// journaled actuator calls.
pub(crate) fn run(job: &ReconcileJob) -> ReconcileOutcome {
    let mut outcome = ReconcileOutcome {
        desired: job.desired.clone(),
        applied: job.applied.clone(),
        applied_units: job.applied_units.clone(),
        frequency_attempted: job.reconcile_frequencies,
        frequency_error: None,
        scheduler_attempted: job.reconcile_scheduler,
        scheduler_error: None,
        scheduler_warning: None,
    };
    let Ok(gate) = job.mutation_gate.lock() else {
        let message = "runtime mutation gate was poisoned".to_owned();
        if job.reconcile_frequencies {
            outcome.frequency_error = Some(message.clone());
        }
        if job.reconcile_scheduler {
            outcome.scheduler_error = Some(message);
        }
        return outcome;
    };

    let scheduler = if job.reconcile_scheduler {
        match resolve_scheduler(
            job.environment.as_ref(),
            job.actuator.as_ref(),
            &job.policy_engine,
            job.active_workload.as_ref(),
        ) {
            Ok(resolution) => {
                outcome.desired.tasks.clone_from(&resolution.tasks);
                outcome.scheduler_warning.clone_from(&resolution.warning);
                Some(resolution)
            }
            Err(error) => {
                outcome.scheduler_error = Some(error);
                None
            }
        }
    } else {
        None
    };

    if job.reconcile_frequencies {
        match job.frequency_safety.with_upper_caps(|upper_caps| {
            reconcile_frequencies(
                job.actuator.as_ref(),
                job.environment.monotonic_millis(),
                &outcome.desired,
                upper_caps,
                &mut outcome.applied,
            )
        }) {
            Ok(Ok(())) => {}
            Ok(Err(error)) | Err(error) => outcome.frequency_error = Some(error),
        }
    }

    if let Some(resolution) = scheduler
        && let Err(error) = reconcile_scheduler(
            job.actuator.as_ref(),
            job.active_workload.as_ref(),
            &outcome.desired,
            resolution.cgroup,
            &mut outcome.applied,
            &mut outcome.applied_units,
            &mut outcome.scheduler_warning,
        )
    {
        outcome.scheduler_error = Some(error);
    }

    drop(gate);
    outcome
}

fn reconcile_frequencies(
    actuator: &FrequencyActuator,
    now: MonotonicMillis,
    desired: &DesiredPlan,
    upper_caps: &BTreeMap<TargetId, Hertz>,
    applied: &mut AppliedState,
) -> Result<(), String> {
    let mut effective = desired
        .frequencies
        .iter()
        .map(|(id, plan)| (id.clone(), plan.limits))
        .collect::<BTreeMap<_, _>>();
    for (id, cap) in upper_caps {
        if let Some(limits) = effective.get_mut(id) {
            *limits = apply_upper_cap(*limits, *cap);
        } else {
            let current = actuator
                .read_limits(id)
                .map_err(|error| format!("read safety-capped frequency target {id}: {error}"))?;
            effective.insert(id.clone(), apply_upper_cap(current, *cap));
        }
    }

    let removed = applied
        .frequencies
        .keys()
        .filter(|id| !effective.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    actuator
        .restore_targets(&removed)
        .map_err(|error| format!("restore inactive frequency targets: {error}"))?;
    for id in removed {
        applied.frequencies.remove(&id);
    }

    let mut requests = Vec::new();
    for (id, limits) in &effective {
        let current = actuator
            .read_limits(id)
            .map_err(|error| format!("read frequency target {id}: {error}"))?;
        if current == *limits {
            applied.frequencies.insert(
                id.clone(),
                AppliedTargetState {
                    limits: current,
                    generation: desired.generation,
                    verified_at: now,
                },
            );
        } else {
            requests.push(FrequencyRequest {
                target: id.clone(),
                limits: *limits,
            });
        }
    }
    let result = actuator
        .apply_batch(&requests)
        .map_err(|error| format!("apply frequency batch: {error}"))?;
    for (id, limits) in result.applied {
        applied.frequencies.insert(
            id,
            AppliedTargetState {
                limits,
                generation: desired.generation,
                verified_at: now,
            },
        );
    }
    Ok(())
}

fn apply_upper_cap(limits: uperf_core::FrequencyLimits, cap: Hertz) -> uperf_core::FrequencyLimits {
    let max = limits.max.min(cap);
    uperf_core::FrequencyLimits {
        min: limits.min.min(max),
        max,
    }
}

fn resolve_scheduler(
    environment: &dyn RuntimePlatform,
    actuator: &FrequencyActuator,
    policy_engine: &PolicyEngine,
    workload: Option<&ProcessInfo>,
) -> Result<SchedulerResolution, String> {
    if !policy_engine.config().scheduler.enabled {
        return Ok(SchedulerResolution {
            tasks: BTreeMap::new(),
            cgroup: None,
            warning: None,
        });
    }
    let Some(workload) = workload else {
        return Ok(SchedulerResolution {
            tasks: BTreeMap::new(),
            cgroup: None,
            warning: None,
        });
    };

    let first_listing = environment
        .list_threads(workload.identity.pid)
        .map_err(|error| format!("list workload threads: {error}"))?;
    let mut threads = Vec::with_capacity(first_listing.len());
    for thread_id in first_listing {
        if thread_id == workload.identity.pid {
            threads.push(workload.clone());
            continue;
        }
        match environment.process_identity(thread_id) {
            Ok(thread) => threads.push(thread),
            Err(PlatformError::Disappeared(_)) => {}
            Err(error) => {
                return Err(format!(
                    "resolve workload thread {}: {error}",
                    thread_id.get()
                ));
            }
        }
    }
    let confirmed = environment
        .list_threads(workload.identity.pid)
        .map_err(|error| format!("recheck workload threads: {error}"))?
        .into_iter()
        .collect::<BTreeSet<_>>();
    if !confirmed.contains(&workload.identity.pid) {
        return Err("active workload exited during thread classification".to_owned());
    }
    threads.retain(|thread| confirmed.contains(&thread.identity.pid));

    let mut decision = policy_engine
        .evaluate_scheduler(workload, &threads)
        .map_err(|error| error.to_string())?;
    let online = environment
        .online_cpus()
        .map_err(|error| format!("read online CPU mask: {error}"))?;
    let mut warning = None;
    decision.tasks.retain(|identity, plan| {
        let Some(affinity) = &plan.affinity else {
            return true;
        };
        let available = affinity.intersection(&online);
        if available.is_empty() {
            append_warning(
                &mut warning,
                format!(
                    "task {} has no configured CPU online; leaving its affinity unchanged",
                    identity.pid.get()
                ),
            );
            false
        } else {
            plan.affinity = Some(available);
            true
        }
    });

    let (cgroup, cgroup_warning) = decision.cgroup_class.as_deref().map_or_else(
        || (None, None),
        |class_id| {
            resolve_cgroup(
                actuator,
                policy_engine,
                workload.identity,
                class_id,
                &online,
            )
        },
    );
    if let Some(cgroup_warning) = cgroup_warning {
        append_warning(&mut warning, cgroup_warning);
    }
    Ok(SchedulerResolution {
        tasks: decision.tasks,
        cgroup,
        warning,
    })
}

fn resolve_cgroup(
    actuator: &FrequencyActuator,
    policy_engine: &PolicyEngine,
    workload: ProcessIdentity,
    class_id: &str,
    online: &uperf_core::CpuSet,
) -> (Option<DesiredCgroup>, Option<String>) {
    let Some(class) = policy_engine
        .config()
        .scheduler
        .cgroup_classes
        .iter()
        .find(|class| class.id == class_id)
    else {
        return (None, Some(format!("unknown cgroup class {class_id}")));
    };
    let unit = match actuator.unit_for_process(workload) {
        Ok(Some(unit)) => unit,
        Ok(None) => {
            return (None, Some("active workload has no systemd unit".to_owned()));
        }
        Err(error) => {
            return (None, Some(format!("cannot resolve workload unit: {error}")));
        }
    };
    if let Some(warning) = unit_ownership_warning(actuator, workload, &unit) {
        return (None, Some(warning));
    }
    let allowed_cpus = class.allowed_cpus.intersection(online);
    if allowed_cpus.is_empty() {
        return (
            None,
            Some(format!(
                "cgroup class {class_id} has no configured CPU online; leaving {unit} unchanged"
            )),
        );
    }
    (
        Some(DesiredCgroup {
            unit,
            properties: SystemdUnitProperties {
                cpu_weight: Some(u64::from(class.cpu_weight)),
                allowed_cpus: Some(allowed_cpus),
            },
        }),
        None,
    )
}

fn append_warning(existing: &mut Option<String>, warning: String) {
    if let Some(existing) = existing {
        existing.push_str("; ");
        existing.push_str(&warning);
    } else {
        *existing = Some(warning);
    }
}

fn unit_ownership_warning(
    actuator: &FrequencyActuator,
    workload: ProcessIdentity,
    unit: &str,
) -> Option<String> {
    let mut members = match actuator.unit_processes(unit) {
        Ok(members) => members,
        Err(error) => return Some(format!("cannot enumerate {unit}: {error}")),
    };
    members.sort_unstable();
    members.dedup();
    if members != [workload.pid] {
        return Some(format!(
            "refusing shared or ambiguous unit {unit}; members are {members:?}"
        ));
    }
    match actuator.unit_for_process(workload) {
        Ok(Some(current)) if current == unit => None,
        Ok(_) => Some(format!(
            "workload unit changed while verifying ownership of {unit}"
        )),
        Err(error) => Some(format!("cannot revalidate ownership of {unit}: {error}")),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the worker receives explicit snapshots instead of borrowing mutable reducer state"
)]
fn reconcile_scheduler(
    actuator: &FrequencyActuator,
    workload: Option<&ProcessInfo>,
    desired: &DesiredPlan,
    desired_cgroup: Option<DesiredCgroup>,
    applied: &mut AppliedState,
    applied_units: &mut BTreeMap<String, SystemdUnitProperties>,
    warning: &mut Option<String>,
) -> Result<(), String> {
    let removed_tasks = applied
        .tasks
        .keys()
        .filter(|identity| !desired.tasks.contains_key(*identity))
        .copied()
        .collect::<Vec<_>>();
    actuator
        .restore_tasks(&removed_tasks)
        .map_err(|error| format!("restore inactive tasks: {error}"))?;
    for identity in removed_tasks {
        applied.tasks.remove(&identity);
    }

    let mut requests = Vec::new();
    let mut verified = BTreeMap::new();
    for (identity, plan) in &desired.tasks {
        let current = match actuator.read_task_state(*identity) {
            Ok(current) => current,
            Err(error) if process_disappeared(&error) => {
                applied.tasks.remove(identity);
                continue;
            }
            Err(error) => {
                return Err(format!("read task {}: {error}", identity.pid.get()));
            }
        };
        if task_plan_matches(&current, plan) {
            verified.insert(*identity, scheduling_state_as_plan(&current));
        } else {
            requests.push(TaskRequest {
                identity: *identity,
                desired: apply_task_plan(current, plan),
            });
        }
    }
    let result = actuator
        .apply_tasks(&requests)
        .map_err(|error| format!("apply task batch: {error}"))?;
    for (identity, scheduling) in result.applied {
        verified.insert(identity, scheduling_state_as_plan(&scheduling));
    }
    applied.tasks.extend(verified);

    let desired_unit = desired_cgroup.as_ref().map(|intent| intent.unit.as_str());
    let removed_units = applied_units
        .keys()
        .filter(|unit| Some(unit.as_str()) != desired_unit)
        .cloned()
        .collect::<Vec<_>>();
    actuator
        .restore_units(&removed_units)
        .map_err(|error| format!("restore inactive systemd units: {error}"))?;
    for unit in removed_units {
        applied_units.remove(&unit);
    }

    if let Some(intent) = desired_cgroup {
        let Some(workload) = workload else {
            return Ok(());
        };
        if let Some(ownership_warning) =
            unit_ownership_warning(actuator, workload.identity, &intent.unit)
        {
            *warning = Some(ownership_warning);
            if applied_units.contains_key(&intent.unit) {
                actuator
                    .restore_units(std::slice::from_ref(&intent.unit))
                    .map_err(|error| format!("restore unit after ownership changed: {error}"))?;
                applied_units.remove(&intent.unit);
            }
            return Ok(());
        }
        if applied_units.get(&intent.unit) != Some(&intent.properties) {
            let result = actuator
                .apply_units(&[UnitRequest {
                    unit: intent.unit.clone(),
                    desired: intent.properties,
                }])
                .map_err(|error| format!("apply systemd unit policy: {error}"))?;
            applied_units.extend(result.applied);
        }
    }
    Ok(())
}

fn process_disappeared(error: &ActuatorError) -> bool {
    matches!(
        error,
        ActuatorError::ProcessIdentityChanged(_)
            | ActuatorError::Platform(PlatformError::Disappeared(_))
    )
}

fn task_plan_matches(actual: &ProcessSchedulingState, desired: &TaskPlan) -> bool {
    desired
        .affinity
        .as_ref()
        .is_none_or(|affinity| actual.affinity == *affinity)
        && desired.nice.is_none_or(|nice| actual.nice == nice)
        && desired.scheduling_class.is_none_or(|class| {
            actual.policy
                == match class {
                    SchedulingClass::Other => SchedulingPolicy::Other,
                    SchedulingClass::Batch => SchedulingPolicy::Batch,
                    SchedulingClass::Idle => SchedulingPolicy::Idle,
                }
        })
        && desired.uclamp.is_none_or(|limits| {
            actual.uclamp_min == Some(limits.min) && actual.uclamp_max == Some(limits.max)
        })
}

fn apply_task_plan(
    mut current: ProcessSchedulingState,
    desired: &TaskPlan,
) -> ProcessSchedulingState {
    if let Some(affinity) = &desired.affinity {
        current.affinity.clone_from(affinity);
    }
    if let Some(nice) = desired.nice {
        current.nice = nice;
    }
    if let Some(class) = desired.scheduling_class {
        current.policy = match class {
            SchedulingClass::Other => SchedulingPolicy::Other,
            SchedulingClass::Batch => SchedulingPolicy::Batch,
            SchedulingClass::Idle => SchedulingPolicy::Idle,
        };
    }
    if let Some(limits) = desired.uclamp {
        current.uclamp_min = Some(limits.min);
        current.uclamp_max = Some(limits.max);
    }
    current
}

fn scheduling_state_as_plan(state: &ProcessSchedulingState) -> TaskPlan {
    let uclamp = state
        .uclamp_min
        .zip(state.uclamp_max)
        .map(|(min, max)| uperf_core::UclampLimits { min, max });
    TaskPlan {
        affinity: Some(state.affinity.clone()),
        nice: Some(state.nice),
        scheduling_class: Some(match state.policy {
            SchedulingPolicy::Other => SchedulingClass::Other,
            SchedulingPolicy::Batch => SchedulingClass::Batch,
            SchedulingPolicy::Idle => SchedulingClass::Idle,
        }),
        uclamp,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };

    use uperf_core::{FrequencyLimits, Hertz, TargetId};

    use super::{FrequencySafetyFence, apply_upper_cap};

    #[test]
    fn upper_cap_lowers_an_inverted_minimum_to_the_safe_maximum() {
        assert_eq!(
            apply_upper_cap(
                FrequencyLimits {
                    min: Hertz::new(2_000),
                    max: Hertz::new(3_000),
                },
                Hertz::new(1_500),
            ),
            FrequencyLimits {
                min: Hertz::new(1_500),
                max: Hertz::new(1_500),
            }
        );
    }

    #[test]
    fn safety_update_is_linearized_after_an_existing_transaction() {
        let fence = Arc::new(FrequencySafetyFence::default());
        let target = TargetId::new("cpu.policy0").unwrap();
        fence
            .replace_upper_caps([(target.clone(), Hertz::new(2_000))].into())
            .unwrap();

        let (worker_entered_tx, worker_entered_rx) = mpsc::channel();
        let (release_worker_tx, release_worker_rx) = mpsc::channel();
        let worker_fence = fence.clone();
        let worker_target = target.clone();
        let worker = thread::spawn(move || {
            worker_fence
                .with_upper_caps(|caps| {
                    worker_entered_tx.send(caps[&worker_target]).unwrap();
                    release_worker_rx.recv().unwrap();
                })
                .unwrap();
        });
        assert_eq!(worker_entered_rx.recv().unwrap(), Hertz::new(2_000));

        let (updated_tx, updated_rx) = mpsc::channel();
        let updater_fence = fence.clone();
        let updater_target = target.clone();
        let updater = thread::spawn(move || {
            updater_fence
                .replace_upper_caps([(updater_target, Hertz::new(1_000))].into())
                .unwrap();
            updated_tx.send(()).unwrap();
        });
        assert!(
            updated_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "the new safety generation must not publish during an old transaction"
        );
        release_worker_tx.send(()).unwrap();
        worker.join().unwrap();
        updated_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        updater.join().unwrap();

        let observed = fence
            .with_upper_caps(|caps| caps[&target])
            .expect("read the new safety generation");
        assert_eq!(observed, Hertz::new(1_000));
    }
}
