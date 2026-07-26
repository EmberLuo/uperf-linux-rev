//! Blocking reconciliation worker.
//!
//! Every operation in this module may perform filesystem I/O, durable `fsync`,
//! process scheduler syscalls, or a blocking systemd D-Bus call.  The runtime
//! therefore executes [`run`] with `tokio::task::spawn_blocking`; the single
//! state reducer only consumes the resulting snapshot.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};

use uperf_actuator::{
    ActuatorError, FrequencyActuator, FrequencyRequest, ScalarRequest, ScalarValue, TaskRequest,
    UnitRequest,
};
use uperf_core::{
    AppliedState, DesiredPlan, Hertz, PolicyEngine, ProcessIdentity, ProcessInfo,
    ScalarSettingValue, TargetId, TaskPlan, WorkloadSource, scheduler_scene_for,
};
use uperf_platform::{
    PlatformError, ProcessSchedulingState, RuntimePlatform, SystemdUnitProperties,
};

use crate::decision_trace::{DecisionTraceContext, DecisionTraceStore};

/// One immutable state snapshot submitted to a blocking mutation lane.
#[derive(Clone)]
pub(crate) struct ReconcileJob {
    pub actuator: Arc<FrequencyActuator>,
    pub environment: Arc<dyn RuntimePlatform>,
    pub policy_engine: PolicyEngine,
    pub workload: Option<ProcessInfo>,
    pub workload_source: WorkloadSource,
    pub desired: DesiredPlan,
    pub applied: AppliedState,
    pub applied_units: BTreeMap<String, SystemdUnitProperties>,
    pub reconcile_frequencies: bool,
    pub reconcile_scheduler: bool,
    pub mutation_gate: Arc<Mutex<()>>,
    pub frequency_safety: Arc<FrequencySafetyFence>,
    pub decision_trace: Arc<DecisionTraceStore>,
    pub decision_trace_context: DecisionTraceContext,
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
    pub scheduler_report: SchedulerReport,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SchedulerReport {
    pub workload: Option<ProcessIdentity>,
    pub matched_rule: Option<String>,
    pub cgroup_class: Option<String>,
    pub systemd_unit: Option<String>,
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
    matched_rule: Option<String>,
    cgroup_class: Option<String>,
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

    #[cfg(test)]
    pub(crate) fn hold_for_test(&self, operation: impl FnOnce()) -> Result<(), String> {
        self.with_upper_caps(|_| operation())
    }
}

/// Execute one reconciliation snapshot.
///
/// Scheduler discovery deliberately happens before entering the daemon-owned
/// mutation gate. Walking procfs and resolving the systemd unit can block, but
/// neither operation mutates owned state. The gate therefore spans only the
/// frequency/scalar and task/unit transactions that must not interleave with a
/// suspend/shutdown restoration.
pub(crate) fn run(job: &ReconcileJob) -> ReconcileOutcome {
    let started_at = job.environment.monotonic_millis().get();
    let elapsed = Instant::now();
    let mut outcome = ReconcileOutcome {
        desired: job.desired.clone(),
        applied: job.applied.clone(),
        applied_units: job.applied_units.clone(),
        frequency_attempted: job.reconcile_frequencies,
        frequency_error: None,
        scheduler_attempted: job.reconcile_scheduler,
        scheduler_error: None,
        scheduler_warning: None,
        scheduler_report: SchedulerReport::default(),
    };
    let scheduler = if job.reconcile_scheduler {
        match resolve_scheduler(
            job.environment.as_ref(),
            job.actuator.as_ref(),
            &job.policy_engine,
            job.workload.as_ref(),
            job.workload_source,
            scheduler_scene_for(job.desired.dominant_scene),
        ) {
            Ok(resolution) => {
                outcome.desired.tasks.clone_from(&resolution.tasks);
                outcome.scheduler_warning.clone_from(&resolution.warning);
                outcome.scheduler_report = SchedulerReport {
                    workload: job.workload.as_ref().map(|process| process.identity),
                    matched_rule: resolution.matched_rule.clone(),
                    cgroup_class: resolution.cgroup_class.clone(),
                    systemd_unit: resolution.cgroup.as_ref().map(|intent| intent.unit.clone()),
                };
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

    if !job.reconcile_frequencies && scheduler.is_none() {
        record_trace(job, &outcome, started_at, elapsed.elapsed());
        return outcome;
    }

    let Ok(gate) = job.mutation_gate.lock() else {
        let message = "runtime mutation gate was poisoned".to_owned();
        if job.reconcile_frequencies {
            outcome.frequency_error = Some(message.clone());
        }
        if scheduler.is_some() {
            outcome.scheduler_error = Some(message);
        }
        record_trace(job, &outcome, started_at, elapsed.elapsed());
        return outcome;
    };

    if job.reconcile_frequencies {
        match job.frequency_safety.with_upper_caps(|upper_caps| {
            reconcile_frequencies(
                job.actuator.as_ref(),
                &outcome.desired,
                upper_caps,
                &mut outcome.applied,
            )
        }) {
            Ok(Ok(())) => {
                if let Err(error) = reconcile_scalars(
                    job.actuator.as_ref(),
                    &outcome.desired,
                    &mut outcome.applied,
                ) {
                    outcome.frequency_error = Some(error);
                }
            }
            Ok(Err(error)) | Err(error) => outcome.frequency_error = Some(error),
        }
    }

    if let Some(resolution) = scheduler
        && let Err(error) = reconcile_scheduler(
            job.actuator.as_ref(),
            job.workload.as_ref(),
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
    record_trace(job, &outcome, started_at, elapsed.elapsed());
    outcome
}

fn record_trace(
    job: &ReconcileJob,
    outcome: &ReconcileOutcome,
    monotonic_ms: u64,
    duration: std::time::Duration,
) {
    job.decision_trace.record_reconcile(
        monotonic_ms,
        duration,
        &outcome.desired,
        &outcome.applied,
        outcome.frequency_attempted,
        outcome.scheduler_attempted,
        outcome.frequency_error.as_deref(),
        outcome.scheduler_error.as_deref(),
        &job.decision_trace_context,
    );
}

fn reconcile_frequencies(
    actuator: &FrequencyActuator,
    desired: &DesiredPlan,
    upper_caps: &BTreeMap<TargetId, Hertz>,
    applied: &mut AppliedState,
) -> Result<(), String> {
    let mut effective = desired.frequencies.clone();
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
            applied.frequencies.insert(id.clone(), current);
        } else {
            requests.push(FrequencyRequest {
                target: id.clone(),
                limits: *limits,
            });
        }
    }
    if requests.is_empty() {
        return Ok(());
    }
    let result = match actuator.apply_owned_batch_fast(&requests) {
        Ok(result) => result,
        Err(ActuatorError::OwnershipRequired(_)) => actuator
            .apply_batch(&requests)
            .map_err(|error| format!("durably claim frequency batch: {error}"))?,
        Err(error) => return Err(format!("apply owned frequency batch: {error}")),
    };
    for (id, limits) in result.applied {
        applied.frequencies.insert(id, limits);
    }
    Ok(())
}

fn reconcile_scalars(
    actuator: &FrequencyActuator,
    desired: &DesiredPlan,
    applied: &mut AppliedState,
) -> Result<(), String> {
    let removed = applied
        .scalars
        .keys()
        .filter(|id| !desired.scalars.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    actuator
        .restore_scalars(&removed)
        .map_err(|error| format!("restore inactive scalar targets: {error}"))?;
    for id in removed {
        applied.scalars.remove(&id);
    }

    let mut requests = Vec::new();
    for (id, desired_value) in &desired.scalars {
        let current = actuator
            .read_scalar(id)
            .map_err(|error| format!("read scalar target {id}: {error}"))?;
        let desired_value = actuator_scalar_value(desired_value);
        if current == desired_value {
            applied
                .scalars
                .insert(id.clone(), policy_scalar_value(current));
        } else {
            requests.push(ScalarRequest {
                target: id.clone(),
                value: desired_value,
            });
        }
    }
    if requests.is_empty() {
        return Ok(());
    }
    let result = actuator
        .apply_scalars(&requests)
        .map_err(|error| format!("apply scalar batch: {error}"))?;
    for (id, value) in result.applied {
        applied.scalars.insert(id, policy_scalar_value(value));
    }
    Ok(())
}

fn actuator_scalar_value(value: &ScalarSettingValue) -> ScalarValue {
    match value {
        ScalarSettingValue::Integer(value) => ScalarValue::Integer(*value),
        ScalarSettingValue::String(value) => ScalarValue::String(value.clone()),
        ScalarSettingValue::CpuList(value) => ScalarValue::CpuList(value.clone()),
    }
}

fn policy_scalar_value(value: ScalarValue) -> ScalarSettingValue {
    match value {
        ScalarValue::Integer(value) => ScalarSettingValue::Integer(value),
        ScalarValue::String(value) => ScalarSettingValue::String(value),
        ScalarValue::CpuList(value) => ScalarSettingValue::CpuList(value),
    }
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
    source: WorkloadSource,
    scheduler_scene: uperf_core::SchedulerScene,
) -> Result<SchedulerResolution, String> {
    if !policy_engine.config().scheduler.enabled {
        return Ok(SchedulerResolution {
            tasks: BTreeMap::new(),
            cgroup: None,
            warning: None,
            matched_rule: None,
            cgroup_class: None,
        });
    }
    let Some(workload) = workload else {
        return Ok(SchedulerResolution {
            tasks: BTreeMap::new(),
            cgroup: None,
            warning: None,
            matched_rule: None,
            cgroup_class: None,
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
        .evaluate_scheduler(workload, &threads, source, scheduler_scene)
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
        matched_rule: decision.matched_rule,
        cgroup_class: decision.cgroup_class,
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
        let desired_state = apply_task_plan(current.clone(), plan);
        if desired_state == current {
            verified.insert(*identity, scheduling_state_as_plan(&current));
        } else {
            requests.push(TaskRequest {
                identity: *identity,
                desired: desired_state,
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
        current.policy = class;
        if class != uperf_core::SchedulingClass::Fifo {
            current.rt_priority = None;
        }
    }
    if let Some(priority) = desired.rt_priority {
        current.rt_priority = Some(priority);
    }
    match (desired.uclamp_min, desired.uclamp_max) {
        (Some(minimum), Some(maximum)) => {
            current.uclamp_min = Some(minimum);
            current.uclamp_max = Some(maximum);
        }
        (Some(minimum), None) => {
            current.uclamp_min = Some(
                current
                    .uclamp_max
                    .map_or(minimum, |maximum| minimum.min(maximum)),
            );
        }
        (None, Some(maximum)) => {
            current.uclamp_max = Some(
                current
                    .uclamp_min
                    .map_or(maximum, |minimum| maximum.max(minimum)),
            );
        }
        (None, None) => {}
    }
    current
}

fn scheduling_state_as_plan(state: &ProcessSchedulingState) -> TaskPlan {
    TaskPlan {
        affinity: Some(state.affinity.clone()),
        nice: Some(state.nice),
        scheduling_class: Some(state.policy),
        rt_priority: state.rt_priority,
        uclamp_min: state.uclamp_min,
        uclamp_max: state.uclamp_max,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex, mpsc},
        thread,
        time::Duration,
    };

    use uperf_actuator::{ScalarDomain, ScalarTarget, TargetRegistry};
    use uperf_core::{
        CpuId, CpuSet, FrequencyLimits, Hertz, ProcessId, ProcessIdentity, ProcessInfo,
        ScalarSettingValue, Scene, SchedulingClass, TargetId, TaskPlan, UserId,
    };
    use uperf_platform::{PlatformResult, ProcessController, StateStore, SysfsIo};
    use uperf_testkit::FakeProc;

    use super::{
        AppliedState, DesiredPlan, FrequencyActuator, FrequencySafetyFence, ProcessSchedulingState,
        apply_task_plan, apply_upper_cap, reconcile_scalars, reconcile_scheduler,
        scheduling_state_as_plan,
    };

    #[derive(Default)]
    struct DenyingSysfs;

    impl SysfsIo for DenyingSysfs {
        fn read_string(&self, path: &Path) -> PlatformResult<String> {
            Err(uperf_platform::PlatformError::invalid(
                path.display().to_string(),
                "frequency paths are unused by task reconciliation",
            ))
        }

        fn write_string(&self, path: &Path, _value: &str) -> PlatformResult<()> {
            Err(uperf_platform::PlatformError::invalid(
                path.display().to_string(),
                "frequency paths are unused by task reconciliation",
            ))
        }
    }

    #[derive(Default)]
    struct MemorySysfs {
        values: Mutex<BTreeMap<PathBuf, String>>,
        writes: Mutex<Vec<(PathBuf, String)>>,
    }

    impl MemorySysfs {
        fn insert(&self, path: impl Into<PathBuf>, value: impl Into<String>) {
            lock(&self.values).insert(path.into(), value.into());
        }

        fn value(&self, path: &Path) -> String {
            lock(&self.values)
                .get(path)
                .cloned()
                .expect("known scalar path")
        }

        fn take_writes(&self) -> Vec<(PathBuf, String)> {
            std::mem::take(&mut *lock(&self.writes))
        }
    }

    impl SysfsIo for MemorySysfs {
        fn read_string(&self, path: &Path) -> PlatformResult<String> {
            lock(&self.values).get(path).cloned().ok_or_else(|| {
                uperf_platform::PlatformError::invalid(
                    path.display().to_string(),
                    "unknown scalar path",
                )
            })
        }

        fn write_string(&self, path: &Path, value: &str) -> PlatformResult<()> {
            let mut values = lock(&self.values);
            let Some(stored) = values.get_mut(path) else {
                return Err(uperf_platform::PlatformError::invalid(
                    path.display().to_string(),
                    "unknown scalar path",
                ));
            };
            *stored = value.to_owned();
            drop(values);
            lock(&self.writes).push((path.to_path_buf(), value.to_owned()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryStore(Mutex<Option<Vec<u8>>>);

    impl StateStore for MemoryStore {
        fn load(&self) -> PlatformResult<Option<Vec<u8>>> {
            Ok(lock(&self.0).clone())
        }

        fn store_durable(&self, bytes: &[u8]) -> PlatformResult<()> {
            *lock(&self.0) = Some(bytes.to_vec());
            Ok(())
        }

        fn remove_durable(&self) -> PlatformResult<()> {
            *lock(&self.0) = None;
            Ok(())
        }
    }

    /// Records the exact order of scheduler writes so restore-before-apply
    /// ordering is observable.
    #[derive(Default)]
    struct RecordingController {
        states: Mutex<BTreeMap<ProcessId, ProcessSchedulingState>>,
        writes: Mutex<Vec<(ProcessId, Option<u16>)>>,
    }

    impl RecordingController {
        fn insert(&self, pid: ProcessId) {
            lock(&self.states).insert(
                pid,
                ProcessSchedulingState {
                    affinity: CpuSet::from_ids([CpuId::new(0)]),
                    nice: 0,
                    policy: SchedulingClass::Other,
                    rt_priority: None,
                    uclamp_min: Some(0),
                    uclamp_max: Some(768),
                },
            );
        }

        fn writes(&self) -> Vec<(ProcessId, Option<u16>)> {
            lock(&self.writes).clone()
        }
    }

    impl ProcessController for RecordingController {
        fn read_scheduling(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState> {
            lock(&self.states).get(&process).cloned().ok_or_else(|| {
                uperf_platform::PlatformError::Disappeared(format!("task {}", process.get()))
            })
        }

        fn write_scheduling(
            &self,
            process: ProcessId,
            desired: &ProcessSchedulingState,
        ) -> PlatformResult<ProcessSchedulingState> {
            lock(&self.writes).push((process, desired.uclamp_min));
            lock(&self.states).insert(process, desired.clone());
            Ok(desired.clone())
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn focused(pid: u32) -> ProcessInfo {
        ProcessInfo {
            identity: ProcessIdentity {
                pid: ProcessId::new(pid),
                start_time_ticks: u64::from(pid),
                uid: UserId::new(1_000),
            },
            owner_control_safe: true,
            comm: format!("app-{pid}"),
            executable: Some(format!("/usr/bin/app-{pid}")),
            desktop_id: None,
        }
    }

    fn focus_plan() -> TaskPlan {
        TaskPlan {
            uclamp_min: Some(205),
            ..TaskPlan::default()
        }
    }

    fn desired_with(tasks: BTreeMap<ProcessIdentity, TaskPlan>) -> DesiredPlan {
        DesiredPlan {
            generation: 1,
            effective_profile: uperf_core::ProfileId::Balance,
            dominant_scene: Scene::Idle,
            frequencies: BTreeMap::new(),
            scalars: BTreeMap::new(),
            tasks,
        }
    }

    #[test]
    fn scalar_reconcile_restores_removed_skips_equal_and_tracks_applied_values() {
        let integer_id = TargetId::new("scalar.integer").expect("integer ID");
        let mode_id = TargetId::new("scalar.mode").expect("mode ID");
        let integer_path = PathBuf::from("/sys/class/test/integer");
        let mode_path = PathBuf::from("/sys/class/test/mode");
        let sysfs = Arc::new(MemorySysfs::default());
        sysfs.insert(&integer_path, "10");
        sysfs.insert(&mode_path, "powersave");
        let registry = TargetRegistry::default()
            .with_scalar_targets([
                ScalarTarget::new(
                    integer_id.clone(),
                    integer_path.clone(),
                    ScalarDomain::IntegerRange {
                        minimum: 0,
                        maximum: 100,
                    },
                )
                .expect("integer target"),
                ScalarTarget::new(
                    mode_id.clone(),
                    mode_path.clone(),
                    ScalarDomain::StringEnum {
                        values: vec!["performance".to_owned(), "powersave".to_owned()],
                    },
                )
                .expect("mode target"),
            ])
            .expect("scalar registry");
        let actuator = FrequencyActuator::new(
            sysfs.clone(),
            Arc::new(MemoryStore::default()),
            registry,
            "boot-a",
            "device-a",
        );
        let mut applied = AppliedState::default();

        let mut first = desired_with(BTreeMap::new());
        first
            .scalars
            .insert(integer_id.clone(), ScalarSettingValue::Integer(20));
        reconcile_scalars(&actuator, &first, &mut applied).expect("apply first scalar");
        assert_eq!(sysfs.value(&integer_path), "20");
        assert_eq!(
            applied.scalars.get(&integer_id),
            Some(&ScalarSettingValue::Integer(20))
        );
        sysfs.take_writes();

        let mut second = desired_with(BTreeMap::new());
        second.scalars.insert(
            mode_id.clone(),
            ScalarSettingValue::String("powersave".to_owned()),
        );
        reconcile_scalars(&actuator, &second, &mut applied)
            .expect("restore removed scalar and read-skip equal scalar");
        assert_eq!(sysfs.value(&integer_path), "10");
        assert_eq!(
            sysfs.take_writes(),
            vec![(integer_path.clone(), "10".to_owned())],
            "the already-equal scalar must not be written"
        );
        assert!(!applied.scalars.contains_key(&integer_id));
        assert_eq!(
            applied.scalars.get(&mode_id),
            Some(&ScalarSettingValue::String("powersave".to_owned()))
        );

        second.scalars.insert(
            mode_id.clone(),
            ScalarSettingValue::String("performance".to_owned()),
        );
        reconcile_scalars(&actuator, &second, &mut applied).expect("apply changed scalar");
        assert_eq!(sysfs.value(&mode_path), "performance");
        assert_eq!(
            applied.scalars.get(&mode_id),
            Some(&ScalarSettingValue::String("performance".to_owned()))
        );
    }

    /// A focus switch must relinquish the previous task before boosting the new
    /// one; the reverse order would leave a defocused app clamped if the second
    /// half of the transaction failed.
    #[test]
    fn a_focus_switch_restores_the_previous_task_before_boosting_the_next() {
        let procfs = Arc::new(FakeProc::default());
        let controller = Arc::new(RecordingController::default());
        let previous = focused(41);
        let next = focused(42);
        for process in [&previous, &next] {
            procfs.insert_process(process.clone());
            controller.insert(process.identity.pid);
        }
        let actuator = FrequencyActuator::new(
            Arc::new(DenyingSysfs),
            Arc::new(MemoryStore::default()),
            TargetRegistry::default(),
            "boot-a",
            "device-a",
        )
        .with_process_backend(procfs, controller.clone());
        let mut applied = AppliedState::default();
        let mut applied_units = BTreeMap::new();
        let mut warning = None;

        let boosted = desired_with([(previous.identity, focus_plan())].into());
        reconcile_scheduler(
            &actuator,
            Some(&previous),
            &boosted,
            None,
            &mut applied,
            &mut applied_units,
            &mut warning,
        )
        .expect("boost the first focused task");
        assert_eq!(
            controller.writes(),
            vec![(previous.identity.pid, Some(205))]
        );
        assert_eq!(
            controller
                .read_scheduling(previous.identity.pid)
                .expect("read boosted task")
                .uclamp_max,
            Some(768),
            "a minimum-only focus plan must preserve the current ceiling"
        );

        let switched = desired_with([(next.identity, focus_plan())].into());
        reconcile_scheduler(
            &actuator,
            Some(&next),
            &switched,
            None,
            &mut applied,
            &mut applied_units,
            &mut warning,
        )
        .expect("switch focus to the second task");

        assert_eq!(
            controller.writes(),
            vec![
                (previous.identity.pid, Some(205)),
                (previous.identity.pid, Some(0)),
                (next.identity.pid, Some(205)),
            ],
            "the defocused task must be restored to its original uclamp first"
        );
        assert!(!applied.tasks.contains_key(&previous.identity));
        assert!(applied.tasks.contains_key(&next.identity));
        assert!(warning.is_none());
    }

    #[test]
    fn partial_uclamp_plan_preserves_the_unspecified_bound() {
        let state = ProcessSchedulingState {
            affinity: CpuSet::from_ids([CpuId::new(0)]),
            nice: 0,
            policy: SchedulingClass::Other,
            rt_priority: None,
            uclamp_min: Some(64),
            uclamp_max: Some(128),
        };

        let raised = apply_task_plan(
            state.clone(),
            &TaskPlan {
                uclamp_min: Some(205),
                ..TaskPlan::default()
            },
        );
        assert_eq!(raised.uclamp_min, Some(128));
        assert_eq!(raised.uclamp_max, Some(128));

        let lowered = apply_task_plan(
            state,
            &TaskPlan {
                uclamp_max: Some(32),
                ..TaskPlan::default()
            },
        );
        assert_eq!(lowered.uclamp_min, Some(64));
        assert_eq!(lowered.uclamp_max, Some(64));
    }

    #[test]
    fn task_plan_maps_fifo_priority_as_part_of_the_scheduler_policy() {
        let original = ProcessSchedulingState {
            affinity: CpuSet::from_ids([CpuId::new(1)]),
            nice: 0,
            policy: SchedulingClass::Other,
            rt_priority: None,
            uclamp_min: Some(64),
            uclamp_max: Some(768),
        };
        let fifo = apply_task_plan(
            original,
            &TaskPlan {
                scheduling_class: Some(SchedulingClass::Fifo),
                rt_priority: Some(20),
                ..TaskPlan::default()
            },
        );
        assert_eq!(fifo.policy, SchedulingClass::Fifo);
        assert_eq!(fifo.rt_priority, Some(20));
        assert_eq!(scheduling_state_as_plan(&fifo).rt_priority, Some(20));

        let normal = apply_task_plan(
            fifo,
            &TaskPlan {
                scheduling_class: Some(SchedulingClass::Other),
                ..TaskPlan::default()
            },
        );
        assert_eq!(normal.policy, SchedulingClass::Other);
        assert_eq!(normal.rt_priority, None);
    }

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
