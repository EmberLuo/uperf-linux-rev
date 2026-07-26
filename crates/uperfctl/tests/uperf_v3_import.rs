use std::{fs, path::Path, process::Command};

use tempfile::tempdir;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn cli_writes_a_typed_review_only_uperf_v3_draft() {
    let directory = tempdir().expect("temporary directory");
    let output = directory.path().join("draft");
    let command = Command::new(env!("CARGO_BIN_EXE_uperfctl"))
        .args(["config", "import-uperf-v3"])
        .arg(fixture("uperf-v3-valid.json"))
        .arg("--output-dir")
        .arg(&output)
        .args(["--cluster-cpus", "0-1", "--cluster-cpus", "2"])
        .output()
        .expect("run Uperf v3 importer");
    assert!(
        command.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&command.stderr)
    );
    assert!(String::from_utf8_lossy(&command.stdout).contains("REVIEW ONLY"));

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(output.join("import-report.json")).expect("read import report"),
    )
    .expect("parse import report");
    assert_eq!(report["review_only"], true);
    assert_eq!(report["format"], "uperf-v3-import-report-v1");

    for name in ["device.json", "policy.json"] {
        let validation = Command::new(env!("CARGO_BIN_EXE_uperfctl"))
            .args(["config", "validate"])
            .arg(output.join(name))
            .output()
            .expect("validate typed draft");
        assert!(
            validation.status.success(),
            "{name} is not typed: {}",
            String::from_utf8_lossy(&validation.stderr)
        );
    }
    assert!(
        !output.join("apps.json").exists(),
        "Android application rules must not be activated"
    );
}

#[test]
fn cli_rejects_duplicate_object_keys_without_writing_outputs() {
    let directory = tempdir().expect("temporary directory");
    let output = directory.path().join("draft");
    let command = Command::new(env!("CARGO_BIN_EXE_uperfctl"))
        .args(["config", "import-uperf-v3"])
        .arg(fixture("uperf-v3-duplicate-keys.json"))
        .arg("--output-dir")
        .arg(&output)
        .output()
        .expect("run Uperf v3 importer");
    assert!(!command.status.success());
    let stderr = String::from_utf8_lossy(&command.stderr);
    assert!(stderr.contains("duplicate JSON object keys"));
    assert!(stderr.contains("/modules/sched/affinity/worker"));
    assert!(stderr.contains("/modules/sched/prio/critical"));
    assert!(!output.exists(), "invalid input must not create outputs");
}
