# ADR 0001: Capability-driven userspace scheduler

## Status

Accepted.

## Decision

The daemon is a userspace performance scheduler, not a kernel scheduling
class. Pure policy is separated from Linux observation and from privileged
mutation. Runtime state is represented as observed, desired and applied
snapshots. Only the actuator may mutate machine state.

The first release uses one systemd-hardened root daemon. The actuator boundary
is intentionally suitable for a future minimal privileged helper without
changing policy or public API.

The public API and configuration start at new major versions. Legacy C
configuration is migrated offline rather than supported as a second runtime
contract.

