# ADR 0001: Capability-driven userspace scheduler

## Status

Accepted.

## Decision

The daemon is a userspace performance scheduler, not a kernel scheduling
class. Pure policy is separated from Linux observation and from privileged
mutation. Runtime state is represented as observed, desired and applied
snapshots. Only the actuator may mutate machine state.

The control loop is explicitly:

```text
authenticated scene and workload events
→ observed load, thermal and session state
→ deterministic energy-governor transition
→ latest-wins frequency worker + independent scheduler worker
→ verified applied state
→ bounded decision timeline
```

Frequency planning is stateful but pure: `GovernorInput` plus
`GovernorState` produces a `GovernorTransition`, its next state, limits, and
diagnostics. Wall-clock reads, sampling, sysfs writes, journaling, and D-Bus
do not occur inside the planner.

The first mutation of a frequency target durably records its full hardware
range and ownership manifest. Later OPP updates to an already owned target use
a verified fast path without a journal `fsync` on every 10–40 ms cycle.
Restore, reload, target removal, sleep, and shutdown still quiesce workers and
use the complete persistent transaction. Typed scalar resources use the same
ownership model but remain root-declared, path-exact, and closed-domain.

Frequency and scheduler reconciliation are independent single-owner workers.
Frequency requests use latest-wins coalescing and recheck the safety generation
immediately before mutation; thread enumeration, systemd D-Bus, and scheduler
snapshot persistence cannot hold up the frequency lane.

Safety precedence is fixed rather than configurable:

```text
hardware/admin/thermal cap
> forced mode
> display-blanked or locked profile override
> application profile
> default profile
```

Burst bypasses ramp latency and the package power budget; prediction bypasses
ramp latency only. Neither can bypass a hardware, administrator, thermal,
recovery, or degraded-state constraint.

Some configuration vocabulary and curve semantics were informed by the public
[Uperf v3 reference](https://github.com/yinwanxi/Uperf-Game-Turbo). No upstream
configuration corpus, binary, injection library, or script is embedded in this
repository. The SM8550 device profile contains an audited transcription of the
`sdm8g2` CPU reference curve and preset power budgets from commit
`b2f10bbf0a3f192387e48b0abc929cb93f4eee43`. The energy governor independently
reconstructs the reference sparse control-OPP construction,
absolute-cost ordering, capacity-scaled demand, asymmetric predictor, bounded
normal/burst growth, per-core load power estimate, energy-bucket sign switch,
and stateful 0.9/1.1 package-cap feedback. Binary-guided behavior is covered by
independent golden tests, but the wider control path is not claimed to be an
instruction-for-instruction clone. Linux observed-frequency readback, hard
safety caps and ownership remain deliberate integration choices.
The shipped sampler and ramp timing are a shared, balance-oriented Linux
setting rather than a scene-by-scene clone of every Android preset override.

`measured-opp-v1` remains calibration-required. The bundled
`reference-curve-v1` uses Android-derived model values, not Linux rail-power
measurements, and is not rail-power certification; Linux thermal and ownership
safeguards remain authoritative.

The first release uses one systemd-hardened root daemon. The actuator boundary
is intentionally suitable for a future minimal privileged helper without
changing policy or public API.

Only the current configuration and recovery-journal contracts are accepted.

## Consequences

- A device JSON existing in the tree does not certify its energy model or
  writable nodes. CPU planning requires either a calibrated `measured-opp-v1`
  model or a root-reviewed `reference-curve-v1`; the latter is not measured
  rail-power certification. Scalar/interconnect targets require exact
  live-node review.
- Hardware certification, latency percentiles, energy/frame-time A/B results,
  and 24-hour soak evidence remain release artifacts; the implementation does
  not manufacture a passing result.
- Compositor frame and display hints are advisory and authenticated. logind
  `IdleHint`/`LockedHint` cannot claim that a physical display is blank.
- Competing power daemons are detected and reported, never killed or
  reconfigured automatically.
- Experimental FIFO requires independent policy and systemd opt-ins and does
  not accept the reference priorities unchanged.
