//! Read-only procfs observations.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use uperf_core::{CpuId, MonotonicMillis, ProcessId, ProcessIdentity, ProcessInfo, UserId};
use uperf_platform::{Clock, CpuTimeSnapshot, CpuTimes, PlatformError, PlatformResult, ProcReader};

/// Monotonic clock relative to adapter construction.
#[derive(Clone, Debug)]
pub struct LinuxClock {
    epoch: Instant,
}

impl Default for LinuxClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Clock for LinuxClock {
    fn monotonic_millis(&self) -> MonotonicMillis {
        let millis = self.epoch.elapsed().as_millis();
        MonotonicMillis(u64::try_from(millis).unwrap_or(u64::MAX))
    }
}

/// A read-only procfs adapter rooted at a host or fixture directory.
#[derive(Clone, Debug)]
pub struct LinuxProc {
    root: PathBuf,
    clock: LinuxClock,
}

impl LinuxProc {
    /// Construct an adapter after validating the procfs root.
    ///
    /// # Errors
    ///
    /// Returns an error if the supplied root does not exist or cannot be
    /// canonicalized.
    pub fn new(root: impl AsRef<Path>, clock: LinuxClock) -> PlatformResult<Self> {
        let requested = root.as_ref();
        let root = requested
            .canonicalize()
            .map_err(|error| PlatformError::io("canonicalize procfs root", requested, error))?;
        Ok(Self { root, clock })
    }

    /// Physical procfs root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl ProcReader for LinuxProc {
    fn cpu_times(&self) -> PlatformResult<CpuTimeSnapshot> {
        let path = self.root.join("stat");
        let contents =
            fs::read_to_string(&path).map_err(|error| PlatformError::io("read", &path, error))?;
        parse_cpu_times(&path, &contents, self.clock.monotonic_millis())
    }

    fn list_processes(&self) -> PlatformResult<Vec<ProcessId>> {
        list_numeric_directories(&self.root, "list procfs")
    }

    fn list_threads(&self, process: ProcessId) -> PlatformResult<Vec<ProcessId>> {
        let task_directory = self.root.join(process.0.to_string()).join("task");
        list_numeric_directories(&task_directory, "list process tasks")
    }

    fn process_identity(&self, pid: ProcessId) -> PlatformResult<ProcessInfo> {
        let directory = self.root.join(pid.0.to_string());
        let stat_path = directory.join("stat");
        let stat =
            fs::read(&stat_path).map_err(|error| map_proc_io("read", stat_path.clone(), error))?;
        let before = parse_process_stat_bytes(&stat_path, &stat, pid)?;

        let status_path = directory.join("status");
        let status = fs::read(&status_path)
            .map_err(|error| map_proc_io("read", status_path.clone(), error))?;
        let uids = parse_uids(&status_path, &String::from_utf8_lossy(&status))?;
        let uid = uids[0];
        let owner_control_safe = uids.iter().all(|candidate| *candidate == uid);

        let executable = match fs::read_link(directory.join("exe")) {
            Ok(path) => Some(path.to_string_lossy().into_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => None,
            Err(error) => {
                return Err(map_proc_io("read link", directory.join("exe"), error));
            }
        };

        // The PID may be recycled between any two procfs reads. Re-reading
        // stat after status/exe turns a mixed-generation observation into a
        // disappeared identity instead of a trusted Frankenstein record.
        let stat =
            fs::read(&stat_path).map_err(|error| map_proc_io("read", stat_path.clone(), error))?;
        let after = parse_process_stat_bytes(&stat_path, &stat, pid)?;
        if before.start_time_ticks != after.start_time_ticks {
            return Err(PlatformError::Disappeared(format!(
                "process {} changed identity while procfs was sampled",
                pid.get()
            )));
        }

        Ok(ProcessInfo {
            identity: ProcessIdentity {
                pid,
                start_time_ticks: after.start_time_ticks,
                uid: UserId(uid),
            },
            owner_control_safe,
            comm: after.comm,
            executable,
            desktop_id: None,
        })
    }
}

fn list_numeric_directories(
    directory: &Path,
    operation: &'static str,
) -> PlatformResult<Vec<ProcessId>> {
    let entries =
        fs::read_dir(directory).map_err(|error| PlatformError::io(operation, directory, error))?;
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(operation, directory, error))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = name.parse::<u32>() else {
            continue;
        };
        ids.push(ProcessId(id));
    }
    ids.sort_unstable();
    Ok(ids)
}

#[derive(Debug, Eq, PartialEq)]
struct ParsedProcessStat {
    comm: String,
    start_time_ticks: u64,
}

#[cfg(test)]
fn parse_process_stat(
    path: &Path,
    contents: &str,
    expected_pid: ProcessId,
) -> PlatformResult<ParsedProcessStat> {
    parse_process_stat_bytes(path, contents.as_bytes(), expected_pid)
}

fn parse_process_stat_bytes(
    path: &Path,
    contents: &[u8],
    expected_pid: ProcessId,
) -> PlatformResult<ParsedProcessStat> {
    let open = contents
        .iter()
        .position(|byte| *byte == b'(')
        .ok_or_else(|| PlatformError::invalid(path, "missing command opening parenthesis"))?;
    let close = contents
        .iter()
        .rposition(|byte| *byte == b')')
        .filter(|close| *close > open)
        .ok_or_else(|| PlatformError::invalid(path, "missing command closing parenthesis"))?;

    let parsed_pid = std::str::from_utf8(&contents[..open])
        .map_err(|error| PlatformError::invalid(path, format!("invalid PID bytes: {error}")))?
        .trim()
        .parse::<u32>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid PID: {error}")))?;
    if parsed_pid != expected_pid.0 {
        return Err(PlatformError::invalid(
            path,
            format!(
                "PID in stat ({parsed_pid}) differs from directory ({})",
                expected_pid.0
            ),
        ));
    }

    // Tokens following comm begin at proc(5) field 3 (`state`).  starttime is
    // field 22, therefore index 19 in this slice.
    let tail_text = std::str::from_utf8(&contents[close + 1..])
        .map_err(|error| PlatformError::invalid(path, format!("invalid stat fields: {error}")))?;
    let tail: Vec<&str> = tail_text.split_whitespace().collect();
    let start_time = tail
        .get(19)
        .ok_or_else(|| PlatformError::invalid(path, "stat has fewer than 22 fields"))?
        .parse::<u64>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid starttime: {error}")))?;

    Ok(ParsedProcessStat {
        // comm is display/matching metadata, not part of stable identity. A
        // hostile or unusual byte sequence must not make journal recovery
        // unable to verify pid/starttime/uid.
        comm: String::from_utf8_lossy(&contents[open + 1..close]).into_owned(),
        start_time_ticks: start_time,
    })
}

fn parse_uids(path: &Path, contents: &str) -> PlatformResult<[u32; 4]> {
    let uid_line = contents
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or_else(|| PlatformError::invalid(path, "missing Uid line"))?;
    let values = uid_line["Uid:".len()..]
        .split_whitespace()
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|error| PlatformError::invalid(path, format!("invalid UID: {error}")))
        })
        .collect::<PlatformResult<Vec<_>>>()?;
    values.try_into().map_err(|values: Vec<u32>| {
        PlatformError::invalid(
            path,
            format!("Uid line has {} values instead of four", values.len()),
        )
    })
}

fn parse_cpu_times(
    path: &Path,
    contents: &str,
    observed_at: MonotonicMillis,
) -> PlatformResult<CpuTimeSnapshot> {
    let mut aggregate = None;
    let mut cpus = BTreeMap::new();

    for line in contents.lines() {
        let Some(label) = line.split_whitespace().next() else {
            continue;
        };
        if !label.starts_with("cpu") {
            continue;
        }

        let times = parse_cpu_line(path, line)?;
        if label == "cpu" {
            if aggregate.replace(times).is_some() {
                return Err(PlatformError::invalid(path, "duplicate aggregate cpu line"));
            }
            continue;
        }

        let id = label["cpu".len()..]
            .parse::<u32>()
            .map_err(|error| PlatformError::invalid(path, format!("invalid CPU label: {error}")))?;
        if cpus.insert(CpuId(id), times).is_some() {
            return Err(PlatformError::invalid(
                path,
                format!("duplicate cpu{id} line"),
            ));
        }
    }

    let aggregate =
        aggregate.ok_or_else(|| PlatformError::invalid(path, "missing aggregate cpu line"))?;
    Ok(CpuTimeSnapshot {
        observed_at,
        aggregate,
        cpus,
    })
}

fn parse_cpu_line(path: &Path, line: &str) -> PlatformResult<CpuTimes> {
    let values = line
        .split_whitespace()
        .skip(1)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| PlatformError::invalid(path, format!("invalid counter: {error}")))
        })
        .collect::<PlatformResult<Vec<_>>>()?;
    if values.len() < 4 {
        return Err(PlatformError::invalid(
            path,
            "cpu line has fewer than four counters",
        ));
    }

    Ok(CpuTimes {
        user: values[0],
        nice: values[1],
        system: values[2],
        idle: values[3],
        io_wait: values.get(4).copied().unwrap_or_default(),
        irq: values.get(5).copied().unwrap_or_default(),
        soft_irq: values.get(6).copied().unwrap_or_default(),
        steal: values.get(7).copied().unwrap_or_default(),
    })
}

fn map_proc_io(operation: &'static str, path: PathBuf, error: std::io::Error) -> PlatformError {
    if error.kind() == std::io::ErrorKind::NotFound {
        PlatformError::Disappeared(path.display().to_string())
    } else {
        PlatformError::io(operation, path, error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_sparse_cpu_ids_and_ignores_guest_double_accounting() {
        let input = "\
cpu  100 2 30 400 5 6 7 8 9 10
cpu0 20 1 10 100 2 3 4 5 6 7
cpu7 80 1 20 300 3 3 3 3 3 3
intr 0
";
        let snapshot =
            parse_cpu_times(Path::new("/proc/stat"), input, MonotonicMillis(42)).unwrap();
        assert_eq!(
            snapshot.cpus.keys().copied().collect::<Vec<_>>(),
            [CpuId(0), CpuId(7)]
        );
        assert_eq!(snapshot.aggregate.total(), 558);
    }

    #[test]
    fn process_stat_handles_spaces_and_parentheses_in_comm() {
        let stat =
            "123 (render worker (gpu)) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777 20";
        let parsed = parse_process_stat(Path::new("/proc/123/stat"), stat, ProcessId(123)).unwrap();
        assert_eq!(parsed.comm, "render worker (gpu)");
        assert_eq!(parsed.start_time_ticks, 777);
    }

    #[test]
    fn process_stat_treats_invalid_utf8_comm_as_lossy_metadata() {
        let mut stat = b"123 (render ".to_vec();
        stat.push(0xff);
        stat.extend_from_slice(b") S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777 20");
        let parsed =
            parse_process_stat_bytes(Path::new("/proc/123/stat"), &stat, ProcessId(123)).unwrap();
        assert_eq!(parsed.start_time_ticks, 777);
        assert!(parsed.comm.contains('\u{fffd}'));
    }

    #[test]
    fn uid_parser_retains_all_linux_credential_uids() {
        assert_eq!(
            parse_uids(
                Path::new("/proc/7/status"),
                "Name:\ttest\nUid:\t1000 0 0 0\n"
            )
            .unwrap(),
            [1000, 0, 0, 0]
        );
        assert!(parse_uids(Path::new("/proc/7/status"), "Uid:\t1000\n").is_err());
    }

    #[test]
    fn computes_load_only_for_monotonic_counters() {
        let old = CpuTimes {
            user: 10,
            idle: 90,
            ..CpuTimes::default()
        };
        let new = CpuTimes {
            user: 30,
            idle: 100,
            ..CpuTimes::default()
        };
        assert!((new.utilization_since(old).unwrap() - (2.0 / 3.0)).abs() < f64::EPSILON);
        assert!(old.utilization_since(new).is_none());
    }

    #[test]
    fn numeric_directory_listing_preserves_sparse_thread_ids() {
        let root = tempdir().expect("temporary proc root");
        fs::create_dir(root.path().join("7")).expect("numeric directory");
        fs::create_dir(root.path().join("42")).expect("numeric directory");
        fs::create_dir(root.path().join("self")).expect("non-numeric directory");

        assert_eq!(
            list_numeric_directories(root.path(), "test").expect("list"),
            vec![ProcessId(7), ProcessId(42)]
        );
    }
}
