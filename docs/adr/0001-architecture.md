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
→ deterministic legacy/shadow/energy transition
→ latest-wins frequency worker + independent scheduler worker
→ verified applied state
→ bounded decision timeline and offline replay
```

Frequency planning is stateful but pure: `GovernorInput` plus
`GovernorState` produces a `GovernorTransition`, its next state, limits, and
diagnostics. Wall-clock reads, sampling, sysfs writes, journaling, and D-Bus
do not occur inside the planner. This makes recorded traces deterministic and
allows the new algorithm to run in shadow before it can control hardware.

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

Burst and prediction may bypass ramp latency and power budget, but never a
hardware, administrator, thermal, recovery, or degraded-state constraint.

Some configuration vocabulary and curve semantics were informed by the public
[Uperf v3 reference](https://github.com/yinwanxi/Uperf-Game-Turbo). No upstream
configuration corpus, binary, injection library, script, or derived device
calibration is embedded in this repository. The optional importer operates on
a document supplied by its user; algorithms are independently implemented and
tested with project-authored synthetic fixtures and locally measured data.

The first release uses one systemd-hardened root daemon. The actuator boundary
is intentionally suitable for a future minimal privileged helper without
changing policy or public API.

The public API and configuration start at new major versions. Legacy C
configuration is migrated offline rather than supported as a second runtime
contract.

## Consequences

- A device JSON existing in the tree does not certify its energy model or
  writable nodes. Active energy rollout requires a calibrated model, while
  scalar/interconnect targets require exact live-node review.
- Hardware certification, latency percentiles, energy/frame-time A/B results,
  and 24-hour soak evidence remain release artifacts; the implementation does
  not manufacture a passing result.
- Compositor frame and display hints are advisory and authenticated. logind
  `IdleHint`/`LockedHint` cannot claim that a physical display is blank.
- Competing power daemons are detected and reported, never killed or
  reconfigured automatically.
- Experimental FIFO requires independent policy and systemd opt-ins and is not
  a compatibility promise for reference priorities.
