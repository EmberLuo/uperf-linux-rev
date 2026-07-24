//! Deterministic fake platform ports for reducer and actuator tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use uperf_core::{CpuSet, MonotonicMillis, ProcessId, ProcessInfo};
use uperf_platform::{
    Clock, CpuTimeSnapshot, OnlineCpuSource, PlatformError, PlatformResult, ProcReader,
};

/// A manually advanced monotonic clock.
#[derive(Clone, Debug, Default)]
pub struct FakeClock {
    now: Arc<AtomicU64>,
}

impl FakeClock {
    #[must_use]
    pub fn new(now: MonotonicMillis) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(now.0)),
        }
    }

    pub fn set(&self, now: MonotonicMillis) {
        self.now.store(now.0, Ordering::SeqCst);
    }

    #[must_use]
    pub fn advance(&self, milliseconds: u64) -> MonotonicMillis {
        let previous = self.now.fetch_add(milliseconds, Ordering::SeqCst);
        MonotonicMillis(previous.saturating_add(milliseconds))
    }
}

impl Clock for FakeClock {
    fn monotonic_millis(&self) -> MonotonicMillis {
        MonotonicMillis(self.now.load(Ordering::SeqCst))
    }
}

/// In-memory procfs observations.
#[derive(Clone, Debug, Default)]
pub struct FakeProc {
    cpu_times: Arc<Mutex<Option<CpuTimeSnapshot>>>,
    processes: Arc<Mutex<BTreeMap<ProcessId, ProcessInfo>>>,
    top_level_processes: Arc<Mutex<BTreeSet<ProcessId>>>,
    threads: Arc<Mutex<BTreeMap<ProcessId, Vec<ProcessId>>>>,
}

impl FakeProc {
    pub fn set_cpu_times(&self, snapshot: CpuTimeSnapshot) {
        *lock(&self.cpu_times) = Some(snapshot);
    }

    pub fn insert_process(&self, process: ProcessInfo) {
        lock(&self.top_level_processes).insert(process.identity.pid);
        lock(&self.processes).insert(process.identity.pid, process);
    }

    pub fn remove_process(&self, pid: ProcessId) {
        lock(&self.processes).remove(&pid);
        lock(&self.top_level_processes).remove(&pid);
        lock(&self.threads).remove(&pid);
    }

    pub fn set_threads(&self, process: ProcessId, threads: impl IntoIterator<Item = ProcessInfo>) {
        let threads = threads.into_iter().collect::<Vec<_>>();
        let ids = threads
            .iter()
            .map(|thread| thread.identity.pid)
            .collect::<Vec<_>>();
        lock(&self.processes).extend(
            threads
                .into_iter()
                .map(|thread| (thread.identity.pid, thread)),
        );
        lock(&self.threads).insert(process, ids);
    }
}

impl ProcReader for FakeProc {
    fn cpu_times(&self) -> PlatformResult<CpuTimeSnapshot> {
        lock(&self.cpu_times)
            .clone()
            .ok_or_else(|| PlatformError::invalid("/proc/stat", "fake CPU sample not configured"))
    }

    fn list_processes(&self) -> PlatformResult<Vec<ProcessId>> {
        Ok(lock(&self.top_level_processes).iter().copied().collect())
    }

    fn list_threads(&self, process: ProcessId) -> PlatformResult<Vec<ProcessId>> {
        Ok(lock(&self.threads)
            .get(&process)
            .cloned()
            .unwrap_or_else(|| vec![process]))
    }

    fn process_identity(&self, pid: ProcessId) -> PlatformResult<ProcessInfo> {
        lock(&self.processes)
            .get(&pid)
            .cloned()
            .ok_or_else(|| PlatformError::Disappeared(format!("fake process {}", pid.0)))
    }
}

/// Aggregate read-only runtime port backed by a fake clock and procfs.
#[derive(Clone, Debug)]
pub struct FakeRuntime {
    clock: FakeClock,
    procfs: FakeProc,
    online_cpus: Arc<Mutex<CpuSet>>,
}

impl FakeRuntime {
    #[must_use]
    pub fn new(clock: FakeClock, procfs: FakeProc, online_cpus: CpuSet) -> Self {
        Self {
            clock,
            procfs,
            online_cpus: Arc::new(Mutex::new(online_cpus)),
        }
    }

    #[must_use]
    pub const fn clock(&self) -> &FakeClock {
        &self.clock
    }

    #[must_use]
    pub const fn procfs(&self) -> &FakeProc {
        &self.procfs
    }

    pub fn set_online_cpus(&self, online_cpus: CpuSet) {
        *lock(&self.online_cpus) = online_cpus;
    }
}

impl Clock for FakeRuntime {
    fn monotonic_millis(&self) -> MonotonicMillis {
        self.clock.monotonic_millis()
    }
}

impl ProcReader for FakeRuntime {
    fn cpu_times(&self) -> PlatformResult<CpuTimeSnapshot> {
        self.procfs.cpu_times()
    }

    fn list_processes(&self) -> PlatformResult<Vec<ProcessId>> {
        self.procfs.list_processes()
    }

    fn list_threads(&self, process: ProcessId) -> PlatformResult<Vec<ProcessId>> {
        self.procfs.list_threads(process)
    }

    fn process_identity(&self, pid: ProcessId) -> PlatformResult<ProcessInfo> {
        self.procfs.process_identity(pid)
    }
}

impl OnlineCpuSource for FakeRuntime {
    fn online_cpus(&self) -> PlatformResult<CpuSet> {
        Ok(lock(&self.online_cpus).clone())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32) -> ProcessInfo {
        ProcessInfo {
            identity: uperf_core::ProcessIdentity {
                pid: ProcessId::new(pid),
                start_time_ticks: u64::from(pid),
                uid: uperf_core::UserId::new(1000),
            },
            owner_control_safe: true,
            comm: format!("task-{pid}"),
            executable: None,
            desktop_id: None,
        }
    }

    #[test]
    fn fake_clock_is_shared_and_monotonic() {
        let clock = FakeClock::new(MonotonicMillis(10));
        let clone = clock.clone();
        assert_eq!(clock.advance(15), MonotonicMillis(25));
        assert_eq!(clone.monotonic_millis(), MonotonicMillis(25));
    }

    #[test]
    fn fake_proc_keeps_top_level_processes_separate_from_threads() {
        let procfs = FakeProc::default();
        procfs.insert_process(process(10));
        procfs.set_threads(ProcessId::new(10), [process(10), process(11), process(12)]);

        assert_eq!(
            procfs.list_processes().expect("list process leaders"),
            [ProcessId::new(10)]
        );
        assert_eq!(
            procfs
                .list_threads(ProcessId::new(10))
                .expect("list process threads"),
            [ProcessId::new(10), ProcessId::new(11), ProcessId::new(12)]
        );
    }
}
