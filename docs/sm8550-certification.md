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
- Every enabled input class has evidence: type-B multitouch where present,
  EV_KEY press events for keyboards, and SYN-coalesced EV_REL for pointer
  movement/wheels. A hybrid MT touchpad produces one hint, not two.
- `uperf-probe --energy-draft` records the live OPP tables and marks every
  unmeasured power value as requiring calibration.
- Every calibration input was produced from the target board or an explicitly
  documented laboratory measurement; imported reference values are never
  described as measured hardware data.
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
- A previously durable-owned target accepts a 10 ms stream of latest-wins OPP
  submissions without one journal `fsync` per submission.
- Injected failure at every transaction step either restores the pre-call
  state or leaves a durable journal and read-only degraded actuator.
- Crash injection before claim, after claim, and between paired min/max writes
  restores the complete original hardware range after restart.
- SIGTERM restores and verifies state before the D-Bus name is released.
- SIGKILL at every journal/write/readback step is recovered on the same boot.
- A corrupt journal, device-fingerprint mismatch, or incomplete recovery
  blocks all further automatic and manual mutations.
- Sleep/wake, CPU hotplug, target disappearance, input hotplug, and
  `SYN_DROPPED` do not violate state identity or safety caps.
- Active workload exit and PID/TID reuse restore only the verified original
  scheduling state.
- Reload, sleep, shutdown, and target removal wait for the frequency worker to
  quiesce before the full restore transaction begins.

## Governor and power-budget gate

- `reference-curve-v1` is continuous at `plain_frequency_hz` and
  `sweet_frequency_hz`; its typical point reproduces configured typical power.
- The measured model contains every live OPP used by planning, with monotonic
  capacity and power values and documented measurement conditions.
- Fixed replay traces are deterministic under sampling jitter and agree with a
  small brute-force multi-cluster allocation oracle.
- The active/idle sampler moves only within 10–40 ms and 80 ms respectively;
  the thermal observer remains independent.
- Prediction and nonzero burst bypass ramp latency; downscaling and a newly
  tighter hardware/admin/thermal cap apply immediately.
- A short workload uses the fast budget, exhausts its energy bucket, and
  settles under the slow budget. Suspend time does not charge or refill the
  bucket.
- `shadow` applies the exact legacy limits while recording candidate limits.
  `energy` is root opt-in and automatically refuses an incomplete model or
  power budget.
- On the certified workload suite, energy rollout either lowers mean energy at
  equal frame time or improves p95 frame time at equal energy without a
  regression outside the agreed confidence interval. Otherwise legacy remains
  the default.

## Focus reporting gate

Requires `scheduler.focus.enabled` and a reporter, either the GNOME extension or
`uperfctl foreground`. Every item must read back the focused thread group's
`uclamp.min` and `uclamp.max` from `/proc/<tid>/sched` before and after, and the
defocused process must return to its captured values, not to a default.

- Switching focus between two applications boosts only the newly focused thread
  group and restores the previous one first, in that order.
- Focusing a window with no PID, or losing focus entirely, releases the lease.
- Locking immediately clears the focused workload even though the reporter
  remains alive in `unlock-dialog` mode to observe physical display power.
- The separate compositor-reporter lease survives focus clear, is renewed by
  idempotent blank-state reports, and still expires or is revoked when its peer
  disappears. It cannot select a workload by itself.
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

## Scene scheduler and compositor gate

- Idle, touch/trigger/gesture/junk, and boost/switch/wake map to the documented
  scheduler scenes; a dominant-scene transition begins reconciliation within
  100 ms.
- A `leader` selector matches exactly `tid == pid`, including a process whose
  truncated `comm` cannot identify its main thread.
- Newly created threads receive their matching plan within 500 ms. Exited or
  reused TIDs are never restored from another task's snapshot.
- Independent `uclamp_min` and `uclamp_max` patches preserve an omitted bound.
- A compositor paint starts only the current interaction generation. A
  presentation interval over 1.5 times the live output interval produces a
  rate-limited `Junk` hint only in that generation.
- After 50 ms without a paint the reporter emits render idle; after the
  daemon's 200 ms slack, only touch/trigger/gesture/junk hints from the same
  interaction end. Boost and Wake remain untouched.
- Mutter `PowerSaveMode` is the sole source of `DisplayBlanked`; logind
  `IdleHint` and `LockedHint` never impersonate physical blanking.
- A rejected frame reporter (wrong peer, UID, inactive session, or expired
  lease) performs no scene, profile, or frequency mutation.

## Optional scalar and FIFO gate

These gates apply only when the certified profile enables the corresponding
feature.

- Each LLCC/L3/DDR scalar target is an exact root-owned path with a closed
  integer/string/CPU-list domain. Every write reads back, and injected batch
  failures restore attempted targets in reverse order.
- A third-party scalar value is preserved and ownership relinquished rather
  than overwritten during restore.
- Automatic GPU control remains unavailable without a separately certified
  utilization source.
- FIFO remains disabled in the shipped policy and service. An experiment
  requires a reviewed priority no greater than the configured maximum, an
  affinity disjoint from at least one housekeeping CPU, and the separate
  systemd drop-in. Exact class/priority rollback is fault-injected; reference
  priorities 97/98 are never imported.

## Release gate

- Keyboard, pointer, touch, and focus-event to verified CPU apply each report
  p50/p95/p99; p95 is at most 50 ms under the documented test load.
- Scene-to-task reconciliation starts within 100 ms and a new matching thread
  is applied within 500 ms.
- A 24-hour soak shows no state leak and idle average CPU use remains below
  1% of one core.
- Active governor overhead remains below 3% of one core.
- Install, upgrade, failed reload, SIGTERM, SIGKILL, and removal tests leave no
  unrecoverable mutation.
- The signed certification report includes probe output, fault-injection
  results, latency measurements, soak logs, package versions, and recovery
  checksums.

Only after all gates pass should a release note call that exact hardware and
software combination certified.

## Current local evidence (not certification)

The following historical development evidence was collected on 2026-07-23
from a Xiaomi Pad 6S Pro 12.4 running the repository's local SM8550 kernel.
It predates the energy governor, fast path, compositor hints, typed scalar
resources, and FIFO implementation and therefore cannot validate them:

- the read-only probe resolved CPU masks `0-2`, `3-6`, and `7`, the
  `3d00000.gpu` devfreq target, 46 thermal zones, and a type-B multitouch
  device;
- a real session-bus `GetStatus` call completed against the release daemon,
  reported observed/desired values separately, and the daemon exited cleanly
  after SIGINT;
- a 45 second release-mode, read-only interval consumed 0.30 CPU-seconds from
  100 Hz `/proc` accounting, or approximately 0.67% of one core; its steady
  state had five named threads and 18,484 KiB RSS;
- the then-current 206 workspace tests, strict Clippy, rustdoc warnings, and
  release builds passed with that revision's locked offline dependency set.

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
