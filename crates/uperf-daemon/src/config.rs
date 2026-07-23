//! Transactional configuration loading and hardware-selector resolution.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use uperf_actuator::{FrequencyTarget, TargetRegistry};
use uperf_api::{ApiVersion, Capabilities, ModeInfo, TargetCapability, feature};
use uperf_core::{
    AppRuleEngine, AppsConfig, CONFIG_SCHEMA_VERSION, ConfigBundle, CpuSet, CpuTargetPolicy,
    DeviceConfig, FrequencyLimits, FrequencyPolicy, Hertz, MAX_CONFIG_FILE_BYTES, PolicyConfig,
    PolicyEngine, TargetId, ThermalZoneConfig, Validate,
};
use uperf_linux::{FrequencyTargetPaths, LinuxDiscovery};

/// Files participating in one configuration generation.
#[derive(Clone, Debug)]
pub struct ConfigurationPaths {
    pub device: PathBuf,
    pub policy: PathBuf,
    pub apps: PathBuf,
}

impl ConfigurationPaths {
    #[must_use]
    pub fn system() -> Self {
        Self {
            device: PathBuf::from("/etc/uperf-linux/device.json"),
            policy: PathBuf::from("/etc/uperf-linux/policy.json"),
            apps: PathBuf::from("/var/lib/uperf-linux/apps.json"),
        }
    }

    #[must_use]
    pub fn below(config_directory: impl AsRef<Path>, state_directory: impl AsRef<Path>) -> Self {
        Self {
            device: config_directory.as_ref().join("device.json"),
            policy: config_directory.as_ref().join("policy.json"),
            apps: state_directory.as_ref().join("apps.json"),
        }
    }
}

/// One configured logical target resolved against current Linux discovery.
#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    pub id: TargetId,
    pub kind: &'static str,
    pub label: String,
    pub cpus: CpuSet,
    pub hardware_limits: FrequencyLimits,
    pub available_frequencies: Vec<Hertz>,
    pub paths: FrequencyTargetPaths,
    pub automatic_policy: Option<FrequencyPolicy>,
    pub administrator_cap: Option<Hertz>,
    pub critical_cap: Hertz,
    pub sensor_failure_cap: Hertz,
}

impl ResolvedTarget {
    /// Build the actuator-facing representation of this resolved target.
    ///
    /// # Errors
    ///
    /// Returns an error when the discovered limits, OPP table, or sysfs unit
    /// conversion cannot form a valid actuator target.
    pub fn actuator_target(&self) -> Result<FrequencyTarget> {
        FrequencyTarget::new(
            self.id.clone(),
            self.paths.minimum.clone(),
            self.paths.maximum.clone(),
            self.hardware_limits.min,
            self.hardware_limits.max,
            self.available_frequencies.clone(),
        )
        .and_then(|target| target.with_hertz_per_unit(self.paths.hertz_per_unit))
        .map_err(anyhow::Error::from)
    }

    #[must_use]
    pub fn api_capability(&self) -> TargetCapability {
        TargetCapability {
            id: self.id.to_string(),
            kind: self.kind.to_owned(),
            label: self.label.clone(),
            cpus: self.cpus.iter().map(|cpu| cpu.get()).collect(),
            minimum_hz: self.hardware_limits.min.get(),
            maximum_hz: self.hardware_limits.max.get(),
            available_hz: self
                .available_frequencies
                .iter()
                .map(|frequency| frequency.get())
                .collect(),
            can_override: true,
        }
    }
}

/// Fully parsed, semantically checked, and hardware-resolved candidate.
#[derive(Debug)]
pub struct ResolvedConfiguration {
    pub device: DeviceConfig,
    pub policy: PolicyConfig,
    pub apps: AppsConfig,
    pub app_rule_engine: AppRuleEngine,
    pub policy_engine: PolicyEngine,
    pub targets: BTreeMap<TargetId, ResolvedTarget>,
    pub thermal_zones: Vec<ThermalZoneConfig>,
    pub warnings: Vec<String>,
}

impl ResolvedConfiguration {
    /// Load every v2 file and resolve every selector before returning.
    ///
    /// Missing `apps.json` is treated as a new, empty daemon-managed rule set.
    /// Device and policy files are mandatory.
    ///
    /// # Errors
    ///
    /// Returns an error when a required file cannot be read, any configuration
    /// fails syntactic or semantic validation, or a configured resource cannot
    /// be resolved uniquely against the discovered hardware.
    pub fn load(paths: &ConfigurationPaths, discovery: &LinuxDiscovery) -> Result<Self> {
        let device_json = read_required(&paths.device)?;
        let policy_json = read_required(&paths.policy)?;
        let apps_json = match File::open(&paths.apps) {
            Ok(file) => read_open_config(file, &paths.apps)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                format!(r#"{{"schema_version":{CONFIG_SCHEMA_VERSION},"rules":[]}}"#)
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", paths.apps.display()));
            }
        };

        let device = DeviceConfig::from_json(&device_json)
            .with_context(|| format!("validate {}", paths.device.display()))?;
        let policy = PolicyConfig::from_json(&policy_json)
            .with_context(|| format!("validate {}", paths.policy.display()))?;
        let apps = AppsConfig::from_json(&apps_json)
            .with_context(|| format!("validate {}", paths.apps.display()))?;
        ConfigBundle {
            device: device.clone(),
            policy: policy.clone(),
            apps: apps.clone(),
        }
        .validate()
        .context("validate references across device, policy, and application rules")?;
        let policy_engine = PolicyEngine::new(policy.clone())?;
        let app_rule_engine = AppRuleEngine::new(&apps)?;
        validate_device_match(&device, discovery)?;
        let (targets, warnings) = resolve_targets(&device, discovery)?;
        let thermal_zones = resolve_thermal_zones(&device, discovery)?;

        Ok(Self {
            device,
            policy,
            apps,
            app_rule_engine,
            policy_engine,
            targets,
            thermal_zones,
            warnings,
        })
    }

    /// Build the complete registry used by the actuator.
    ///
    /// # Errors
    ///
    /// Returns an error when any resolved target cannot be converted into a
    /// valid actuator target or when the registry contains conflicting IDs.
    pub fn actuator_registry(&self) -> Result<TargetRegistry> {
        self.targets
            .values()
            .map(ResolvedTarget::actuator_target)
            .collect::<Result<Vec<_>>>()
            .and_then(|targets| TargetRegistry::new(targets).map_err(anyhow::Error::from))
    }

    #[must_use]
    pub fn cpu_target_policies(&self) -> BTreeMap<TargetId, CpuTargetPolicy> {
        self.targets
            .iter()
            .filter_map(|(id, target)| {
                target.automatic_policy.clone().map(|frequency| {
                    (
                        id.clone(),
                        CpuTargetPolicy {
                            cpus: target.cpus.clone(),
                            frequency,
                        },
                    )
                })
            })
            .collect()
    }

    #[must_use]
    pub fn manual_target_policies(&self) -> BTreeMap<TargetId, FrequencyPolicy> {
        self.targets
            .iter()
            .filter(|(_, target)| target.automatic_policy.is_none())
            .map(|(id, target)| {
                (
                    id.clone(),
                    FrequencyPolicy {
                        hardware_limits: target.hardware_limits,
                        floor: target.hardware_limits.min,
                        reference: target.hardware_limits.max,
                        efficient_cap: target.hardware_limits.max,
                        hertz_per_unit: target.paths.hertz_per_unit,
                        available_frequencies: target.available_frequencies.clone(),
                    },
                )
            })
            .collect()
    }

    #[must_use]
    pub fn administrator_caps(&self) -> BTreeMap<TargetId, Hertz> {
        self.targets
            .iter()
            .filter_map(|(id, target)| target.administrator_cap.map(|cap| (id.clone(), cap)))
            .collect()
    }

    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        let mut features = vec![
            feature::LOAD_GOVERNOR.to_owned(),
            feature::THERMAL_GUARD.to_owned(),
            feature::ACTIVE_WORKLOAD.to_owned(),
            feature::CONFIG_RELOAD_V2.to_owned(),
            feature::LOGIND_SLEEP_WAKE.to_owned(),
        ];
        if self.policy.input.enabled {
            features.push(feature::EVDEV_SCENES.to_owned());
        }
        if self.device.device_id == "qcom-sm8550" {
            features.push(feature::DEVICE_PROFILE_SM8550.to_owned());
        }
        Capabilities {
            api_version: ApiVersion::CURRENT,
            features,
            modes: vec![
                mode(
                    "auto",
                    "Automatic",
                    "Use the active workload rule or balanced default",
                ),
                mode(
                    "powersave",
                    "Power saver",
                    "Prefer efficient operating points",
                ),
                mode(
                    "balance",
                    "Balanced",
                    "Balance responsiveness and efficiency",
                ),
                mode(
                    "performance",
                    "Performance",
                    "Prefer responsiveness within safety caps",
                ),
            ],
            targets: self
                .targets
                .values()
                .map(ResolvedTarget::api_capability)
                .collect(),
            config_schema_min: CONFIG_SCHEMA_VERSION,
            config_schema_max: CONFIG_SCHEMA_VERSION,
        }
    }
}

fn read_required(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    read_open_config(file, path)
}

fn read_open_config(file: File, path: &Path) -> Result<String> {
    let read_limit = u64::try_from(MAX_CONFIG_FILE_BYTES)
        .expect("the platform can represent the configuration byte limit")
        + 1;
    let mut bytes = Vec::with_capacity(MAX_CONFIG_FILE_BYTES.min(64 * 1024));
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() > MAX_CONFIG_FILE_BYTES {
        bail!(
            "{} exceeds the {} byte configuration file limit",
            path.display(),
            MAX_CONFIG_FILE_BYTES
        );
    }
    String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", path.display()))
}

fn validate_device_match(device: &DeviceConfig, discovery: &LinuxDiscovery) -> Result<()> {
    let Some(selector) = &device.device_match else {
        return Ok(());
    };
    if let Some(compatible) = &selector.compatible
        && !discovery
            .capabilities
            .compatible
            .iter()
            .any(|value| value == compatible)
    {
        bail!(
            "device profile {} requires compatible {compatible:?}",
            device.device_id
        );
    }
    if let Some(product_name) = &selector.product_name
        && discovery.capabilities.device_name.as_deref() != Some(product_name.as_str())
    {
        bail!(
            "device profile {} requires product name {product_name:?}",
            device.device_id
        );
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "CPU and devfreq resolution share validation and warning bookkeeping that is clearer as one atomic pass"
)]
fn resolve_targets(
    device: &DeviceConfig,
    discovery: &LinuxDiscovery,
) -> Result<(BTreeMap<TargetId, ResolvedTarget>, Vec<String>)> {
    let mut resolved = BTreeMap::new();
    let mut claimed_discovery_ids = BTreeSet::new();

    for configured in &device.cpu_policies {
        let matches = discovery
            .capabilities
            .cpu_policies
            .iter()
            .filter(|candidate| {
                candidate.cpus == configured.related_cpus
                    && configured.sysfs_path.as_deref().is_none_or(|path| {
                        discovered_directory(discovery, &candidate.id) == Some(Path::new(path))
                    })
            })
            .collect::<Vec<_>>();
        let capability = unique_match(
            &configured.id,
            "CPU related_cpus selector",
            matches.as_slice(),
        )?;
        let paths = discovery_paths(discovery, &capability.id)?;
        validate_optional_directory(configured.sysfs_path.as_deref(), paths)?;
        let policy = FrequencyPolicy {
            hardware_limits: capability.limits,
            floor: configured.floor_hz,
            reference: configured.reference_hz,
            efficient_cap: configured.efficient_cap_hz,
            hertz_per_unit: paths.hertz_per_unit,
            available_frequencies: capability.available_frequencies.clone(),
        };
        policy.validate()?;
        validate_cap(
            configured.admin_cap_hz,
            capability.limits,
            &configured.id,
            "admin",
            paths.hertz_per_unit,
        )?;
        validate_cap(
            configured.critical_cap_hz,
            capability.limits,
            &configured.id,
            "critical",
            paths.hertz_per_unit,
        )?;
        validate_cap(
            configured.sensor_failure_cap_hz,
            capability.limits,
            &configured.id,
            "sensor failure",
            paths.hertz_per_unit,
        )?;
        let critical_cap = configured.critical_cap_hz.unwrap_or(capability.limits.min);
        let sensor_failure_cap = configured
            .sensor_failure_cap_hz
            .unwrap_or(capability.limits.min);
        if sensor_failure_cap > critical_cap {
            bail!(
                "{}: sensor failure cap must not exceed critical cap",
                configured.id
            );
        }
        claimed_discovery_ids.insert(capability.id.clone());
        resolved.insert(
            configured.id.clone(),
            ResolvedTarget {
                id: configured.id.clone(),
                kind: "cpufreq",
                label: format!(
                    "CPU {}",
                    configured
                        .related_cpus
                        .iter()
                        .map(|cpu| cpu.get().to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                cpus: configured.related_cpus.clone(),
                hardware_limits: capability.limits,
                available_frequencies: capability.available_frequencies.clone(),
                paths: paths.clone(),
                automatic_policy: Some(policy),
                administrator_cap: configured.admin_cap_hz,
                critical_cap,
                sensor_failure_cap,
            },
        );
    }

    for configured in &device.devfreq_targets {
        let matches = discovery
            .capabilities
            .devfreq_targets
            .iter()
            .filter(|candidate| {
                (candidate.device_name == configured.device_name
                    || discovered_directory(discovery, &candidate.id)
                        .and_then(Path::file_name)
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|name| name == configured.device_name))
                    && configured
                        .compatible
                        .iter()
                        .all(|required| candidate.compatible.contains(required))
                    && configured.sysfs_path.as_deref().is_none_or(|path| {
                        discovered_directory(discovery, &candidate.id) == Some(Path::new(path))
                    })
            })
            .collect::<Vec<_>>();
        let capability = unique_match(&configured.id, "devfreq device_name selector", &matches)?;
        let paths = discovery_paths(discovery, &capability.id)?;
        validate_optional_directory(configured.sysfs_path.as_deref(), paths)?;
        if capability.available_frequencies.is_empty() {
            bail!(
                "{}: devfreq target exposes no immutable OPP table; min_freq/max_freq are a mutable window and cannot establish safe hardware bounds",
                configured.id
            );
        }
        validate_cap(
            configured.admin_cap_hz,
            capability.limits,
            &configured.id,
            "admin",
            paths.hertz_per_unit,
        )?;
        validate_cap(
            configured.critical_cap_hz,
            capability.limits,
            &configured.id,
            "critical",
            paths.hertz_per_unit,
        )?;
        validate_cap(
            configured.sensor_failure_cap_hz,
            capability.limits,
            &configured.id,
            "sensor failure",
            paths.hertz_per_unit,
        )?;
        let critical_cap = configured.critical_cap_hz.unwrap_or(capability.limits.min);
        let sensor_failure_cap = configured
            .sensor_failure_cap_hz
            .unwrap_or(capability.limits.min);
        if sensor_failure_cap > critical_cap {
            bail!(
                "{}: sensor failure cap must not exceed critical cap",
                configured.id
            );
        }
        claimed_discovery_ids.insert(capability.id.clone());
        resolved.insert(
            configured.id.clone(),
            ResolvedTarget {
                id: configured.id.clone(),
                kind: "devfreq",
                label: capability.device_name.clone(),
                cpus: CpuSet::new(),
                hardware_limits: capability.limits,
                available_frequencies: capability.available_frequencies.clone(),
                paths: paths.clone(),
                automatic_policy: None,
                administrator_cap: configured.admin_cap_hz,
                critical_cap,
                sensor_failure_cap,
            },
        );
    }

    let mut warnings = discovery.warnings.clone();
    for id in discovery.frequency_targets.keys() {
        if !claimed_discovery_ids.contains(id) {
            warnings.push(format!(
                "discovered target {id} is not selected by device.json and remains read-only"
            ));
        }
    }
    Ok((resolved, warnings))
}

fn unique_match<'a, T>(id: &TargetId, selector: &str, candidates: &[&'a T]) -> Result<&'a T> {
    match candidates {
        [candidate] => Ok(*candidate),
        [] => bail!("{id}: {selector} did not match discovered hardware"),
        _ => bail!("{id}: {selector} is ambiguous"),
    }
}

fn discovery_paths<'a>(
    discovery: &'a LinuxDiscovery,
    id: &TargetId,
) -> Result<&'a FrequencyTargetPaths> {
    discovery
        .frequency_targets
        .get(id)
        .ok_or_else(|| anyhow!("discovery did not retain private paths for {id}"))
}

fn discovered_directory<'a>(discovery: &'a LinuxDiscovery, id: &TargetId) -> Option<&'a Path> {
    discovery.frequency_targets.get(id)?.minimum.parent()
}

fn validate_optional_directory(
    configured: Option<&str>,
    discovered: &FrequencyTargetPaths,
) -> Result<()> {
    let Some(configured) = configured else {
        return Ok(());
    };
    let parent = discovered
        .minimum
        .parent()
        .ok_or_else(|| anyhow!("discovered target path has no parent"))?;
    if Path::new(configured) != parent {
        bail!(
            "configured sysfs override {} does not match discovered target {}",
            configured,
            parent.display()
        );
    }
    Ok(())
}

fn validate_cap(
    cap: Option<Hertz>,
    hardware: FrequencyLimits,
    id: &TargetId,
    kind: &str,
    hertz_per_unit: u64,
) -> Result<()> {
    if hertz_per_unit == 0 {
        bail!("{id}: kernel target unit must be non-zero");
    }
    if cap.is_some_and(|value| value < hardware.min || value > hardware.max) {
        bail!("{id}: {kind} cap lies outside hardware limits");
    }
    if cap.is_some_and(|value| !value.get().is_multiple_of(hertz_per_unit)) {
        bail!("{id}: {kind} cap is not representable in the kernel target unit");
    }
    Ok(())
}

fn resolve_thermal_zones(
    device: &DeviceConfig,
    discovery: &LinuxDiscovery,
) -> Result<Vec<ThermalZoneConfig>> {
    let mut claimed = BTreeSet::new();
    let mut resolved = Vec::with_capacity(device.thermal_zones.len());
    for configured in &device.thermal_zones {
        let matching = discovery
            .capabilities
            .thermal_zones
            .iter()
            .filter(|zone| {
                if zone.zone_type != configured.zone_type {
                    return false;
                }
                if let Some(path) = &configured.sysfs_path {
                    discovery
                        .thermal_zone_paths
                        .get(&zone.id)
                        .is_some_and(|discovered| discovered == Path::new(path))
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();
        let discovered = match matching.as_slice() {
            [discovered] => *discovered,
            [] => bail!(
                "thermal zone {} ({}) was not discovered",
                configured.id,
                configured.zone_type
            ),
            _ => bail!(
                "thermal zone {} ({}) is ambiguous; use a root-owned path override",
                configured.id,
                configured.zone_type
            ),
        };
        if !claimed.insert(discovered.id.as_str()) {
            bail!(
                "thermal zone {} resolves to already claimed sensor {}",
                configured.id,
                discovered.id
            );
        }
        let path = discovery
            .thermal_zone_paths
            .get(&discovered.id)
            .ok_or_else(|| {
                anyhow!(
                    "discovery did not retain the path for thermal sensor {}",
                    discovered.id
                )
            })?
            .to_string_lossy()
            .into_owned();
        let mut zone = configured.clone();
        zone.sysfs_path = Some(path);
        resolved.push(zone);
    }
    Ok(resolved)
}

fn mode(id: &str, display_name: &str, description: &str) -> ModeInfo {
    ModeInfo {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        description: description.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use tempfile::tempdir;
    use uperf_core::{
        AppsConfig, ConfigBundle, CpuId, CpuPolicyCapability, CpuSet, DevfreqCapability,
        DeviceCapabilities, DeviceConfig, FrequencyLimits, Hertz, MAX_CONFIG_FILE_BYTES,
        PolicyConfig, TargetId, ThermalZoneCapability, Validate,
    };
    use uperf_linux::{FrequencyTargetPaths, LinuxDiscovery};

    use super::{ConfigurationPaths, ResolvedConfiguration, read_required, resolve_thermal_zones};

    #[test]
    fn configuration_reader_rejects_oversized_files() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("device.json");
        fs::write(&path, vec![b' '; MAX_CONFIG_FILE_BYTES + 1]).expect("write test file");
        let error = read_required(&path).expect_err("oversized file must fail");
        assert!(error.to_string().contains("configuration file limit"));
    }

    #[test]
    fn bundled_sm8550_configuration_is_cross_file_valid() {
        let device = DeviceConfig::from_json(include_str!("../../../config/devices/sm8550.json"))
            .expect("device configuration");
        let policy = PolicyConfig::from_json(include_str!("../../../config/policy.json"))
            .expect("policy configuration");
        let apps = AppsConfig::from_json(include_str!("../../../config/apps.json"))
            .expect("apps configuration");
        ConfigBundle {
            device,
            policy,
            apps,
        }
        .validate()
        .expect("cross-file configuration");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the complete SM8550 fixture is intentionally kept in one test so its cross-resource relationships remain visible"
    )]
    fn bundled_sm8550_configuration_resolves_against_its_linux_fixture() {
        let temporary = tempdir().expect("temporary root");
        let config_directory = temporary.path().join("etc");
        let state_directory = temporary.path().join("state");
        fs::create_dir_all(&config_directory).unwrap();
        fs::create_dir_all(&state_directory).unwrap();
        fs::write(
            config_directory.join("device.json"),
            include_bytes!("../../../config/devices/sm8550.json"),
        )
        .unwrap();
        fs::write(
            config_directory.join("policy.json"),
            include_bytes!("../../../config/policy.json"),
        )
        .unwrap();
        fs::write(
            state_directory.join("apps.json"),
            include_bytes!("../../../config/apps.json"),
        )
        .unwrap();

        let cpu_specs = [
            (
                "cpu.policy0",
                [0, 1, 2].as_slice(),
                [307_200_000, 1_344_000_000, 1_785_600_000, 2_016_000_000].as_slice(),
            ),
            (
                "cpu.policy3",
                [3, 4, 5, 6].as_slice(),
                [499_200_000, 1_920_000_000, 2_457_600_000, 2_803_200_000].as_slice(),
            ),
            (
                "cpu.policy7",
                [7].as_slice(),
                [595_200_000, 2_227_200_000, 2_726_400_000, 2_956_800_000].as_slice(),
            ),
        ];
        let mut cpu_policies = Vec::new();
        let mut frequency_targets = BTreeMap::new();
        for (name, cpus, frequencies) in cpu_specs {
            let id = TargetId::new(name).unwrap();
            let available = frequencies
                .iter()
                .copied()
                .map(Hertz::new)
                .collect::<Vec<_>>();
            let limits = FrequencyLimits {
                min: available[0],
                max: *available.last().unwrap(),
            };
            cpu_policies.push(CpuPolicyCapability {
                id: id.clone(),
                policy_name: name.trim_start_matches("cpu.").to_owned(),
                cpus: CpuSet::from_ids(cpus.iter().copied().map(CpuId::new)),
                limits,
                available_frequencies: available,
                governor: Some("schedutil".to_owned()),
            });
            let directory = PathBuf::from("/sys/devices/system/cpu/cpufreq")
                .join(name.trim_start_matches("cpu."));
            frequency_targets.insert(
                id.clone(),
                FrequencyTargetPaths {
                    id,
                    minimum: directory.join("scaling_min_freq"),
                    maximum: directory.join("scaling_max_freq"),
                    current: directory.join("scaling_cur_freq"),
                    hertz_per_unit: 1_000,
                },
            );
        }

        let gpu_id = TargetId::new("devfreq.3d00000.gpu").unwrap();
        frequency_targets.insert(
            gpu_id.clone(),
            FrequencyTargetPaths {
                id: gpu_id.clone(),
                minimum: PathBuf::from("/sys/class/devfreq/3d00000.gpu/min_freq"),
                maximum: PathBuf::from("/sys/class/devfreq/3d00000.gpu/max_freq"),
                current: PathBuf::from("/sys/class/devfreq/3d00000.gpu/cur_freq"),
                hertz_per_unit: 1,
            },
        );
        let thermal_types = [
            "cpuss0-thermal",
            "cpuss1-thermal",
            "cpuss2-thermal",
            "cpuss3-thermal",
            "gpuss-0-thermal",
        ];
        let thermal_zones = thermal_types
            .iter()
            .enumerate()
            .map(|(index, zone_type)| ThermalZoneCapability {
                id: format!("thermal_zone{index}"),
                zone_type: (*zone_type).to_owned(),
                current: None,
            })
            .collect::<Vec<_>>();
        let thermal_zone_paths = thermal_zones
            .iter()
            .map(|zone| {
                (
                    zone.id.clone(),
                    PathBuf::from("/sys/class/thermal").join(&zone.id),
                )
            })
            .collect();
        let discovery = LinuxDiscovery {
            capabilities: DeviceCapabilities {
                device_name: Some("SM8550 fixture".to_owned()),
                compatible: vec!["qcom,sm8550".to_owned()],
                matched_profile: Some("qcom-sm8550".to_owned()),
                cpu_policies,
                devfreq_targets: vec![DevfreqCapability {
                    id: gpu_id,
                    // The bundled selector intentionally uses the stable entry
                    // name while discovery may expose a different driver name.
                    device_name: "kgsl-3d0".to_owned(),
                    compatible: vec!["qcom,adreno".to_owned()],
                    limits: FrequencyLimits {
                        min: Hertz::new(220_000_000),
                        max: Hertz::new(680_000_000),
                    },
                    available_frequencies: vec![
                        Hertz::new(220_000_000),
                        Hertz::new(295_000_000),
                        Hertz::new(680_000_000),
                    ],
                    governor: Some("simple_ondemand".to_owned()),
                }],
                thermal_zones,
                input_devices: Vec::new(),
            },
            frequency_targets,
            thermal_zone_paths,
            warnings: Vec::new(),
        };
        let resolved = ResolvedConfiguration::load(
            &ConfigurationPaths::below(&config_directory, &state_directory),
            &discovery,
        )
        .expect("bundled SM8550 configuration must resolve");
        assert_eq!(resolved.targets.len(), 4);
        assert_eq!(resolved.thermal_zones.len(), 5);
    }

    #[test]
    fn thermal_path_override_must_equal_the_discovered_path() {
        let mut device = DeviceConfig::from_json(
            r#"{
                "schema_version": 2,
                "device_id": "test",
                "cpu_policies": [{
                    "id": "cpu.test",
                    "related_cpus": [0],
                    "floor_hz": 1000,
                    "reference_hz": 1000,
                    "efficient_cap_hz": 1000
                }],
                "thermal_zones": [{
                    "id": "soc",
                    "zone_type": "soc-thermal",
                    "sysfs_path": "/sys/class/thermal/thermal_zone7",
                    "warning": 70000,
                    "throttled": 80000,
                    "critical": 90000,
                    "hysteresis": 5000,
                    "dwell_ms": 100,
                    "stale_after_ms": 1000
                }]
            }"#,
        )
        .expect("device configuration");
        let discovery = LinuxDiscovery {
            capabilities: DeviceCapabilities {
                device_name: None,
                compatible: Vec::new(),
                matched_profile: None,
                cpu_policies: Vec::new(),
                devfreq_targets: Vec::new(),
                thermal_zones: vec![ThermalZoneCapability {
                    id: "thermal_zone7".to_owned(),
                    zone_type: "soc-thermal".to_owned(),
                    current: None,
                }],
                input_devices: Vec::new(),
            },
            frequency_targets: BTreeMap::new(),
            thermal_zone_paths: BTreeMap::from([(
                "thermal_zone7".to_owned(),
                PathBuf::from("/sys/class/thermal/thermal_zone7"),
            )]),
            warnings: Vec::new(),
        };

        let resolved = resolve_thermal_zones(&device, &discovery).expect("exact discovered path");
        assert_eq!(
            resolved[0].sysfs_path.as_deref(),
            Some("/sys/class/thermal/thermal_zone7")
        );
        device.thermal_zones[0].sysfs_path = Some("/sys/devices/pretend/thermal_zone7".to_owned());
        assert!(resolve_thermal_zones(&device, &discovery).is_err());

        device.thermal_zones[0].sysfs_path = Some("/sys/class/thermal/thermal_zone7".to_owned());
        device.thermal_zones[0].zone_type = "battery".to_owned();
        assert!(
            resolve_thermal_zones(&device, &discovery).is_err(),
            "an exact path must not bypass the trusted thermal-zone type"
        );
    }
}
