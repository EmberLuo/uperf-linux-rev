# uperf-linux-rev

`uperf-linux-rev` is a Rust userspace performance scheduler for Ubuntu/Linux
ARM64. It observes CPU load, thermal sensors, multitouch input, sleep/wake
events, and an explicitly registered active workload. It then coordinates
standard Linux controls: CPU/GPU frequency bounds, affinity, nice, uclamp, and
owned systemd cgroup properties.

It does **not** replace CFS/EEVDF or implement an Android framework service.
The policy loop runs in userspace and leaves final scheduling and thermal
protection to the Linux kernel.

The project is pre-release. An SM8550 device profile is bundled as the first
certification target; the profile being present is not, by itself, proof that
a particular board has passed the hardware-write and crash-recovery suite.
Other ARM64 systems may be probed read-only but remain uncertified.

## Safety model

- The daemon is the only public mutation entry point; clients use logical
  target IDs and can never submit arbitrary sysfs paths.
- No machine mutation is allowed before the original state is durably
  journaled and the journal file and parent directory are synchronized.
- Every write is read back. Failed transactions roll back to their
  pre-transaction state and verify the rollback.
- Failed or ambiguous crash recovery places the actuator in read-only degraded
  mode, blocking automatic and manual writes.
- Hardware limits and thermal constraints override profiles, boosts, and
  manual frequency overrides.
- Runtime state distinguishes what was observed, what policy requested, and
  what was actually applied and read back.

`/run/uperf-linux/recovery.json` is recovery evidence for the current boot.
Never delete it to silence an error. Resolve recovery while the matching
daemon and device are still available.

## Workspace

- `uperf-core`: units, identities, strict configuration, hints, thermal guard,
  and deterministic policy.
- `uperf-platform`: operating-system port traits.
- `uperf-linux`: read-only Linux discovery plus typed Linux adapters.
- `uperf-actuator`: transactional mutation, readback, rollback, and recovery.
- `uperf-api`: shared versioned D-Bus contract and DTOs.
- `uperf-daemon`: observers, state reducer, policy, and reconciliation.
- `uperfctl`: script-friendly command-line client and offline config tools.
- `uperf-probe`: read-only hardware/capability report.
- `uperf-testkit`: fake backends, clocks, and fault-injection support.
- `uperf-gui`: optional capability-driven GTK4/libadwaita client.

The architecture decision and C-to-Rust behavior choices are documented in
[ADR 0001](docs/adr/0001-architecture.md) and the
[behavior matrix](docs/behavior-matrix.md). Changes must also follow the
[contribution and safety rules](docs/contributing.md).

## Configuration and installed paths

The daemon reads a strict JSON Schema v2 bundle:

| Path | Ownership and role |
| --- | --- |
| `/etc/uperf-linux/device.json` | optional root-reviewed override for the matched shared profile |
| `/etc/uperf-linux/policy.json` | root-reviewed profiles, hints, load, thermal, and scheduler rules |
| `/var/lib/uperf-linux/apps.json` | daemon-managed global application rules; missing means an empty set |
| `/run/uperf-linux/recovery.json` | current-boot transaction journal; not configuration |

The package also installs immutable templates in
`/usr/share/uperf-linux/{devices,defaults}` and generated schemas in
`/usr/share/uperf-linux/schema`. Repository copies live under
[`config`](config). Schema validation catches structural mistakes; the Rust
validator additionally checks ranges, duplicate IDs, regexes, references,
CPU masks, and unsafe paths.

When the optional override is absent, the daemon parses every JSON file in
`/usr/share/uperf-linux/devices`, selects the single profile whose
`device_match` exactly matches the discovered device-tree compatible/model,
and rejects zero or ambiguous matches. Fresh packages do not create a
device-specific `/etc` file. An older retained `/etc/uperf-linux/device.json`
remains an explicit administrator override and takes precedence.

Each profile declares logical `cpu_groups`. Once a profile is the unique exact
identity match, every topology, frequency, devfreq, thermal, and cross-policy
selector must resolve against the live machine before the normal journaled
actuator is constructed. Adding a SoC therefore requires one device JSON plus
its tests or probe evidence, not a Rust or packaging edit.

## Read-only discovery

`uperf-probe` never writes procfs, sysfs, devices, or systemd:

```bash
cargo run --locked --package uperf-probe -- --pretty
cargo run --locked --package uperf-probe -- --device-draft
```

Inspect its CPU policy masks, OPPs, devfreq identities, thermal zone types,
input capabilities, device-tree compatible values, and warnings before
selecting a device profile. `--device-draft` prints a strict v2 device
document to standard output, but deliberately omits trusted thermal zones:
redirect it to a review file, add only verified sensors and safety limits, and
validate the complete bundle before installation.

## Build and test

Rust 1.96 is pinned by `rust-toolchain.toml`. Building the full workspace on
Ubuntu also needs GTK 4.10+, libadwaita 1.4+, and pkg-config:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo build --workspace --release --locked
```

Ordinary tests and CI never opt into real hardware writes. The GitHub workflow
also cross-builds the daemon, CLI, and probe for
`aarch64-unknown-linux-gnu`; GUI linkage is validated natively because GTK
cross sysroots are distribution-specific.

## Validate and migrate configuration

Validate one file or a directory containing the complete three-file bundle:

```bash
for profile in config/devices/*.json; do
  cargo run --locked --package uperfctl -- config validate "$profile"
done

install -d target/config-check
install -m 0644 config/devices/PROFILE.json target/config-check/device.json
install -m 0644 config/policy.json target/config-check/policy.json
install -m 0644 config/apps.json target/config-check/apps.json
cargo run --locked --package uperfctl -- config validate target/config-check
```

Legacy C schema v1 is migrated offline into a reviewable draft:

```bash
uperfctl config migrate-c-v1 old-config.json --output-dir migrated
# Edit migrated/device.json: add trusted thermal zones and verify all device
# selectors plus frequency/safety caps before validating the complete bundle.
uperfctl config validate migrated
```

Migration intentionally drops unknown sysfs writes and states without a real
event source. Its output is deliberately not activation-ready: review all
warnings, add trusted thermal sensors, verify hardware limits, and only install
the files after the complete directory passes `config validate`.

Every configuration document is limited to 1 MiB. Collection sizes are also
bounded in both semantic validation and the published JSON Schemas so malformed
or hostile rule sets cannot cause unbounded validation work.

## Service and clients

The system service owns `org.uperflinux.Daemon1`. Useful commands include:

```bash
uperfctl status
uperfctl health
uperfctl targets
uperfctl mode set balance
uperfctl workload set 1234
uperfctl frequency set cpu.performance 499200000Hz 2457600000Hz --ttl 30s
uperfctl diagnose
```

Every D-Bus frequency field and operating point is an exact integer number of
hertz. `uperfctl` treats bare frequency numbers as hertz and also accepts
explicit `Hz`, `kHz`, `MHz`, and `GHz` suffixes when they resolve to a whole
hertz. This preserves generic devfreq tables whose values are not divisible by
1000.

Status and telemetry are unprivileged. A workload-selection request contains
only a PID; the daemon reads its start time and UID before authorization and
records that stable identity itself. A caller can register only its own
workload unless it is root. Clearing operates on the daemon's current identity
without accepting client-supplied identity fields. Global mode changes use the
`control` PolicyKit action; frequency overrides, reloads, and persistent rules
use the `admin` action.

`GetCapabilities.features` contains exact identifiers, not searchable tags;
clients must test whole values. Device support is reported with the generic
`device-profile` feature; chip names are data, not API feature constants.

Persistent application rules in D-Bus v1 are administrator-owned global
rules. They contain an optional exact `executable` path (the full
`/proc/<pid>/exe` value), an optional `comm_regex` over the kernel process
name, or both as an AND matcher. Glob, package, full-command-line, and
desktop-ID matchers are not accepted. The v2 configuration model reserves
`desktop_id` for a future trusted desktop adapter, but semantic validation
currently rejects it.

D-Bus API v1.1 also exposes read-only running-workload discovery. The daemon
checks broad, case-insensitive game and compatibility-layer patterns such as
Wine, Proton, Steam and common emulators every five seconds. A match is only a
GUI/CLI candidate: it never selects the workload or changes the global mode.
Non-root clients see only candidates owned by their UID, full command lines and
executable paths are not exposed, and selecting a candidate still goes through
the stable PID/start-time/UID checks described above. The same view reports the
active workload's matched scheduler rule, desired/applied task counts and
owned systemd cgroup state.

Debian/Ubuntu releases provide two self-contained alternatives:
`uperf-linux` is the headless edition, while `uperf-linux-gui` contains the
same daemon and tools plus the GUI. Installing the GUI edition does not install
the headless package; APT replaces one edition with the other when switching.

Debian/Ubuntu packages deliberately leave the service disabled and stopped.
Review and validate the complete live configuration first, then enable it
explicitly. The GUI edition exposes an **Enable & Start** action while
disconnected; it asks PolicyKit to enable and start only
`uperf-linux.service`, then reconnects automatically. The equivalent command
is:

```bash
sudo systemctl enable --now uperf-linux.service
```

The GUI follows the system language by default and includes English and
Simplified Chinese. A persistent language selector is available under
Settings; reopening the application applies a changed language.

See [packaging and installation](docs/packaging.md) and the
[SM8550 certification checklist](docs/sm8550-certification.md) before any
hardware-write test.

## License

This project is licensed under
[GPL-3.0-or-later](LICENSE). The repository carries the complete GPLv3 text;
Cargo and Debian metadata record the project’s “or later” choice.
