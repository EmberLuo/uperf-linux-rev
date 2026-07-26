use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};
use uperf_core::{CONFIG_SCHEMA_VERSION, MAX_CONFIG_FILE_BYTES};

use crate::config::{ensure_output_directory, write_json_atomic};

const MAX_CPU_ID: u32 = 4_095;
const SUPPORTED_PROFILES: [&str; 3] = ["powersave", "balance", "performance"];
const SUPPORTED_SCENES: [&str; 6] = ["idle", "touch", "trigger", "gesture", "junk", "switch"];
const MAX_LOCAL_POLICIES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    Imported,
    Inferred,
    Unsupported,
    RequiresCalibration,
}

#[derive(Debug, Serialize)]
pub struct ImportItem {
    pub path: String,
    pub status: ImportStatus,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ImportSource {
    pub path: PathBuf,
    pub sha256: String,
    pub name: String,
    pub author: String,
}

#[derive(Debug, Serialize)]
pub struct DisabledCandidates {
    pub android_sysfs: Option<Value>,
    pub android_scheduler: Option<Value>,
    pub android_switcher: Option<Value>,
    pub unsupported_presets: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub format: &'static str,
    pub review_only: bool,
    pub source: ImportSource,
    pub items: Vec<ImportItem>,
    pub disabled_candidates: DisabledCandidates,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct UperfV3Import {
    pub device: Value,
    pub policy: Value,
    pub report: ImportReport,
}

#[derive(Debug)]
pub struct ImportOutputs {
    pub device: PathBuf,
    pub policy: PathBuf,
    pub report: PathBuf,
}

#[derive(Debug)]
struct SourcePowerModel {
    cores: usize,
    relative_performance: u32,
    typical_power_mw_per_core: u32,
    typical_frequency_hz: u64,
    sweet_frequency_hz: u64,
    plain_frequency_hz: u64,
    free_frequency_hz: u64,
}

struct ClusterMapping {
    cpus: BTreeSet<u32>,
    status: ImportStatus,
    message: String,
}

struct LocalCpuPolicy {
    name: String,
    cpus: BTreeSet<u32>,
    maximum_frequency: u64,
}

/// Parse and convert one Uperf v3 JSON document.
///
/// This is intentionally an offline, review-only conversion. Android sysfs
/// paths and scheduler rules are retained only as disabled report candidates.
#[allow(clippy::too_many_lines)]
pub fn import_path(
    path: &Path,
    explicit_cluster_cpus: &[String],
    sysfs_root: &Path,
) -> Result<UperfV3Import> {
    let text = read_source(path)?;
    let root = parse_strict(&text).with_context(|| format!("parse {}", path.display()))?;
    let root_object = root
        .as_object()
        .context("Uperf v3 document root must be an object")?;
    let modules = required_object(root_object, "modules", "modules")?;
    let cpu = required_object(modules, "cpu", "modules.cpu")?;
    let models_value = cpu
        .get("powerModel")
        .context("missing modules.cpu.powerModel")?;
    let models_array = models_value
        .as_array()
        .context("modules.cpu.powerModel must be an array")?;
    if models_array.is_empty() {
        bail!("modules.cpu.powerModel must contain at least one calibrated cluster");
    }
    let models = models_array
        .iter()
        .enumerate()
        .map(|(index, value)| parse_power_model(value, index))
        .collect::<Result<Vec<_>>>()?;
    let mut items = Vec::new();
    let cluster_mappings = resolve_cluster_cpus(
        modules,
        &models,
        explicit_cluster_cpus,
        sysfs_root,
        &mut items,
    )?;

    let meta = root_object.get("meta").and_then(Value::as_object);
    let source_name = meta
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("unknown")
        })
        .to_owned();
    let source_author = meta
        .and_then(|value| value.get("author"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();

    let device = build_device(path, &source_name, &models, &cluster_mappings, &mut items);
    let policy = build_policy(root_object, &mut items)?;

    let presets = root_object
        .get("presets")
        .and_then(Value::as_object)
        .context("missing or invalid presets object")?;
    let unsupported_presets = presets
        .iter()
        .filter(|(name, _)| !SUPPORTED_PROFILES.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    for name in unsupported_presets.keys() {
        items.push(ImportItem {
            path: format!("presets.{name}"),
            status: ImportStatus::Unsupported,
            message: "profile is outside the Linux powersave/balance/performance API".into(),
        });
    }

    let sysfs = modules.get("sysfs").cloned();
    if sysfs.is_some() {
        items.push(ImportItem {
            path: "modules.sysfs".into(),
            status: ImportStatus::Unsupported,
            message: "Android paths are retained in the report and never activated".into(),
        });
    }
    let scheduler = modules.get("sched").cloned();
    if scheduler.is_some() {
        items.push(ImportItem {
            path: "modules.sched".into(),
            status: ImportStatus::Unsupported,
            message: "pinned processes, FIFO priorities, and Android rules remain disabled".into(),
        });
    }
    let switcher = modules.get("switcher").cloned();
    if switcher.is_some() {
        items.push(ImportItem {
            path: "modules.switcher".into(),
            status: ImportStatus::Unsupported,
            message: "Android package and /sdcard rule sources are not imported".into(),
        });
    }

    let checksum = Sha256::digest(text.as_bytes());
    let report = ImportReport {
        format: "uperf-v3-import-report-v1",
        review_only: true,
        source: ImportSource {
            path: path.to_path_buf(),
            sha256: format!("{checksum:x}"),
            name: source_name,
            author: source_author,
        },
        items,
        disabled_candidates: DisabledCandidates {
            android_sysfs: sysfs,
            android_scheduler: scheduler,
            android_switcher: switcher,
            unsupported_presets,
        },
        warnings: vec![
            "REVIEW ONLY: the generated files are not eligible for automatic activation".into(),
            "CPU masks and power data require verification against Linux related_cpus and OPPs"
                .into(),
            "no device matcher or trusted thermal zone is inferred from Android data".into(),
        ],
    };

    Ok(UperfV3Import {
        device,
        policy,
        report,
    })
}

pub fn write_import(
    directory: &Path,
    import: &UperfV3Import,
    force: bool,
) -> Result<ImportOutputs> {
    ensure_output_directory(directory)?;
    let outputs = ImportOutputs {
        device: directory.join("device.json"),
        policy: directory.join("policy.json"),
        report: directory.join("import-report.json"),
    };
    for path in [&outputs.device, &outputs.policy, &outputs.report] {
        if path.exists() && !force {
            bail!(
                "{} already exists; pass --force to replace all import outputs",
                path.display()
            );
        }
    }
    write_json_atomic(&outputs.device, &import.device, force)?;
    write_json_atomic(&outputs.policy, &import.policy, force)?;
    write_json_atomic(&outputs.report, &import.report, force)?;
    Ok(outputs)
}

fn build_device(
    path: &Path,
    source_name: &str,
    models: &[SourcePowerModel],
    cluster_mappings: &[ClusterMapping],
    items: &mut Vec<ImportItem>,
) -> Value {
    let policies = models
        .iter()
        .zip(cluster_mappings)
        .enumerate()
        .map(|(index, (model, mapping))| {
            let efficient_cap_hz = model
                .sweet_frequency_hz
                .max(model.free_frequency_hz);
            items.push(ImportItem {
                path: format!("modules.cpu.powerModel[{index}]"),
                status: ImportStatus::Imported,
                message: "converted GHz to Hz and W/core to mW/core".into(),
            });
            items.push(ImportItem {
                path: format!("cpu_policies[{index}].related_cpus"),
                status: mapping.status,
                message: mapping.message.clone(),
            });
            items.push(ImportItem {
                path: format!("cpu_policies[{index}].frequency_limits"),
                status: ImportStatus::RequiresCalibration,
                message: if model.free_frequency_hz > model.sweet_frequency_hz {
                    "freeFreq exceeds sweetFreq, so the draft efficient cap was raised to the freeFreq floor; all bounds still require measured Linux OPP calibration"
                        .into()
                } else {
                    "draft bounds use free/sweet/typical points, not measured Linux OPPs".into()
                },
            });
            json!({
                "id": format!("cpu.cluster{index}"),
                "related_cpus": &mapping.cpus,
                "floor_hz": model.free_frequency_hz,
                "reference_hz": model.typical_frequency_hz,
                "efficient_cap_hz": efficient_cap_hz,
                "admin_cap_hz": model.typical_frequency_hz,
                "critical_cap_hz": model.free_frequency_hz,
                "sensor_failure_cap_hz": model.free_frequency_hz,
                "energy_model": {
                    "kind": "reference-curve-v1",
                    "relative_performance": model.relative_performance,
                    "typical_power_mw_per_core": model.typical_power_mw_per_core,
                    "typical_frequency_hz": model.typical_frequency_hz,
                    "sweet_frequency_hz": model.sweet_frequency_hz,
                    "plain_frequency_hz": model.plain_frequency_hz,
                    "free_frequency_hz": model.free_frequency_hz,
                }
            })
        })
        .collect::<Vec<_>>();
    let all_cpus = cluster_mappings
        .iter()
        .flat_map(|mapping| mapping.cpus.iter())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut groups = Map::new();
    groups.insert("all".into(), json!(all_cpus));
    for (index, mapping) in cluster_mappings.iter().enumerate() {
        groups.insert(format!("cluster{index}"), json!(&mapping.cpus));
    }
    items.push(ImportItem {
        path: "thermal_zones".into(),
        status: ImportStatus::RequiresCalibration,
        message: "left empty because Android thermal paths are not trustworthy on Linux".into(),
    });

    json!({
        "schema_version": CONFIG_SCHEMA_VERSION,
        "device_id": format!("uperf-v3-{}", safe_id(source_name, path)),
        "cpu_groups": groups,
        "cpu_policies": policies,
        "devfreq_targets": [],
        "thermal_zones": [],
    })
}

#[allow(clippy::too_many_lines)]
fn build_policy(root: &Map<String, Value>, items: &mut Vec<ImportItem>) -> Result<Value> {
    let initials = required_object(root, "initials", "initials")?;
    let initial_cpu = required_object(initials, "cpu", "initials.cpu")?;
    let presets = root
        .get("presets")
        .and_then(Value::as_object)
        .context("missing or invalid presets object")?;

    let active_sample_ms = scaled_field(initial_cpu, "baseSampleTime", 1_000, "initials.cpu")?;
    let idle_sample_ms = presets
        .get("balance")
        .and_then(Value::as_object)
        .and_then(|profile| profile.get("idle"))
        .and_then(Value::as_object)
        .and_then(|scene| scene.get("cpu.baseSlackTime"))
        .map(|value| scaled_decimal(value, 1_000, "presets.balance.idle.cpu.baseSlackTime"))
        .transpose()?
        .unwrap_or(80);
    let ramp_latency_ms = scaled_field(initial_cpu, "latencyTime", 1_000, "initials.cpu")?;
    let predict_threshold = initial_cpu
        .get("predictThd")
        .map(|value| value_ratio(value, "initials.cpu.predictThd"))
        .transpose()?
        .unwrap_or(0.15);

    items.extend([
        ImportItem {
            path: "governor.active_sample_ms".into(),
            status: ImportStatus::Imported,
            message: "converted initials.cpu.baseSampleTime from seconds to milliseconds".into(),
        },
        ImportItem {
            path: "governor.idle_sample_ms".into(),
            status: ImportStatus::Imported,
            message: "converted balance idle slack from seconds to milliseconds".into(),
        },
        ImportItem {
            path: "governor.ema_time_constant_ms".into(),
            status: ImportStatus::Inferred,
            message: "Uperf v3 does not define a load EMA; conservative Linux default used".into(),
        },
    ]);

    let profiles = SUPPORTED_PROFILES
        .into_iter()
        .map(|id| build_profile(id, initial_cpu, presets, items))
        .collect::<Result<Vec<_>>>()?;
    let modules = required_object(root, "modules", "modules")?;
    let input = modules.get("input").and_then(Value::as_object);
    let switcher = modules.get("switcher").and_then(Value::as_object);
    let hint_duration = switcher
        .and_then(|value| value.get("hintDuration"))
        .and_then(Value::as_object);
    let duration = |name: &str, fallback: u64| -> Result<u64> {
        hint_duration
            .and_then(|value| value.get(name))
            .map(|value| {
                scaled_decimal(
                    value,
                    1_000,
                    &format!("modules.switcher.hintDuration.{name}"),
                )
            })
            .transpose()
            .map(|value| value.unwrap_or(fallback))
    };
    let input_enabled = input
        .and_then(|value| value.get("enable"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let swipe_distance = input
        .and_then(|value| value.get("swipeThd"))
        .and_then(Value::as_f64)
        .unwrap_or(0.03);
    let edge_width = input
        .and_then(|value| value.get("gestureThdX"))
        .and_then(Value::as_f64)
        .unwrap_or(0.03);
    items.push(ImportItem {
        path: "input".into(),
        status: ImportStatus::Imported,
        message: "converted reusable hint durations and normalized gesture thresholds".into(),
    });

    Ok(json!({
        "schema_version": CONFIG_SCHEMA_VERSION,
        "default_profile": "balance",
        "profiles": profiles,
        "load": {
            "enabled": true,
            "sample_interval_ms": 250,
            "ema_time_constant_ms": 500,
            "heavy_enter": 0.60,
            "heavy_exit": 0.20,
            "heavy_dwell_ms": 1_000,
        },
        "governor": {
            "rollout": "shadow",
            "active_sample_ms": active_sample_ms,
            "idle_sample_ms": idle_sample_ms,
            "active_load_threshold": 0.30,
            "idle_load_threshold": 0.15,
            "ema_time_constant_ms": 40,
            "predict_threshold": predict_threshold,
            "prediction_gain": 1.0,
            "ramp_latency_ms": ramp_latency_ms,
            "min_opp_residency_ms": 10,
        },
        "thermal": {
            "sample_interval_ms": 250,
        },
        "input": {
            "enabled": input_enabled,
            "trigger_duration_ms": duration("trigger", 30)?,
            "gesture_duration_ms": duration("gesture", 100)?,
            "switch_duration_ms": duration("switch", 400)?,
            "wake_duration_ms": 500,
            "swipe_distance": swipe_distance,
            "edge_width": edge_width,
        },
        "scheduler": {
            "enabled": false,
        },
    }))
}

#[allow(clippy::too_many_lines)]
fn build_profile(
    id: &str,
    initial_cpu: &Map<String, Value>,
    presets: &Map<String, Value>,
    items: &mut Vec<ImportItem>,
) -> Result<Value> {
    let preset = presets
        .get(id)
        .and_then(Value::as_object)
        .with_context(|| format!("missing or invalid presets.{id}"))?;
    let base = preset
        .get("*")
        .and_then(Value::as_object)
        .with_context(|| format!("missing or invalid presets.{id}.*"))?;
    let effective = |name: &str| {
        base.get(&format!("cpu.{name}"))
            .or_else(|| initial_cpu.get(name))
    };
    let required_effective = |name: &str| -> Result<&Value> {
        effective(name).with_context(|| format!("missing effective cpu.{name} for preset {id}"))
    };
    let margin = value_ratio(
        required_effective("margin")?,
        &format!("presets.{id}.*.cpu.margin"),
    )?;
    let burst = value_ratio(
        required_effective("burst")?,
        &format!("presets.{id}.*.cpu.burst"),
    )?;
    let limit_efficiency = effective("limitEfficiency")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let source_slow_limit_power_mw = scaled_decimal(
        required_effective("slowLimitPower")?,
        1_000,
        &format!("presets.{id}.*.cpu.slowLimitPower"),
    )?;
    let slow_limit_power_mw = source_slow_limit_power_mw.max(1);
    if source_slow_limit_power_mw == 0 {
        items.push(ImportItem {
            path: format!("presets.{id}.*.cpu.slowLimitPower"),
            status: ImportStatus::RequiresCalibration,
            message: "0 W cannot satisfy the Linux positive-budget invariant; draft uses 1 mW"
                .into(),
        });
    }
    let recover_path = format!("presets.{id}.*.cpu.fastLimitRecoverScale");
    let recover_scale = bounded_recover_scale(
        required_effective("fastLimitRecoverScale")?,
        &recover_path,
        items,
    )?;
    let power_budget = json!({
        "slow_limit_power_mw": slow_limit_power_mw,
        "fast_limit_power_mw": scaled_decimal(
            required_effective("fastLimitPower")?,
            1_000,
            &format!("presets.{id}.*.cpu.fastLimitPower"),
        )?,
        "fast_limit_capacity_mj": scaled_decimal(
            required_effective("fastLimitCapacity")?,
            1_000,
            &format!("presets.{id}.*.cpu.fastLimitCapacity"),
        )?,
        "fast_limit_recover_scale": recover_scale,
    });
    audit_profile_fields(id, "*", base, None, items);

    let mut scenes = Map::new();
    for scene_name in SUPPORTED_SCENES {
        let Some(source) = preset.get(scene_name).and_then(Value::as_object) else {
            continue;
        };
        let mut patch = Map::new();
        for (source_name, output_name) in [
            ("margin", "margin"),
            ("burst", "burst"),
            ("limitEfficiency", "limit_efficiency"),
        ] {
            if let Some(value) = source.get(&format!("cpu.{source_name}")) {
                patch.insert(output_name.into(), value.clone());
            }
        }
        let mut budget = Map::new();
        for (source_name, output_name, scale) in [
            ("slowLimitPower", "slow_limit_power_mw", 1_000),
            ("fastLimitPower", "fast_limit_power_mw", 1_000),
            ("fastLimitCapacity", "fast_limit_capacity_mj", 1_000),
        ] {
            if let Some(value) = source.get(&format!("cpu.{source_name}")) {
                budget.insert(
                    output_name.into(),
                    json!(scaled_decimal(
                        value,
                        scale,
                        &format!("presets.{id}.{scene_name}.cpu.{source_name}"),
                    )?),
                );
            }
        }
        if let Some(value) = source.get("cpu.fastLimitRecoverScale") {
            let recover_source_path =
                format!("presets.{id}.{scene_name}.cpu.fastLimitRecoverScale");
            budget.insert(
                "fast_limit_recover_scale".into(),
                json!(bounded_recover_scale(value, &recover_source_path, items)?),
            );
        }
        if !budget.is_empty() {
            patch.insert("power_budget".into(), Value::Object(budget));
        }
        if !patch.is_empty() {
            scenes.insert(scene_name.into(), Value::Object(patch));
        }
        audit_profile_fields(id, scene_name, source, Some(scene_name), items);
    }
    items.push(ImportItem {
        path: format!("presets.{id}"),
        status: ImportStatus::Imported,
        message: "imported CPU margins and PL1/PL2-style budget with shadow rollout".into(),
    });

    Ok(json!({
        "id": id,
        "margin": margin,
        "burst": burst,
        "limit_efficiency": limit_efficiency,
        "power_budget": power_budget,
        "scenes": scenes,
    }))
}

fn audit_profile_fields(
    profile: &str,
    section: &str,
    source: &Map<String, Value>,
    scene: Option<&str>,
    items: &mut Vec<ImportItem>,
) {
    for (key, value) in source {
        let path = format!("presets.{profile}.{section}.{key}");
        if is_imported_cpu_patch(key) {
            continue;
        }
        if key == "sched.scene" {
            let Some(scene) = scene else {
                items.push(ImportItem {
                    path,
                    status: ImportStatus::Unsupported,
                    message: "profile-wide sched.scene has no unambiguous Linux scene mapping"
                        .into(),
                });
                continue;
            };
            let expected = expected_scheduler_scene(scene);
            if value.as_str() == Some(expected) {
                items.push(ImportItem {
                    path,
                    status: ImportStatus::Inferred,
                    message: format!(
                        "source agrees with fixed scheduler_scene_for({scene}) -> {expected}; no mutable scheduler field is emitted"
                    ),
                });
            } else {
                items.push(ImportItem {
                    path,
                    status: ImportStatus::Unsupported,
                    message: format!(
                        "source value {value} conflicts with fixed scheduler_scene_for({scene}) -> {expected}; value was ignored"
                    ),
                });
            }
            continue;
        }
        items.push(ImportItem {
            path,
            status: ImportStatus::Unsupported,
            message: if key.starts_with("cpu.") {
                "sampling/latency CPU field has no current scene-patch representation and was ignored"
                    .into()
            } else {
                "unknown or non-CPU preset field was ignored and is not activatable".into()
            },
        });
    }
}

fn is_imported_cpu_patch(key: &str) -> bool {
    matches!(
        key,
        "cpu.margin"
            | "cpu.burst"
            | "cpu.limitEfficiency"
            | "cpu.slowLimitPower"
            | "cpu.fastLimitPower"
            | "cpu.fastLimitCapacity"
            | "cpu.fastLimitRecoverScale"
    )
}

fn expected_scheduler_scene(scene: &str) -> &'static str {
    match scene {
        "idle" => "idle",
        "touch" | "trigger" | "gesture" | "junk" => "touch",
        "switch" => "boost",
        _ => unreachable!("called only for supported frequency scenes"),
    }
}

fn parse_power_model(value: &Value, index: usize) -> Result<SourcePowerModel> {
    let path = format!("modules.cpu.powerModel[{index}]");
    let object = value
        .as_object()
        .with_context(|| format!("{path} must be an object"))?;
    let cores_u64 = integer_field(object, "nr", &path)?;
    let cores = usize::try_from(cores_u64).context("cluster core count is too large")?;
    if cores == 0 {
        bail!("{path}.nr must be greater than zero");
    }
    let relative_performance = u32::try_from(integer_field(object, "efficiency", &path)?)
        .with_context(|| format!("{path}.efficiency exceeds u32"))?;
    let typical_power_mw_per_core =
        u32::try_from(scaled_field(object, "typicalPower", 1_000, &path)?)
            .with_context(|| format!("{path}.typicalPower exceeds u32 mW"))?;
    let model = SourcePowerModel {
        cores,
        relative_performance,
        typical_power_mw_per_core,
        typical_frequency_hz: scaled_field(object, "typicalFreq", 1_000_000_000, &path)?,
        sweet_frequency_hz: scaled_field(object, "sweetFreq", 1_000_000_000, &path)?,
        plain_frequency_hz: scaled_field(object, "plainFreq", 1_000_000_000, &path)?,
        free_frequency_hz: scaled_field(object, "freeFreq", 1_000_000_000, &path)?,
    };
    if relative_performance == 0 || typical_power_mw_per_core == 0 {
        bail!("{path} efficiency and typicalPower must be greater than zero");
    }
    if !(model.plain_frequency_hz <= model.sweet_frequency_hz
        && model.sweet_frequency_hz <= model.typical_frequency_hz
        && model.free_frequency_hz <= model.typical_frequency_hz)
    {
        bail!("{path} frequencies must satisfy plain <= sweet <= typical and free <= typical");
    }
    Ok(model)
}

fn resolve_cluster_cpus(
    modules: &Map<String, Value>,
    models: &[SourcePowerModel],
    explicit: &[String],
    sysfs_root: &Path,
    items: &mut Vec<ImportItem>,
) -> Result<Vec<ClusterMapping>> {
    let mappings = if explicit.is_empty() {
        infer_local_cluster_cpus(models, sysfs_root)?
    } else {
        explicit_cluster_cpus(models, explicit)?
    };
    let mut used = BTreeSet::new();
    for mapping in &mappings {
        if !used.is_disjoint(&mapping.cpus) {
            bail!("cluster CPU mappings overlap");
        }
        used.extend(&mapping.cpus);
    }
    audit_android_cluster_masks(modules, models, &mappings, items);
    Ok(mappings)
}

fn explicit_cluster_cpus(
    models: &[SourcePowerModel],
    explicit: &[String],
) -> Result<Vec<ClusterMapping>> {
    if explicit.len() != models.len() {
        bail!(
            "received {} --cluster-cpus values for {} power-model clusters",
            explicit.len(),
            models.len()
        );
    }
    explicit
        .iter()
        .zip(models)
        .enumerate()
        .map(|(index, (list, model))| {
            let cpus = parse_cpu_list(list)
                .with_context(|| format!("parse --cluster-cpus value {}", index + 1))?;
            validate_cluster_size(&cpus, model, &format!("--cluster-cpus #{}", index + 1))?;
            Ok(ClusterMapping {
                cpus,
                status: ImportStatus::Imported,
                message:
                    "explicit operator mapping; verify it equals one local policy*/related_cpus set before activation"
                        .into(),
            })
        })
        .collect()
}

fn infer_local_cluster_cpus(
    models: &[SourcePowerModel],
    sysfs_root: &Path,
) -> Result<Vec<ClusterMapping>> {
    let policies = read_local_cpu_policies(sysfs_root).with_context(|| {
        format!(
            "cannot infer a safe local cluster mapping below {}; pass one explicit --cluster-cpus LIST per power-model cluster",
            sysfs_root.display()
        )
    })?;
    if policies.len() != models.len() {
        bail!(
            "found {} local cpufreq policies below {} for {} power-model clusters; pass one explicit --cluster-cpus LIST per model",
            policies.len(),
            sysfs_root.display(),
            models.len()
        );
    }
    let mut model_groups = BTreeMap::<usize, Vec<usize>>::new();
    for (index, model) in models.iter().enumerate() {
        model_groups.entry(model.cores).or_default().push(index);
    }
    let mut policy_groups = BTreeMap::<usize, Vec<usize>>::new();
    for (index, policy) in policies.iter().enumerate() {
        policy_groups
            .entry(policy.cpus.len())
            .or_default()
            .push(index);
    }
    if model_groups.keys().collect::<Vec<_>>() != policy_groups.keys().collect::<Vec<_>>() {
        bail!(
            "local related_cpus sizes do not match powerModel[].nr; pass explicit --cluster-cpus values"
        );
    }

    let mut assignments = vec![None; models.len()];
    for (core_count, model_indices) in model_groups {
        let policy_indices = policy_groups
            .get(&core_count)
            .expect("group keys were compared above");
        if model_indices.len() != policy_indices.len() {
            bail!(
                "found {} local policies with {core_count} CPUs for {} matching power models; pass explicit --cluster-cpus values",
                policy_indices.len(),
                model_indices.len()
            );
        }
        rank_policy_group(
            models,
            &policies,
            &model_indices,
            policy_indices,
            &mut assignments,
        )?;
    }
    validate_inferred_frequency_order(models, &policies, &assignments)?;
    assignments
        .into_iter()
        .enumerate()
        .map(|(model_index, policy_index)| {
            let policy_index = policy_index.context("internal incomplete cluster assignment")?;
            let policy = &policies[policy_index];
            Ok(ClusterMapping {
                cpus: policy.cpus.clone(),
                status: ImportStatus::Inferred,
                message: format!(
                    "inferred from {}/devices/system/cpu/cpufreq/{}/related_cpus using nr={} and immutable max-frequency rank {}; verify before activation",
                    sysfs_root.display(),
                    policy.name,
                    models[model_index].cores,
                    policy.maximum_frequency
                ),
            })
        })
        .collect()
}

fn rank_policy_group(
    models: &[SourcePowerModel],
    policies: &[LocalCpuPolicy],
    model_indices: &[usize],
    policy_indices: &[usize],
    assignments: &mut [Option<usize>],
) -> Result<()> {
    if model_indices.len() == 1 {
        assignments[model_indices[0]] = Some(policy_indices[0]);
        return Ok(());
    }
    let mut ranked_models = model_indices.to_vec();
    ranked_models.sort_unstable_by_key(|index| models[*index].typical_frequency_hz);
    if ranked_models
        .windows(2)
        .any(|pair| models[pair[0]].typical_frequency_hz == models[pair[1]].typical_frequency_hz)
    {
        bail!(
            "multiple same-size power models have equal typicalFreq; pass explicit --cluster-cpus values"
        );
    }
    let mut ranked_policies = policy_indices.to_vec();
    ranked_policies.sort_unstable_by_key(|index| policies[*index].maximum_frequency);
    if ranked_policies
        .windows(2)
        .any(|pair| policies[pair[0]].maximum_frequency == policies[pair[1]].maximum_frequency)
    {
        bail!(
            "multiple same-size local policies have equal maximum frequency; pass explicit --cluster-cpus values"
        );
    }
    for (model_index, policy_index) in ranked_models.into_iter().zip(ranked_policies) {
        assignments[model_index] = Some(policy_index);
    }
    Ok(())
}

fn validate_inferred_frequency_order(
    models: &[SourcePowerModel],
    policies: &[LocalCpuPolicy],
    assignments: &[Option<usize>],
) -> Result<()> {
    for left in 0..models.len() {
        for right in left + 1..models.len() {
            let model_order = models[left]
                .typical_frequency_hz
                .cmp(&models[right].typical_frequency_hz);
            if model_order == std::cmp::Ordering::Equal {
                continue;
            }
            let local_left = policies
                [assignments[left].context("missing inferred policy assignment")?]
            .maximum_frequency;
            let local_right = policies
                [assignments[right].context("missing inferred policy assignment")?]
            .maximum_frequency;
            if local_left.cmp(&local_right) != model_order {
                bail!(
                    "nr-based local mapping conflicts with typicalFreq/max-frequency ordering; pass explicit --cluster-cpus values"
                );
            }
        }
    }
    Ok(())
}

fn read_local_cpu_policies(sysfs_root: &Path) -> Result<Vec<LocalCpuPolicy>> {
    let root = sysfs_root.join("devices/system/cpu/cpufreq");
    let entries =
        fs::read_dir(&root).with_context(|| format!("enumerate local {}", root.display()))?;
    let mut directories = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("policy"))
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
        .collect::<Vec<_>>();
    directories.sort_unstable_by_key(std::fs::DirEntry::file_name);
    if directories.len() > MAX_LOCAL_POLICIES {
        bail!("local cpufreq policy count exceeds supported maximum {MAX_LOCAL_POLICIES}");
    }
    let mut policies = Vec::with_capacity(directories.len());
    let mut claimed_cpus = BTreeSet::new();
    for directory in directories {
        let path = directory.path();
        let name = directory.file_name().to_string_lossy().into_owned();
        let related = fs::read_to_string(path.join("related_cpus"))
            .with_context(|| format!("read {name}/related_cpus"))?;
        let related = related.split_whitespace().collect::<Vec<_>>().join(",");
        let cpus =
            parse_cpu_list(&related).with_context(|| format!("parse {name}/related_cpus"))?;
        if !claimed_cpus.is_disjoint(&cpus) {
            bail!("local cpufreq related_cpus sets overlap at {name}");
        }
        claimed_cpus.extend(&cpus);
        policies.push(LocalCpuPolicy {
            name,
            cpus,
            maximum_frequency: read_local_maximum_frequency(&path)?,
        });
    }
    if policies.is_empty() {
        bail!(
            "no local policy*/related_cpus entries found below {}; pass explicit --cluster-cpus values",
            root.display()
        );
    }
    Ok(policies)
}

fn read_local_maximum_frequency(policy: &Path) -> Result<u64> {
    let cpuinfo = policy.join("cpuinfo_max_freq");
    match fs::read_to_string(&cpuinfo) {
        Ok(value) => {
            return value
                .trim()
                .parse()
                .with_context(|| format!("parse {}", cpuinfo.display()));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", cpuinfo.display())),
    }
    let available = policy.join("scaling_available_frequencies");
    let values = fs::read_to_string(&available)
        .with_context(|| format!("read immutable frequency evidence {}", available.display()))?;
    values
        .split_whitespace()
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("parse {}", available.display()))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .with_context(|| format!("{} is empty", available.display()))
}

fn audit_android_cluster_masks(
    modules: &Map<String, Value>,
    models: &[SourcePowerModel],
    mappings: &[ClusterMapping],
    items: &mut Vec<ImportItem>,
) {
    let Some(masks) = modules
        .get("sched")
        .and_then(Value::as_object)
        .and_then(|sched| sched.get("cpumask"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for (index, (model, mapping)) in models.iter().zip(mappings).enumerate() {
        let name = format!("c{index}");
        let path = format!("modules.sched.cpumask.{name}");
        let Some(value) = masks.get(&name) else {
            items.push(ImportItem {
                path,
                status: ImportStatus::Unsupported,
                message: "Android cluster mask is missing and was not used for mapping".into(),
            });
            continue;
        };
        match parse_cpu_array(value, &path) {
            Ok(candidate) if candidate.len() != model.cores => items.push(ImportItem {
                path,
                status: ImportStatus::Unsupported,
                message: format!(
                    "Android candidate has {} CPUs but powerModel.nr={}; it was not used",
                    candidate.len(),
                    model.cores
                ),
            }),
            Ok(candidate) if candidate == mapping.cpus => items.push(ImportItem {
                path,
                status: ImportStatus::Inferred,
                message:
                    "Android mask agrees with the explicit/local Linux mapping but remains non-authoritative evidence"
                        .into(),
            }),
            Ok(candidate) => items.push(ImportItem {
                path,
                status: ImportStatus::RequiresCalibration,
                message: format!(
                    "Android candidate {:?} differs from selected Linux mapping {:?}; Android value was ignored",
                    candidate, mapping.cpus
                ),
            }),
            Err(error) => items.push(ImportItem {
                path,
                status: ImportStatus::Unsupported,
                message: format!("invalid Android mapping candidate was ignored: {error:#}"),
            }),
        }
    }
}

fn validate_cluster_size(cpus: &BTreeSet<u32>, model: &SourcePowerModel, path: &str) -> Result<()> {
    if cpus.len() != model.cores {
        bail!(
            "{path} contains {} CPUs but the corresponding power model declares nr={}; pass an explicit --cluster-cpus mapping",
            cpus.len(),
            model.cores
        );
    }
    Ok(())
}

fn parse_cpu_array(value: &Value, path: &str) -> Result<BTreeSet<u32>> {
    let array = value
        .as_array()
        .with_context(|| format!("{path} must be an array"))?;
    let mut cpus = BTreeSet::new();
    for (index, value) in array.iter().enumerate() {
        let cpu = value
            .as_u64()
            .with_context(|| format!("{path}[{index}] must be a non-negative integer"))?;
        let cpu = u32::try_from(cpu).with_context(|| format!("{path}[{index}] exceeds u32"))?;
        if cpu > MAX_CPU_ID {
            bail!("{path}[{index}] exceeds supported CPU ID {MAX_CPU_ID}");
        }
        if !cpus.insert(cpu) {
            bail!("{path} contains duplicate CPU {cpu}");
        }
    }
    if cpus.is_empty() {
        bail!("{path} must not be empty");
    }
    Ok(cpus)
}

fn parse_cpu_list(value: &str) -> Result<BTreeSet<u32>> {
    let mut cpus = BTreeSet::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            bail!("CPU list contains an empty component");
        }
        let (start, end) = if let Some((start, end)) = part.split_once('-') {
            let start = parse_cpu_id(start)?;
            let end = parse_cpu_id(end)?;
            if start > end {
                bail!("CPU range {part:?} is descending");
            }
            (start, end)
        } else {
            let cpu = parse_cpu_id(part)?;
            (cpu, cpu)
        };
        for cpu in start..=end {
            if !cpus.insert(cpu) {
                bail!("CPU list contains duplicate CPU {cpu}");
            }
        }
    }
    if cpus.is_empty() {
        bail!("CPU list must not be empty");
    }
    Ok(cpus)
}

fn parse_cpu_id(value: &str) -> Result<u32> {
    let cpu = value
        .parse::<u32>()
        .with_context(|| format!("invalid CPU ID {value:?}"))?;
    if cpu > MAX_CPU_ID {
        bail!("CPU ID {cpu} exceeds supported maximum {MAX_CPU_ID}");
    }
    Ok(cpu)
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<&'a Map<String, Value>> {
    object
        .get(field)
        .and_then(Value::as_object)
        .with_context(|| format!("missing or invalid {path} object"))
}

fn integer_field(object: &Map<String, Value>, field: &str, path: &str) -> Result<u64> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("{path}.{field} must be a non-negative integer"))
}

fn scaled_field(object: &Map<String, Value>, field: &str, scale: u64, path: &str) -> Result<u64> {
    let value = object
        .get(field)
        .with_context(|| format!("missing {path}.{field}"))?;
    scaled_decimal(value, scale, &format!("{path}.{field}"))
}

fn value_ratio(value: &Value, path: &str) -> Result<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite())
        .with_context(|| format!("{path} must be a finite number"))
}

fn bounded_recover_scale(value: &Value, path: &str, items: &mut Vec<ImportItem>) -> Result<f64> {
    let source = value_ratio(value, path)?;
    let bounded = source.clamp(0.1, 10.0);
    if !(0.1..=10.0).contains(&source) {
        items.push(ImportItem {
            path: path.into(),
            status: ImportStatus::RequiresCalibration,
            message: format!(
                "source recover scale {source} is outside Linux's 0.1..=10 bound; draft uses {bounded}"
            ),
        });
    }
    Ok(bounded)
}

fn scaled_decimal(value: &Value, scale: u64, path: &str) -> Result<u64> {
    let number = value
        .as_number()
        .with_context(|| format!("{path} must be a JSON number"))?;
    scale_decimal_text(&number.to_string(), scale)
        .with_context(|| format!("{path} does not convert exactly to the required base unit"))
}

fn scale_decimal_text(text: &str, scale: u64) -> Result<u64> {
    if text.starts_with(['-', '+']) {
        bail!("negative and explicitly signed values are not supported");
    }
    let (mantissa, exponent) = text
        .split_once(['e', 'E'])
        .map_or((text, 0_i32), |(mantissa, exponent)| {
            (mantissa, exponent.parse::<i32>().unwrap_or(i32::MIN))
        });
    if exponent == i32::MIN {
        bail!("invalid decimal exponent");
    }
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("invalid decimal number");
    }
    let digits = format!("{whole}{fraction}")
        .parse::<u128>()
        .context("decimal number is too large")?;
    let scale_power = decimal_power(scale).context("conversion scale is not a power of ten")?;
    let fraction_len = i32::try_from(fraction.len()).context("decimal precision is too large")?;
    let resulting_power = exponent
        .checked_add(scale_power)
        .and_then(|value| value.checked_sub(fraction_len))
        .context("decimal exponent overflow")?;
    let scaled = if resulting_power >= 0 {
        digits
            .checked_mul(power_of_ten(
                u32::try_from(resulting_power).context("decimal exponent overflow")?,
            )?)
            .context("scaled decimal is too large")?
    } else {
        let divisor = power_of_ten(resulting_power.unsigned_abs())?;
        if digits % divisor != 0 {
            bail!("value does not resolve to a whole base unit");
        }
        digits / divisor
    };
    u64::try_from(scaled).context("scaled decimal exceeds u64")
}

fn decimal_power(mut value: u64) -> Option<i32> {
    let mut power = 0_i32;
    while value > 1 && value.is_multiple_of(10) {
        value /= 10;
        power += 1;
    }
    (value == 1).then_some(power)
}

fn power_of_ten(power: u32) -> Result<u128> {
    10_u128
        .checked_pow(power)
        .context("decimal exponent is too large")
}

fn safe_id(source_name: &str, path: &Path) -> String {
    let fallback = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown");
    let result = slug(source_name);
    if result.is_empty() {
        slug(fallback)
    } else {
        result
    }
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut last_hyphen = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character.to_ascii_lowercase());
            last_hyphen = false;
        } else if !result.is_empty() && !last_hyphen {
            result.push('-');
            last_hyphen = true;
        }
        if result.len() >= 48 {
            break;
        }
    }
    result.trim_matches('-').to_owned()
}

fn read_source(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let read_limit = u64::try_from(MAX_CONFIG_FILE_BYTES)
        .expect("the platform can represent the configuration byte limit")
        + 1;
    let mut bytes = Vec::with_capacity(MAX_CONFIG_FILE_BYTES.min(64 * 1_024));
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

fn parse_strict(text: &str) -> Result<Value> {
    let strict: StrictValue = serde_json::from_str(text).context("invalid JSON")?;
    if !strict.duplicates.is_empty() {
        bail!(
            "duplicate JSON object keys: {}",
            strict.duplicates.join(", ")
        );
    }
    Ok(strict.value)
}

#[derive(Debug)]
struct StrictValue {
    value: Value,
    duplicates: Vec<String>,
}

impl StrictValue {
    fn scalar(value: Value) -> Self {
        Self {
            value,
            duplicates: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue::scalar(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue::scalar(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue::scalar(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(StrictValue::scalar)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue::scalar(Value::String(value.into())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue::scalar(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::scalar(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::scalar(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        let mut duplicates = Vec::new();
        while let Some(mut child) = sequence.next_element::<StrictValue>()? {
            let index = values.len();
            for duplicate in &mut child.duplicates {
                *duplicate = format!("/{index}{duplicate}");
            }
            duplicates.extend(child.duplicates);
            values.push(child.value);
        }
        Ok(StrictValue {
            value: Value::Array(values),
            duplicates,
        })
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        let mut duplicates = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let mut child = map.next_value::<StrictValue>()?;
            let segment = json_pointer_escape(&key);
            for duplicate in &mut child.duplicates {
                *duplicate = format!("/{segment}{duplicate}");
            }
            duplicates.extend(child.duplicates);
            if values.contains_key(&key) {
                duplicates.push(format!("/{segment}"));
            }
            values.insert(key, child.value);
        }
        Ok(StrictValue {
            value: Value::Object(values),
            duplicates,
        })
    }
}

fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use serde_json::{Value, json};
    use tempfile::tempdir;
    use uperf_core::{DeviceConfig, PolicyConfig};

    use super::{
        ImportStatus, import_path, parse_cpu_list, parse_power_model, parse_strict,
        scale_decimal_text, write_import,
    };

    fn source() -> &'static str {
        r#"{
          "meta": {"name":"test-soc","author":"test"},
          "modules": {
            "switcher": {"hintDuration":{"trigger":0.03,"gesture":0.1,"switch":0.4}},
            "input": {"enable":true,"swipeThd":0.03,"gestureThdX":0.04},
            "cpu": {"powerModel":[
              {"efficiency":140,"nr":2,"typicalPower":0.5,"typicalFreq":1.9,
               "sweetFreq":1.6,"plainFreq":1.4,"freeFreq":1.7},
              {"efficiency":320,"nr":1,"typicalPower":1.9,"typicalFreq":2.8,
               "sweetFreq":1.8,"plainFreq":1.0,"freeFreq":0.7}
            ]},
            "sysfs": {"enable":true,"knob":{"DDRmax":"/sys/android/ddr"}},
            "sched": {
              "enable":true,
              "cpumask":{"all":[0,1,2],"c0":[0,1],"c1":[2]},
              "prio":{"rtusr":{"bg":98}},
              "rules":[{"name":"android","pinned":true,"rules":[]}]
            }
          },
          "initials": {"cpu":{
            "baseSampleTime":0.01,"baseSlackTime":0.01,"latencyTime":0.5,
            "slowLimitPower":4.0,"fastLimitPower":8.0,"fastLimitCapacity":12.0,
            "fastLimitRecoverScale":0.3,"predictThd":0.3,
            "margin":0.18,"burst":0,"limitEfficiency":false
          }},
          "presets": {
            "powersave":{"*":{"cpu.slowLimitPower":1.0,"cpu.fastLimitPower":2.0,
              "cpu.fastLimitCapacity":1.0,"cpu.margin":0.1},
              "idle":{"cpu.baseSlackTime":0.08,"cpu.limitEfficiency":true},
              "junk":{"cpu.burst":0.08,"cpu.margin":0.2,
                "sched.scene":"touch","mystery":7}},
            "balance":{"*":{"cpu.slowLimitPower":2.0,"cpu.fastLimitPower":2.0,
              "cpu.fastLimitCapacity":16.0,"cpu.margin":0.21},
              "idle":{"cpu.baseSlackTime":0.08,"cpu.limitEfficiency":true},
              "switch":{"cpu.slowLimitPower":4.0,"cpu.fastLimitPower":6.0,
                "sched.scene":"idle"}},
            "performance":{"*":{"cpu.slowLimitPower":999.0,"cpu.fastLimitPower":999.0,
              "cpu.fastLimitCapacity":999.0,"cpu.margin":0.2,"cpu.burst":0.22,
              "sysfs.L3max":999999999}},
            "fast":{"*":{"cpu.burst":1.0}}
          }
        }"#
    }

    fn write_local_policy(
        sysfs_root: &Path,
        name: &str,
        related_cpus: &str,
        maximum_frequency: u64,
    ) {
        let policy = sysfs_root.join("devices/system/cpu/cpufreq").join(name);
        fs::create_dir_all(&policy).unwrap();
        fs::write(policy.join("related_cpus"), related_cpus).unwrap();
        fs::write(
            policy.join("cpuinfo_max_freq"),
            maximum_frequency.to_string(),
        )
        .unwrap();
    }

    #[test]
    fn strict_parser_reports_every_duplicate_path() {
        let error = parse_strict(r#"{"outer":{"x":1,"x":2},"y":1,"y":2}"#)
            .expect_err("duplicates must fail");
        let message = error.to_string();
        assert!(message.contains("/outer/x"));
        assert!(message.contains("/y"));
    }

    #[test]
    fn decimal_units_are_exact() {
        assert_eq!(
            scale_decimal_text("1.9", 1_000_000_000).unwrap(),
            1_900_000_000
        );
        assert_eq!(scale_decimal_text("0.03", 1_000).unwrap(), 30);
        assert_eq!(scale_decimal_text("5e-1", 1_000).unwrap(), 500);
        assert!(scale_decimal_text("0.0001", 1_000).is_err());
    }

    #[test]
    fn free_frequency_may_be_above_plain_but_not_typical() {
        let mut model = json!({
            "efficiency": 100,
            "nr": 1,
            "typicalPower": 1.0,
            "typicalFreq": 2.0,
            "sweetFreq": 1.6,
            "plainFreq": 1.2,
            "freeFreq": 1.8
        });
        parse_power_model(&model, 0).expect("free above plain and sweet remains valid");
        model["freeFreq"] = Value::from(2.1);
        assert!(parse_power_model(&model, 0).is_err());
        model["freeFreq"] = Value::from(1.0);
        model["plainFreq"] = Value::from(1.7);
        assert!(parse_power_model(&model, 0).is_err());
    }

    #[test]
    fn cpu_list_parser_is_strict() {
        assert_eq!(
            parse_cpu_list("0-2,4")
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 4]
        );
        assert!(parse_cpu_list("0-2,2").is_err());
        assert!(parse_cpu_list("3-1").is_err());
    }

    #[test]
    fn importer_converts_units_and_quarantines_android_actions() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("source.json");
        fs::write(&input, source()).unwrap();
        let sysfs = directory.path().join("sys");
        write_local_policy(&sysfs, "policy2", "2", 2_800_000);
        write_local_policy(&sysfs, "policy0", "0 1", 1_900_000);
        let import = import_path(&input, &[], &sysfs).expect("import source");

        assert_eq!(
            import
                .device
                .pointer("/cpu_policies/0/energy_model/typical_power_mw_per_core"),
            Some(&Value::from(500))
        );
        assert_eq!(
            import
                .device
                .pointer("/cpu_policies/0/energy_model/typical_frequency_hz"),
            Some(&Value::from(1_900_000_000_u64))
        );
        assert_eq!(
            import.device.pointer("/cpu_policies/0/efficient_cap_hz"),
            Some(&Value::from(1_700_000_000_u64))
        );
        assert_eq!(
            import.policy.pointer("/governor/active_sample_ms"),
            Some(&Value::from(10))
        );
        assert_eq!(
            import.policy.pointer("/governor/idle_sample_ms"),
            Some(&Value::from(80))
        );
        assert_eq!(
            import.policy.pointer("/governor/ramp_latency_ms"),
            Some(&Value::from(500))
        );
        assert_eq!(
            import
                .policy
                .pointer("/profiles/1/power_budget/fast_limit_capacity_mj"),
            Some(&Value::from(16_000))
        );
        assert_eq!(
            import.policy.pointer("/scheduler/enabled"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            import.policy.pointer("/profiles/0/scenes/junk/burst"),
            Some(&Value::from(0.08))
        );
        assert_eq!(
            import.policy.pointer("/profiles/0/scenes/junk/margin"),
            Some(&Value::from(0.2))
        );
        assert!(import.report.review_only);
        assert!(import.report.disabled_candidates.android_sysfs.is_some());
        assert!(
            import
                .report
                .disabled_candidates
                .android_scheduler
                .is_some()
        );
        assert!(import.report.items.iter().any(|item| {
            item.path == "presets.fast" && item.status == ImportStatus::Unsupported
        }));
        assert!(import.report.items.iter().any(|item| {
            item.path == "presets.powersave.junk.sched.scene"
                && item.status == ImportStatus::Inferred
        }));
        assert!(import.report.items.iter().any(|item| {
            item.path == "presets.powersave.junk.mystery"
                && item.status == ImportStatus::Unsupported
        }));
        assert!(import.report.items.iter().any(|item| {
            item.path == "presets.balance.switch.sched.scene"
                && item.status == ImportStatus::Unsupported
        }));
        assert!(import.report.items.iter().any(|item| {
            item.path == "presets.performance.*.sysfs.L3max"
                && item.status == ImportStatus::Unsupported
        }));

        DeviceConfig::from_json(&serde_json::to_string(&import.device).unwrap())
            .expect("device draft remains typed");
        PolicyConfig::from_json(&serde_json::to_string(&import.policy).unwrap())
            .expect("policy draft remains typed");

        let outputs = write_import(&directory.path().join("draft"), &import, false).unwrap();
        assert!(outputs.device.is_file());
        assert!(outputs.policy.is_file());
        assert!(outputs.report.is_file());
    }

    #[test]
    fn local_same_size_clusters_are_ranked_by_immutable_frequency() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("source.json");
        let mut document: Value = serde_json::from_str(source()).unwrap();
        document["modules"]["cpu"]["powerModel"][1]["nr"] = Value::from(2);
        document["modules"]["sched"]["cpumask"]["all"] = json!([0, 1, 2, 3]);
        document["modules"]["sched"]["cpumask"]["c1"] = json!([2, 3]);
        fs::write(&input, serde_json::to_vec(&document).unwrap()).unwrap();
        let sysfs = directory.path().join("sys");
        write_local_policy(&sysfs, "policy0", "2 3", 2_800_000);
        write_local_policy(&sysfs, "policy4", "0 1", 1_900_000);

        let import = import_path(&input, &[], &sysfs).expect("frequency-ranked inference");
        assert_eq!(
            import.device.pointer("/cpu_policies/0/related_cpus"),
            Some(&json!([0, 1]))
        );
        assert_eq!(
            import.device.pointer("/cpu_policies/1/related_cpus"),
            Some(&json!([2, 3]))
        );
    }

    #[test]
    fn missing_or_conflicting_local_topology_never_falls_back_to_android_masks() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("source.json");
        fs::write(&input, source()).unwrap();
        let missing = import_path(&input, &[], &directory.path().join("missing-sys"))
            .expect_err("missing local topology must stop");
        assert!(format!("{missing:#}").contains("--cluster-cpus"));

        let sysfs = directory.path().join("inverted-sys");
        write_local_policy(&sysfs, "policy0", "0 1", 3_000_000);
        write_local_policy(&sysfs, "policy2", "2", 1_000_000);
        let conflict =
            import_path(&input, &[], &sysfs).expect_err("frequency-rank conflict must stop");
        assert!(conflict.to_string().contains("frequency ordering"));
    }

    #[test]
    fn explicit_cluster_mapping_must_match_nr() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("source.json");
        fs::write(&input, source()).unwrap();
        let error = import_path(
            &input,
            &["0".into(), "1-2".into()],
            &directory.path().join("unused-sysfs"),
        )
        .expect_err("reversed cluster sizes must fail");
        assert!(error.to_string().contains("declares nr"));
    }
}
