//! Read-only Linux hardware discovery.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use uperf_core::{
    CpuId, CpuPolicyCapability, CpuSet, DevfreqCapability, DeviceCapabilities, FrequencyLimits,
    Hertz, InputDeviceCapability, MilliCelsius, MonotonicMillis, SensorHealth, TargetId,
    ThermalReading, ThermalZoneCapability,
};
use uperf_platform::{Clock, PlatformError, PlatformResult, SysfsIo, ThermalSample};

use crate::{LinuxClock, RootedSysfs};

const MIN_PLAUSIBLE_TEMPERATURE: MilliCelsius = MilliCelsius(-40_000);
const MAX_PLAUSIBLE_TEMPERATURE: MilliCelsius = MilliCelsius(200_000);

/// Files needed to observe and mutate one discovered frequency target.
///
/// These paths are derived from Linux enumeration, not from a D-Bus request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrequencyTargetPaths {
    pub id: TargetId,
    pub minimum: PathBuf,
    pub maximum: PathBuf,
    pub current: PathBuf,
    /// Number of hertz represented by one integer in these sysfs attributes.
    pub hertz_per_unit: u64,
}

/// Hardware discovery plus the private resource registry used by the actuator.
#[derive(Clone, Debug)]
pub struct LinuxDiscovery {
    pub capabilities: DeviceCapabilities,
    pub frequency_targets: BTreeMap<TargetId, FrequencyTargetPaths>,
    /// Exact logical sysfs directory for each discovered thermal-zone ID.
    ///
    /// This registry is intentionally kept out of the public capability DTO:
    /// clients select logical sensors, while root-owned configuration may pin
    /// a sensor to an exact discovered path.
    pub thermal_zone_paths: BTreeMap<String, PathBuf>,
    pub warnings: Vec<String>,
}

pub(crate) fn discover(
    sys_root: &Path,
    sysfs: &RootedSysfs,
    clock: &LinuxClock,
) -> PlatformResult<LinuxDiscovery> {
    let mut warnings = Vec::new();
    let mut targets = BTreeMap::new();
    let cpu_policies = discover_cpu_policies(sys_root, sysfs, &mut targets, &mut warnings)?;
    let devfreq_targets = discover_devfreq(sys_root, sysfs, &mut targets, &mut warnings)?;
    let (thermal_zones, thermal_zone_paths) =
        discover_thermal(sys_root, sysfs, clock, &mut warnings)?;
    let input_devices = discover_input(sys_root, sysfs, &mut warnings)?;
    let (device_name, compatible) = device_identity(sys_root);

    Ok(LinuxDiscovery {
        capabilities: DeviceCapabilities {
            device_name,
            compatible,
            cpu_policies,
            devfreq_targets,
            thermal_zones,
            input_devices,
        },
        frequency_targets: targets,
        thermal_zone_paths,
        warnings,
    })
}

pub(crate) fn discover_device_identity(sys_root: &Path) -> LinuxDiscovery {
    let (device_name, compatible) = device_identity(sys_root);
    LinuxDiscovery {
        capabilities: DeviceCapabilities {
            device_name,
            compatible,
            cpu_policies: Vec::new(),
            devfreq_targets: Vec::new(),
            thermal_zones: Vec::new(),
            input_devices: Vec::new(),
        },
        frequency_targets: BTreeMap::new(),
        thermal_zone_paths: BTreeMap::new(),
        warnings: Vec::new(),
    }
}

pub(crate) fn discover_recovery_targets(
    sys_root: &Path,
    sysfs: &RootedSysfs,
) -> PlatformResult<LinuxDiscovery> {
    let mut discovery = discover_device_identity(sys_root);
    discovery.capabilities.cpu_policies = discover_cpu_policies(
        sys_root,
        sysfs,
        &mut discovery.frequency_targets,
        &mut discovery.warnings,
    )?;
    discovery.capabilities.devfreq_targets = discover_devfreq(
        sys_root,
        sysfs,
        &mut discovery.frequency_targets,
        &mut discovery.warnings,
    )?;
    Ok(discovery)
}

pub(crate) fn read_thermal_samples(
    sys_root: &Path,
    sysfs: &RootedSysfs,
    clock: &LinuxClock,
) -> PlatformResult<Vec<ThermalSample>> {
    let directory = sys_root.join("class/thermal");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io("list thermal zones", directory, error)),
    };

    let mut zones = collect_prefixed(entries, "thermal_zone")?;
    zones.sort_by_key(|(index, _)| *index);
    let now = clock.monotonic_millis();
    let mut readings = Vec::with_capacity(zones.len());
    for (_, name) in zones {
        let base = PathBuf::from("/sys/class/thermal").join(&name);
        readings.push(read_thermal_sample_at(sysfs, &base, now));
    }
    Ok(readings)
}

pub(crate) fn read_thermal_samples_at(
    sysfs: &RootedSysfs,
    clock: &LinuxClock,
    paths: &[PathBuf],
) -> Vec<ThermalSample> {
    let now = clock.monotonic_millis();
    paths
        .iter()
        .map(|path| read_thermal_sample_at(sysfs, path, now))
        .collect()
}

fn read_thermal_sample_at(sysfs: &RootedSysfs, base: &Path, now: MonotonicMillis) -> ThermalSample {
    let zone_id = base
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_owned();
    let zone_type =
        read_optional(sysfs, &base.join("type")).unwrap_or_else(|| "unknown".to_owned());
    let temperature = read_optional(sysfs, &base.join("temp"))
        .and_then(|value| value.parse::<i64>().ok())
        .map(MilliCelsius)
        .filter(|temperature| {
            (MIN_PLAUSIBLE_TEMPERATURE..=MAX_PLAUSIBLE_TEMPERATURE).contains(temperature)
        });
    let health = if temperature.is_some() {
        SensorHealth::Healthy
    } else {
        SensorHealth::Unavailable
    };
    ThermalSample {
        zone_id,
        zone_type,
        path: base.to_path_buf(),
        reading: ThermalReading {
            temperature,
            sampled_at: now,
            health,
        },
    }
}

fn discover_cpu_policies(
    sys_root: &Path,
    sysfs: &RootedSysfs,
    target_paths: &mut BTreeMap<TargetId, FrequencyTargetPaths>,
    warnings: &mut Vec<String>,
) -> PlatformResult<Vec<CpuPolicyCapability>> {
    let relative = if sys_root.join("devices/system/cpu/cpufreq").is_dir() {
        "devices/system/cpu/cpufreq"
    } else {
        "class/cpufreq"
    };
    let directory = sys_root.join(relative);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io("list cpufreq policies", directory, error)),
    };
    let mut entries = collect_prefixed(entries, "policy")?;
    entries.sort_by_key(|(index, _)| *index);

    let mut policies = Vec::new();
    for (_, policy_name) in entries {
        let base = PathBuf::from("/sys").join(relative).join(&policy_name);
        let parsed = (|| -> PlatformResult<(CpuPolicyCapability, FrequencyTargetPaths)> {
            let cpus_text = read_required_with_fallback(
                sysfs,
                &base.join("related_cpus"),
                &base.join("affected_cpus"),
            )?;
            let cpus = parse_cpu_set(&base.join("related_cpus"), &cpus_text)?;
            if cpus.is_empty() {
                return Err(PlatformError::invalid(
                    base.join("related_cpus"),
                    "CPU policy has an empty CPU set",
                ));
            }
            let minimum = read_khz(sysfs, &base.join("cpuinfo_min_freq"))?;
            let maximum = read_khz(sysfs, &base.join("cpuinfo_max_freq"))?;
            let limits = FrequencyLimits::new(minimum, maximum).map_err(|error| {
                PlatformError::invalid(base.join("cpuinfo_max_freq"), error.to_string())
            })?;
            let available_frequencies = cpu_available_frequencies(sysfs, &base, limits)?;
            let governor = read_optional(sysfs, &base.join("scaling_governor"));
            let id = TargetId::new(format!("cpu.{policy_name}")).map_err(|error| {
                PlatformError::invalid(base.clone(), format!("invalid target ID: {error}"))
            })?;
            let paths = FrequencyTargetPaths {
                id: id.clone(),
                minimum: base.join("scaling_min_freq"),
                maximum: base.join("scaling_max_freq"),
                current: base.join("scaling_cur_freq"),
                hertz_per_unit: 1_000,
            };
            Ok((
                CpuPolicyCapability {
                    id,
                    policy_name: policy_name.clone(),
                    cpus,
                    limits,
                    available_frequencies,
                    governor,
                },
                paths,
            ))
        })();

        match parsed {
            Ok((capability, paths)) => {
                target_paths.insert(capability.id.clone(), paths);
                policies.push(capability);
            }
            Err(error) => warnings.push(format!("skipping cpufreq {policy_name}: {error}")),
        }
    }
    Ok(policies)
}

fn discover_devfreq(
    sys_root: &Path,
    sysfs: &RootedSysfs,
    target_paths: &mut BTreeMap<TargetId, FrequencyTargetPaths>,
    warnings: &mut Vec<String>,
) -> PlatformResult<Vec<DevfreqCapability>> {
    let candidates = devfreq_candidates(sys_root, warnings)?;

    let mut capabilities = Vec::new();
    let mut used_ids = BTreeSet::new();
    for (index, candidate) in candidates.into_iter().enumerate() {
        let DevfreqCandidate {
            entry_name,
            logical_base: base,
            canonical_identity,
        } = candidate;
        let parsed = (|| -> PlatformResult<(DevfreqCapability, FrequencyTargetPaths)> {
            let current_minimum = read_hz(sysfs, &base.join("min_freq"))?;
            let current_maximum = read_hz(sysfs, &base.join("max_freq"))?;
            let current_limits =
                FrequencyLimits::new(current_minimum, current_maximum).map_err(|error| {
                    PlatformError::invalid(base.join("max_freq"), error.to_string())
                })?;
            let mut available_frequencies = devfreq_available_frequencies(sysfs, &base)?;
            let limits = available_frequencies
                .first()
                .copied()
                .zip(available_frequencies.last().copied())
                .map_or(Ok(current_limits), |(minimum, maximum)| {
                    if current_limits.min < minimum || current_limits.max > maximum {
                        return Err(PlatformError::invalid(
                            base.join("available_frequencies"),
                            "current devfreq window lies outside the advertised OPP table",
                        ));
                    }
                    FrequencyLimits::new(minimum, maximum).map_err(|error| {
                        PlatformError::invalid(
                            base.join("available_frequencies"),
                            error.to_string(),
                        )
                    })
                })?;
            normalize_opps(&mut available_frequencies, limits);
            let governor = read_optional(sysfs, &base.join("governor"));
            let device_name = devfreq_device_name(sysfs, &base, &entry_name);
            let compatible = devfreq_compatible(sysfs, &base);
            let id = unique_devfreq_id(&entry_name, &canonical_identity, index, &mut used_ids)?;
            let paths = FrequencyTargetPaths {
                id: id.clone(),
                minimum: base.join("min_freq"),
                maximum: base.join("max_freq"),
                current: base.join("cur_freq"),
                hertz_per_unit: 1,
            };
            Ok((
                DevfreqCapability {
                    id,
                    device_name,
                    compatible,
                    limits,
                    available_frequencies,
                    governor,
                },
                paths,
            ))
        })();

        match parsed {
            Ok((capability, paths)) => {
                if target_paths.insert(capability.id.clone(), paths).is_some() {
                    return Err(PlatformError::invalid(
                        capability.id.to_string(),
                        "duplicate devfreq target ID would overwrite a mutation path",
                    ));
                }
                capabilities.push(capability);
            }
            Err(error) => warnings.push(format!("skipping devfreq {entry_name}: {error}")),
        }
    }
    Ok(capabilities)
}

#[derive(Debug)]
struct DevfreqCandidate {
    entry_name: String,
    logical_base: PathBuf,
    canonical_identity: PathBuf,
}

fn devfreq_candidates(
    sys_root: &Path,
    warnings: &mut Vec<String>,
) -> PlatformResult<Vec<DevfreqCandidate>> {
    let canonical_sys = sys_root
        .canonicalize()
        .map_err(|error| PlatformError::io("canonicalize sysfs root", sys_root, error))?;
    let mut candidates = Vec::new();
    let mut identities = BTreeSet::new();
    let class_directory = sys_root.join("class/devfreq");

    match fs::read_dir(&class_directory) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        warnings.push(format!("cannot read devfreq class entry: {error}"));
                        continue;
                    }
                };
                let Ok(entry_name) = entry.file_name().into_string() else {
                    warnings.push("ignoring devfreq target with a non-UTF-8 name".to_owned());
                    continue;
                };
                let canonical_identity = match entry.path().canonicalize() {
                    Ok(path) if path.starts_with(&canonical_sys) => path,
                    Ok(_) => {
                        warnings.push(format!(
                            "ignoring devfreq {entry_name}: target escapes /sys"
                        ));
                        continue;
                    }
                    Err(error) => {
                        warnings.push(format!(
                            "ignoring devfreq {entry_name}: cannot canonicalize: {error}"
                        ));
                        continue;
                    }
                };
                if identities.insert(canonical_identity.clone()) {
                    candidates.push(DevfreqCandidate {
                        entry_name: entry_name.clone(),
                        logical_base: PathBuf::from("/sys/class/devfreq").join(entry_name),
                        canonical_identity,
                    });
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!(
            "cannot list {}: {error}",
            class_directory.display()
        )),
    }

    // Some kernels omit /sys/class/devfreq. Search only directories, with a
    // hard depth and entry budget, and stop descending at each devfreq node.
    let devices = sys_root.join("devices");
    let mut visited_entries = 0;
    collect_device_devfreq(
        &devices,
        &canonical_sys,
        0,
        &mut visited_entries,
        &mut identities,
        &mut candidates,
        warnings,
    );
    candidates.sort_by(|left, right| {
        left.entry_name
            .cmp(&right.entry_name)
            .then_with(|| left.canonical_identity.cmp(&right.canonical_identity))
    });
    Ok(candidates)
}

const MAX_DEVICE_SCAN_DEPTH: usize = 16;
const MAX_DEVICE_SCAN_ENTRIES: usize = 100_000;

fn collect_device_devfreq(
    directory: &Path,
    canonical_sys: &Path,
    depth: usize,
    visited_entries: &mut usize,
    identities: &mut BTreeSet<PathBuf>,
    candidates: &mut Vec<DevfreqCandidate>,
    warnings: &mut Vec<String>,
) {
    if depth > MAX_DEVICE_SCAN_DEPTH || *visited_entries >= MAX_DEVICE_SCAN_ENTRIES {
        return;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => {
            warnings.push(format!("cannot scan {}: {error}", directory.display()));
            return;
        }
    };

    for entry in entries {
        if *visited_entries >= MAX_DEVICE_SCAN_ENTRIES {
            warnings.push(format!(
                "stopped devfreq fallback scan after {MAX_DEVICE_SCAN_ENTRIES} entries"
            ));
            return;
        }
        *visited_entries += 1;
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == "devfreq" {
            collect_devfreq_container(&path, canonical_sys, identities, candidates);
            continue;
        }
        collect_device_devfreq(
            &path,
            canonical_sys,
            depth + 1,
            visited_entries,
            identities,
            candidates,
            warnings,
        );
    }
}

fn collect_devfreq_container(
    container: &Path,
    canonical_sys: &Path,
    identities: &mut BTreeSet<PathBuf>,
    candidates: &mut Vec<DevfreqCandidate>,
) {
    let Ok(entries) = fs::read_dir(container) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        let Ok(entry_name) = entry.file_name().into_string() else {
            continue;
        };
        let canonical_identity = match entry.path().canonicalize() {
            Ok(path) if path.starts_with(canonical_sys) => path,
            _ => continue,
        };
        if !identities.insert(canonical_identity.clone()) {
            continue;
        }
        let Ok(relative) = canonical_identity.strip_prefix(canonical_sys) else {
            continue;
        };
        candidates.push(DevfreqCandidate {
            entry_name,
            logical_base: PathBuf::from("/sys").join(relative),
            canonical_identity,
        });
    }
}

fn devfreq_device_name(sysfs: &RootedSysfs, base: &Path, entry_name: &str) -> String {
    if let Some(name) = read_optional(sysfs, &base.join("name")).filter(|name| !name.is_empty()) {
        return name;
    }
    if let Some(compatible) = read_optional(sysfs, &base.join("device/of_node/compatible"))
        && let Some(first) = compatible.split('\0').find(|value| !value.is_empty())
    {
        return first.to_owned();
    }
    entry_name.to_owned()
}

fn devfreq_compatible(sysfs: &RootedSysfs, base: &Path) -> Vec<String> {
    read_optional(sysfs, &base.join("device/of_node/compatible"))
        .map(|value| {
            value
                .split('\0')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn discover_thermal(
    sys_root: &Path,
    sysfs: &RootedSysfs,
    clock: &LinuxClock,
    warnings: &mut Vec<String>,
) -> PlatformResult<(Vec<ThermalZoneCapability>, BTreeMap<String, PathBuf>)> {
    let samples = read_thermal_samples(sys_root, sysfs, clock)?;
    let mut capabilities = Vec::with_capacity(samples.len());
    let mut paths = BTreeMap::new();
    for sample in samples {
        if sample.reading.health != SensorHealth::Healthy {
            warnings.push(format!(
                "thermal zone {} has no valid temperature",
                sample.zone_id
            ));
        }
        paths.insert(sample.zone_id.clone(), sample.path);
        capabilities.push(ThermalZoneCapability {
            id: sample.zone_id,
            zone_type: sample.zone_type,
            current: Some(sample.reading),
        });
    }
    Ok((capabilities, paths))
}

fn discover_input(
    sys_root: &Path,
    sysfs: &RootedSysfs,
    warnings: &mut Vec<String>,
) -> PlatformResult<Vec<InputDeviceCapability>> {
    let directory = sys_root.join("class/input");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io("list input devices", directory, error)),
    };
    let mut events = collect_prefixed(entries, "event")?;
    events.sort_by_key(|(index, _)| *index);
    let mut result = Vec::new();
    for (_, event_name) in events {
        let base = PathBuf::from("/sys/class/input")
            .join(&event_name)
            .join("device");
        let Some(name) = read_optional(sysfs, &base.join("name")) else {
            warnings.push(format!("input {event_name} has no readable name"));
            continue;
        };
        let multi_touch = read_optional(sysfs, &base.join("capabilities/abs"))
            .is_some_and(|mask| has_type_b_multitouch_axes(&mask));
        result.push(InputDeviceCapability {
            id: event_name,
            name,
            multi_touch,
        });
    }
    Ok(result)
}

fn device_identity(sys_root: &Path) -> (Option<String>, Vec<String>) {
    let device_tree = sys_root.join("firmware/devicetree/base");
    let device_name = fs::read(device_tree.join("model"))
        .ok()
        .and_then(|bytes| nul_terminated_string(&bytes));
    let compatible_bytes = fs::read(device_tree.join("compatible")).unwrap_or_default();
    let compatible = compatible_bytes
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    (device_name, compatible)
}

fn cpu_available_frequencies(
    sysfs: &RootedSysfs,
    base: &Path,
    limits: FrequencyLimits,
) -> PlatformResult<Vec<Hertz>> {
    let mut frequencies =
        if let Some(value) = read_optional(sysfs, &base.join("scaling_available_frequencies")) {
            parse_frequency_list(&base.join("scaling_available_frequencies"), &value, 1_000)?
        } else if let Some(value) = read_optional(sysfs, &base.join("stats/time_in_state")) {
            let first_columns = value
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .collect::<Vec<_>>()
                .join(" ");
            parse_frequency_list(&base.join("stats/time_in_state"), &first_columns, 1_000)?
        } else {
            Vec::new()
        };
    normalize_opps(&mut frequencies, limits);
    Ok(frequencies)
}

fn devfreq_available_frequencies(sysfs: &RootedSysfs, base: &Path) -> PlatformResult<Vec<Hertz>> {
    let value = read_optional(sysfs, &base.join("available_frequencies"))
        .or_else(|| read_optional(sysfs, &base.join("available_freqs")));
    let mut frequencies = match value {
        Some(value) => parse_frequency_list(&base.join("available_frequencies"), &value, 1)?,
        None => Vec::new(),
    };
    frequencies.sort_unstable();
    frequencies.dedup();
    Ok(frequencies)
}

fn normalize_opps(frequencies: &mut Vec<Hertz>, limits: FrequencyLimits) {
    if frequencies.is_empty() {
        return;
    }
    frequencies.retain(|frequency| *frequency >= limits.min && *frequency <= limits.max);
    frequencies.push(limits.min);
    frequencies.push(limits.max);
    frequencies.sort_unstable();
    frequencies.dedup();
}

fn read_khz(sysfs: &RootedSysfs, path: &Path) -> PlatformResult<Hertz> {
    let khz = read_required(sysfs, path)?
        .parse::<u64>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid kHz value: {error}")))?;
    let hz = khz
        .checked_mul(1_000)
        .ok_or_else(|| PlatformError::invalid(path, "kHz value overflows hertz"))?;
    Ok(Hertz(hz))
}

fn read_hz(sysfs: &RootedSysfs, path: &Path) -> PlatformResult<Hertz> {
    read_required(sysfs, path)?
        .parse::<u64>()
        .map(Hertz)
        .map_err(|error| PlatformError::invalid(path, format!("invalid hertz value: {error}")))
}

fn read_required(sysfs: &RootedSysfs, path: &Path) -> PlatformResult<String> {
    sysfs.read_string(path)
}

fn read_required_with_fallback(
    sysfs: &RootedSysfs,
    preferred: &Path,
    fallback: &Path,
) -> PlatformResult<String> {
    match sysfs.read_string(preferred) {
        Ok(value) => Ok(value),
        Err(PlatformError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            sysfs.read_string(fallback)
        }
        Err(error) => Err(error),
    }
}

fn read_optional(sysfs: &RootedSysfs, path: &Path) -> Option<String> {
    sysfs.read_string(path).ok()
}

fn parse_frequency_list(path: &Path, value: &str, multiplier: u64) -> PlatformResult<Vec<Hertz>> {
    value
        .split_whitespace()
        .map(|token| {
            let raw = token.parse::<u64>().map_err(|error| {
                PlatformError::invalid(path, format!("invalid frequency `{token}`: {error}"))
            })?;
            raw.checked_mul(multiplier).map(Hertz).ok_or_else(|| {
                PlatformError::invalid(path, format!("frequency `{token}` overflows hertz"))
            })
        })
        .collect()
}

fn parse_cpu_set(path: &Path, value: &str) -> PlatformResult<CpuSet> {
    let mut cpus = CpuSet::new();
    for token in value
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|token| !token.is_empty())
    {
        if let Some((start, end)) = token.split_once('-') {
            let start = parse_cpu_id(path, start)?;
            let end = parse_cpu_id(path, end)?;
            if start > end {
                return Err(PlatformError::invalid(
                    path,
                    format!("reversed CPU range `{token}`"),
                ));
            }
            for id in start..=end {
                cpus.insert(CpuId(id));
            }
        } else {
            cpus.insert(CpuId(parse_cpu_id(path, token)?));
        }
    }
    Ok(cpus)
}

fn parse_cpu_id(path: &Path, value: &str) -> PlatformResult<u32> {
    value
        .parse::<u32>()
        .map_err(|error| PlatformError::invalid(path, format!("invalid CPU ID `{value}`: {error}")))
}

fn collect_prefixed(entries: fs::ReadDir, prefix: &str) -> PlatformResult<Vec<(u32, String)>> {
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            PlatformError::io("read directory entry", PathBuf::from(prefix), error)
        })?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(prefix) else {
            continue;
        };
        let Ok(index) = suffix.parse::<u32>() else {
            continue;
        };
        result.push((index, name));
    }
    Ok(result)
}

fn unique_devfreq_id(
    entry_name: &str,
    canonical_identity: &Path,
    index: usize,
    used: &mut BTreeSet<TargetId>,
) -> PlatformResult<TargetId> {
    let mut sanitized = entry_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let maximum_name_len = TargetId::MAX_LEN.saturating_sub("devfreq.".len());
    sanitized.truncate(maximum_name_len);
    let base = if sanitized.is_empty() {
        format!("devfreq.target{index}")
    } else {
        format!("devfreq.{sanitized}")
    };
    let candidate = TargetId::new(base.clone()).map_err(|error| {
        PlatformError::invalid(entry_name, format!("cannot create target ID: {error}"))
    })?;
    if used.insert(candidate.clone()) {
        return Ok(candidate);
    }

    let identity_hash = stable_path_hash(canonical_identity);
    for discriminator in 0_u32..=u32::MAX {
        let suffix = if discriminator == 0 {
            format!(".{identity_hash:016x}")
        } else {
            format!(".{identity_hash:016x}.{discriminator}")
        };
        let keep = TargetId::MAX_LEN.saturating_sub(suffix.len());
        let mut disambiguated = base.clone();
        disambiguated.truncate(keep);
        disambiguated.push_str(&suffix);
        let candidate = TargetId::new(disambiguated).map_err(|error| {
            PlatformError::invalid(
                entry_name,
                format!("cannot create unique target ID: {error}"),
            )
        })?;
        if used.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(PlatformError::invalid(
        entry_name,
        "exhausted devfreq target-ID discriminators",
    ))
}

fn stable_path_hash(path: &Path) -> u64 {
    // FNV-1a is used only as a deterministic identity suffix, never for
    // security. The full candidate is still checked for uniqueness.
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn linux_capability_bit(mask: &str, bit: usize) -> bool {
    // Linux prints bitmap words most-significant word first.  Parsing the
    // whitespace-separated words in reverse restores word 0 first.
    let words = mask
        .split_whitespace()
        .rev()
        .filter_map(|word| u64::from_str_radix(word, 16).ok())
        .collect::<Vec<_>>();
    let word_bits = usize::BITS as usize;
    let word_index = bit / word_bits;
    let bit_index = bit % word_bits;
    words
        .get(word_index)
        .is_some_and(|word| word & (1_u64 << bit_index) != 0)
}

fn has_type_b_multitouch_axes(mask: &str) -> bool {
    // A slot bit alone is not enough. The runtime also requires tracking IDs
    // and non-degenerate X/Y ranges before it will open a device.
    [0x2f, 0x35, 0x36, 0x39]
        .into_iter()
        .all(|bit| linux_capability_bit(mask, bit))
}

fn nul_terminated_string(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let value = std::str::from_utf8(&bytes[..end]).ok()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_list_supports_spaces_commas_ranges_and_sparse_ids() {
        let parsed = parse_cpu_set(Path::new("related_cpus"), "0-2, 7 128").unwrap();
        assert_eq!(
            parsed.iter().copied().collect::<Vec<_>>(),
            [CpuId(0), CpuId(1), CpuId(2), CpuId(7), CpuId(128)]
        );
    }

    #[test]
    fn capability_bitmap_requires_all_type_b_multitouch_axes() {
        assert!(has_type_b_multitouch_axes("260800000000000"));
        assert!(!has_type_b_multitouch_axes("800000000000"));
        assert!(!has_type_b_multitouch_axes("0"));
    }

    #[test]
    fn opp_normalization_is_dynamic_sorted_and_bounded() {
        let limits = FrequencyLimits {
            min: Hertz(200),
            max: Hertz(800),
        };
        let mut values = vec![Hertz(900), Hertz(400), Hertz(400), Hertz(100)];
        normalize_opps(&mut values, limits);
        assert_eq!(values, [Hertz(200), Hertz(400), Hertz(800)]);
    }

    #[test]
    fn missing_opp_table_remains_a_continuous_range() {
        let mut values = Vec::new();
        normalize_opps(
            &mut values,
            FrequencyLimits {
                min: Hertz(200),
                max: Hertz(800),
            },
        );
        assert!(values.is_empty());
    }

    #[test]
    fn sanitized_devfreq_collisions_never_reuse_a_fallback_id() {
        let mut used = BTreeSet::new();
        let ids = [
            unique_devfreq_id("target2", Path::new("/sys/devices/target2"), 0, &mut used).unwrap(),
            unique_devfreq_id("z:a", Path::new("/sys/devices/z:a"), 1, &mut used).unwrap(),
            unique_devfreq_id("z_a", Path::new("/sys/devices/z_a"), 2, &mut used).unwrap(),
        ];
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
        assert_eq!(used.len(), ids.len());
    }

    #[test]
    fn implausible_thermal_sentinels_are_not_healthy_readings() {
        assert!(
            !(MIN_PLAUSIBLE_TEMPERATURE..=MAX_PLAUSIBLE_TEMPERATURE)
                .contains(&MilliCelsius(-273_150))
        );
        assert!(
            (MIN_PLAUSIBLE_TEMPERATURE..=MAX_PLAUSIBLE_TEMPERATURE).contains(&MilliCelsius(95_000))
        );
    }
}
