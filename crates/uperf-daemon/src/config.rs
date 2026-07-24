//! Transactional configuration loading and hardware-selector resolution.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use uperf_actuator::{FrequencyTarget, TargetRegistry};
use uperf_api::{ApiVersion, Capabilities, ModeInfo, TargetCapability, feature};
use uperf_core::{
    AppRuleEngine, AppsConfig, CONFIG_SCHEMA_VERSION, ConfigBundle, CpuSet, CpuTargetPolicy,
    DeviceConfig, FrequencyLimits, FrequencyPolicy, Hertz, MAX_CONFIG_FILE_BYTES, PolicyConfig,
    PolicyEngine, TargetId, ThermalZoneConfig,
};
use uperf_linux::{FrequencyTargetPaths, LinuxDiscovery};

/// Files participating in one configuration generation.
#[derive(Clone, Debug)]
pub struct ConfigurationPaths {
    pub device_override: PathBuf,
    pub device_profiles: PathBuf,
    pub policy: PathBuf,
    pub apps: PathBuf,
}

impl ConfigurationPaths {
    #[must_use]
    pub fn below(config_directory: impl AsRef<Path>, state_directory: impl AsRef<Path>) -> Self {
        Self {
            device_override: config_directory.as_ref().join("device.json"),
            device_profiles: config_directory.as_ref().join("devices"),
            policy: config_directory.as_ref().join("policy.json"),
            apps: state_directory.as_ref().join("apps.json"),
        }
    }

    #[must_use]
    pub fn with_device_profiles(mut self, directory: impl Into<PathBuf>) -> Self {
        self.device_profiles = directory.into();
        self
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
        let device = load_device_config(paths, discovery)?;
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

        let configured_policy = PolicyConfig::from_json(&policy_json)
            .with_context(|| format!("validate {}", paths.policy.display()))?;
        let apps = AppsConfig::from_json(&apps_json)
            .with_context(|| format!("validate {}", paths.apps.display()))?;
        let bundle = ConfigBundle {
            device: device.clone(),
            policy: configured_policy,
        };
        let policy = bundle
            .materialize_cpu_groups()
            .context("validate references across device and policy configuration")?;
        let policy_engine = PolicyEngine::new(policy.clone())?;
        let app_rule_engine = AppRuleEngine::new(&apps)?;
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
        features.push(feature::DEVICE_PROFILE.to_owned());
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

fn load_device_config(
    paths: &ConfigurationPaths,
    discovery: &LinuxDiscovery,
) -> Result<DeviceConfig> {
    match File::open(&paths.device_override) {
        Ok(file) => {
            let json = read_open_config(file, &paths.device_override)?;
            let device = DeviceConfig::from_json(&json)
                .with_context(|| format!("validate {}", paths.device_override.display()))?;
            validate_device_match(&device, discovery)?;
            Ok(device)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            load_catalog_device(paths, discovery)
        }
        Err(error) => {
            Err(error).with_context(|| format!("read {}", paths.device_override.display()))
        }
    }
}

fn load_catalog_device(
    paths: &ConfigurationPaths,
    discovery: &LinuxDiscovery,
) -> Result<DeviceConfig> {
    let entries = fs::read_dir(&paths.device_profiles).with_context(|| {
        format!(
            "read device profile directory {}",
            paths.device_profiles.display()
        )
    })?;
    let mut profile_paths = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| {
                format!(
                    "read entry in device profile directory {}",
                    paths.device_profiles.display()
                )
            })?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            profile_paths.push(path);
        }
    }
    profile_paths.sort();

    let mut matches = Vec::new();
    for path in profile_paths {
        let json = read_required(&path)?;
        let device = DeviceConfig::from_json(&json)
            .with_context(|| format!("validate shared device profile {}", path.display()))?;
        if device_matches(&device, discovery) {
            matches.push((device, path));
        }
    }

    match matches.as_mut_slice() {
        [(device, _)] => Ok(device.clone()),
        [] => bail!(
            "no device profile in {} exactly matches compatible values {:?} and model {:?}",
            paths.device_profiles.display(),
            discovery.capabilities.compatible,
            discovery.capabilities.device_name
        ),
        _ => {
            let descriptions = matches
                .iter()
                .map(|(device, path)| format!("{} ({})", device.device_id, path.display()))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("multiple shared device profiles match discovered hardware: {descriptions}")
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

fn device_matches(device: &DeviceConfig, discovery: &LinuxDiscovery) -> bool {
    let Some(selector) = &device.device_match else {
        return false;
    };
    selector.compatible.as_ref().is_none_or(|compatible| {
        discovery
            .capabilities
            .compatible
            .iter()
            .any(|value| value == compatible)
    }) && selector.product_name.as_ref().is_none_or(|product_name| {
        discovery.capabilities.device_name.as_deref() == Some(product_name.as_str())
    })
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
        AppsConfig, ConfigBundle, CpuId, CpuPolicyCapability, CpuSet, DeviceCapabilities,
        DeviceConfig, FrequencyLimits, Hertz, MAX_CONFIG_FILE_BYTES, PolicyConfig, TargetId,
        ThermalZoneCapability,
    };
    use uperf_linux::{FrequencyTargetPaths, LinuxDiscovery};

    use super::{
        ConfigurationPaths, ResolvedConfiguration, load_device_config, read_required,
        resolve_thermal_zones,
    };

    #[test]
    fn configuration_reader_rejects_oversized_files() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("device.json");
        fs::write(&path, vec![b' '; MAX_CONFIG_FILE_BYTES + 1]).expect("write test file");
        let error = read_required(&path).expect_err("oversized file must fail");
        assert!(error.to_string().contains("configuration file limit"));
    }

    #[test]
    fn every_bundled_device_configuration_is_cross_file_valid() {
        let policy = PolicyConfig::from_json(include_str!("../../../config/policy.json"))
            .expect("policy configuration");
        let _apps = AppsConfig::from_json(include_str!("../../../config/apps.json"))
            .expect("apps configuration");
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/devices");
        let mut count = 0;
        for entry in fs::read_dir(directory).expect("bundled device directory") {
            let path = entry.expect("device directory entry").path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let json = fs::read_to_string(&path).expect("read bundled device configuration");
            let device = DeviceConfig::from_json(&json).unwrap_or_else(|error| {
                panic!(
                    "validate bundled device configuration {}: {error}",
                    path.display()
                )
            });
            ConfigBundle {
                device,
                policy: policy.clone(),
            }
            .validate_cross_references()
            .unwrap_or_else(|error| panic!("validate {} against policy: {error}", path.display()));
            count += 1;
        }
        assert!(count > 0, "at least one bundled device profile is required");
    }

    #[test]
    fn catalog_selects_one_exact_match_and_override_takes_precedence() {
        let temporary = tempdir().expect("temporary root");
        let profiles = temporary.path().join("profiles");
        fs::create_dir_all(&profiles).unwrap();
        fs::write(
            profiles.join("soc-a.json"),
            synthetic_device_json("vendor-soc-a", "vendor,soc-a"),
        )
        .unwrap();
        fs::write(
            profiles.join("soc-b.json"),
            synthetic_device_json("vendor-soc-b", "vendor,soc-b"),
        )
        .unwrap();
        let paths =
            ConfigurationPaths::below(temporary.path().join("etc"), temporary.path().join("state"))
                .with_device_profiles(&profiles);
        let discovery = identity_discovery(&["vendor,board", "vendor,soc-b"]);

        let selected = load_device_config(&paths, &discovery).expect("one catalog match");
        assert_eq!(selected.device_id, "vendor-soc-b");

        fs::create_dir_all(paths.device_override.parent().unwrap()).unwrap();
        fs::write(
            &paths.device_override,
            synthetic_device_json("vendor-soc-b", "vendor,soc-b"),
        )
        .unwrap();
        let selected = load_device_config(&paths, &discovery).expect("administrator override");
        assert_eq!(selected.device_id, "vendor-soc-b");
    }

    #[test]
    fn catalog_rejects_ambiguous_and_substring_matches() {
        let temporary = tempdir().expect("temporary root");
        let profiles = temporary.path().join("profiles");
        fs::create_dir_all(&profiles).unwrap();
        fs::write(
            profiles.join("soc.json"),
            synthetic_device_json("vendor-soc", "vendor,soc"),
        )
        .unwrap();
        let paths =
            ConfigurationPaths::below(temporary.path().join("etc"), temporary.path().join("state"))
                .with_device_profiles(&profiles);

        let substring_only = identity_discovery(&["vendor,soc-gpu"]);
        let error = load_device_config(&paths, &substring_only)
            .expect_err("compatible matching must be exact");
        assert!(error.to_string().contains("no device profile"));

        let mut duplicate: serde_json::Value =
            serde_json::from_slice(&synthetic_device_json("vendor-soc", "vendor,soc")).unwrap();
        duplicate["device_id"] = serde_json::Value::from("vendor-soc-duplicate");
        fs::write(
            profiles.join("duplicate.json"),
            serde_json::to_vec(&duplicate).unwrap(),
        )
        .unwrap();
        let discovery = identity_discovery(&["vendor,soc"]);
        let error =
            load_device_config(&paths, &discovery).expect_err("ambiguous profiles must fail");
        assert!(
            error
                .to_string()
                .contains("multiple shared device profiles")
        );
    }

    #[test]
    fn device_configuration_resolves_against_a_linux_fixture() {
        let temporary = tempdir().expect("temporary root");
        let config_directory = temporary.path().join("etc");
        let state_directory = temporary.path().join("state");
        fs::create_dir_all(&config_directory).unwrap();
        fs::create_dir_all(&state_directory).unwrap();
        fs::write(
            config_directory.join("device.json"),
            synthetic_device_json("vendor-test-soc", "vendor,test-soc"),
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

        let id = TargetId::new("cpu.policy0").unwrap();
        let limits = FrequencyLimits {
            min: Hertz::new(1_000),
            max: Hertz::new(4_000),
        };
        let cpu_policies = vec![CpuPolicyCapability {
            id: id.clone(),
            policy_name: "policy0".to_owned(),
            cpus: CpuSet::from_ids([CpuId::new(0)]),
            limits,
            available_frequencies: vec![
                Hertz::new(1_000),
                Hertz::new(2_000),
                Hertz::new(3_000),
                Hertz::new(4_000),
            ],
            governor: Some("schedutil".to_owned()),
        }];
        let mut frequency_targets = BTreeMap::new();
        frequency_targets.insert(
            id.clone(),
            FrequencyTargetPaths {
                id,
                minimum: PathBuf::from("/sys/devices/system/cpu/cpufreq/policy0/scaling_min_freq"),
                maximum: PathBuf::from("/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq"),
                current: PathBuf::from("/sys/devices/system/cpu/cpufreq/policy0/scaling_cur_freq"),
                hertz_per_unit: 1,
            },
        );
        let discovery = LinuxDiscovery {
            capabilities: DeviceCapabilities {
                device_name: Some("test SoC fixture".to_owned()),
                compatible: vec!["vendor,test-soc".to_owned()],
                cpu_policies,
                devfreq_targets: Vec::new(),
                thermal_zones: vec![ThermalZoneCapability {
                    id: "thermal_zone0".to_owned(),
                    zone_type: "soc-thermal".to_owned(),
                    current: None,
                }],
                input_devices: Vec::new(),
            },
            frequency_targets,
            thermal_zone_paths: BTreeMap::from([(
                "thermal_zone0".to_owned(),
                PathBuf::from("/sys/class/thermal/thermal_zone0"),
            )]),
            warnings: Vec::new(),
        };
        let resolved = ResolvedConfiguration::load(
            &ConfigurationPaths::below(&config_directory, &state_directory),
            &discovery,
        )
        .expect("device configuration must resolve");
        assert_eq!(resolved.targets.len(), 1);
        assert_eq!(resolved.thermal_zones.len(), 1);
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

    fn identity_discovery(compatible: &[&str]) -> LinuxDiscovery {
        LinuxDiscovery {
            capabilities: DeviceCapabilities {
                device_name: Some("test board".to_owned()),
                compatible: compatible.iter().map(|value| (*value).to_owned()).collect(),
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

    fn synthetic_device_json(device_id: &str, compatible: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 2,
            "device_id": device_id,
            "device_match": { "compatible": compatible },
            "cpu_groups": {
                "all": [0],
                "balanced": [0],
                "efficient": [0],
                "performance": [0]
            },
            "cpu_policies": [{
                "id": "cpu.main",
                "related_cpus": [0],
                "floor_hz": 1_000,
                "reference_hz": 2_000,
                "efficient_cap_hz": 3_000,
                "admin_cap_hz": 4_000,
                "critical_cap_hz": 1_000,
                "sensor_failure_cap_hz": 1_000
            }],
            "thermal_zones": [{
                "id": "soc",
                "zone_type": "soc-thermal",
                "warning": 70_000,
                "throttled": 80_000,
                "critical": 90_000,
                "hysteresis": 5_000,
                "dwell_ms": 500,
                "stale_after_ms": 1_000
            }]
        }))
        .expect("serialize synthetic device configuration")
    }
}
