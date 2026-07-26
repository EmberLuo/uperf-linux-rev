//! Safe Linux process and thread scheduling controls.
//!
//! The public adapter contains no FFI. Affinity and nice values are handled by
//! `rustix`. Linux does not currently expose `sched_setscheduler(2)` or
//! `sched_setattr(2)` through `rustix`, so policy and uclamp changes are
//! delegated to root-owned util-linux tools and always verified from
//! `/proc/<tid>/sched` and `/proc/<tid>/stat`.

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
    PlatformError, PlatformResult, ProcessController, ProcessSchedulingState, SchedulingClass,
};

const PROC_ROOT: &str = "/proc";
const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";
const UCLAMP_MAX: u16 = 1024;
const LINUX_RT_PRIORITY_MAX: u8 = 99;
const TOOL_TIMEOUT: Duration = Duration::from_secs(1);
const TOOL_POLL_INTERVAL: Duration = Duration::from_millis(5);

trait SchedulingApi: Send + Sync {
    fn read(&self, process: ProcessId) -> PlatformResult<ProcessSchedulingState>;
    fn set_affinity(&self, process: ProcessId, affinity: &CpuSet) -> PlatformResult<()>;
    fn set_nice(&self, process: ProcessId, nice: i8) -> PlatformResult<()>;
    fn set_policy(
        &self,
        process: ProcessId,
        policy: SchedulingClass,
        rt_priority: Option<u8>,
    ) -> PlatformResult<()>;
    fn set_uclamp(&self, process: ProcessId, minimum: u16, maximum: u16) -> PlatformResult<()>;
}

/// Linux implementation of typed process scheduling controls.
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
        validate_policy_priority(
            Path::new("/proc/<tid>/sched"),
            desired.policy,
            desired.rt_priority,
        )?;
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
            && let Err(error) = self
                .api
                .set_policy(process, original.policy, original.rt_priority)
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
        if desired.policy != original.policy || desired.rt_priority != original.rt_priority {
            if let Err(error) = self
                .api
                .set_policy(process, desired.policy, desired.rt_priority)
            {
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
        && actual.rt_priority == desired.rt_priority
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
        let stat_path = scheduler_stat_path(&self.proc_root, process);
        let stat_contents = fs::read_to_string(&stat_path)
            .map_err(|error| map_process_io("read", &stat_path, error))?;
        let stat = parse_scheduler_stat(&stat_path, &stat_contents)?;
        if parsed.policy != stat.policy {
            return Err(PlatformError::invalid(
                &stat_path,
                format!(
                    "scheduler policy changed during read: sched reported {:?}, stat reported {:?}",
                    parsed.policy, stat.policy
                ),
            ));
        }
        Ok(ProcessSchedulingState {
            affinity,
            nice,
            policy: stat.policy,
            rt_priority: stat.rt_priority,
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

    fn set_policy(
        &self,
        process: ProcessId,
        policy: SchedulingClass,
        rt_priority: Option<u8>,
    ) -> PlatformResult<()> {
        let Some(tool) = &self.chrt else {
            return Err(PlatformError::Unsupported(
                "trusted /usr/bin/chrt is unavailable",
            ));
        };
        let resource = scheduling_path(&self.proc_root, process);
        run_util_linux(
            tool,
            &chrt_arguments(&resource, process, policy, rt_priority)?,
            &resource,
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
            &uclampset_arguments(process, minimum, maximum),
            &scheduling_path(&self.proc_root, process),
        )
    }
}

fn chrt_arguments(
    resource: &Path,
    process: ProcessId,
    policy: SchedulingClass,
    rt_priority: Option<u8>,
) -> PlatformResult<[String; 4]> {
    validate_policy_priority(resource, policy, rt_priority)?;
    let (flag, priority) = match policy {
        SchedulingClass::Other => ("--other", 0),
        SchedulingClass::Batch => ("--batch", 0),
        SchedulingClass::Idle => ("--idle", 0),
        SchedulingClass::Fifo => (
            "--fifo",
            rt_priority.expect("validated FIFO priority is present"),
        ),
    };
    Ok([
        flag.to_owned(),
        "--pid".to_owned(),
        priority.to_string(),
        process.0.to_string(),
    ])
}

fn uclampset_arguments(process: ProcessId, minimum: u16, maximum: u16) -> [String; 6] {
    // The short forms are the stable util-linux interface. Some released
    // versions, including Ubuntu's, do not recognize --util-min/--util-max.
    [
        "-m".to_owned(),
        minimum.to_string(),
        "-M".to_owned(),
        maximum.to_string(),
        "-p".to_owned(),
        process.0.to_string(),
    ]
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
    policy: SchedulingClass,
    uclamp_min: Option<u16>,
    uclamp_max: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ParsedSchedulerStat {
    policy: SchedulingClass,
    rt_priority: Option<u8>,
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
                policy = Some(scheduling_class_from_linux(path, raw)?);
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

fn parse_scheduler_stat(path: &Path, contents: &str) -> PlatformResult<ParsedSchedulerStat> {
    // `comm` is parenthesized and may itself contain spaces or `)`.  No field
    // after comm contains `)`, so the final delimiter is unambiguous.
    let comm_end = contents.rfind(") ").ok_or_else(|| {
        PlatformError::invalid(path, "missing closing process name in stat record")
    })?;
    let fields = contents[comm_end + 2..]
        .split_whitespace()
        .collect::<Vec<_>>();
    // `fields[0]` is stat field 3. rt_priority and policy are fields 40/41.
    let raw_priority = fields
        .get(37)
        .ok_or_else(|| PlatformError::invalid(path, "stat record is missing rt_priority"))?
        .parse::<u32>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid rt_priority: {error}")))?;
    let raw_policy = fields
        .get(38)
        .ok_or_else(|| PlatformError::invalid(path, "stat record is missing policy"))?
        .parse::<u32>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid policy: {error}")))?;
    let policy = scheduling_class_from_linux(path, raw_policy)?;
    let rt_priority = match policy {
        SchedulingClass::Fifo => Some(u8::try_from(raw_priority).map_err(|error| {
            PlatformError::invalid(
                path,
                format!("FIFO priority {raw_priority} cannot be represented: {error}"),
            )
        })?),
        SchedulingClass::Other | SchedulingClass::Batch | SchedulingClass::Idle => {
            if raw_priority != 0 {
                return Err(PlatformError::invalid(
                    path,
                    format!("non-real-time policy {policy:?} reported rt_priority {raw_priority}"),
                ));
            }
            None
        }
    };
    validate_policy_priority(path, policy, rt_priority)?;
    Ok(ParsedSchedulerStat {
        policy,
        rt_priority,
    })
}

fn scheduling_class_from_linux(path: &Path, raw: u32) -> PlatformResult<SchedulingClass> {
    let base = raw & !0x4000_0000;
    match base {
        0 => Ok(SchedulingClass::Other),
        1 => Ok(SchedulingClass::Fifo),
        3 => Ok(SchedulingClass::Batch),
        5 => Ok(SchedulingClass::Idle),
        2 | 6 | 7 => Err(PlatformError::Unsupported(
            "SCHED_RR, deadline, and sched_ext tasks are outside controlled scheduling classes",
        )),
        _ => Err(PlatformError::invalid(
            path,
            format!("unknown Linux scheduler policy {base}"),
        )),
    }
}

fn validate_policy_priority(
    path: &Path,
    policy: SchedulingClass,
    rt_priority: Option<u8>,
) -> PlatformResult<()> {
    match (policy, rt_priority) {
        (SchedulingClass::Fifo, Some(priority))
            if (1..=LINUX_RT_PRIORITY_MAX).contains(&priority) =>
        {
            Ok(())
        }
        (SchedulingClass::Fifo, Some(priority)) => Err(PlatformError::invalid(
            path,
            format!("FIFO priority {priority} is outside 1..={LINUX_RT_PRIORITY_MAX}"),
        )),
        (SchedulingClass::Fifo, None) => Err(PlatformError::invalid(
            path,
            "SCHED_FIFO requires an explicit real-time priority",
        )),
        (
            SchedulingClass::Other | SchedulingClass::Batch | SchedulingClass::Idle,
            Some(priority),
        ) => Err(PlatformError::invalid(
            path,
            format!("non-real-time policy cannot carry rt_priority {priority}"),
        )),
        (SchedulingClass::Other | SchedulingClass::Batch | SchedulingClass::Idle, None) => Ok(()),
    }
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

fn scheduler_stat_path(proc_root: &Path, process: ProcessId) -> PathBuf {
    proc_root.join(process.0.to_string()).join("stat")
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
        assert_eq!(parsed.policy, SchedulingClass::Batch);
        assert_eq!(parsed.uclamp_min, Some(128));
        assert_eq!(parsed.uclamp_max, Some(900));
    }

    #[test]
    fn parses_exact_fifo_priority_from_proc_stat() {
        let sched = parse_scheduler_file(
            Path::new("sched"),
            "policy : 1\nuclamp.min : 0\nuclamp.max : 1024\n",
        )
        .unwrap();
        assert_eq!(sched.policy, SchedulingClass::Fifo);

        let mut fields = vec!["0"; 39];
        fields[0] = "S";
        fields[37] = "12";
        fields[38] = "1";
        let contents = format!("42 (render ) worker) {}\n", fields.join(" "));

        let parsed = parse_scheduler_stat(Path::new("stat"), &contents).unwrap();

        assert_eq!(parsed.policy, SchedulingClass::Fifo);
        assert_eq!(parsed.rt_priority, Some(12));
    }

    #[test]
    fn uclampset_uses_the_portable_util_linux_cli() {
        assert_eq!(
            uclampset_arguments(ProcessId(42), 205, 512),
            ["-m", "205", "-M", "512", "-p", "42"].map(str::to_owned)
        );
    }

    #[test]
    fn chrt_uses_an_explicit_fifo_priority() {
        assert_eq!(
            chrt_arguments(
                Path::new("sched"),
                ProcessId(42),
                SchedulingClass::Fifo,
                Some(20),
            )
            .unwrap(),
            ["--fifo", "--pid", "20", "42"].map(str::to_owned)
        );
        assert!(
            chrt_arguments(
                Path::new("sched"),
                ProcessId(42),
                SchedulingClass::Fifo,
                None,
            )
            .is_err()
        );
        assert!(
            chrt_arguments(
                Path::new("sched"),
                ProcessId(42),
                SchedulingClass::Other,
                Some(1),
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_sched_rr_and_invalid_uclamp() {
        assert!(
            parse_scheduler_file(
                Path::new("sched"),
                "policy : 2\nuclamp.min : 0\nuclamp.max : 1024\n",
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
        let original = state(&[0, 1], 0, SchedulingClass::Other, Some(0), Some(1024));
        let api = Arc::new(FakeSchedulingApi::new(original));
        let controller = LinuxProcessController::with_api(api.clone(), online);
        let requested = state(&[0, 3], 0, SchedulingClass::Other, None, None);

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
        let original = state(&[0, 1], 0, SchedulingClass::Other, Some(0), Some(1024));
        let api = Arc::new(FakeSchedulingApi::new(original));
        let controller = LinuxProcessController::with_api(api.clone(), online);
        let requested = state(&[4, 7], 5, SchedulingClass::Batch, Some(256), Some(768));

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
        let original = state(&[0, 1], 0, SchedulingClass::Other, Some(0), Some(1024));
        let api = Arc::new(FakeSchedulingApi::new(original.clone()));
        *api.fail_on.lock().unwrap() = Some("policy");
        let controller = LinuxProcessController::with_api(api.clone(), online);
        let requested = state(&[4], 5, SchedulingClass::Batch, None, None);

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
    fn fifo_priority_is_verified_and_rolled_back_as_part_of_policy() {
        let directory = tempdir().unwrap();
        let online = directory.path().join("online");
        fs::write(&online, "0-7\n").unwrap();
        let original = state(&[0, 1], 0, SchedulingClass::Other, Some(0), Some(1024));
        let api = Arc::new(FakeSchedulingApi::new(original.clone()));
        let controller = LinuxProcessController::with_api(api.clone(), online);
        let mut requested = original.clone();
        requested.policy = SchedulingClass::Fifo;
        requested.rt_priority = Some(20);
        requested.uclamp_min = Some(256);
        *api.fail_on.lock().unwrap() = Some("uclamp");

        assert!(
            controller
                .write_scheduling(ProcessId(42), &requested)
                .is_err()
        );
        assert_eq!(*api.state.lock().unwrap(), original);
        assert_eq!(
            *api.operations.lock().unwrap(),
            ["policy", "uclamp", "policy"]
        );
    }

    #[test]
    fn rejects_pid_zero_without_touching_kernel() {
        let controller = LinuxProcessController {
            api: Arc::new(FakeSchedulingApi::new(state(
                &[0],
                0,
                SchedulingClass::Other,
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
        policy: SchedulingClass,
        minimum: Option<u16>,
        maximum: Option<u16>,
    ) -> ProcessSchedulingState {
        ProcessSchedulingState {
            affinity: cpus.iter().copied().map(CpuId).collect(),
            nice,
            policy,
            rt_priority: None,
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

        fn set_policy(
            &self,
            _process: ProcessId,
            policy: SchedulingClass,
            rt_priority: Option<u8>,
        ) -> PlatformResult<()> {
            self.record("policy")?;
            let mut state = self.state.lock().unwrap();
            state.policy = policy;
            state.rt_priority = rt_priority;
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
