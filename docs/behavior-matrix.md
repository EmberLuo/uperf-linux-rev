# C implementation to Rust behavior matrix

| Area | C revision behavior | Rust decision |
| --- | --- | --- |
| CPU topology | Exact `related_cpus` mask matching | Preserve with dynamic CPU sets |
| Frequency limits | OPP snap, paired min/max writes | Preserve behind verified transactions; public values use exact Hz |
| Recovery | Runtime snapshots and best-effort restore | Versioned journal; failed restore blocks all mutation |
| Load policy | Per-policy maximum CPU load | Preserve; make smoothing time-based |
| Power model | Efficiency cancels from a linear formula | Replace with explicitly named load governor |
| Scenes | Single FSM including unreachable states | Timestamped hints with a derived dominant scene |
| Modes | `fast` inconsistently aliases performance | Only auto, powersave, balance and performance |
| Thermal | Coupled to heavy-load; manual pins bypass it | Independent observer and non-bypassable safety envelope |
| GPU | Manual override only | Manual override only in v1 |
| Workload identity | PID/start-time in some subsystems | Client submits PID only; daemon captures and retains PID, start-time and UID |
| Game detection | Broad substring scan may change global mode | Discovery only; trusted active workload controls mode |
| Foreground application | No concept; the workload was always explicit | Compositor-reported focus is a workload source under an expiring authorized lease; it never selects a profile tier |
| Scheduler | Affinity, nice, uclamp and optional FIFO | Affinity, nice, uclamp, OTHER/BATCH/IDLE; no RT in v1 |
| Cgroups | Modify existing dedicated systemd units | Preserve with ownership-aware restore |
| D-Bus | Fixed tuple shapes and cluster indexes | Versioned capabilities and stable target IDs |
| GUI | Polling and SM8550 hard-coding | Signal-driven and capability-generated |
| Arbitrary sysfs | Generic raw writer | Typed, allowlisted resources only |
