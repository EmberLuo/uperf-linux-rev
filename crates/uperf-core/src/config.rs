//! Strict version-2 configuration models and semantic validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Component, Path},
};

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{CpuSet, Hertz, MilliCelsius, ProfileId, Scene, SchedulingClass, TargetId, TaskPlan};

pub const CONFIG_SCHEMA_VERSION: u32 = 2;
/// Maximum UTF-8 size of any one v2 configuration document.
///
/// File readers enforce this before allocating the complete document. Keeping
/// the limit in the domain crate gives every frontend one canonical contract.
pub const MAX_CONFIG_FILE_BYTES: usize = 1024 * 1024;

pub const MAX_CPU_POLICIES: usize = 256;
pub const MAX_DEVFREQ_TARGETS: usize = 256;
pub const MAX_THERMAL_ZONES: usize = 256;
pub const MAX_DEVFREQ_COMPATIBLE_STRINGS: usize = 64;
pub const MAX_PROFILE_CONFIGS: usize = 8;
pub const MAX_TASK_PROFILES: usize = 256;
pub const MAX_PROCESS_RULES: usize = 1024;
pub const MAX_THREAD_RULES_PER_PROCESS: usize = 256;
pub const MAX_TOTAL_THREAD_RULES: usize = 4096;
pub const MAX_CGROUP_CLASSES: usize = 256;
pub const MAX_APP_RULES: usize = 4096;
pub const MAX_FOCUS_PROTECTED: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

impl ValidationIssue {
    #[must_use]
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors {
    issues: Vec<ValidationIssue>,
}

impl ValidationErrors {
    #[must_use]
    pub fn new(issues: Vec<ValidationIssue>) -> Self {
        Self { issues }
    }

    #[must_use]
    pub fn issues(&self) -> &[ValidationIssue] {
        &self.issues
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.issues.iter().enumerate() {
            if index != 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{}: {}", issue.path, issue.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

#[derive(Debug, Error)]
pub enum ConfigLoadError {
    #[error("configuration is {actual_bytes} bytes; maximum supported size is {max_bytes} bytes")]
    TooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("configuration validation failed: {0}")]
    Validation(#[from] ValidationErrors),
}

pub trait Validate {
    /// Perform semantic and cross-reference validation after deserialization.
    ///
    /// # Errors
    ///
    /// Returns all discovered issues rather than stopping at the first one.
    fn validate(&self) -> Result<(), ValidationErrors>;
}

/// Deserialize strict JSON and then run semantic validation.
///
/// # Errors
///
/// Returns [`ConfigLoadError::Json`] for syntax/type/unknown-field errors and
/// [`ConfigLoadError::Validation`] for invalid values or references.
pub fn parse_validated<T>(json: &str) -> Result<T, ConfigLoadError>
where
    T: DeserializeOwned + Validate,
{
    if json.len() > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigLoadError::TooLarge {
            actual_bytes: json.len(),
            max_bytes: MAX_CONFIG_FILE_BYTES,
        });
    }
    let value: T = serde_json::from_str(json)?;
    value.validate()?;
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatible: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CpuPolicyConfig {
    pub id: TargetId,
    /// Stable selector.  The Linux adapter resolves it against each policy's
    /// current `related_cpus`; policy directory numbering is not persisted.
    pub related_cpus: CpuSet,
    /// Optional administrator override for unusual kernels.  Normal profiles
    /// leave this unset and rely on `related_cpus`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysfs_path: Option<String>,
    pub floor_hz: Hertz,
    pub reference_hz: Hertz,
    pub efficient_cap_hz: Hertz,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_cap_hz: Option<Hertz>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_cap_hz: Option<Hertz>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_failure_cap_hz: Option<Hertz>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DevfreqTargetConfig {
    pub id: TargetId,
    /// Stable devfreq directory/device name used during discovery.
    pub device_name: String,
    /// Optional device-tree compatible strings used to disambiguate similarly
    /// named devfreq devices.
    #[serde(default)]
    #[schemars(length(max = MAX_DEVFREQ_COMPATIBLE_STRINGS))]
    pub compatible: Vec<String>,
    /// Optional administrator override for devices without a stable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysfs_path: Option<String>,
    #[serde(default = "default_true")]
    pub manual_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_cap_hz: Option<Hertz>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_cap_hz: Option<Hertz>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_failure_cap_hz: Option<Hertz>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThermalZoneConfig {
    pub id: String,
    /// Stable value of the thermal zone's `type` file.
    pub zone_type: String,
    /// Optional explicit override.  Normal profiles select by `zone_type`
    /// because `thermal_zoneN` numbering is not stable across boots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysfs_path: Option<String>,
    pub warning: MilliCelsius,
    pub throttled: MilliCelsius,
    pub critical: MilliCelsius,
    pub hysteresis: MilliCelsius,
    pub dwell_ms: u64,
    pub stale_after_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub schema_version: u32,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_match: Option<DeviceMatch>,
    #[serde(default)]
    pub cpu_groups: BTreeMap<String, CpuSet>,
    #[serde(default)]
    #[schemars(length(max = MAX_CPU_POLICIES))]
    pub cpu_policies: Vec<CpuPolicyConfig>,
    #[serde(default)]
    #[schemars(length(max = MAX_DEVFREQ_TARGETS))]
    pub devfreq_targets: Vec<DevfreqTargetConfig>,
    #[serde(default)]
    #[schemars(length(max = MAX_THERMAL_ZONES))]
    pub thermal_zones: Vec<ThermalZoneConfig>,
}

impl DeviceConfig {
    /// Parse and validate a v2 device configuration.
    ///
    /// # Errors
    ///
    /// Returns a JSON or semantic validation error.
    pub fn from_json(json: &str) -> Result<Self, ConfigLoadError> {
        parse_validated(json)
    }
}

impl Validate for DeviceConfig {
    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();
        validate_schema_version(self.schema_version, &mut issues);
        validate_name("device_id", &self.device_id, &mut issues);
        validate_collection_len(
            "cpu_policies",
            self.cpu_policies.len(),
            MAX_CPU_POLICIES,
            &mut issues,
        );
        validate_collection_len(
            "devfreq_targets",
            self.devfreq_targets.len(),
            MAX_DEVFREQ_TARGETS,
            &mut issues,
        );
        validate_collection_len(
            "thermal_zones",
            self.thermal_zones.len(),
            MAX_THERMAL_ZONES,
            &mut issues,
        );
        if let Some(device_match) = &self.device_match
            && device_match.compatible.is_none()
            && device_match.product_name.is_none()
        {
            issues.push(ValidationIssue::new(
                "device_match",
                "at least one stable device matcher is required",
            ));
        }
        if self.cpu_policies.is_empty() {
            issues.push(ValidationIssue::new(
                "cpu_policies",
                "at least one CPU policy selector is required",
            ));
        }

        let mut target_ids = BTreeSet::new();
        let mut used_cpus = CpuSet::new();
        for (index, policy) in self.cpu_policies.iter().take(MAX_CPU_POLICIES).enumerate() {
            let base = format!("cpu_policies[{index}]");
            if !target_ids.insert(policy.id.clone()) {
                issues.push(ValidationIssue::new(
                    format!("{base}.id"),
                    "duplicate frequency target id",
                ));
            }
            if policy.related_cpus.is_empty() {
                issues.push(ValidationIssue::new(
                    format!("{base}.related_cpus"),
                    "must contain at least one CPU",
                ));
            }
            if !used_cpus.is_disjoint(&policy.related_cpus) {
                issues.push(ValidationIssue::new(
                    format!("{base}.related_cpus"),
                    "overlaps a previous CPU policy",
                ));
            }
            for cpu in &policy.related_cpus {
                used_cpus.insert(*cpu);
            }
            if let Some(path) = &policy.sysfs_path {
                validate_sysfs_directory(&format!("{base}.sysfs_path"), path, &mut issues);
            }
            validate_frequency_model(policy, &base, &mut issues);
        }
        for (name, cpus) in &self.cpu_groups {
            validate_name(&format!("cpu_groups.{name}"), name, &mut issues);
            if cpus.is_empty() {
                issues.push(ValidationIssue::new(
                    format!("cpu_groups.{name}"),
                    "must contain at least one CPU",
                ));
            }
            if !cpus.is_subset(&used_cpus) {
                issues.push(ValidationIssue::new(
                    format!("cpu_groups.{name}"),
                    "references a CPU outside the device CPU policies",
                ));
            }
        }

        for (index, target) in self
            .devfreq_targets
            .iter()
            .take(MAX_DEVFREQ_TARGETS)
            .enumerate()
        {
            let base = format!("devfreq_targets[{index}]");
            if !target_ids.insert(target.id.clone()) {
                issues.push(ValidationIssue::new(
                    format!("{base}.id"),
                    "duplicate frequency target id",
                ));
            }
            validate_name(
                &format!("{base}.device_name"),
                &target.device_name,
                &mut issues,
            );
            validate_collection_len(
                &format!("{base}.compatible"),
                target.compatible.len(),
                MAX_DEVFREQ_COMPATIBLE_STRINGS,
                &mut issues,
            );
            for (compatible_index, compatible) in target
                .compatible
                .iter()
                .take(MAX_DEVFREQ_COMPATIBLE_STRINGS)
                .enumerate()
            {
                validate_name(
                    &format!("{base}.compatible[{compatible_index}]"),
                    compatible,
                    &mut issues,
                );
            }
            if let Some(path) = &target.sysfs_path {
                validate_sysfs_directory(&format!("{base}.sysfs_path"), path, &mut issues);
            }
            for (field, cap) in [
                ("admin_cap_hz", target.admin_cap_hz),
                ("critical_cap_hz", target.critical_cap_hz),
                ("sensor_failure_cap_hz", target.sensor_failure_cap_hz),
            ] {
                if cap.is_some_and(|value| value == Hertz::ZERO) {
                    issues.push(ValidationIssue::new(
                        format!("{base}.{field}"),
                        "must be greater than zero",
                    ));
                }
            }
            if let (Some(sensor_failure), Some(critical)) =
                (target.sensor_failure_cap_hz, target.critical_cap_hz)
                && sensor_failure > critical
            {
                issues.push(ValidationIssue::new(
                    format!("{base}.sensor_failure_cap_hz"),
                    "must not exceed critical_cap_hz",
                ));
            }
            if !target.manual_only {
                issues.push(ValidationIssue::new(
                    format!("{base}.manual_only"),
                    "v2 supports devfreq targets in manual-only mode",
                ));
            }
        }

        let mut zone_ids = BTreeSet::new();
        for (index, zone) in self
            .thermal_zones
            .iter()
            .take(MAX_THERMAL_ZONES)
            .enumerate()
        {
            let base = format!("thermal_zones[{index}]");
            validate_name(&format!("{base}.id"), &zone.id, &mut issues);
            validate_name(&format!("{base}.zone_type"), &zone.zone_type, &mut issues);
            if !zone_ids.insert(zone.id.as_str()) {
                issues.push(ValidationIssue::new(
                    format!("{base}.id"),
                    "duplicate thermal zone id",
                ));
            }
            if let Some(path) = &zone.sysfs_path {
                validate_sysfs_directory(&format!("{base}.sysfs_path"), path, &mut issues);
            }
            if !(zone.warning < zone.throttled && zone.throttled < zone.critical) {
                issues.push(ValidationIssue::new(
                    base.clone(),
                    "temperature thresholds must satisfy warning < throttled < critical",
                ));
            }
            for (field, temperature) in [
                ("warning", zone.warning),
                ("throttled", zone.throttled),
                ("critical", zone.critical),
            ] {
                if !(0..=250_000).contains(&temperature.get()) {
                    issues.push(ValidationIssue::new(
                        format!("{base}.{field}"),
                        "hot threshold must be in 0..=250000 millidegrees Celsius",
                    ));
                }
            }
            if zone.hysteresis.get() < 0 {
                issues.push(ValidationIssue::new(
                    format!("{base}.hysteresis"),
                    "must not be negative",
                ));
            }
            if zone.hysteresis.get() >= zone.critical.get() - zone.warning.get() {
                issues.push(ValidationIssue::new(
                    format!("{base}.hysteresis"),
                    "must be smaller than critical - warning",
                ));
            }
            validate_positive_duration(&format!("{base}.dwell_ms"), zone.dwell_ms, &mut issues);
            validate_positive_duration(
                &format!("{base}.stale_after_ms"),
                zone.stale_after_ms,
                &mut issues,
            );
        }

        finish_validation(issues)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScenePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_efficiency: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub id: ProfileId,
    pub margin: f64,
    pub burst: f64,
    pub limit_efficiency: bool,
    #[serde(default)]
    pub scenes: BTreeMap<Scene, ScenePatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub sample_interval_ms: u64,
    pub ema_time_constant_ms: u64,
    pub heavy_enter: f64,
    pub heavy_exit: f64,
    pub heavy_dwell_ms: u64,
}

impl Default for LoadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 250,
            ema_time_constant_ms: 500,
            heavy_enter: 0.60,
            heavy_exit: 0.20,
            heavy_dwell_ms: 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThermalPolicyConfig {
    pub sample_interval_ms: u64,
}

impl Default for ThermalPolicyConfig {
    fn default() -> Self {
        Self {
            sample_interval_ms: 250,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub trigger_duration_ms: u64,
    pub gesture_duration_ms: u64,
    pub switch_duration_ms: u64,
    pub wake_duration_ms: u64,
    /// Normalized movement threshold in `[0, 1]`.
    pub swipe_distance: f64,
    /// Normalized distance from a display edge in `[0, 0.5]`.
    pub edge_width: f64,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_duration_ms: 30,
            gesture_duration_ms: 100,
            switch_duration_ms: 400,
            wake_duration_ms: 500,
            swipe_distance: 0.03,
            edge_width: 0.03,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkloadMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comm_regex: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskProfileConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_group: Option<String>,
    pub plan: TaskPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadRuleConfig {
    pub comm_regex: String,
    pub task_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessRuleConfig {
    pub name: String,
    pub matcher: WorkloadMatcher,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_class: Option<String>,
    #[serde(default)]
    #[schemars(length(max = MAX_THREAD_RULES_PER_PROCESS))]
    pub threads: Vec<ThreadRuleConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CgroupClassConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_cpu_group: Option<String>,
    #[serde(default)]
    pub allowed_cpus: CpuSet,
    pub cpu_weight: u16,
}

/// Focus-driven workload selection.
///
/// A trusted desktop adapter reports which process owns the focused window; the
/// daemon treats that report as a *workload source*, never as a profile tier.
/// When no process rule matches the focused workload, [`Self::task_profile`]
/// supplies a gentle default plan so the feature is useful without requiring
/// operators to author per-application rules first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FocusConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Task profile applied to a focused workload that matches no process rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_profile: Option<String>,
    /// Lease lifetime; a reporter that stops renewing loses the boost.
    #[serde(default = "default_focus_lease_ttl_ms")]
    #[schemars(range(min = 15_000, max = 600_000))]
    pub lease_ttl_ms: u64,
    /// Coalescing window for rapid window switching.
    #[serde(default = "default_focus_debounce_ms")]
    pub debounce_ms: u64,
    /// Processes that may never be leased as a focused workload.
    #[serde(default)]
    #[schemars(length(max = MAX_FOCUS_PROTECTED))]
    pub protected: Vec<WorkloadMatcher>,
}

const fn default_focus_lease_ttl_ms() -> u64 {
    15_000
}

const fn default_focus_debounce_ms() -> u64 {
    150
}

impl Default for FocusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            task_profile: None,
            lease_ttl_ms: default_focus_lease_ttl_ms(),
            debounce_ms: default_focus_debounce_ms(),
            protected: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    #[schemars(length(max = MAX_TASK_PROFILES))]
    pub task_profiles: Vec<TaskProfileConfig>,
    #[serde(default)]
    #[schemars(length(max = MAX_PROCESS_RULES))]
    pub process_rules: Vec<ProcessRuleConfig>,
    #[serde(default)]
    #[schemars(length(max = MAX_CGROUP_CLASSES))]
    pub cgroup_classes: Vec<CgroupClassConfig>,
    #[serde(default)]
    pub focus: FocusConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    pub schema_version: u32,
    pub default_profile: ProfileId,
    #[schemars(length(max = MAX_PROFILE_CONFIGS))]
    pub profiles: Vec<ProfileConfig>,
    #[serde(default)]
    pub load: LoadConfig,
    #[serde(default)]
    pub thermal: ThermalPolicyConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
}

impl PolicyConfig {
    /// Parse and validate a v2 policy configuration.
    ///
    /// # Errors
    ///
    /// Returns a JSON or semantic validation error.
    pub fn from_json(json: &str) -> Result<Self, ConfigLoadError> {
        parse_validated(json)
    }

    #[must_use]
    pub fn profile(&self, id: ProfileId) -> Option<&ProfileConfig> {
        self.profiles.iter().find(|profile| profile.id == id)
    }
}

impl Validate for PolicyConfig {
    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();
        validate_schema_version(self.schema_version, &mut issues);
        validate_collection_len(
            "profiles",
            self.profiles.len(),
            MAX_PROFILE_CONFIGS,
            &mut issues,
        );

        let mut profiles = BTreeSet::new();
        for (index, profile) in self.profiles.iter().take(MAX_PROFILE_CONFIGS).enumerate() {
            let base = format!("profiles[{index}]");
            if !profiles.insert(profile.id) {
                issues.push(ValidationIssue::new(
                    format!("{base}.id"),
                    "duplicate profile id",
                ));
            }
            validate_ratio(
                &format!("{base}.margin"),
                profile.margin,
                0.0,
                10.0,
                &mut issues,
            );
            validate_ratio(
                &format!("{base}.burst"),
                profile.burst,
                0.0,
                1.0,
                &mut issues,
            );
            for (scene, patch) in &profile.scenes {
                let patch_path = format!("{base}.scenes.{scene}");
                if let Some(margin) = patch.margin {
                    validate_ratio(
                        &format!("{patch_path}.margin"),
                        margin,
                        0.0,
                        10.0,
                        &mut issues,
                    );
                }
                if let Some(burst) = patch.burst {
                    validate_ratio(&format!("{patch_path}.burst"), burst, 0.0, 1.0, &mut issues);
                }
            }
        }
        if !profiles.contains(&self.default_profile) {
            issues.push(ValidationIssue::new(
                "default_profile",
                "does not reference a configured profile",
            ));
        }
        for required in [
            ProfileId::Powersave,
            ProfileId::Balance,
            ProfileId::Performance,
        ] {
            if !profiles.contains(&required) {
                issues.push(ValidationIssue::new(
                    "profiles",
                    format!("missing required `{required}` profile"),
                ));
            }
        }

        validate_positive_duration(
            "load.sample_interval_ms",
            self.load.sample_interval_ms,
            &mut issues,
        );
        validate_positive_duration(
            "load.ema_time_constant_ms",
            self.load.ema_time_constant_ms,
            &mut issues,
        );
        validate_ratio(
            "load.heavy_enter",
            self.load.heavy_enter,
            0.0,
            1.0,
            &mut issues,
        );
        validate_ratio(
            "load.heavy_exit",
            self.load.heavy_exit,
            0.0,
            1.0,
            &mut issues,
        );
        if self.load.heavy_exit >= self.load.heavy_enter {
            issues.push(ValidationIssue::new(
                "load",
                "heavy_exit must be less than heavy_enter",
            ));
        }
        validate_positive_duration("load.heavy_dwell_ms", self.load.heavy_dwell_ms, &mut issues);
        validate_positive_duration(
            "thermal.sample_interval_ms",
            self.thermal.sample_interval_ms,
            &mut issues,
        );
        for (path, value) in [
            ("input.trigger_duration_ms", self.input.trigger_duration_ms),
            ("input.gesture_duration_ms", self.input.gesture_duration_ms),
            ("input.switch_duration_ms", self.input.switch_duration_ms),
            ("input.wake_duration_ms", self.input.wake_duration_ms),
        ] {
            validate_positive_duration(path, value, &mut issues);
        }
        validate_ratio(
            "input.swipe_distance",
            self.input.swipe_distance,
            f64::EPSILON,
            1.0,
            &mut issues,
        );
        validate_ratio(
            "input.edge_width",
            self.input.edge_width,
            f64::EPSILON,
            0.5,
            &mut issues,
        );
        validate_scheduler(&self.scheduler, &mut issues);

        finish_validation(issues)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppRule {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Higher values are evaluated first. Equal priorities retain JSON order.
    #[serde(default)]
    pub priority: i32,
    pub matcher: WorkloadMatcher,
    pub profile: ProfileId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppsConfig {
    pub schema_version: u32,
    #[serde(default)]
    #[schemars(length(max = MAX_APP_RULES))]
    pub rules: Vec<AppRule>,
}

impl AppsConfig {
    /// Parse and validate a v2 application-rule configuration.
    ///
    /// # Errors
    ///
    /// Returns a JSON or semantic validation error.
    pub fn from_json(json: &str) -> Result<Self, ConfigLoadError> {
        parse_validated(json)
    }

    /// Return enabled rules in deterministic first-match evaluation order.
    #[must_use]
    pub fn ordered_enabled_rules(&self) -> Vec<&AppRule> {
        let mut rules = self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.enabled)
            .collect::<Vec<_>>();
        rules.sort_by(|(left_index, left), (right_index, right)| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left_index.cmp(right_index))
        });
        rules.into_iter().map(|(_, rule)| rule).collect()
    }
}

impl Validate for AppsConfig {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();
        validate_schema_version(self.schema_version, &mut issues);
        validate_collection_len("rules", self.rules.len(), MAX_APP_RULES, &mut issues);
        let mut ids = BTreeSet::new();
        for (index, rule) in self.rules.iter().take(MAX_APP_RULES).enumerate() {
            let base = format!("rules[{index}]");
            validate_name(&format!("{base}.id"), &rule.id, &mut issues);
            if !ids.insert(rule.id.as_str()) {
                issues.push(ValidationIssue::new(
                    format!("{base}.id"),
                    "duplicate app rule id",
                ));
            }
            validate_matcher(&format!("{base}.matcher"), &rule.matcher, &mut issues);
        }
        finish_validation(issues)
    }
}

/// Runtime configuration documents with cross-file references.
///
/// This is not another on-disk format; it exists to validate references spanning
/// the device and policy documents. App rules are validated independently.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigBundle {
    pub device: DeviceConfig,
    pub policy: PolicyConfig,
}

impl ConfigBundle {
    /// Validate only references and invariants spanning separately validated
    /// configuration documents.
    ///
    /// # Errors
    ///
    /// Returns all cross-document issues.
    pub fn validate_cross_references(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();

        if self.device.thermal_zones.is_empty() {
            issues.push(ValidationIssue::new(
                "device.thermal_zones",
                "at least one trusted thermal zone is required",
            ));
        }
        for (index, zone) in self.device.thermal_zones.iter().enumerate() {
            if zone.stale_after_ms < self.policy.thermal.sample_interval_ms {
                issues.push(ValidationIssue::new(
                    format!("device.thermal_zones[{index}].stale_after_ms"),
                    "must not be shorter than the thermal sampling interval",
                ));
            }
        }

        let configured_cpus = self
            .device
            .cpu_policies
            .iter()
            .flat_map(|policy| policy.related_cpus.iter().copied())
            .collect::<CpuSet>();
        for (index, profile) in self.policy.scheduler.task_profiles.iter().enumerate() {
            if let Some(group) = &profile.affinity_group
                && !self.device.cpu_groups.contains_key(group)
            {
                issues.push(ValidationIssue::new(
                    format!("policy.scheduler.task_profiles[{index}].affinity_group"),
                    format!("references unknown device CPU group {group:?}"),
                ));
            }
            if let Some(affinity) = &profile.plan.affinity
                && !affinity.is_subset(&configured_cpus)
            {
                issues.push(ValidationIssue::new(
                    format!("policy.scheduler.task_profiles[{index}].plan.affinity"),
                    "references a CPU outside the device CPU policies",
                ));
            }
        }
        for (index, class) in self.policy.scheduler.cgroup_classes.iter().enumerate() {
            if let Some(group) = &class.allowed_cpu_group
                && !self.device.cpu_groups.contains_key(group)
            {
                issues.push(ValidationIssue::new(
                    format!("policy.scheduler.cgroup_classes[{index}].allowed_cpu_group"),
                    format!("references unknown device CPU group {group:?}"),
                ));
            }
            if !class.allowed_cpus.is_subset(&configured_cpus) {
                issues.push(ValidationIssue::new(
                    format!("policy.scheduler.cgroup_classes[{index}].allowed_cpus"),
                    "references a CPU outside the device CPU policies",
                ));
            }
        }

        finish_validation(issues)
    }

    /// Resolve device-defined logical CPU groups into the concrete scheduler
    /// CPU sets consumed by the policy engine.
    ///
    /// # Errors
    ///
    /// Returns the same cross-document validation errors as
    /// [`Self::validate_cross_references`] before materializing any reference.
    pub fn materialize_cpu_groups(&self) -> Result<PolicyConfig, ValidationErrors> {
        self.validate_cross_references()?;
        let mut policy = self.policy.clone();
        for profile in &mut policy.scheduler.task_profiles {
            if let Some(group) = profile.affinity_group.take() {
                profile.plan.affinity = self.device.cpu_groups.get(&group).cloned();
            }
        }
        for class in &mut policy.scheduler.cgroup_classes {
            if let Some(group) = class.allowed_cpu_group.take() {
                class.allowed_cpus = self
                    .device
                    .cpu_groups
                    .get(&group)
                    .cloned()
                    .unwrap_or_default();
            }
        }
        Ok(policy)
    }
}

#[must_use]
pub fn device_config_schema() -> schemars::Schema {
    schemars::schema_for!(DeviceConfig)
}

#[must_use]
pub fn policy_config_schema() -> schemars::Schema {
    schemars::schema_for!(PolicyConfig)
}

#[must_use]
pub fn apps_config_schema() -> schemars::Schema {
    schemars::schema_for!(AppsConfig)
}

fn validate_schema_version(version: u32, issues: &mut Vec<ValidationIssue>) {
    if version != CONFIG_SCHEMA_VERSION {
        issues.push(ValidationIssue::new(
            "schema_version",
            format!("expected {CONFIG_SCHEMA_VERSION}, got {version}"),
        ));
    }
}

fn validate_frequency_model(
    policy: &CpuPolicyConfig,
    base: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    if policy.floor_hz == Hertz::ZERO {
        issues.push(ValidationIssue::new(
            format!("{base}.floor_hz"),
            "must be greater than zero",
        ));
    }
    if policy.reference_hz < policy.floor_hz || policy.efficient_cap_hz < policy.floor_hz {
        issues.push(ValidationIssue::new(
            base,
            "reference_hz and efficient_cap_hz must not be lower than floor_hz",
        ));
    }
    for (field, cap) in [
        ("admin_cap_hz", policy.admin_cap_hz),
        ("critical_cap_hz", policy.critical_cap_hz),
        ("sensor_failure_cap_hz", policy.sensor_failure_cap_hz),
    ] {
        if let Some(value) = cap {
            if value == Hertz::ZERO {
                issues.push(ValidationIssue::new(
                    format!("{base}.{field}"),
                    "must be greater than zero",
                ));
            }
            if value < policy.floor_hz {
                issues.push(ValidationIssue::new(
                    format!("{base}.{field}"),
                    "must not be lower than floor_hz",
                ));
            }
        }
    }
    if let (Some(sensor_failure), Some(critical)) =
        (policy.sensor_failure_cap_hz, policy.critical_cap_hz)
        && sensor_failure > critical
    {
        issues.push(ValidationIssue::new(
            format!("{base}.sensor_failure_cap_hz"),
            "must not exceed critical_cap_hz",
        ));
    }
}

fn validate_scheduler(config: &SchedulerConfig, issues: &mut Vec<ValidationIssue>) {
    validate_collection_len(
        "scheduler.task_profiles",
        config.task_profiles.len(),
        MAX_TASK_PROFILES,
        issues,
    );
    validate_collection_len(
        "scheduler.process_rules",
        config.process_rules.len(),
        MAX_PROCESS_RULES,
        issues,
    );
    validate_collection_len(
        "scheduler.cgroup_classes",
        config.cgroup_classes.len(),
        MAX_CGROUP_CLASSES,
        issues,
    );
    let mut task_profiles = BTreeSet::new();
    for (index, profile) in config
        .task_profiles
        .iter()
        .take(MAX_TASK_PROFILES)
        .enumerate()
    {
        let base = format!("scheduler.task_profiles[{index}]");
        validate_name(&format!("{base}.id"), &profile.id, issues);
        if !task_profiles.insert(profile.id.as_str()) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "duplicate task profile id",
            ));
        }
        if let Some(group) = &profile.affinity_group {
            validate_name(&format!("{base}.affinity_group"), group, issues);
            if profile.plan.affinity.is_some() {
                issues.push(ValidationIssue::new(
                    base.clone(),
                    "affinity_group and plan.affinity are mutually exclusive",
                ));
            }
        }
        validate_task_plan(&format!("{base}.plan"), &profile.plan, issues);
    }

    let mut cgroups = BTreeSet::new();
    for (index, class) in config
        .cgroup_classes
        .iter()
        .take(MAX_CGROUP_CLASSES)
        .enumerate()
    {
        let base = format!("scheduler.cgroup_classes[{index}]");
        validate_name(&format!("{base}.id"), &class.id, issues);
        if !cgroups.insert(class.id.as_str()) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "duplicate cgroup class id",
            ));
        }
        if let Some(group) = &class.allowed_cpu_group {
            validate_name(&format!("{base}.allowed_cpu_group"), group, issues);
        }
        if class.allowed_cpu_group.is_some() && !class.allowed_cpus.is_empty() {
            issues.push(ValidationIssue::new(
                base.clone(),
                "allowed_cpu_group and allowed_cpus are mutually exclusive",
            ));
        }
        if class.allowed_cpu_group.is_none() && class.allowed_cpus.is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.allowed_cpus"),
                "must contain at least one CPU when allowed_cpu_group is unset",
            ));
        }
        if !(1..=10_000).contains(&class.cpu_weight) {
            issues.push(ValidationIssue::new(
                format!("{base}.cpu_weight"),
                "must be in 1..=10000",
            ));
        }
    }

    validate_focus(&config.focus, &task_profiles, issues);
    validate_process_rules(config, &task_profiles, &cgroups, issues);
}

fn validate_focus(
    focus: &FocusConfig,
    task_profiles: &BTreeSet<&str>,
    issues: &mut Vec<ValidationIssue>,
) {
    validate_collection_len(
        "scheduler.focus.protected",
        focus.protected.len(),
        MAX_FOCUS_PROTECTED,
        issues,
    );
    match &focus.task_profile {
        Some(profile) if !task_profiles.contains(profile.as_str()) => {
            issues.push(ValidationIssue::new(
                "scheduler.focus.task_profile",
                "references an unknown task profile",
            ));
        }
        None if focus.enabled => {
            issues.push(ValidationIssue::new(
                "scheduler.focus.task_profile",
                "is required when focus is enabled",
            ));
        }
        _ => {}
    }
    if !(15_000..=600_000).contains(&focus.lease_ttl_ms) {
        issues.push(ValidationIssue::new(
            "scheduler.focus.lease_ttl_ms",
            "must be in 15000..=600000",
        ));
    }
    if focus.debounce_ms > 2_000 {
        issues.push(ValidationIssue::new(
            "scheduler.focus.debounce_ms",
            "must not exceed 2000",
        ));
    }
    if focus.debounce_ms >= focus.lease_ttl_ms {
        issues.push(ValidationIssue::new(
            "scheduler.focus.debounce_ms",
            "must be shorter than lease_ttl_ms",
        ));
    }
    for (index, matcher) in focus.protected.iter().take(MAX_FOCUS_PROTECTED).enumerate() {
        validate_matcher(
            &format!("scheduler.focus.protected[{index}]"),
            matcher,
            issues,
        );
    }
}

fn validate_process_rules(
    config: &SchedulerConfig,
    task_profiles: &BTreeSet<&str>,
    cgroups: &BTreeSet<&str>,
    issues: &mut Vec<ValidationIssue>,
) {
    let total_thread_rules = config
        .process_rules
        .iter()
        .take(MAX_PROCESS_RULES)
        .fold(0_usize, |total, rule| {
            total.saturating_add(rule.threads.len())
        });
    validate_collection_len(
        "scheduler.process_rules[].threads",
        total_thread_rules,
        MAX_TOTAL_THREAD_RULES,
        issues,
    );

    let mut validated_thread_rules = 0_usize;
    for (index, rule) in config
        .process_rules
        .iter()
        .take(MAX_PROCESS_RULES)
        .enumerate()
    {
        let base = format!("scheduler.process_rules[{index}]");
        validate_name(&format!("{base}.name"), &rule.name, issues);
        validate_matcher(&format!("{base}.matcher"), &rule.matcher, issues);
        if let Some(profile) = &rule.task_profile
            && !task_profiles.contains(profile.as_str())
        {
            issues.push(ValidationIssue::new(
                format!("{base}.task_profile"),
                "references an unknown task profile",
            ));
        }
        if let Some(class) = &rule.cgroup_class
            && !cgroups.contains(class.as_str())
        {
            issues.push(ValidationIssue::new(
                format!("{base}.cgroup_class"),
                "references an unknown cgroup class",
            ));
        }
        validate_collection_len(
            &format!("{base}.threads"),
            rule.threads.len(),
            MAX_THREAD_RULES_PER_PROCESS,
            issues,
        );
        let remaining_thread_budget = MAX_TOTAL_THREAD_RULES.saturating_sub(validated_thread_rules);
        let thread_limit = MAX_THREAD_RULES_PER_PROCESS.min(remaining_thread_budget);
        for (thread_index, thread) in rule.threads.iter().take(thread_limit).enumerate() {
            let thread_base = format!("{base}.threads[{thread_index}]");
            validate_regex(
                &format!("{thread_base}.comm_regex"),
                &thread.comm_regex,
                issues,
            );
            if !task_profiles.contains(thread.task_profile.as_str()) {
                issues.push(ValidationIssue::new(
                    format!("{thread_base}.task_profile"),
                    "references an unknown task profile",
                ));
            }
        }
        validated_thread_rules =
            validated_thread_rules.saturating_add(rule.threads.len().min(thread_limit));
    }
}

fn validate_task_plan(path: &str, plan: &TaskPlan, issues: &mut Vec<ValidationIssue>) {
    if plan.affinity.as_ref().is_some_and(CpuSet::is_empty) {
        issues.push(ValidationIssue::new(
            format!("{path}.affinity"),
            "must not be empty",
        ));
    }
    if plan.nice.is_some_and(|nice| !(-20..=19).contains(&nice)) {
        issues.push(ValidationIssue::new(
            format!("{path}.nice"),
            "must be in -20..=19",
        ));
    }
    if !matches!(
        plan.scheduling_class,
        None | Some(SchedulingClass::Other | SchedulingClass::Batch | SchedulingClass::Idle)
    ) {
        issues.push(ValidationIssue::new(
            format!("{path}.scheduling_class"),
            "unsupported scheduling class",
        ));
    }
    if plan.uclamp_min.is_some_and(|minimum| minimum > 1_024) {
        issues.push(ValidationIssue::new(
            format!("{path}.uclamp_min"),
            "must not exceed 1024",
        ));
    }
    if plan.uclamp_max.is_some_and(|maximum| maximum > 1_024) {
        issues.push(ValidationIssue::new(
            format!("{path}.uclamp_max"),
            "must not exceed 1024",
        ));
    }
    if let (Some(minimum), Some(maximum)) = (plan.uclamp_min, plan.uclamp_max)
        && minimum > maximum
    {
        issues.push(ValidationIssue::new(
            path,
            "uclamp_min must not exceed uclamp_max when both are set",
        ));
    }
}

fn validate_matcher(path: &str, matcher: &WorkloadMatcher, issues: &mut Vec<ValidationIssue>) {
    if matcher.executable.is_none() && matcher.desktop_id.is_none() && matcher.comm_regex.is_none()
    {
        issues.push(ValidationIssue::new(
            path,
            "at least one matcher field is required",
        ));
    }
    for (field, value) in [
        ("executable", matcher.executable.as_deref()),
        ("desktop_id", matcher.desktop_id.as_deref()),
    ] {
        if let Some(value) = value
            && value.trim().is_empty()
        {
            issues.push(ValidationIssue::new(
                format!("{path}.{field}"),
                "must not be empty",
            ));
        }
    }
    if matcher.desktop_id.is_some() {
        issues.push(ValidationIssue::new(
            format!("{path}.desktop_id"),
            "desktop_id matching is reserved for a future trusted desktop adapter and is unsupported in schema v2",
        ));
    }
    if let Some(regex) = &matcher.comm_regex {
        validate_regex(&format!("{path}.comm_regex"), regex, issues);
    }
}

fn validate_regex(path: &str, value: &str, issues: &mut Vec<ValidationIssue>) {
    if value.is_empty() {
        issues.push(ValidationIssue::new(path, "must not be empty"));
    } else if value.len() > 1_024 {
        issues.push(ValidationIssue::new(path, "must not exceed 1024 bytes"));
    } else if let Err(error) = Regex::new(value) {
        issues.push(ValidationIssue::new(
            path,
            format!("invalid regex: {error}"),
        ));
    }
}

fn validate_name(path: &str, value: &str, issues: &mut Vec<ValidationIssue>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        issues.push(ValidationIssue::new(path, "must not be empty"));
    } else if trimmed.len() > 128 {
        issues.push(ValidationIssue::new(path, "must not exceed 128 bytes"));
    } else if trimmed.chars().any(char::is_control) {
        issues.push(ValidationIssue::new(
            path,
            "must not contain control characters",
        ));
    }
}

fn validate_collection_len(
    path: &str,
    actual: usize,
    maximum: usize,
    issues: &mut Vec<ValidationIssue>,
) {
    if actual > maximum {
        issues.push(ValidationIssue::new(
            path,
            format!("must not contain more than {maximum} entries (got {actual})"),
        ));
    }
}

fn validate_sysfs_directory(path: &str, value: &str, issues: &mut Vec<ValidationIssue>) {
    let candidate = Path::new(value);
    let has_forbidden_component = candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::CurDir | Component::Prefix(_)
        )
    });
    let under_allowed_root =
        candidate.starts_with("/sys/devices") || candidate.starts_with("/sys/class");
    if !candidate.is_absolute() || has_forbidden_component || !under_allowed_root {
        issues.push(ValidationIssue::new(
            path,
            "must be an absolute normalized directory below /sys/devices or /sys/class",
        ));
    }
}

fn validate_ratio(
    path: &str,
    value: f64,
    minimum: f64,
    maximum: f64,
    issues: &mut Vec<ValidationIssue>,
) {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        issues.push(ValidationIssue::new(
            path,
            format!("must be finite and in {minimum}..={maximum}"),
        ));
    }
}

fn validate_positive_duration(path: &str, value: u64, issues: &mut Vec<ValidationIssue>) {
    const MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1_000;
    if value == 0 || value > MAX_DURATION_MS {
        issues.push(ValidationIssue::new(
            path,
            format!("must be in 1..={MAX_DURATION_MS} milliseconds"),
        ));
    }
}

fn finish_validation(issues: Vec<ValidationIssue>) -> Result<(), ValidationErrors> {
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors::new(issues))
    }
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{CpuId, ProfileId};

    fn target(value: &str) -> TargetId {
        TargetId::new(value).expect("valid target id")
    }

    fn valid_device() -> DeviceConfig {
        DeviceConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            device_id: "test-soc".into(),
            device_match: None,
            cpu_groups: BTreeMap::new(),
            cpu_policies: vec![
                CpuPolicyConfig {
                    id: target("cpu.efficiency"),
                    related_cpus: CpuSet::from_ids([CpuId(0), CpuId(1), CpuId(2)]),
                    sysfs_path: None,
                    floor_hz: Hertz(307_200_000),
                    reference_hz: Hertz(1_500_000_000),
                    efficient_cap_hz: Hertz(2_016_000_000),
                    admin_cap_hz: None,
                    critical_cap_hz: Some(Hertz(600_000_000)),
                    sensor_failure_cap_hz: Some(Hertz(600_000_000)),
                },
                CpuPolicyConfig {
                    id: target("cpu.prime"),
                    related_cpus: CpuSet::from_ids([CpuId(7)]),
                    sysfs_path: Some("/sys/class/devfreq/../not-allowed".into()),
                    floor_hz: Hertz(595_200_000),
                    reference_hz: Hertz(2_200_000_000),
                    efficient_cap_hz: Hertz(2_957_000_000),
                    admin_cap_hz: None,
                    critical_cap_hz: Some(Hertz(739_000_000)),
                    sensor_failure_cap_hz: Some(Hertz(739_000_000)),
                },
            ],
            devfreq_targets: Vec::new(),
            thermal_zones: Vec::new(),
        }
    }

    fn valid_policy() -> PolicyConfig {
        PolicyConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            default_profile: ProfileId::Balance,
            profiles: [
                ProfileId::Powersave,
                ProfileId::Balance,
                ProfileId::Performance,
            ]
            .into_iter()
            .map(|id| ProfileConfig {
                id,
                margin: 0.2,
                burst: 0.0,
                limit_efficiency: id == ProfileId::Powersave,
                scenes: BTreeMap::new(),
            })
            .collect(),
            load: LoadConfig::default(),
            thermal: ThermalPolicyConfig::default(),
            input: InputConfig::default(),
            scheduler: SchedulerConfig::default(),
        }
    }

    #[test]
    fn focus_lease_ttl_matches_bundled_reporter_contract() {
        let mut policy = valid_policy();
        for ttl in [15_000, 600_000] {
            policy.scheduler.focus.lease_ttl_ms = ttl;
            policy.validate().expect("supported focus lease TTL");
        }

        for ttl in [14_999, 600_001] {
            policy.scheduler.focus.lease_ttl_ms = ttl;
            let error = policy.validate().expect_err("unsupported focus lease TTL");
            assert!(error.issues().iter().any(|issue| {
                issue.path == "scheduler.focus.lease_ttl_ms"
                    && issue.message == "must be in 15000..=600000"
            }));
        }
    }

    #[test]
    fn strict_json_rejects_unknown_fields() {
        let json = r#"{
            "schema_version": 2,
            "rules": [],
            "surprise": true
        }"#;
        assert!(matches!(
            AppsConfig::from_json(json),
            Err(ConfigLoadError::Json(_))
        ));
    }

    #[test]
    fn parser_rejects_documents_over_the_canonical_byte_limit() {
        let oversized = " ".repeat(MAX_CONFIG_FILE_BYTES + 1);
        assert!(matches!(
            AppsConfig::from_json(&oversized),
            Err(ConfigLoadError::TooLarge {
                actual_bytes,
                max_bytes: MAX_CONFIG_FILE_BYTES,
            }) if actual_bytes == MAX_CONFIG_FILE_BYTES + 1
        ));
    }

    #[test]
    fn collection_limits_bound_rule_and_selector_work() {
        let matcher = WorkloadMatcher {
            executable: Some("/usr/bin/game".into()),
            desktop_id: None,
            comm_regex: None,
        };
        let apps = AppsConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            rules: (0..=MAX_APP_RULES)
                .map(|index| AppRule {
                    id: format!("app-{index}"),
                    enabled: true,
                    priority: 0,
                    matcher: matcher.clone(),
                    profile: ProfileId::Balance,
                })
                .collect(),
        };
        let app_errors = apps.validate().expect_err("oversized app list");
        assert!(app_errors.issues().iter().any(|issue| {
            issue.path == "rules"
                && issue
                    .message
                    .contains(&format!("more than {MAX_APP_RULES}"))
        }));

        let mut device = valid_device();
        device.devfreq_targets.push(DevfreqTargetConfig {
            id: target("gpu.main"),
            device_name: "gpu".into(),
            compatible: vec!["vendor,gpu".into(); MAX_DEVFREQ_COMPATIBLE_STRINGS + 1],
            sysfs_path: None,
            manual_only: true,
            admin_cap_hz: None,
            critical_cap_hz: None,
            sensor_failure_cap_hz: None,
        });
        let device_errors = device.validate().expect_err("oversized compatible list");
        assert!(device_errors.issues().iter().any(|issue| {
            issue.path == "devfreq_targets[0].compatible"
                && issue
                    .message
                    .contains(&format!("more than {MAX_DEVFREQ_COMPATIBLE_STRINGS}"))
        }));
    }

    #[test]
    fn device_validation_rejects_parent_traversal() {
        let error = valid_device().validate().expect_err("path must fail");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.path == "cpu_policies[1].sysfs_path")
        );
    }

    #[test]
    fn device_validation_rejects_overlapping_cpu_masks() {
        let mut config = valid_device();
        config.cpu_policies[1].sysfs_path = Some("/sys/devices/system/cpu/cpufreq/policy7".into());
        config.cpu_policies[1].related_cpus.insert(CpuId(2));
        let error = config.validate().expect_err("overlap must fail");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.message.contains("overlaps"))
        );
    }

    #[test]
    fn policy_requires_all_three_profiles_and_hysteresis() {
        let mut config = valid_policy();
        config.profiles.pop();
        config.load.heavy_exit = config.load.heavy_enter;
        let error = config.validate().expect_err("invalid policy");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.message.contains("performance"))
        );
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.message.contains("heavy_exit"))
        );
    }

    #[test]
    fn scheduler_references_and_bounds_are_validated() {
        let mut config = valid_policy();
        config.scheduler.enabled = true;
        config.scheduler.process_rules.push(ProcessRuleConfig {
            name: "game".into(),
            matcher: WorkloadMatcher {
                executable: None,
                desktop_id: None,
                comm_regex: Some("[".into()),
            },
            task_profile: Some("missing".into()),
            cgroup_class: None,
            threads: Vec::new(),
        });
        let error = config.validate().expect_err("invalid scheduler");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.message.contains("regex"))
        );
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.message.contains("unknown task profile"))
        );
    }

    #[test]
    fn scheduler_uclamp_fields_are_independent_and_bounded() {
        let mut config = valid_policy();
        config.scheduler.task_profiles = vec![
            TaskProfileConfig {
                id: "minimum-only".into(),
                affinity_group: None,
                plan: TaskPlan {
                    uclamp_min: Some(205),
                    ..TaskPlan::default()
                },
            },
            TaskProfileConfig {
                id: "maximum-only".into(),
                affinity_group: None,
                plan: TaskPlan {
                    uclamp_max: Some(768),
                    ..TaskPlan::default()
                },
            },
        ];
        config.validate().expect("independent uclamp fields");

        config.scheduler.task_profiles[0].plan.uclamp_min = Some(1_025);
        config.scheduler.task_profiles[1].plan.uclamp_max = Some(1_025);
        let error = config.validate().expect_err("uclamp values are bounded");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.path.ends_with(".uclamp_min"))
        );
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.path.ends_with(".uclamp_max"))
        );
    }

    #[test]
    fn scheduler_rejects_reversed_complete_uclamp_patch() {
        let mut config = valid_policy();
        config.scheduler.task_profiles.push(TaskProfileConfig {
            id: "reversed".into(),
            affinity_group: None,
            plan: TaskPlan {
                uclamp_min: Some(513),
                uclamp_max: Some(512),
                ..TaskPlan::default()
            },
        });

        let error = config.validate().expect_err("uclamp pair must be ordered");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.message.contains("uclamp_min must not exceed"))
        );
    }

    #[test]
    fn valid_policy_round_trips_and_validates() {
        let policy = valid_policy();
        policy.validate().expect("valid policy");
        let json = serde_json::to_string(&policy).expect("serialize");
        let decoded = PolicyConfig::from_json(&json).expect("deserialize");
        assert_eq!(decoded, policy);
    }

    #[test]
    fn bundle_validates_cross_file_thermal_and_cpu_references() {
        let mut device = valid_device();
        device.cpu_policies[1].sysfs_path = Some("/sys/devices/system/cpu/cpufreq/policy7".into());
        let mut policy = valid_policy();
        policy.scheduler.task_profiles.push(TaskProfileConfig {
            id: "outside".into(),
            affinity_group: None,
            plan: TaskPlan {
                affinity: Some(CpuSet::from_ids([CpuId(99)])),
                ..TaskPlan::default()
            },
        });
        let bundle = ConfigBundle { device, policy };
        let error = bundle
            .validate_cross_references()
            .expect_err("cross-file errors");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.path == "device.thermal_zones")
        );
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.message.contains("outside the device"))
        );
    }

    #[test]
    fn bundle_materializes_device_cpu_groups_for_scheduler_policy() {
        let mut device = valid_device();
        device.cpu_policies[1].sysfs_path = None;
        device.thermal_zones.push(ThermalZoneConfig {
            id: "soc".into(),
            zone_type: "soc-thermal".into(),
            sysfs_path: None,
            warning: MilliCelsius(70_000),
            throttled: MilliCelsius(80_000),
            critical: MilliCelsius(90_000),
            hysteresis: MilliCelsius(5_000),
            dwell_ms: 100,
            stale_after_ms: 1_000,
        });
        let all = device
            .cpu_policies
            .iter()
            .flat_map(|policy| policy.related_cpus.iter().copied())
            .collect::<CpuSet>();
        device.cpu_groups.insert("all".into(), all.clone());
        let mut policy = valid_policy();
        policy.scheduler.task_profiles.push(TaskProfileConfig {
            id: "grouped".into(),
            affinity_group: Some("all".into()),
            plan: TaskPlan::default(),
        });
        policy.scheduler.cgroup_classes.push(CgroupClassConfig {
            id: "grouped".into(),
            allowed_cpu_group: Some("all".into()),
            allowed_cpus: CpuSet::new(),
            cpu_weight: 100,
        });

        let materialized = ConfigBundle { device, policy }
            .materialize_cpu_groups()
            .expect("known CPU groups");
        assert_eq!(
            materialized.scheduler.task_profiles[0].plan.affinity,
            Some(all.clone())
        );
        assert_eq!(materialized.scheduler.cgroup_classes[0].allowed_cpus, all);
        assert_eq!(materialized.scheduler.task_profiles[0].affinity_group, None);
        assert_eq!(
            materialized.scheduler.cgroup_classes[0].allowed_cpu_group,
            None
        );
    }

    #[test]
    fn schemas_are_available_for_all_three_files() {
        let device = serde_json::to_value(device_config_schema()).expect("serialize device schema");
        let policy = serde_json::to_value(policy_config_schema()).expect("serialize policy schema");
        let apps = serde_json::to_value(apps_config_schema()).expect("serialize apps schema");
        assert!(device.to_string().contains("cpu_policies"));
        assert!(policy.to_string().contains("default_profile"));
        assert!(apps.to_string().contains("rules"));
    }

    #[test]
    fn installed_schema_snapshots_match_domain_types() {
        let snapshots = [
            (
                include_str!("../../../config/schema/device-v2.schema.json"),
                serde_json::to_value(device_config_schema()).expect("device schema"),
            ),
            (
                include_str!("../../../config/schema/policy-v2.schema.json"),
                serde_json::to_value(policy_config_schema()).expect("policy schema"),
            ),
            (
                include_str!("../../../config/schema/apps-v2.schema.json"),
                serde_json::to_value(apps_config_schema()).expect("apps schema"),
            ),
        ];
        for (snapshot, generated) in snapshots {
            let snapshot: serde_json::Value =
                serde_json::from_str(snapshot).expect("valid schema snapshot");
            assert_eq!(snapshot, generated);
        }
    }

    #[test]
    fn app_rules_are_priority_ordered_stably_and_disabled_rules_are_skipped() {
        let matcher = WorkloadMatcher {
            executable: Some("/usr/bin/game".into()),
            desktop_id: None,
            comm_regex: None,
        };
        let apps = AppsConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            rules: vec![
                AppRule {
                    id: "equal-first".into(),
                    enabled: true,
                    priority: 10,
                    matcher: matcher.clone(),
                    profile: ProfileId::Balance,
                },
                AppRule {
                    id: "disabled-high".into(),
                    enabled: false,
                    priority: 100,
                    matcher: matcher.clone(),
                    profile: ProfileId::Performance,
                },
                AppRule {
                    id: "highest".into(),
                    enabled: true,
                    priority: 20,
                    matcher: matcher.clone(),
                    profile: ProfileId::Performance,
                },
                AppRule {
                    id: "equal-second".into(),
                    enabled: true,
                    priority: 10,
                    matcher,
                    profile: ProfileId::Powersave,
                },
            ],
        };
        assert_eq!(
            apps.ordered_enabled_rules()
                .into_iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            ["highest", "equal-first", "equal-second"]
        );
    }

    #[test]
    fn app_rules_reject_reserved_desktop_ids_and_accept_composite_matchers() {
        let mut apps = AppsConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            rules: vec![AppRule {
                id: "desktop".into(),
                enabled: true,
                priority: 0,
                matcher: WorkloadMatcher {
                    executable: None,
                    desktop_id: Some("example.desktop".into()),
                    comm_regex: None,
                },
                profile: ProfileId::Performance,
            }],
        };

        let error = apps.validate().expect_err("desktop IDs are not observable");
        assert!(
            error
                .issues()
                .iter()
                .any(|issue| issue.path.ends_with(".desktop_id")
                    && issue.message.contains("future trusted desktop adapter"))
        );

        apps.rules[0].matcher = WorkloadMatcher {
            executable: Some("/usr/bin/example".into()),
            desktop_id: None,
            comm_regex: Some("^example$".into()),
        };
        apps.validate().expect("wire rule preserves AND semantics");
    }
}
