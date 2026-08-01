use std::{fs::File, io::Read, path::Path};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use uperf_core::{AppsConfig, ConfigBundle, DeviceConfig, MAX_CONFIG_FILE_BYTES, PolicyConfig};

#[derive(Debug)]
pub struct ValidationReport {
    pub kind: &'static str,
    pub schema_version: Option<u64>,
    pub errors: Vec<String>,
}

impl ValidationReport {
    pub fn valid(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn as_json(&self, path: &Path) -> Value {
        json!({
            "path": path,
            "kind": self.kind,
            "valid": self.valid(),
            "schema_version": self.schema_version,
            "errors": self.errors,
        })
    }

    pub fn print_human(&self, path: &Path) {
        if self.valid() {
            println!(
                "{}: valid {} configuration schema v{}",
                path.display(),
                self.kind,
                self.schema_version.unwrap_or_default()
            );
        } else {
            println!("{}: invalid {} configuration", path.display(), self.kind);
        }
        for error in &self.errors {
            eprintln!("error: {error}");
        }
    }
}

pub fn validate_path(path: &Path) -> Result<ValidationReport> {
    if path.is_dir() {
        validate_bundle(path)
    } else {
        validate_file(path)
    }
}

fn validate_file(path: &Path) -> Result<ValidationReport> {
    let text = read_config_text(path)?;
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return Ok(ValidationReport {
                kind: "unknown",
                schema_version: None,
                errors: vec![format!("invalid JSON: {error}")],
            });
        }
    };
    let schema_version = value.get("schema_version").and_then(Value::as_u64);
    let kind = detect_kind(&value, path);
    let error = match kind {
        "device" => DeviceConfig::from_json(&text).err().map(|error| error.to_string()),
        "policy" => PolicyConfig::from_json(&text).err().map(|error| error.to_string()),
        "apps" => AppsConfig::from_json(&text).err().map(|error| error.to_string()),
        _ => Some(
            "unable to determine config kind; use device.json, policy.json, or apps.json and include its required discriminator fields"
                .into(),
        ),
    };
    Ok(ValidationReport {
        kind,
        schema_version,
        errors: error.into_iter().collect(),
    })
}

fn validate_bundle(directory: &Path) -> Result<ValidationReport> {
    let device_path = directory.join("device.json");
    let policy_path = directory.join("policy.json");
    let apps_path = directory.join("apps.json");
    let mut errors = Vec::new();

    let device = parse_document(&device_path, "device", DeviceConfig::from_json, &mut errors)?;
    let policy = parse_document(&policy_path, "policy", PolicyConfig::from_json, &mut errors)?;
    let apps = parse_document(&apps_path, "apps", AppsConfig::from_json, &mut errors)?;

    if let (Some(device), Some(policy), Some(_)) = (device, policy, apps)
        && let Err(bundle_errors) = (ConfigBundle { device, policy }).validate_cross_references()
    {
        errors.extend(
            bundle_errors
                .issues()
                .iter()
                .map(|issue| format!("{}: {}", issue.path, issue.message)),
        );
    }

    Ok(ValidationReport {
        kind: "bundle",
        schema_version: Some(u64::from(uperf_core::CONFIG_SCHEMA_VERSION)),
        errors,
    })
}

fn parse_document<T>(
    path: &Path,
    kind: &str,
    parse: impl FnOnce(&str) -> Result<T, uperf_core::ConfigLoadError>,
    errors: &mut Vec<String>,
) -> Result<Option<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            errors.push(format!("{kind}: missing {}", path.display()));
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    let text = read_open_config(file, path)?;
    match parse(&text) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            errors.push(format!("{kind}: {error}"));
            Ok(None)
        }
    }
}

pub(crate) fn read_config_text(path: &Path) -> Result<String> {
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

fn detect_kind(value: &Value, path: &Path) -> &'static str {
    let object = value.as_object();
    let device = object.is_some_and(|object| {
        object.contains_key("device_id")
            || object.contains_key("cpu_policies")
            || object.contains_key("devfreq_targets")
    });
    let policy = object.is_some_and(|object| {
        object.contains_key("default_profile")
            || object.contains_key("profiles")
            || object.contains_key("scheduler")
    });
    let apps = object
        .is_some_and(|object| object.contains_key("rules") && !object.contains_key("profiles"));
    match (device, policy, apps) {
        (true, false, false) => "device",
        (false, true, false) => "policy",
        (false, false, true) => "apps",
        _ => match path.file_stem().and_then(|name| name.to_str()) {
            Some("device") => "device",
            Some("policy") => "policy",
            Some("apps") => "apps",
            _ => "unknown",
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;
    use uperf_core::MAX_CONFIG_FILE_BYTES;

    use super::{detect_kind, validate_path};

    #[test]
    fn detects_each_separate_v2_document() {
        assert_eq!(
            detect_kind(
                &json!({"schema_version": 2, "device_id": "test"}),
                std::path::Path::new("unknown.json")
            ),
            "device"
        );
        assert_eq!(
            detect_kind(
                &json!({"schema_version": 2, "default_profile": "balance"}),
                std::path::Path::new("unknown.json")
            ),
            "policy"
        );
        assert_eq!(
            detect_kind(
                &json!({"schema_version": 2, "rules": []}),
                std::path::Path::new("unknown.json")
            ),
            "apps"
        );
    }

    #[test]
    fn directory_validation_reports_missing_bundle_files() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("apps.json"),
            r#"{"schema_version":2,"rules":[]}"#,
        )
        .unwrap();
        let report = validate_path(directory.path()).unwrap();
        assert!(!report.valid());
        assert!(report.errors.iter().any(|error| error.contains("device")));
        assert!(report.errors.iter().any(|error| error.contains("policy")));
    }

    #[test]
    fn invalid_single_document_uses_core_parser() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("apps.json");
        fs::write(&path, r#"{"schema_version":2,"rules":[],"unknown":true}"#).unwrap();
        let report = validate_path(&path).unwrap();
        assert!(!report.valid());
        assert!(report.errors[0].contains("unknown field"));
    }

    #[test]
    fn readers_reject_configuration_files_over_the_byte_limit() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("apps.json");
        fs::write(&path, vec![b' '; MAX_CONFIG_FILE_BYTES + 1]).unwrap();
        let error = validate_path(&path).expect_err("oversized config must fail");
        assert!(error.to_string().contains("configuration file limit"));
    }
}
