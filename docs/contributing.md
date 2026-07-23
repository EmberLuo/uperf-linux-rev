# Contributing

## Scope and architecture

Keep dependencies pointing inward: pure policy belongs in `uperf-core`,
platform contracts in `uperf-platform`, Linux knowledge in `uperf-linux`, and
all mutation in `uperf-actuator`. API clients must never acquire a second
hardware-write path.

Changes to public configuration or D-Bus contracts require an explicit
versioning decision, contract tests, migration impact, and documentation.
Configuration-type changes must regenerate all committed JSON schemas.
Dependency and Rust toolchain upgrades are reviewed as explicit pull requests;
they are not rolled silently into unrelated work.

## Required checks

Run the same checks as CI:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo build --workspace --release --locked
```

Add deterministic unit or fake-backend coverage for policy and failure paths.
Never make ordinary tests depend on root privileges, a particular CPU count,
fixed policy numbering, or a real writable sysfs.

## Hardware-write changes

Real mutation tests are always explicit opt-in. Record the device, kernel,
firmware, current boot ID, probe report, original values, and recovery result.
Stop any installed daemon before a standalone hardware test. Both success and
failure cleanup must independently read back restored state.

Do not merge a change that weakens any global invariant:

- no durable journal means no mutation;
- incomplete recovery means read-only degraded operation;
- thermal caps cannot be bypassed;
- only stable logical IDs cross the D-Bus boundary;
- observed, desired, and applied/read-back state remain distinguishable;
- package installation never enables or starts the service.

Use the SM8550 checklist for certification work. Other devices remain
uncertified until their own reviewed profile and complete evidence exist.
