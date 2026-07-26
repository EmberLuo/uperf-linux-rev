//! Conservative migration of the supported subset of the C implementation's v1
//! monolithic JSON configuration.
//!
//! Migration produces a draft, not an assertion that a device is certified.  In
//! particular, v1 trusted every thermal zone and permitted arbitrary sysfs knobs;
//! neither behaviour is carried into v2.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    AppsConfig, CONFIG_SCHEMA_VERSION, CpuId, CpuPolicyConfig, CpuSet, DevfreqTargetConfig,
    DeviceConfig, Hertz, InputConfig, LoadConfig, PolicyConfig, ProfileConfig, ProfileId, Scene,
    ScenePatch, SchedulerConfig, TargetId, ThermalPolicyConfig, Validate, ValidationErrors,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationWarning {
    pub path: String,
    pub message: String,
}

impl MigrationWarning {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationResult {
    pub device: DeviceConfig,
    pub policy: PolicyConfig,
    pub apps: AppsConfig,
    pub warnings: Vec<MigrationWarning>,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("v1 configuration root must be a JSON object")]
    RootNotObject,
    #[error("expected C configuration schema version 1, got {0:?}")]
    UnsupportedSchema(Option<u64>),
    #[error("missing or invalid field `{path}`: {message}")]
    InvalidField { path: String, message: String },
    #[error("migrated device configuration is invalid: {0}")]
    InvalidDevice(ValidationErrors),
    #[error("migrated policy configuration is invalid: {0}")]
    InvalidPolicy(ValidationErrors),
}

/// Migrate the safe, understood portion of a C v1 configuration to v2 drafts.
///
/// Arbitrary knobs, legacy scheduler priority encodings, broad thermal discovery,
/// and scenes without a production event source are intentionally not copied.
///
/// # Errors
///
/// Returns [`MigrationError`] if the input is not schema v1, a required safe
/// subset cannot be interpreted, or the generated v2 draft fails validation.
pub fn migrate_c_v1(root: &Value) -> Result<MigrationResult, MigrationError> {
    let object = root.as_object().ok_or(MigrationError::RootNotObject)?;
    let schema = at(object, &["meta", "schemaVersion"]).and_then(Value::as_u64);
    if schema != Some(1) {
        return Err(MigrationError::UnsupportedSchema(schema));
    }

    let mut warnings = Vec::new();
    let device_id = at(object, &["meta", "name"])
        .and_then(Value::as_str)
        .unwrap_or("migrated-c-v1")
        .to_owned();
    let cpu_policies = migrate_cpu_policies(object, &mut warnings)?;
    let devfreq_targets = migrate_gpu(object, &mut warnings);

    warnings.push(MigrationWarning::new(
        "modules.thermal",
        "v1 trusted every discovered thermal zone; this output is a non-activatable draft: add at least one explicit trusted thermal zone, verify device selectors and frequency/safety caps, then run `uperfctl config validate <output-directory>`",
    ));
    warnings.push(MigrationWarning::new(
        "modules.sched",
        "legacy scheduler/cgroup classes use ambiguous priority and scene encodings and were not migrated",
    ));
    warnings.push(MigrationWarning::new(
        "modules.sysfs.knob",
        "arbitrary sysfs knobs were dropped; only a recognized GPU min/max pair can be migrated",
    ));
    warnings.push(MigrationWarning::new(
        "modules.switcher.perapp",
        "the external per-app file is not embedded in JSON; apps.json was initialized empty and `fast` must be reviewed as `performance`",
    ));

    let device = DeviceConfig {
        schema_version: CONFIG_SCHEMA_VERSION,
        device_id,
        device_match: None,
        cpu_groups: BTreeMap::new(),
        cpu_policies,
        devfreq_targets,
        scalar_targets: Vec::new(),
        thermal_zones: Vec::new(),
    };
    let policy = migrate_policy(object, &mut warnings)?;
    let apps = AppsConfig {
        schema_version: CONFIG_SCHEMA_VERSION,
        rules: Vec::new(),
    };

    device.validate().map_err(MigrationError::InvalidDevice)?;
    policy.validate().map_err(MigrationError::InvalidPolicy)?;
    Ok(MigrationResult {
        device,
        policy,
        apps,
        warnings,
    })
}

fn migrate_cpu_policies(
    root: &Map<String, Value>,
    warnings: &mut Vec<MigrationWarning>,
) -> Result<Vec<CpuPolicyConfig>, MigrationError> {
    let models = required(root, &["modules", "cpu", "powerModel"])?
        .as_array()
        .ok_or_else(|| invalid("modules.cpu.powerModel", "expected an array"))?;
    let masks = required(root, &["modules", "sched", "cpumask"])?
        .as_object()
        .ok_or_else(|| invalid("modules.sched.cpumask", "expected an object"))?;
    let mut policies = Vec::with_capacity(models.len());

    for (index, value) in models.iter().enumerate() {
        let path = format!("modules.cpu.powerModel[{index}]");
        let model = value
            .as_object()
            .ok_or_else(|| invalid(&path, "expected an object"))?;
        let mask_name = model
            .get("cpumask")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("{path}.cpumask"), "expected a string"))?;
        let cpu_values = masks
            .get(mask_name)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                invalid(
                    format!("modules.sched.cpumask.{mask_name}"),
                    "missing CPU array referenced by powerModel",
                )
            })?;
        let mut related_cpus = CpuSet::new();
        for (cpu_index, cpu) in cpu_values.iter().enumerate() {
            let value = cpu.as_u64().ok_or_else(|| {
                invalid(
                    format!("modules.sched.cpumask.{mask_name}[{cpu_index}]"),
                    "expected a non-negative CPU id",
                )
            })?;
            let value = u32::try_from(value).map_err(|_| {
                invalid(
                    format!("modules.sched.cpumask.{mask_name}[{cpu_index}]"),
                    "CPU id exceeds u32",
                )
            })?;
            related_cpus.insert(CpuId(value));
        }
        let first_cpu = *related_cpus
            .iter()
            .next()
            .ok_or_else(|| invalid(format!("modules.sched.cpumask.{mask_name}"), "empty mask"))?;
        let floor = old_mhz(model, "freeFreq", &path)?;
        let reference = old_mhz(model, "typicalFreq", &path)?;
        let efficient_cap = old_mhz(model, "sweetFreq", &path)?;
        let id = TargetId::new(format!("cpu.{mask_name}"))
            .map_err(|error| invalid(format!("{path}.cpumask"), error.to_string()))?;

        warnings.push(MigrationWarning::new(
            format!("{path}.cpumask"),
            format!(
                "mapped `{mask_name}` to a dynamic related_cpus selector beginning at CPU {}; verify the discovered policy before installation",
                first_cpu.get(),
            ),
        ));
        warnings.push(MigrationWarning::new(
            format!("{path}.efficiency"),
            "the legacy efficiency coefficient algebraically cancelled from its frequency model and was dropped",
        ));

        policies.push(CpuPolicyConfig {
            id,
            related_cpus,
            sysfs_path: None,
            floor_hz: floor,
            reference_hz: reference,
            efficient_cap_hz: efficient_cap,
            admin_cap_hz: None,
            critical_cap_hz: Some(floor),
            sensor_failure_cap_hz: Some(floor),
            energy_model: None,
        });
    }
    Ok(policies)
}

fn migrate_gpu(
    root: &Map<String, Value>,
    warnings: &mut Vec<MigrationWarning>,
) -> Vec<DevfreqTargetConfig> {
    let min = at(root, &["modules", "sysfs", "knob", "gpuMinFreq"]).and_then(Value::as_str);
    let max = at(root, &["modules", "sysfs", "knob", "gpuMaxFreq"]).and_then(Value::as_str);
    let (Some(min), Some(max)) = (min, max) else {
        return Vec::new();
    };
    let min_path = Path::new(min);
    let max_path = Path::new(max);
    let recognized = min_path.file_name().and_then(|name| name.to_str()) == Some("min_freq")
        && max_path.file_name().and_then(|name| name.to_str()) == Some("max_freq")
        && min_path.parent() == max_path.parent()
        && min_path
            .parent()
            .is_some_and(|parent| parent.starts_with("/sys/class/devfreq"));
    if !recognized {
        warnings.push(MigrationWarning::new(
            "modules.sysfs.knob",
            "GPU paths were not a matching /sys/class/devfreq/.../{min_freq,max_freq} pair and were dropped",
        ));
        return Vec::new();
    }

    vec![DevfreqTargetConfig {
        id: TargetId::new("gpu.0").expect("static id is valid"),
        device_name: min_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("gpu")
            .to_owned(),
        compatible: Vec::new(),
        sysfs_path: Some(
            min_path
                .parent()
                .expect("recognized path has parent")
                .to_string_lossy()
                .into_owned(),
        ),
        manual_only: true,
        admin_cap_hz: None,
        critical_cap_hz: None,
        sensor_failure_cap_hz: None,
    }]
}

#[allow(clippy::too_many_lines)]
fn migrate_policy(
    root: &Map<String, Value>,
    warnings: &mut Vec<MigrationWarning>,
) -> Result<PolicyConfig, MigrationError> {
    let initial_cpu = at(root, &["initials", "cpu"]).and_then(Value::as_object);
    let base_margin = optional_number(initial_cpu, "margin")?.unwrap_or(0.25);
    let base_burst = optional_number(initial_cpu, "burst")?.unwrap_or(0.0);
    let base_limit = initial_cpu
        .and_then(|value| value.get("limitEfficiency"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if initial_cpu.is_some_and(|value| value.contains_key("baseSampleTime")) {
        warnings.push(MigrationWarning::new(
            "initials.cpu.baseSampleTime",
            "dynamic scene-controlled polling was dropped; v2 observers have independent fixed cadences",
        ));
    }

    let presets = at(root, &["presets"]).and_then(Value::as_object);
    if presets.is_some_and(|value| value.contains_key("fast")) {
        warnings.push(MigrationWarning::new(
            "presets.fast",
            "`fast` was an inconsistent legacy alias and was mapped to `performance` when no performance preset exists",
        ));
    }
    let durations = at(root, &["modules", "switcher", "hintDuration"]).and_then(Value::as_object);

    let mut profiles = Vec::new();
    for (id, name) in [
        (ProfileId::Powersave, "powersave"),
        (ProfileId::Balance, "balance"),
        (ProfileId::Performance, "performance"),
    ] {
        let preset = presets
            .and_then(|all| all.get(name))
            .or_else(|| {
                (id == ProfileId::Performance)
                    .then(|| presets.and_then(|all| all.get("fast")))
                    .flatten()
            })
            .and_then(Value::as_object);
        let wildcard = preset
            .and_then(|value| value.get("*"))
            .and_then(Value::as_object);
        let margin = old_action_number(wildcard, "cpu.margin")?.unwrap_or(base_margin);
        let burst = old_action_number(wildcard, "cpu.burst")?.unwrap_or(base_burst);
        let limit_efficiency =
            old_action_bool(wildcard, "cpu.limitEfficiency")?.unwrap_or(base_limit);
        if wildcard.is_some_and(|value| value.contains_key("cpu.baseSampleTime")) {
            warnings.push(MigrationWarning::new(
                format!("presets.{name}.*.cpu.baseSampleTime"),
                "dynamic polling cadence was dropped",
            ));
        }

        let mut scenes = BTreeMap::new();
        for (scene, scene_name) in [
            (Scene::Idle, "idle"),
            (Scene::Touch, "touch"),
            (Scene::Trigger, "trigger"),
            (Scene::Gesture, "gesture"),
            (Scene::Boost, "boost"),
            (Scene::Switch, "switch"),
            (Scene::Wake, "wake"),
        ] {
            let old = preset
                .and_then(|value| value.get(scene_name))
                .and_then(Value::as_object);
            let patch_margin = old_action_number(old, "cpu.margin")?;
            let patch_burst = old_action_number(old, "cpu.burst")?;
            let patch_limit = old_action_bool(old, "cpu.limitEfficiency")?;
            if old.is_some_and(|value| value.contains_key("cpu.baseSampleTime")) {
                warnings.push(MigrationWarning::new(
                    format!("presets.{name}.{scene_name}.cpu.baseSampleTime"),
                    "dynamic polling cadence was dropped",
                ));
            }
            if patch_margin.is_some() || patch_burst.is_some() || patch_limit.is_some() {
                scenes.insert(
                    scene,
                    ScenePatch {
                        margin: patch_margin,
                        burst: patch_burst,
                        limit_efficiency: patch_limit,
                        power_budget: None,
                        scalar_values: BTreeMap::new(),
                    },
                );
            }
        }
        if preset.is_some_and(|value| value.contains_key("junk")) {
            warnings.push(MigrationWarning::new(
                format!("presets.{name}.junk"),
                "`junk` had no production event source and was dropped",
            ));
        }
        profiles.push(ProfileConfig {
            id,
            margin,
            burst,
            limit_efficiency,
            power_budget: None,
            scalar_values: BTreeMap::new(),
            scenes,
        });
    }

    let heavy = at(root, &["modules", "heavyload"]).and_then(Value::as_object);
    let sample_interval_ms = old_number(heavy, "sampleTimeMs")?
        .map(float_millis)
        .transpose()?
        .unwrap_or(20);
    let heavy_enter = old_number(heavy, "heavyLoadPct")?.map_or(0.60, |value| value / 100.0);
    let heavy_exit = old_number(heavy, "idleLoadPct")?.map_or(0.20, |value| value / 100.0);
    let heavy_dwell_ms = old_number(heavy, "burstSlackMs")?
        .map(float_millis)
        .transpose()?
        .unwrap_or(1_000);
    let load = LoadConfig {
        enabled: heavy
            .and_then(|value| value.get("enable"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        sample_interval_ms,
        ema_time_constant_ms: 500,
        heavy_enter,
        heavy_exit,
        heavy_dwell_ms,
    };

    let old_input = at(root, &["modules", "input"]).and_then(Value::as_object);
    if old_input.is_some_and(|value| {
        value.contains_key("screen_width") || value.contains_key("screen_height")
    }) {
        warnings.push(MigrationWarning::new(
            "modules.input",
            "fixed screen dimensions were dropped; v2 normalizes each evdev device's axis range",
        ));
    }
    let input = InputConfig {
        enabled: old_input
            .and_then(|value| value.get("enable"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        trigger_duration_ms: migrated_duration(durations, "trigger", 30),
        gesture_duration_ms: migrated_duration(durations, "gesture", 100),
        switch_duration_ms: migrated_duration(durations, "switch", 400),
        wake_duration_ms: migrated_duration(durations, "wake", 500),
        swipe_distance: old_number(old_input, "swipeThd")?.unwrap_or(0.03),
        edge_width: old_number(old_input, "gestureThdX")?.unwrap_or(0.03),
    };

    Ok(PolicyConfig {
        schema_version: CONFIG_SCHEMA_VERSION,
        default_profile: ProfileId::Balance,
        profiles,
        load,
        governor: crate::GovernorConfig::default(),
        thermal: ThermalPolicyConfig {
            sample_interval_ms: 250,
        },
        input,
        scheduler: SchedulerConfig::default(),
        session: None,
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn old_mhz(object: &Map<String, Value>, field: &str, base: &str) -> Result<Hertz, MigrationError> {
    let value = object
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| invalid(format!("{base}.{field}"), "expected a number in MHz"))?;
    if !value.is_finite() || value <= 0.0 || value > u64::MAX as f64 / 1_000_000.0 {
        return Err(invalid(
            format!("{base}.{field}"),
            "MHz value is out of range",
        ));
    }
    Ok(Hertz((value * 1_000_000.0).round() as u64))
}

fn optional_number(
    object: Option<&Map<String, Value>>,
    field: &str,
) -> Result<Option<f64>, MigrationError> {
    old_number(object, field)
}

fn old_action_number(
    object: Option<&Map<String, Value>>,
    field: &str,
) -> Result<Option<f64>, MigrationError> {
    old_number(object, field)
}

fn old_number(
    object: Option<&Map<String, Value>>,
    field: &str,
) -> Result<Option<f64>, MigrationError> {
    let Some(value) = object.and_then(|object| object.get(field)) else {
        return Ok(None);
    };
    let number = value
        .as_f64()
        .ok_or_else(|| invalid(field, "expected a number"))?;
    if number.is_finite() {
        Ok(Some(number))
    } else {
        Err(invalid(field, "number must be finite"))
    }
}

fn old_action_bool(
    object: Option<&Map<String, Value>>,
    field: &str,
) -> Result<Option<bool>, MigrationError> {
    let Some(value) = object.and_then(|object| object.get(field)) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| invalid(field, "expected a boolean"))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn float_millis(value: f64) -> Result<u64, MigrationError> {
    if value.is_finite() && value > 0.0 && value <= 86_400_000.0 {
        Ok(value.round() as u64)
    } else {
        Err(invalid("duration", "milliseconds are out of range"))
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn seconds_to_nonzero_millis(value: f64) -> Option<u64> {
    (value.is_finite() && value > 0.0 && value <= 86_400.0)
        .then(|| (value * 1_000.0).round() as u64)
        .filter(|value| *value > 0)
}

fn migrated_duration(durations: Option<&Map<String, Value>>, name: &str, default: u64) -> u64 {
    durations
        .and_then(|value| value.get(name))
        .and_then(Value::as_f64)
        .and_then(seconds_to_nonzero_millis)
        .unwrap_or(default)
}

fn required<'a>(root: &'a Map<String, Value>, path: &[&str]) -> Result<&'a Value, MigrationError> {
    at(root, path).ok_or_else(|| invalid(path.join("."), "field is required"))
}

fn at<'a>(root: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let mut current = root.get(*path.first()?)?;
    for segment in &path[1..] {
        current = current.as_object()?.get(*segment)?;
    }
    Some(current)
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> MigrationError {
    MigrationError::InvalidField {
        path: path.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c_config() -> Value {
        serde_json::json!({
            "meta": { "name": "test SoC migrated", "schemaVersion": 1 },
            "modules": {
                "switcher": {
                    "perapp": "/etc/uperf-linux/perapp_powermode",
                    "hintDuration": {
                        "touch": 4.0,
                        "trigger": 0.03,
                        "gesture": 0.1,
                        "switch": 0.4,
                        "boost": 0.0
                    }
                },
                "input": {
                    "enable": true,
                    "screen_width": 3048,
                    "screen_height": 2032,
                    "swipeThd": 0.03,
                    "gestureThdX": 0.03
                },
                "cpu": {
                    "powerModel": [
                        {
                            "efficiency": 350,
                            "nr": 1,
                            "cpumask": "prime",
                            "typicalFreq": 2957,
                            "sweetFreq": 2218,
                            "freeFreq": 739
                        }
                    ]
                },
                "sysfs": {
                    "knob": {
                        "gpuMaxFreq": "/sys/class/devfreq/3d00000.gpu/max_freq",
                        "gpuMinFreq": "/sys/class/devfreq/3d00000.gpu/min_freq",
                        "dangerous": "/sys/kernel/arbitrary"
                    }
                },
                "thermal": { "enabled": true },
                "heavyload": {
                    "enable": true,
                    "sampleTimeMs": 10.0,
                    "heavyLoadPct": 60.0,
                    "idleLoadPct": 20.0,
                    "burstSlackMs": 3000.0
                },
                "sched": {
                    "cpumask": {
                        "prime": [7]
                    }
                }
            },
            "initials": {
                "cpu": {
                    "baseSampleTime": 0.01,
                    "margin": 0.25,
                    "burst": 0.0,
                    "limitEfficiency": false
                }
            },
            "presets": {
                "balance": {
                    "*": { "cpu.margin": 0.2 },
                    "junk": { "cpu.burst": 0.6 }
                },
                "powersave": {
                    "*": { "cpu.margin": 0.1 },
                    "idle": { "cpu.limitEfficiency": true }
                },
                "performance": {
                    "*": { "cpu.margin": 0.4, "cpu.burst": 0.2 }
                }
            }
        })
    }

    #[test]
    fn migration_converts_frequency_units_and_known_gpu_pair() {
        let result = migrate_c_v1(&c_config()).expect("migration");
        let cpu = &result.device.cpu_policies[0];
        assert_eq!(cpu.id.as_str(), "cpu.prime");
        assert_eq!(cpu.related_cpus, CpuSet::from_ids([CpuId(7)]));
        assert_eq!(cpu.reference_hz, Hertz(2_957_000_000));
        assert_eq!(cpu.efficient_cap_hz, Hertz(2_218_000_000));
        assert_eq!(result.device.devfreq_targets.len(), 1);
        assert!(result.device.thermal_zones.is_empty());
    }

    #[test]
    fn migration_drops_unimplemented_scenes_and_arbitrary_knobs_with_warnings() {
        let result = migrate_c_v1(&c_config()).expect("migration");
        let balance = result.policy.profile(ProfileId::Balance).expect("balance");
        assert!(!balance.scenes.contains_key(&Scene::Boost));
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.path.contains("junk"))
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.message.contains("arbitrary sysfs"))
        );
    }

    #[test]
    fn migration_rejects_unknown_schema_and_missing_cpu_masks() {
        let mut wrong = c_config();
        wrong["meta"]["schemaVersion"] = Value::from(2);
        assert!(matches!(
            migrate_c_v1(&wrong),
            Err(MigrationError::UnsupportedSchema(Some(2)))
        ));

        let mut missing = c_config();
        missing["modules"]["sched"]["cpumask"] = serde_json::json!({});
        assert!(matches!(
            migrate_c_v1(&missing),
            Err(MigrationError::InvalidField { .. })
        ));
    }

    #[test]
    fn migrated_draft_passes_strict_semantic_validation() {
        let result = migrate_c_v1(&c_config()).expect("migration");
        result.device.validate().expect("device");
        result.policy.validate().expect("policy");
        result.apps.validate().expect("apps");
    }
}
