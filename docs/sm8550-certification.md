# SM8550 certification checklist

The bundled `qcom-sm8550` profile is the first certification candidate. Its
selectors use stable CPU masks, the devfreq device name, device-tree
compatible value, and thermal zone `type` values; they do not depend on
`policyN` or `thermal_zoneN` numbering.

Bundling the profile does not certify every SM8550 product. Firmware, kernel,
cooling, input hardware, OPP tables, and thermal-zone exposure vary by board.
Record the exact board, kernel, firmware, boot ID, and probe report for each
certification run.

## Read-only gate

- `uperf-probe --pretty` completes without warnings that invalidate a target.
- Device-tree compatible data includes `qcom,sm8550`.
- CPU masks are exactly `0-2`, `3-6`, and `7`, with no unexpected overlap.
- Every configured floor, reference, efficient cap, admin cap, critical cap,
  and sensor-failure cap is within the discovered OPP range.
- GPU target `3d00000.gpu` resolves uniquely and remains manual-only.
- Trusted `cpuss0` through `cpuss3` and `gpuss-0` thermal zone types resolve
  uniquely and report plausible values.
- At least one correctly normalized multitouch device is available when input
  hints are enabled.
- The complete three-file v2 configuration passes semantic validation.

Any mismatch fails certification; do not substitute a similarly named node
without a new reviewed profile.

## Mutation and recovery gate

Hardware-write tests require explicit opt-in on an otherwise idle test system.
Stop any installed daemon first and capture every original value independently.

- Each CPU min/max pair and the GPU pair applies in the safe order, reads back,
  and restores to the captured values.
- Manual, Boost, and workload requests never exceed thermal or administrator
  caps.
- No-op reconciliation produces zero sysfs writes.
- Injected failure at every transaction step either restores the pre-call
  state or leaves a durable journal and read-only degraded actuator.
- SIGTERM restores and verifies state before the D-Bus name is released.
- SIGKILL at every journal/write/readback step is recovered on the same boot.
- A corrupt journal, device-fingerprint mismatch, or incomplete recovery
  blocks all further automatic and manual mutations.
- Sleep/wake, CPU hotplug, target disappearance, input hotplug, and
  `SYN_DROPPED` do not violate state identity or safety caps.
- Active workload exit and PID/TID reuse restore only the verified original
  scheduling state.

## Focus reporting gate

Requires `scheduler.focus.enabled` and a reporter, either the GNOME extension or
`uperfctl foreground`. Every item must read back the focused thread group's
`uclamp.min` and `uclamp.max` from `/proc/<tid>/sched` before and after, and the
defocused process must return to its captured values, not to a default.

- Switching focus between two applications boosts only the newly focused thread
  group and restores the previous one first, in that order.
- Focusing a window with no PID, or losing focus entirely, releases the lease.
- Locking the screen disables the extension, which releases the lease.
- Stopping the reporter without a clear expires the lease within
  `lease_ttl_ms`; killing its bus peer releases it immediately.
- The focused process exiting, including PID reuse by a new process, releases
  the lease without touching the reusing process.
- An explicit `uperfctl workload set` outranks the focus lease and is not
  displaced by later focus changes; clearing it falls back to the live lease.
- A report for another UID, a protected process, or a caller outside an active
  local session is refused, appears in `uperfctl health`, and performs zero
  scheduling writes.
- Restarting the daemon leaves no boost behind and the extension re-reports.

## Release gate

- Touch-to-verified-CPU-apply p95 is at most 50 ms under the documented test
  load.
- A 24-hour soak shows no state leak and idle average CPU use remains below
  1% of one core.
- Install, upgrade, failed reload, SIGTERM, SIGKILL, and removal tests leave no
  unrecoverable mutation.
- The signed certification report includes probe output, fault-injection
  results, latency measurements, soak logs, package versions, and recovery
  checksums.

Only after all gates pass should a release note call that exact hardware and
software combination certified.

## Current local evidence (not certification)

The following development evidence was collected on 2026-07-23 from a Xiaomi
Pad 6S Pro 12.4 running the repository's local SM8550 kernel:

- the read-only probe resolved CPU masks `0-2`, `3-6`, and `7`, the
  `3d00000.gpu` devfreq target, 46 thermal zones, and a type-B multitouch
  device;
- a real session-bus `GetStatus` call completed against the release daemon,
  reported observed/desired values separately, and the daemon exited cleanly
  after SIGINT;
- a 45 second release-mode, read-only interval consumed 0.30 CPU-seconds from
  100 Hz `/proc` accounting, or approximately 0.67% of one core; its steady
  state had five named threads and 18,484 KiB RSS;
- all 206 workspace tests, strict Clippy, rustdoc warnings, and release builds
  passed with the locked offline dependency set.

This is deliberately not a signed certification result. No real sysfs,
scheduler, or systemd mutation was performed. The 24-hour soak,
touch-to-verified-apply latency, cross-UID PolicyKit VM tests, package lifecycle
tests, and physical crash-at-every-write-step campaign remain open.

The current evdev recovery path handles `SYN_DROPPED` conservatively by
discarding uncertain contacts and rebuilding state from subsequent kernel
events (plus the current-slot snapshot exposed by the safe evdev API). Full
multi-slot `EVIOCGMTSLOTS` reconstruction is not yet available through the
safe dependency API and remains a release-gate item; gesture continuity after
an overrun must not be claimed until that adapter is added and tested.
