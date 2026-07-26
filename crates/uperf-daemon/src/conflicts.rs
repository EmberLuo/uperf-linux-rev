//! Read-only detection of power-policy controllers that may compete for CPU
//! frequency constraints.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    os::unix::ffi::OsStrExt,
    path::Path,
};

use anyhow::{Context, Result, bail};
use uperf_linux::SystemRoots;

const MAX_CMDLINE_BYTES: u64 = 64 * 1_024;
const MAX_SYSTEMD_ENTRIES: usize = 4_096;
const MAX_SYSTEMD_DEPTH: usize = 6;

struct Controller {
    id: &'static str,
    executables: &'static [&'static str],
    units: &'static [&'static str],
}

const CONTROLLERS: [Controller; 5] = [
    Controller {
        id: "power-profiles-daemon",
        executables: &["power-profiles-daemon"],
        units: &["power-profiles-daemon.service"],
    },
    Controller {
        id: "tuned",
        executables: &["tuned"],
        units: &["tuned.service"],
    },
    Controller {
        id: "TLP",
        executables: &["tlp"],
        units: &["tlp.service", "tlp-sleep.service"],
    },
    Controller {
        id: "auto-cpufreq",
        executables: &["auto-cpufreq"],
        units: &["auto-cpufreq.service"],
    },
    Controller {
        id: "system76-power",
        executables: &["system76-power", "com.system76.PowerDaemon"],
        units: &["com.system76.PowerDaemon.service", "system76-power.service"],
    },
];

/// Scan process command lines and enabled systemd dependencies without
/// changing either controller or service state.
///
/// # Errors
///
/// Returns an error when the proc root cannot be enumerated or the systemd
/// enablement tree exceeds conservative traversal bounds.
pub fn competing_controller_warnings(roots: &SystemRoots) -> Result<Vec<String>> {
    let processes = running_executables(&roots.proc)?;
    let enabled_units = enabled_units(&roots.etc.join("systemd/system"))?;
    let mut warnings = Vec::new();
    for controller in &CONTROLLERS {
        let mut evidence = Vec::new();
        let process_ids = controller
            .executables
            .iter()
            .flat_map(|name| processes.get(*name).into_iter().flatten())
            .copied()
            .collect::<BTreeSet<_>>();
        if !process_ids.is_empty() {
            evidence.push(format!(
                "running PID(s) {}",
                process_ids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        let units = controller
            .units
            .iter()
            .copied()
            .filter(|unit| enabled_units.contains(*unit))
            .collect::<Vec<_>>();
        if !units.is_empty() {
            evidence.push(format!("enabled unit(s) {}", units.join(",")));
        }
        if !evidence.is_empty() {
            warnings.push(format!(
                "competing power controller detected: {} ({}); detection is read-only and uperf-linux will not stop the controller",
                controller.id,
                evidence.join("; ")
            ));
        }
    }
    Ok(warnings)
}

fn running_executables(proc_root: &Path) -> Result<BTreeMap<String, BTreeSet<u32>>> {
    let entries =
        fs::read_dir(proc_root).with_context(|| format!("enumerate {}", proc_root.display()))?;
    let mut running = BTreeMap::<String, BTreeSet<u32>>::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path().join("cmdline");
        let Ok(file) = File::open(&path) else {
            // Processes can exit between readdir and open.
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take(MAX_CMDLINE_BYTES)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        for argument in bytes
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .take(3)
        {
            let path = Path::new(OsStr::from_bytes(argument));
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if CONTROLLERS
                .iter()
                .any(|controller| controller.executables.contains(&name))
            {
                running.entry(name.to_owned()).or_default().insert(pid);
            }
        }
    }
    Ok(running)
}

fn enabled_units(systemd_root: &Path) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut visited = 0;
    collect_enabled_units(systemd_root, 0, &mut visited, &mut found)?;
    Ok(found)
}

fn collect_enabled_units(
    directory: &Path,
    depth: usize,
    visited: &mut usize,
    found: &mut BTreeSet<String>,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("enumerate {}", directory.display()));
        }
    };
    if depth > MAX_SYSTEMD_DEPTH {
        bail!(
            "systemd enablement tree exceeds depth {MAX_SYSTEMD_DEPTH} below {}",
            directory.display()
        );
    }
    let dependency_directory = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(_, suffix)| matches!(suffix, "wants" | "requires"));
    for entry in entries {
        let entry = entry.with_context(|| format!("enumerate {}", directory.display()))?;
        *visited = visited.saturating_add(1);
        if *visited > MAX_SYSTEMD_ENTRIES {
            bail!(
                "systemd enablement tree exceeds {MAX_SYSTEMD_ENTRIES} entries below {}",
                directory.display()
            );
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("inspect {}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_enabled_units(&entry.path(), depth + 1, visited, found)?;
        } else if dependency_directory
            && let Some(name) = entry.file_name().to_str()
            && CONTROLLERS
                .iter()
                .any(|controller| controller.units.contains(&name))
        {
            found.insert(name.to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink, path::Path};

    use tempfile::tempdir;
    use uperf_linux::SystemRoots;

    use super::competing_controller_warnings;

    fn roots(root: &Path) -> SystemRoots {
        let roots = SystemRoots::below(root);
        fs::create_dir_all(&roots.proc).unwrap();
        fs::create_dir_all(&roots.etc).unwrap();
        roots
    }

    #[test]
    fn detects_native_and_interpreter_launched_daemons() {
        let temporary = tempdir().unwrap();
        let roots = roots(temporary.path());
        fs::create_dir_all(roots.proc.join("101")).unwrap();
        fs::write(
            roots.proc.join("101/cmdline"),
            b"/usr/libexec/power-profiles-daemon\0",
        )
        .unwrap();
        fs::create_dir_all(roots.proc.join("202")).unwrap();
        fs::write(
            roots.proc.join("202/cmdline"),
            b"/usr/bin/python3\0/usr/sbin/tuned\0--no-dbus\0",
        )
        .unwrap();

        let warnings = competing_controller_warnings(&roots).unwrap();
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("power-profiles-daemon"));
        assert!(warnings[0].contains("101"));
        assert!(warnings[1].contains("tuned"));
        assert!(warnings[1].contains("202"));
    }

    #[test]
    fn detects_enabled_oneshot_controller_units() {
        let temporary = tempdir().unwrap();
        let roots = roots(temporary.path());
        let wants = roots.etc.join("systemd/system/multi-user.target.wants");
        fs::create_dir_all(&wants).unwrap();
        symlink(
            "/usr/lib/systemd/system/tlp.service",
            wants.join("tlp.service"),
        )
        .unwrap();

        let warnings = competing_controller_warnings(&roots).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("TLP"));
        assert!(warnings[0].contains("tlp.service"));
    }

    #[test]
    fn installed_but_disabled_unit_is_not_reported() {
        let temporary = tempdir().unwrap();
        let roots = roots(temporary.path());
        let systemd = roots.etc.join("systemd/system");
        fs::create_dir_all(&systemd).unwrap();
        fs::write(systemd.join("tlp.service"), b"[Unit]\n").unwrap();

        assert!(competing_controller_warnings(&roots).unwrap().is_empty());
    }
}
