//! Deterministic fake platform ports and fault-injection helpers.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs, io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use tempfile::TempDir;
use uperf_core::{CpuSet, DeviceCapabilities, MonotonicMillis, ProcessId, ProcessInfo};
use uperf_platform::{
    Clock, CpuTimeSnapshot, InputEvent, InputSource, OnlineCpuSource, PlatformError,
    PlatformResult, ProcReader, ProcessController, ProcessSchedulingState, StateStore, SysfsIo,
    SystemdClient, SystemdUnitInstanceIdentity, SystemdUnitInstanceKey, SystemdUnitProperties,
    ThermalSample, ThermalSource, TopologySource,
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

/// One observed fake sysfs mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SysfsWrite {
    pub path: PathBuf,
    pub value: String,
}

#[derive(Debug, Default)]
struct FakeSysfsState {
    values: BTreeMap<PathBuf, String>,
    writes: Vec<SysfsWrite>,
    fail_reads: BTreeMap<PathBuf, VecDeque<String>>,
    fail_writes: BTreeMap<PathBuf, VecDeque<String>>,
}

/// In-memory sysfs supporting deterministic one-shot failures.
#[derive(Clone, Debug, Default)]
pub struct FakeSysfs {
    state: Arc<Mutex<FakeSysfsState>>,
}

impl FakeSysfs {
    #[must_use]
    pub fn with_values<I, P, S>(values: I) -> Self
    where
        I: IntoIterator<Item = (P, S)>,
        P: Into<PathBuf>,
        S: Into<String>,
    {
        let fake = Self::default();
        {
            let mut state = lock(&fake.state);
            state.values.extend(
                values
                    .into_iter()
                    .map(|(path, value)| (path.into(), value.into())),
            );
        }
        fake
    }

    pub fn set(&self, path: impl Into<PathBuf>, value: impl Into<String>) {
        lock(&self.state).values.insert(path.into(), value.into());
    }

    #[must_use]
    pub fn get(&self, path: impl AsRef<Path>) -> Option<String> {
        lock(&self.state).values.get(path.as_ref()).cloned()
    }

    #[must_use]
    pub fn writes(&self) -> Vec<SysfsWrite> {
        lock(&self.state).writes.clone()
    }

    pub fn clear_writes(&self) {
        lock(&self.state).writes.clear();
    }

    pub fn fail_next_read(&self, path: impl Into<PathBuf>, reason: impl Into<String>) {
        lock(&self.state)
            .fail_reads
            .entry(path.into())
            .or_default()
            .push_back(reason.into());
    }

    pub fn fail_next_write(&self, path: impl Into<PathBuf>, reason: impl Into<String>) {
        lock(&self.state)
            .fail_writes
            .entry(path.into())
            .or_default()
            .push_back(reason.into());
    }
}

impl SysfsIo for FakeSysfs {
    fn read_string(&self, path: &Path) -> PlatformResult<String> {
        let mut state = lock(&self.state);
        if let Some(reason) = pop_failure(&mut state.fail_reads, path) {
            return Err(PlatformError::invalid(path, reason));
        }
        state.values.get(path).cloned().ok_or_else(|| {
            PlatformError::io(
                "fake read",
                path,
                io::Error::new(io::ErrorKind::NotFound, "fake attribute not found"),
            )
        })
    }

    fn write_string(&self, path: &Path, value: &str) -> PlatformResult<()> {
        let mut state = lock(&self.state);
        if let Some(reason) = pop_failure(&mut state.fail_writes, path) {
            return Err(PlatformError::invalid(path, reason));
        }
        let Some(stored) = state.values.get_mut(path) else {
            return Err(PlatformError::io(
                "fake write",
                path,
                io::Error::new(io::ErrorKind::NotFound, "fake attribute not found"),
            ));
        };
        stored.clone_from(&value.to_owned());
        state.writes.push(SysfsWrite {
            path: path.to_path_buf(),
            value: value.to_owned(),
        });
        Ok(())
    }
}

/// In-memory procfs observations.
#[derive(Clone, Debug, Default)]
pub struct FakeProc {
    cpu_times: Arc<Mutex<Option<CpuTimeSnapshot>>>,
    processes: Arc<Mutex<BTreeMap<ProcessId, ProcessInfo>>>,
    process_ids: Arc<Mutex<BTreeSet<ProcessId>>>,
    threads: Arc<Mutex<BTreeMap<ProcessId, Vec<ProcessId>>>>,
}

impl FakeProc {
    pub fn set_cpu_times(&self, snapshot: CpuTimeSnapshot) {
        *lock(&self.cpu_times) = Some(snapshot);
    }

    pub fn insert_process(&self, process: ProcessInfo) {
        lock(&self.process_ids).insert(process.identity.pid);
        lock(&self.processes).insert(process.identity.pid, process);
    }

    pub fn remove_process(&self, pid: ProcessId) {
        lock(&self.process_ids).remove(&pid);
        lock(&self.processes).remove(&pid);
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
        Ok(lock(&self.process_ids).iter().copied().collect())
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

/// Static fake hardware discovery.
#[derive(Clone, Debug)]
pub struct FakeTopology {
    capabilities: DeviceCapabilities,
}

impl FakeTopology {
    #[must_use]
    pub fn new(capabilities: DeviceCapabilities) -> Self {
        Self { capabilities }
    }
}

impl TopologySource for FakeTopology {
    fn discover_capabilities(&self) -> PlatformResult<DeviceCapabilities> {
        Ok(self.capabilities.clone())
    }
}

/// Mutable fake thermal source.
#[derive(Clone, Debug, Default)]
pub struct FakeThermal {
    samples: Arc<Mutex<Vec<ThermalSample>>>,
}

impl FakeThermal {
    #[must_use]
    pub fn new(samples: Vec<ThermalSample>) -> Self {
        Self {
            samples: Arc::new(Mutex::new(samples)),
        }
    }

    pub fn set(&self, samples: Vec<ThermalSample>) {
        *lock(&self.samples) = samples;
    }
}

impl ThermalSource for FakeThermal {
    fn read_thermal(&self) -> PlatformResult<Vec<ThermalSample>> {
        Ok(lock(&self.samples).clone())
    }
}

/// Queue-backed normalized input source.
#[derive(Clone, Debug, Default)]
pub struct FakeInput {
    events: Arc<Mutex<VecDeque<InputEvent>>>,
}

impl FakeInput {
    #[must_use]
    pub fn new(events: impl IntoIterator<Item = InputEvent>) -> Self {
        Self {
            events: Arc::new(Mutex::new(events.into_iter().collect())),
        }
    }

    pub fn push(&self, event: InputEvent) {
        lock(&self.events).push_back(event);
    }
}

impl InputSource for FakeInput {
    fn next_event(&mut self) -> PlatformResult<InputEvent> {
        lock(&self.events)
            .pop_front()
            .ok_or_else(|| PlatformError::Disappeared("fake input queue is empty".to_owned()))
    }
}

/// In-memory process scheduler controller.
#[derive(Clone, Debug, Default)]
pub struct FakeProcessController {
    states: Arc<Mutex<BTreeMap<ProcessId, ProcessSchedulingState>>>,
    writes: Arc<Mutex<Vec<(ProcessId, ProcessSchedulingState)>>>,
}

impl FakeProcessController {
    pub fn insert(&self, pid: ProcessId, state: ProcessSchedulingState) {
        lock(&self.states).insert(pid, state);
    }

    #[must_use]
    pub fn writes(&self) -> Vec<(ProcessId, ProcessSchedulingState)> {
        lock(&self.writes).clone()
    }
}

impl ProcessController for FakeProcessController {
    fn read_scheduling(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState> {
        lock(&self.states).get(&process).cloned().ok_or_else(|| {
            PlatformError::Disappeared(format!("fake scheduling state for {}", process.0))
        })
    }

    fn write_scheduling(
        &self,
        process: ProcessId,
        desired: &ProcessSchedulingState,
    ) -> PlatformResult<ProcessSchedulingState> {
        let mut states = lock(&self.states);
        if !states.contains_key(&process) {
            return Err(PlatformError::Disappeared(format!(
                "fake scheduling state for {}",
                process.0
            )));
        }
        states.insert(process, desired.clone());
        lock(&self.writes).push((process, desired.clone()));
        Ok(desired.clone())
    }
}

#[derive(Debug, Default)]
struct FakeSystemdState {
    process_units: BTreeMap<ProcessId, String>,
    units: BTreeMap<String, SystemdUnitProperties>,
    unit_identities: BTreeMap<String, SystemdUnitInstanceIdentity>,
    writes: Vec<(String, SystemdUnitProperties)>,
}

/// In-memory typed systemd/cgroup port.
#[derive(Clone, Debug, Default)]
pub struct FakeSystemd {
    state: Arc<Mutex<FakeSystemdState>>,
}

impl FakeSystemd {
    pub fn insert_unit(
        &self,
        process: ProcessId,
        unit: impl Into<String>,
        properties: SystemdUnitProperties,
    ) {
        let unit = unit.into();
        let mut state = lock(&self.state);
        state.process_units.insert(process, unit.clone());
        state
            .unit_identities
            .entry(unit.clone())
            .or_insert_with(|| SystemdUnitInstanceIdentity {
                unit: unit.clone(),
                key: SystemdUnitInstanceKey::ControlGroup(format!("/fake.slice/{unit}")),
            });
        state.units.insert(unit, properties);
    }

    /// Insert or replace a unit with an explicit activation identity.
    ///
    /// Replacing an existing unit name with a different identity simulates a
    /// stop/start cycle without changing the reusable name.
    pub fn insert_unit_instance(
        &self,
        process: ProcessId,
        identity: SystemdUnitInstanceIdentity,
        properties: SystemdUnitProperties,
    ) {
        let mut state = lock(&self.state);
        state.process_units.insert(process, identity.unit.clone());
        state.units.insert(identity.unit.clone(), properties);
        state
            .unit_identities
            .insert(identity.unit.clone(), identity);
    }

    #[must_use]
    pub fn writes(&self) -> Vec<(String, SystemdUnitProperties)> {
        lock(&self.state).writes.clone()
    }
}

impl SystemdClient for FakeSystemd {
    fn unit_for_process(&self, process: ProcessId) -> PlatformResult<Option<String>> {
        Ok(lock(&self.state).process_units.get(&process).cloned())
    }

    fn unit_instance_for_process(
        &self,
        process: ProcessId,
    ) -> PlatformResult<Option<SystemdUnitInstanceIdentity>> {
        let state = lock(&self.state);
        let Some(unit) = state.process_units.get(&process) else {
            return Ok(None);
        };
        state
            .unit_identities
            .get(unit)
            .cloned()
            .map(Some)
            .ok_or_else(|| PlatformError::Disappeared(format!("fake systemd unit identity {unit}")))
    }

    fn unit_instance_identity(&self, unit: &str) -> PlatformResult<SystemdUnitInstanceIdentity> {
        lock(&self.state)
            .unit_identities
            .get(unit)
            .cloned()
            .ok_or_else(|| PlatformError::Disappeared(format!("fake systemd unit {unit}")))
    }

    fn unit_processes(&self, unit: &str) -> PlatformResult<Vec<ProcessId>> {
        let state = lock(&self.state);
        if !state.units.contains_key(unit) {
            return Err(PlatformError::Disappeared(format!(
                "fake systemd unit {unit}"
            )));
        }
        Ok(state
            .process_units
            .iter()
            .filter_map(|(process, candidate)| (candidate == unit).then_some(*process))
            .collect())
    }

    fn read_unit_properties(&self, unit: &str) -> PlatformResult<SystemdUnitProperties> {
        lock(&self.state)
            .units
            .get(unit)
            .cloned()
            .ok_or_else(|| PlatformError::Disappeared(format!("fake systemd unit {unit}")))
    }

    fn write_unit_properties(
        &self,
        unit: &str,
        desired: &SystemdUnitProperties,
    ) -> PlatformResult<SystemdUnitProperties> {
        let mut state = lock(&self.state);
        if !state.units.contains_key(unit) {
            return Err(PlatformError::Disappeared(format!(
                "fake systemd unit {unit}"
            )));
        }
        state.units.insert(unit.to_owned(), desired.clone());
        state.writes.push((unit.to_owned(), desired.clone()));
        Ok(desired.clone())
    }
}

#[derive(Debug, Default)]
struct MemoryStoreState {
    bytes: Option<Vec<u8>>,
    fail_next_store: bool,
    stores: usize,
    removes: usize,
}

/// In-memory journal store with a one-shot durable-store fault.
#[derive(Clone, Debug, Default)]
pub struct MemoryStateStore {
    state: Arc<Mutex<MemoryStoreState>>,
}

impl MemoryStateStore {
    pub fn fail_next_store(&self) {
        lock(&self.state).fail_next_store = true;
    }

    #[must_use]
    pub fn store_count(&self) -> usize {
        lock(&self.state).stores
    }

    #[must_use]
    pub fn remove_count(&self) -> usize {
        lock(&self.state).removes
    }
}

impl StateStore for MemoryStateStore {
    fn load(&self) -> PlatformResult<Option<Vec<u8>>> {
        Ok(lock(&self.state).bytes.clone())
    }

    fn store_durable(&self, bytes: &[u8]) -> PlatformResult<()> {
        let mut state = lock(&self.state);
        if state.fail_next_store {
            state.fail_next_store = false;
            return Err(PlatformError::invalid(
                "memory-state-store",
                "injected durable-store failure",
            ));
        }
        state.bytes = Some(bytes.to_vec());
        state.stores += 1;
        Ok(())
    }

    fn remove_durable(&self) -> PlatformResult<()> {
        let mut state = lock(&self.state);
        state.bytes = None;
        state.removes += 1;
        Ok(())
    }
}

/// Host-shaped temporary filesystem tree for exercising real read-only Linux
/// adapters.  Paths passed to `write` are always relative to the fixture root.
#[derive(Debug)]
pub struct FixtureRoot {
    temporary: TempDir,
}

impl FixtureRoot {
    /// Create empty `sys`, `proc`, and `etc` fixture roots.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the temporary directory tree cannot be created.
    pub fn new() -> io::Result<Self> {
        let temporary = tempfile::tempdir()?;
        for directory in ["sys", "proc", "etc"] {
            fs::create_dir(temporary.path().join(directory))?;
        }
        Ok(Self { temporary })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.temporary.path()
    }

    /// Write a file beneath the fixture root, creating parent directories.
    ///
    /// # Errors
    ///
    /// Returns an error for absolute/escaping paths or filesystem failures.
    pub fn write(&self, relative: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
        let relative = validate_relative(relative.as_ref())?;
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)
    }
}

fn validate_relative(path: &Path) -> io::Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => result.push(value),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fixture paths must contain only normal relative components",
                ));
            }
        }
    }
    Ok(result)
}

fn pop_failure(failures: &mut BTreeMap<PathBuf, VecDeque<String>>, path: &Path) -> Option<String> {
    let queue = failures.get_mut(path)?;
    let failure = queue.pop_front();
    if queue.is_empty() {
        failures.remove(path);
    }
    failure
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_sysfs_records_only_successful_writes() {
        let path = PathBuf::from("/sys/example");
        let fake = FakeSysfs::with_values([(path.clone(), "old")]);
        fake.fail_next_write(&path, "injected");
        assert!(fake.write_string(&path, "first").is_err());
        fake.write_string(&path, "second").unwrap();
        assert_eq!(fake.get(&path).as_deref(), Some("second"));
        assert_eq!(
            fake.writes(),
            [SysfsWrite {
                path,
                value: "second".to_owned()
            }]
        );
    }

    #[test]
    fn fake_clock_is_shared_and_monotonic() {
        let clock = FakeClock::new(MonotonicMillis(10));
        let clone = clock.clone();
        assert_eq!(clock.advance(15), MonotonicMillis(25));
        assert_eq!(clone.monotonic_millis(), MonotonicMillis(25));
    }

    #[test]
    fn fixture_rejects_escape_paths() {
        let fixture = FixtureRoot::new().unwrap();
        assert!(fixture.write("../outside", b"no").is_err());
        fixture.write("proc/stat", b"cpu 1 2 3 4\n").unwrap();
        assert!(fixture.path().join("proc/stat").is_file());
    }

    #[test]
    fn memory_store_fault_is_one_shot() {
        let store = MemoryStateStore::default();
        store.fail_next_store();
        assert!(store.store_durable(b"first").is_err());
        store.store_durable(b"second").unwrap();
        assert_eq!(store.load().unwrap(), Some(b"second".to_vec()));
        assert_eq!(store.store_count(), 1);
    }

    #[test]
    fn fake_systemd_distinguishes_same_name_activations() {
        let fake = FakeSystemd::default();
        let process = ProcessId(42);
        let properties = SystemdUnitProperties {
            cpu_weight: Some(100),
            allowed_cpus: None,
        };
        let first = SystemdUnitInstanceIdentity {
            unit: "game.scope".to_owned(),
            key: SystemdUnitInstanceKey::InvocationId([1; 16]),
        };
        fake.insert_unit_instance(process, first.clone(), properties.clone());
        assert_eq!(
            fake.unit_instance_for_process(process).unwrap(),
            Some(first.clone())
        );

        let second = SystemdUnitInstanceIdentity {
            unit: first.unit,
            key: SystemdUnitInstanceKey::InvocationId([2; 16]),
        };
        fake.insert_unit_instance(process, second.clone(), properties);
        assert_eq!(
            fake.unit_for_process(process).unwrap().as_deref(),
            Some("game.scope")
        );
        assert_eq!(fake.unit_instance_identity("game.scope").unwrap(), second);
    }
}
