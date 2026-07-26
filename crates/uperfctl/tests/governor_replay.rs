use std::{fs, path::Path, process::Command};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn offline_replay_matches_the_versioned_golden_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_uperfctl"))
        .args(["--json", "config", "replay-governor"])
        .arg(fixture("governor-replay-trace.json"))
        .arg("--policy")
        .arg(fixture("governor-replay-policy.json"))
        .output()
        .expect("run governor replay");
    assert!(
        output.status.success(),
        "replay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        fs::read(fixture("governor-replay-golden.json")).expect("read golden report")
    );
}

#[test]
fn energy_rollout_uses_the_same_offline_planner_without_dbus() {
    let output = Command::new(env!("CARGO_BIN_EXE_uperfctl"))
        .args(["--json", "config", "replay-governor"])
        .arg(fixture("governor-replay-trace.json"))
        .arg("--policy")
        .arg(fixture("governor-replay-policy.json"))
        .args(["--rollout", "energy"])
        .output()
        .expect("run active-rollout replay");
    assert!(
        output.status.success(),
        "replay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse replay report");
    assert_eq!(report["candidate_rollout"], "energy");
    assert_eq!(report["summary"]["target_comparisons"], 8);
}
