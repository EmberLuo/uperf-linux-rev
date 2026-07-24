# Packaging and installation

## Package editions

The Debian metadata produces two mutually exclusive, self-contained ARM64
editions:

- `uperf-linux`: daemon, CLI, read-only probe, systemd unit, D-Bus policy,
  PolicyKit actions, schemas, defaults, and the SM8550 device profile, without
  desktop dependencies;
- `uperf-linux-gui`: the same complete core payload plus the GTK4/libadwaita
  client, desktop entry, AppStream metadata, and icon. It does not require the
  separate `uperf-linux` package.

The editions conflict with and replace one another because they own the same
daemon and configuration paths. Installing one edition with APT therefore
switches from the other instead of allowing two packages to own the same
privileged service. GTK remains linked only into the GUI executable, not the
daemon or command-line tools.

Install exactly one edition:

```bash
# Headless
sudo apt install uperf-linux

# Complete GUI edition; uperf-linux is not a dependency
sudo apt install uperf-linux-gui
```

The repository groups distribution assets below `packaging/`. A release
source tree must expose `packaging/debian` as its top-level `debian`
directory before invoking `dpkg-buildpackage`. On a clean checkout:

```bash
test ! -e debian
cp -a packaging/debian debian
cargo fetch --locked
dpkg-buildpackage --build=binary --unsigned-changes --unsigned-source
```

The rules run Cargo in offline, locked mode after `cargo fetch`; release
builders should instead provide a reviewed vendor directory or distribution
Rust source packages when network-free reproducibility is required.

## Installation layout

| Path | Package behavior |
| --- | --- |
| `/usr/bin/uperf-linux` | root daemon |
| `/usr/bin/uperfctl` | D-Bus client and offline migration/validation |
| `/usr/bin/uperf-probe` | read-only discovery |
| `/usr/bin/uperf-gui` | GUI edition only |
| `/etc/uperf-linux/device.json` | conffile initialized from SM8550 profile |
| `/etc/uperf-linux/policy.json` | policy conffile |
| `/usr/share/uperf-linux/devices/` | immutable device profiles |
| `/usr/share/uperf-linux/defaults/` | immutable policy and app-rule seeds |
| `/usr/share/uperf-linux/schema/` | JSON Schema v2 documents |
| `/var/lib/uperf-linux/apps.json` | daemon-managed; not owned by dpkg |
| `/run/uperf-linux/recovery.json` | actuator-owned current-boot journal |

## Deliberately disabled after install

For both editions, `dh_installsystemd` is called with both `--no-enable` and
`--no-start`. Installation therefore cannot mutate hardware merely because a
package was installed. The administrator must:

1. confirm that the device match, policy masks, target OPPs, and trusted
   thermal zone types agree with `uperf-probe`;
2. stage `device.json`, `policy.json`, and the current or default `apps.json`
   in one directory and run `uperfctl config validate DIRECTORY`;
3. enable and start `uperf-linux.service` explicitly.

The GUI edition presents an **Enable & Start** button when the daemon is
unavailable. It calls the fixed systemd D-Bus methods for
`uperf-linux.service` with interactive PolicyKit authorization, which is
equivalent to `systemctl enable --now uperf-linux.service`. It does not invoke
a shell or accept a client-supplied unit name. Successful activation is
followed by automatic D-Bus reconnection.

The same conservative rule applies to upgrades: a service stopped for package
replacement is not started again implicitly. Revalidate, inspect any retained
recovery evidence, and start it explicitly.

The service has no arbitrary network access, uses only Unix sockets, and is
hardened without hiding `/proc`, `/sys`, or input devices that its stated
function requires. Hardware writes remain constrained by the actuator's
allowlist and durable journal.

## Upgrade and removal safety

The systemd runtime directory is preserved across stop because it contains
recovery evidence. On remove, deconfigure, upgrade, or switching editions,
`prerm`:

1. stops a running daemon and requires the stop to succeed;
2. checks `/run/uperf-linux/recovery.json`;
3. refuses the package operation if the journal still exists.

No package script deletes the journal. A remaining journal means restoration
did not complete and must be resolved with the still-installed daemon. The
daemon-managed app rules in `/var/lib` are also retained.

## Policy files

The D-Bus policy lets root own `org.uperflinux.Daemon1` and lets local clients
send requests and receive signals. Authorization decisions stay in the daemon:
active-workload clients submit only a PID, then the daemon reads the stable
identity and verifies ownership before accepting it. Global profile control
uses `org.uperflinux.control`, and privileged mutations use
`org.uperflinux.admin`.
