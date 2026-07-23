use std::{fs, process::Command};

use serde_json::json;
use tempfile::tempdir;

#[test]
fn cli_writes_migration_draft_then_rejects_it_as_an_activation_bundle() {
    let directory = tempdir().expect("temporary directory");
    let input = directory.path().join("legacy.json");
    let output = directory.path().join("migrated");
    let legacy = json!({
        "meta": {
            "name": "migration-cli-test",
            "schemaVersion": 1
        },
        "modules": {
            "cpu": {
                "powerModel": [{
                    "cpumask": "all",
                    "freeFreq": 300,
                    "typicalFreq": 1500,
                    "sweetFreq": 1000
                }]
            },
            "sched": {
                "cpumask": {
                    "all": [0, 2]
                }
            }
        }
    });
    fs::write(
        &input,
        serde_json::to_vec(&legacy).expect("serialize legacy configuration"),
    )
    .expect("write legacy configuration");

    let migration = Command::new(env!("CARGO_BIN_EXE_uperfctl"))
        .args(["config", "migrate-c-v1"])
        .arg(&input)
        .arg("--output-dir")
        .arg(&output)
        .output()
        .expect("run migration command");
    assert!(
        migration.status.success(),
        "migration failed: {}",
        String::from_utf8_lossy(&migration.stderr)
    );
    let migration_stderr = String::from_utf8_lossy(&migration.stderr);
    assert!(migration_stderr.contains("non-activatable draft"));
    for name in ["device.json", "policy.json", "apps.json"] {
        assert!(output.join(name).is_file(), "missing {name}");
    }

    let validation = Command::new(env!("CARGO_BIN_EXE_uperfctl"))
        .args(["config", "validate"])
        .arg(&output)
        .output()
        .expect("run validation command");
    assert_eq!(validation.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&validation.stderr).contains("trusted thermal zone"));
}
