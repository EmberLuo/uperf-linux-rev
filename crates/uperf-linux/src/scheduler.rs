//! Safe Linux process and thread scheduling controls.
//!
//! The public adapter contains no FFI. Affinity and nice values are handled by
//! `rustix`. Linux does not currently expose `sched_setscheduler(2)` or
//! `sched_setattr(2)` through `rustix`, so policy and uclamp changes are
//! delegated to root-owned util-linux tools and always verified from
//! `/proc/<tid>/sched`.

use std::{
    fmt, fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use rustix::{
    process::{Pid, getpriority_process, setpriority_process},
    thread::{CpuSet as RustixCpuSet, sched_getaffinity, sched_setaffinity},
};
use uperf_core::{CpuId, CpuSet, ProcessId};
use uperf_platform::{
    PlatformError, PlatformResult, ProcessController, ProcessSchedulingState, SchedulingPolicy,
};

const PROC_ROOT: &str = "/proc";
const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";
const UCLAMP_MAX: u16 = 1024;
const TOOL_TIMEOUT: Duration = Duration::from_secs(1);
const TOOL_POLL_INTERVAL: Duration = Duration::from_millis(5);

trait SchedulingApi: Send + Sync {
    fn read(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState>;
    fn set_affinity(&self, process: ProcessId, affinity: &CpuSet) -> PlatformResult<()>;
    fn set_nice(&self, process: ProcessId, nice: i8) -> PlatformResult<()>;
    fn set_policy(&self, process: ProcessId, policy: SchedulingPolicy) -> PlatformResult<()>;
    fn set_uclamp(&self, process: ProcessId, minimum: u16, maximum: u16) -> PlatformResult<()>;
}

/// Linux implementation of typed, non-real-time process scheduling controls.
///
/// A `ProcessId` may denote either a PID or a TID on Linux. Callers which need
/// PID-reuse resistance must validate the corresponding `ProcessIdentity`
/// before each transaction; this low-level trait intentionally accepts only
/// the identifier present in [`ProcessController`].
#[derive(Clone)]
pub struct LinuxProcessController {
    api: Arc<dyn SchedulingApi>,
    online_path: PathBuf,
}

impl fmt::Debug for LinuxProcessController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinuxProcessController")
            .field("online_path", &self.online_path)
            .finish_non_exhaustive()
    }
}

impl LinuxProcessController {
    /// Open the host scheduler adapter.
    ///
    /// Construction verifies the CPU-online source. Missing `chrt` or
    /// `uclampset` does not disable the controls backed directly by `rustix`;
    /// the affected mutation returns an explicit `Unsupported` error.
    ///
    /// # Errors
    ///
    /// Returns an error if the online CPU list is missing or malformed.
    pub fn host() -> PlatformResult<Self> {
        let online_path = PathBuf::from(ONLINE_CPUS);
        read_online_cpus(&online_path)?;
        let api = RealSchedulingApi {
            proc_root: PathBuf::from(PROC_ROOT),
            chrt: trusted_system_tool("chrt"),
            uclampset: trusted_system_tool("uclampset"),
        };
        Ok(Self {
            api: Arc::new(api),
            online_path,
        })
    }

    /// Read the online CPUs from sysfs now.
    ///
    /// This is deliberately not cached: CPU hotplug is validated immediately
    /// before every affinity mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable or malformed online CPU list.
    pub fn online_cpus(&self) -> PlatformResult<CpuSet> {
        read_online_cpus(&self.online_path)
    }

    #[cfg(test)]
    fn with_api(api: Arc<dyn SchedulingApi>, online_path: PathBuf) -> Self {
        Self { api, online_path }
    }

    fn validate_desired(
        &self,
        desired: &ProcessSchedulingState,
        original: &ProcessSchedulingState,
    ) -> PlatformResult<()> {
        if desired.affinity.is_empty() {
            return Err(PlatformError::invalid(
                &self.online_path,
                "CPU affinity must not be empty",
            ));
        }
        let online = self.online_cpus()?;
        if !desired.affinity.is_subset(&online) {
            return Err(PlatformError::invalid(
                &self.online_path,
                "requested affinity contains an offline or nonexistent CPU",
            ));
        }
        for cpu in &desired.affinity {
            let index = usize::try_from(cpu.0).map_err(|error| {
                PlatformError::invalid(
                    &self.online_path,
                    format!("CPU {} cannot be represented: {error}", cpu.0),
                )
            })?;
            if index >= RustixCpuSet::MAX_CPU {
                return Err(PlatformError::invalid(
                    &self.online_path,
                    format!(
                        "CPU {} exceeds the affinity ABI limit {}",
                        cpu.0,
                        RustixCpuSet::MAX_CPU - 1
                    ),
                ));
            }
        }
        if !(-20..=19).contains(&desired.nice) {
            return Err(PlatformError::invalid(
                scheduling_path(Path::new(PROC_ROOT), ProcessId(0)),
                format!("nice value {} is outside -20..=19", desired.nice),
            ));
        }
        validate_uclamp(desired.uclamp_min, desired.uclamp_max)?;
        if (desired.uclamp_min.is_some() || desired.uclamp_max.is_some())
            && (original.uclamp_min.is_none() || original.uclamp_max.is_none())
        {
            return Err(PlatformError::Unsupported(
                "kernel does not expose readable per-task uclamp state",
            ));
        }
        Ok(())
    }

    fn rollback(
        &self,
        process: ProcessId,
        original: &ProcessSchedulingState,
        applied: &[AppliedChange],
    ) -> Vec<String> {
        let mut failures = Vec::new();
        if applied.contains(&AppliedChange::Uclamp)
            && let (Some(minimum), Some(maximum)) = (original.uclamp_min, original.uclamp_max)
            && let Err(error) = self.api.set_uclamp(process, minimum, maximum)
        {
            failures.push(format!("uclamp: {error}"));
        }
        if applied.contains(&AppliedChange::Policy)
            && let Err(error) = self.api.set_policy(process, original.policy)
        {
            failures.push(format!("policy: {error}"));
        }
        if applied.contains(&AppliedChange::Nice)
            && let Err(error) = self.api.set_nice(process, original.nice)
        {
            failures.push(format!("nice: {error}"));
        }
        if applied.contains(&AppliedChange::Affinity)
            && let Err(error) = self.api.set_affinity(process, &original.affinity)
        {
            failures.push(format!("affinity: {error}"));
        }
        failures
    }

    fn fail_with_rollback(
        &self,
        process: ProcessId,
        original: &ProcessSchedulingState,
        applied: &[AppliedChange],
        failure: PlatformError,
    ) -> PlatformError {
        let rollback = self.rollback(process, original, applied);
        if rollback.is_empty() {
            failure
        } else {
            PlatformError::invalid(
                scheduling_path(Path::new(PROC_ROOT), process),
                format!(
                    "scheduling transaction failed ({failure}); rollback also failed: {}",
                    rollback.join("; ")
                ),
            )
        }
    }
}

impl ProcessController for LinuxProcessController {
    fn read_scheduling(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState> {
        validate_process_id(process)?;
        self.api.read(process)
    }

    fn write_scheduling(
        &self,
        process: ProcessId,
        desired: &ProcessSchedulingState,
    ) -> PlatformResult<ProcessSchedulingState> {
        validate_process_id(process)?;
        let original = self.api.read(process)?;
        self.validate_desired(desired, &original)?;

        let mut applied = Vec::with_capacity(4);
        if desired.affinity != original.affinity {
            self.api.set_affinity(process, &desired.affinity)?;
            applied.push(AppliedChange::Affinity);
        }
        if desired.nice != original.nice {
            if let Err(error) = self.api.set_nice(process, desired.nice) {
                return Err(self.fail_with_rollback(process, &original, &applied, error));
            }
            applied.push(AppliedChange::Nice);
        }
        if desired.policy != original.policy {
            if let Err(error) = self.api.set_policy(process, desired.policy) {
                return Err(self.fail_with_rollback(process, &original, &applied, error));
            }
            applied.push(AppliedChange::Policy);
        }

        let requested_uclamp = desired.uclamp_min.is_some() || desired.uclamp_max.is_some();
        if requested_uclamp {
            let minimum = desired
                .uclamp_min
                .or(original.uclamp_min)
                .expect("validated readable uclamp minimum");
            let maximum = desired
                .uclamp_max
                .or(original.uclamp_max)
                .expect("validated readable uclamp maximum");
            if Some(minimum) != original.uclamp_min || Some(maximum) != original.uclamp_max {
                if let Err(error) = self.api.set_uclamp(process, minimum, maximum) {
                    return Err(self.fail_with_rollback(process, &original, &applied, error));
                }
                applied.push(AppliedChange::Uclamp);
            }
        }

        let readback = match self.api.read(process) {
            Ok(readback) => readback,
            Err(error) => {
                return Err(self.fail_with_rollback(process, &original, &applied, error));
            }
        };
        if !matches_desired(&readback, desired) {
            let failure = PlatformError::invalid(
                scheduling_path(Path::new(PROC_ROOT), process),
                format!(
                    "scheduler readback differs from request: requested {desired:?}, got {readback:?}"
                ),
            );
            return Err(self.fail_with_rollback(process, &original, &applied, failure));
        }
        Ok(readback)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppliedChange {
    Affinity,
    Nice,
    Policy,
    Uclamp,
}

fn matches_desired(actual: &ProcessSchedulingState, desired: &ProcessSchedulingState) -> bool {
    actual.affinity == desired.affinity
        && actual.nice == desired.nice
        && actual.policy == desired.policy
        && desired
            .uclamp_min
            .is_none_or(|minimum| actual.uclamp_min == Some(minimum))
        && desired
            .uclamp_max
            .is_none_or(|maximum| actual.uclamp_max == Some(maximum))
}

#[derive(Debug)]
struct RealSchedulingApi {
    proc_root: PathBuf,
    chrt: Option<PathBuf>,
    uclampset: Option<PathBuf>,
}

impl SchedulingApi for RealSchedulingApi {
    fn read(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState> {
        let pid = rustix_pid(process)?;
        let affinity = sched_getaffinity(Some(pid))
            .map_err(|error| {
                map_rustix_error(
                    "read scheduler affinity",
                    &scheduling_path(&self.proc_root, process),
                    error,
                )
            })
            .map(rustix_to_domain_cpuset)?;
        let nice = getpriority_process(Some(pid))
            .map_err(|error| {
                map_rustix_error(
                    "read nice value",
                    &scheduling_path(&self.proc_root, process),
                    error,
                )
            })
            .and_then(|nice| {
                i8::try_from(nice).map_err(|error| {
                    PlatformError::invalid(
                        scheduling_path(&self.proc_root, process),
                        format!("kernel returned unrepresentable nice value {nice}: {error}"),
                    )
                })
            })?;
        let path = scheduling_path(&self.proc_root, process);
        let contents =
            fs::read_to_string(&path).map_err(|error| map_process_io("read", &path, error))?;
        let parsed = parse_scheduler_file(&path, &contents)?;
        Ok(ProcessSchedulingState {
            affinity,
            nice,
            policy: parsed.policy,
            uclamp_min: parsed.uclamp_min,
            uclamp_max: parsed.uclamp_max,
        })
    }

    fn set_affinity(&self, process: ProcessId, affinity: &CpuSet) -> PlatformResult<()> {
        let pid = rustix_pid(process)?;
        let raw = domain_to_rustix_cpuset(affinity)?;
        sched_setaffinity(Some(pid), &raw).map_err(|error| {
            map_rustix_error(
                "set scheduler affinity",
                &scheduling_path(&self.proc_root, process),
                error,
            )
        })
    }

    fn set_nice(&self, process: ProcessId, nice: i8) -> PlatformResult<()> {
        let pid = rustix_pid(process)?;
        setpriority_process(Some(pid), i32::from(nice)).map_err(|error| {
            map_rustix_error(
                "set nice value",
                &scheduling_path(&self.proc_root, process),
                error,
            )
        })
    }

    fn set_policy(&self, process: ProcessId, policy: SchedulingPolicy) -> PlatformResult<()> {
        let Some(tool) = &self.chrt else {
            return Err(PlatformError::Unsupported(
                "trusted /usr/bin/chrt is unavailable",
            ));
        };
        let policy = match policy {
            SchedulingPolicy::Other => "--other",
            SchedulingPolicy::Batch => "--batch",
            SchedulingPolicy::Idle => "--idle",
        };
        run_util_linux(
            tool,
            &[
                policy.to_owned(),
                "--pid".to_owned(),
                "0".to_owned(),
                process.0.to_string(),
            ],
            &scheduling_path(&self.proc_root, process),
        )
    }

    fn set_uclamp(&self, process: ProcessId, minimum: u16, maximum: u16) -> PlatformResult<()> {
        let Some(tool) = &self.uclampset else {
            return Err(PlatformError::Unsupported(
                "trusted /usr/bin/uclampset is unavailable",
            ));
        };
        run_util_linux(
            tool,
            &[
                "--pid".to_owned(),
                process.0.to_string(),
                "--util-min".to_owned(),
                minimum.to_string(),
                "--util-max".to_owned(),
                maximum.to_string(),
            ],
            &scheduling_path(&self.proc_root, process),
        )
    }
}

fn run_util_linux(tool: &Path, arguments: &[String], resource: &Path) -> PlatformResult<()> {
    let child = Command::new(tool)
        .args(arguments)
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| PlatformError::io("execute util-linux scheduler tool", tool, error))?;
    let output = wait_with_deadline(child, tool, resource)?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr);
        Err(PlatformError::invalid(
            resource,
            format!(
                "{} exited with {}: {}",
                tool.display(),
                output.status,
                detail.trim()
            ),
        ))
    }
}

fn wait_with_deadline(mut child: Child, tool: &Path, resource: &Path) -> PlatformResult<Output> {
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child.wait_with_output().map_err(|error| {
                    PlatformError::io("collect util-linux scheduler tool", tool, error)
                });
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(TOOL_POLL_INTERVAL),
            Ok(None) => {
                let kill_error = child.kill().err();
                let _ = child.wait();
                return Err(PlatformError::invalid(
                    resource,
                    kill_error.map_or_else(
                        || {
                            format!(
                                "{} exceeded the {:?} execution deadline",
                                tool.display(),
                                TOOL_TIMEOUT
                            )
                        },
                        |error| {
                            format!(
                                "{} exceeded the {:?} execution deadline and could not be killed: {error}",
                                tool.display(),
                                TOOL_TIMEOUT
                            )
                        },
                    ),
                ));
            }
            Err(error) => {
                return Err(PlatformError::io(
                    "poll util-linux scheduler tool",
                    tool,
                    error,
                ));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ParsedScheduler {
    policy: SchedulingPolicy,
    uclamp_min: Option<u16>,
    uclamp_max: Option<u16>,
}

fn parse_scheduler_file(path: &Path, contents: &str) -> PlatformResult<ParsedScheduler> {
    let mut policy = None;
    let mut uclamp_min = None;
    let mut uclamp_max = None;
    for line in contents.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .split_whitespace()
            .next()
            .ok_or_else(|| PlatformError::invalid(path, format!("empty `{}` field", key.trim())))?;
        match key.trim() {
            "policy" => {
                let raw = value.parse::<u32>().map_err(|error| {
                    PlatformError::invalid(path, format!("invalid scheduler policy: {error}"))
                })?;
                let base = raw & !0x4000_0000;
                policy = Some(match base {
                    0 => SchedulingPolicy::Other,
                    3 => SchedulingPolicy::Batch,
                    5 => SchedulingPolicy::Idle,
                    1 | 2 | 6 | 7 => {
                        return Err(PlatformError::Unsupported(
                            "real-time, deadline, and sched_ext tasks are outside v1 control",
                        ));
                    }
                    _ => {
                        return Err(PlatformError::invalid(
                            path,
                            format!("unknown Linux scheduler policy {base}"),
                        ));
                    }
                });
            }
            "uclamp.min" => uclamp_min = Some(parse_uclamp_value(path, "uclamp.min", value)?),
            "uclamp.max" => uclamp_max = Some(parse_uclamp_value(path, "uclamp.max", value)?),
            _ => {}
        }
    }
    let policy = policy.ok_or_else(|| PlatformError::invalid(path, "missing `policy` field"))?;
    validate_uclamp(uclamp_min, uclamp_max)?;
    Ok(ParsedScheduler {
        policy,
        uclamp_min,
        uclamp_max,
    })
}

fn parse_uclamp_value(path: &Path, field: &str, value: &str) -> PlatformResult<u16> {
    let parsed = value
        .parse::<u16>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid {field}: {error}")))?;
    if parsed > UCLAMP_MAX {
        return Err(PlatformError::invalid(
            path,
            format!("{field} {parsed} exceeds {UCLAMP_MAX}"),
        ));
    }
    Ok(parsed)
}

fn validate_uclamp(minimum: Option<u16>, maximum: Option<u16>) -> PlatformResult<()> {
    if minimum.is_some_and(|value| value > UCLAMP_MAX)
        || maximum.is_some_and(|value| value > UCLAMP_MAX)
    {
        return Err(PlatformError::invalid(
            "/proc/<tid>/sched",
            format!("uclamp values must be within 0..={UCLAMP_MAX}"),
        ));
    }
    if let (Some(minimum), Some(maximum)) = (minimum, maximum)
        && minimum > maximum
    {
        return Err(PlatformError::invalid(
            "/proc/<tid>/sched",
            format!("uclamp minimum {minimum} exceeds maximum {maximum}"),
        ));
    }
    Ok(())
}

pub(crate) fn parse_cpu_list(path: &Path, contents: &str) -> PlatformResult<CpuSet> {
    let mut cpus = CpuSet::new();
    for item in contents.trim().split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(PlatformError::invalid(path, "empty CPU-list element"));
        }
        if let Some((start, end)) = item.split_once('-') {
            let start = parse_cpu_id(path, start)?;
            let end = parse_cpu_id(path, end)?;
            if start > end {
                return Err(PlatformError::invalid(
                    path,
                    format!("reversed CPU range {start}-{end}"),
                ));
            }
            ensure_affinity_cpu(path, end)?;
            for cpu in start..=end {
                cpus.insert(CpuId(cpu));
            }
        } else {
            let cpu = parse_cpu_id(path, item)?;
            ensure_affinity_cpu(path, cpu)?;
            cpus.insert(CpuId(cpu));
        }
    }
    if cpus.is_empty() {
        return Err(PlatformError::invalid(path, "CPU list is empty"));
    }
    Ok(cpus)
}

fn parse_cpu_id(path: &Path, value: &str) -> PlatformResult<u32> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid CPU ID: {error}")))
}

fn ensure_affinity_cpu(path: &Path, cpu: u32) -> PlatformResult<()> {
    let index = usize::try_from(cpu)
        .map_err(|error| PlatformError::invalid(path, format!("invalid CPU ID {cpu}: {error}")))?;
    if index >= RustixCpuSet::MAX_CPU {
        return Err(PlatformError::invalid(
            path,
            format!(
                "CPU {cpu} exceeds the Linux affinity ABI limit {}",
                RustixCpuSet::MAX_CPU - 1
            ),
        ));
    }
    Ok(())
}

fn read_online_cpus(path: &Path) -> PlatformResult<CpuSet> {
    let contents = fs::read_to_string(path)
        .map_err(|error| PlatformError::io("read online CPUs", path, error))?;
    parse_cpu_list(path, &contents)
}

fn domain_to_rustix_cpuset(affinity: &CpuSet) -> PlatformResult<RustixCpuSet> {
    let mut raw = RustixCpuSet::new();
    for cpu in affinity {
        let index = usize::try_from(cpu.0).map_err(|error| {
            PlatformError::invalid(
                ONLINE_CPUS,
                format!("CPU {} cannot be represented: {error}", cpu.0),
            )
        })?;
        if index >= RustixCpuSet::MAX_CPU {
            return Err(PlatformError::invalid(
                ONLINE_CPUS,
                format!(
                    "CPU {} exceeds affinity ABI limit {}",
                    cpu.0,
                    RustixCpuSet::MAX_CPU - 1
                ),
            ));
        }
        raw.set(index);
    }
    Ok(raw)
}

fn rustix_to_domain_cpuset(raw: RustixCpuSet) -> CpuSet {
    (0..RustixCpuSet::MAX_CPU)
        .filter(|cpu| raw.is_set(*cpu))
        .filter_map(|cpu| u32::try_from(cpu).ok())
        .map(CpuId)
        .collect()
}

fn validate_process_id(process: ProcessId) -> PlatformResult<()> {
    raw_process_id(process).map(|_| ())
}

fn raw_process_id(process: ProcessId) -> PlatformResult<i32> {
    let raw = i32::try_from(process.0).map_err(|error| {
        PlatformError::invalid(
            scheduling_path(Path::new(PROC_ROOT), process),
            format!("PID {} is outside Linux pid_t range: {error}", process.0),
        )
    })?;
    if raw <= 0 {
        return Err(PlatformError::invalid(
            scheduling_path(Path::new(PROC_ROOT), process),
            "PID/TID zero is not accepted because it aliases the caller",
        ));
    }
    Ok(raw)
}

fn rustix_pid(process: ProcessId) -> PlatformResult<Pid> {
    let raw = raw_process_id(process)?;
    Pid::from_raw(raw).ok_or_else(|| {
        PlatformError::invalid(
            scheduling_path(Path::new(PROC_ROOT), process),
            "invalid zero PID",
        )
    })
}

fn scheduling_path(proc_root: &Path, process: ProcessId) -> PathBuf {
    proc_root.join(process.0.to_string()).join("sched")
}

fn map_rustix_error(
    operation: &'static str,
    path: &Path,
    error: rustix::io::Errno,
) -> PlatformError {
    let io_error = io::Error::from_raw_os_error(error.raw_os_error());
    map_process_io(operation, path, io_error)
}

fn map_process_io(operation: &'static str, path: &Path, error: io::Error) -> PlatformError {
    if error.kind() == io::ErrorKind::NotFound {
        PlatformError::Disappeared(path.display().to_string())
    } else {
        PlatformError::io(operation, path, error)
    }
}

fn trusted_system_tool(name: &str) -> Option<PathBuf> {
    [
        Path::new("/usr/bin").join(name),
        Path::new("/bin").join(name),
    ]
    .iter()
    .find_map(|candidate| {
        let canonical = candidate.canonicalize().ok()?;
        let metadata = canonical.metadata().ok()?;
        if metadata.is_file()
            && metadata.uid() == 0
            && metadata.mode() & 0o022 == 0
            && canonical.is_absolute()
        {
            Some(canonical)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_sparse_dynamic_cpu_lists() {
        let cpus = parse_cpu_list(Path::new("online"), "0-3,8,65,511\n").unwrap();
        assert_eq!(
            cpus.iter().copied().collect::<Vec<_>>(),
            [0, 1, 2, 3, 8, 65, 511].map(CpuId)
        );
    }

    #[test]
    fn rejects_malformed_cpu_lists() {
        assert!(parse_cpu_list(Path::new("online"), "4-2").is_err());
        assert!(parse_cpu_list(Path::new("online"), "0,,2").is_err());
        assert!(parse_cpu_list(Path::new("online"), "").is_err());
        assert!(
            parse_cpu_list(Path::new("online"), &format!("0-{}", RustixCpuSet::MAX_CPU)).is_err()
        );
    }

    #[test]
    fn parses_supported_scheduler_state() {
        let parsed = parse_scheduler_file(
            Path::new("sched"),
            "policy : 3\nuclamp.min : 128\nuclamp.max : 900\n",
        )
        .unwrap();
        assert_eq!(parsed.policy, SchedulingPolicy::Batch);
        assert_eq!(parsed.uclamp_min, Some(128));
        assert_eq!(parsed.uclamp_max, Some(900));
    }

    #[test]
    fn refuses_realtime_and_invalid_uclamp() {
        assert!(
            parse_scheduler_file(
                Path::new("sched"),
                "policy : 1\nuclamp.min : 0\nuclamp.max : 1024\n",
            )
            .is_err()
        );
        assert!(
            parse_scheduler_file(
                Path::new("sched"),
                "policy : 0\nuclamp.min : 900\nuclamp.max : 100\n",
            )
            .is_err()
        );
    }

    #[test]
    fn validates_hotplug_before_any_mutation() {
        let directory = tempdir().unwrap();
        let online = directory.path().join("online");
        fs::write(&online, "0-2\n").unwrap();
        let original = state(&[0, 1], 0, SchedulingPolicy::Other, Some(0), Some(1024));
        let api = Arc::new(FakeSchedulingApi::new(original));
        let controller = LinuxProcessController::with_api(api.clone(), online);
        let requested = state(&[0, 3], 0, SchedulingPolicy::Other, None, None);

        assert!(
            controller
                .write_scheduling(ProcessId(42), &requested)
                .is_err()
        );
        assert!(api.operations.lock().unwrap().is_empty());
    }

    #[test]
    fn writes_then_reads_back_all_requested_fields() {
        let directory = tempdir().unwrap();
        let online = directory.path().join("online");
        fs::write(&online, "0-7\n").unwrap();
        let original = state(&[0, 1], 0, SchedulingPolicy::Other, Some(0), Some(1024));
        let api = Arc::new(FakeSchedulingApi::new(original));
        let controller = LinuxProcessController::with_api(api.clone(), online);
        let requested = state(&[4, 7], 5, SchedulingPolicy::Batch, Some(256), Some(768));

        let applied = controller
            .write_scheduling(ProcessId(42), &requested)
            .unwrap();
        assert_eq!(applied, requested);
        assert_eq!(
            *api.operations.lock().unwrap(),
            ["affinity", "nice", "policy", "uclamp"]
        );
    }

    #[test]
    fn failed_partial_transaction_restores_original_state() {
        let directory = tempdir().unwrap();
        let online = directory.path().join("online");
        fs::write(&online, "0-7\n").unwrap();
        let original = state(&[0, 1], 0, SchedulingPolicy::Other, Some(0), Some(1024));
        let api = Arc::new(FakeSchedulingApi::new(original.clone()));
        *api.fail_on.lock().unwrap() = Some("policy");
        let controller = LinuxProcessController::with_api(api.clone(), online);
        let requested = state(&[4], 5, SchedulingPolicy::Batch, None, None);

        assert!(
            controller
                .write_scheduling(ProcessId(42), &requested)
                .is_err()
        );
        assert_eq!(*api.state.lock().unwrap(), original);
        assert_eq!(
            *api.operations.lock().unwrap(),
            ["affinity", "nice", "policy", "nice", "affinity"]
        );
    }

    #[test]
    fn rejects_pid_zero_without_touching_kernel() {
        let controller = LinuxProcessController {
            api: Arc::new(FakeSchedulingApi::new(state(
                &[0],
                0,
                SchedulingPolicy::Other,
                Some(0),
                Some(1024),
            ))),
            online_path: PathBuf::from("/does/not/matter"),
        };
        assert!(controller.read_scheduling(ProcessId(0)).is_err());
    }

    #[test]
    fn reads_current_process_without_privilege() {
        let controller = LinuxProcessController::host().unwrap();
        let state = controller
            .read_scheduling(ProcessId(std::process::id()))
            .unwrap();
        assert!(!state.affinity.is_empty());
        assert!((-20..=19).contains(&state.nice));
    }

    fn state(
        cpus: &[u32],
        nice: i8,
        policy: SchedulingPolicy,
        minimum: Option<u16>,
        maximum: Option<u16>,
    ) -> ProcessSchedulingState {
        ProcessSchedulingState {
            affinity: cpus.iter().copied().map(CpuId).collect(),
            nice,
            policy,
            uclamp_min: minimum,
            uclamp_max: maximum,
        }
    }

    #[derive(Debug)]
    struct FakeSchedulingApi {
        state: Mutex<ProcessSchedulingState>,
        operations: Mutex<Vec<&'static str>>,
        fail_on: Mutex<Option<&'static str>>,
    }

    impl FakeSchedulingApi {
        fn new(state: ProcessSchedulingState) -> Self {
            Self {
                state: Mutex::new(state),
                operations: Mutex::new(Vec::new()),
                fail_on: Mutex::new(None),
            }
        }

        fn record(&self, operation: &'static str) -> PlatformResult<()> {
            self.operations.lock().unwrap().push(operation);
            if *self.fail_on.lock().unwrap() == Some(operation) {
                *self.fail_on.lock().unwrap() = None;
                Err(PlatformError::invalid("fake", "injected failure"))
            } else {
                Ok(())
            }
        }
    }

    impl SchedulingApi for FakeSchedulingApi {
        fn read(&self, _process: ProcessId) -> PlatformResult<ProcessSchedulingState> {
            Ok(self.state.lock().unwrap().clone())
        }

        fn set_affinity(&self, _process: ProcessId, affinity: &CpuSet) -> PlatformResult<()> {
            self.record("affinity")?;
            self.state.lock().unwrap().affinity = affinity.clone();
            Ok(())
        }

        fn set_nice(&self, _process: ProcessId, nice: i8) -> PlatformResult<()> {
            self.record("nice")?;
            self.state.lock().unwrap().nice = nice;
            Ok(())
        }

        fn set_policy(&self, _process: ProcessId, policy: SchedulingPolicy) -> PlatformResult<()> {
            self.record("policy")?;
            self.state.lock().unwrap().policy = policy;
            Ok(())
        }

        fn set_uclamp(
            &self,
            _process: ProcessId,
            minimum: u16,
            maximum: u16,
        ) -> PlatformResult<()> {
            self.record("uclamp")?;
            let mut state = self.state.lock().unwrap();
            state.uclamp_min = Some(minimum);
            state.uclamp_max = Some(maximum);
            Ok(())
        }
    }
}
