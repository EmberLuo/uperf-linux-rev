# Control-loop behavior and safety matrix

The Linux implementation is independent. Public Uperf v3 configuration
semantics informed part of the comparison column, but no upstream corpus or
binary is embedded and every executable test fixture is authored in this
project.

| Area | Reference semantics | `uperf-linux-rev` implementation |
| --- | --- | --- |
| CPU topology | Cluster index plus configured core count | Exact live `related_cpus` masks and dynamic sparse CPU sets |
| Frequency limits | OPP selection and paired bounds | Exact-Hz OPP snap, safe min/max write order, readback, rollback, and durable ownership |
| Recovery | Android service/script lifecycle restore | Current v6 journal only; failed or ambiguous restore makes every mutation read-only |
| Load dynamics | 10–40 ms active sampling, 80 ms idle, prediction and shared ramp latency | Time-weighted EMA, adaptive cadence, one-step prediction, shared ramp progress, and minimum OPP residency in a stateful pure governor |
| Power model | Piecewise reference curve and slow/fast power limits | `reference-curve-v1` plus explicit measured OPP tables, cross-cluster marginal Perf/W allocation, and a suspend-safe PL1/PL2-style energy bucket |
| Governor | Controller is always active | Fail-closed energy planning; every automatic CPU target requires a calibrated model |
| Scenes | Android hint FSM | Timestamped hints with deterministic dominance; `Junk` is generation-scoped and render idle can end only interaction hints |
| Desktop input | Touch, key, and pointer operations | Type-B multitouch, EV_KEY press, and SYN-coalesced EV_REL; hybrid MT devices do not double-report |
| Rendering | SurfaceFlinger analysis and junk hints | Authenticated GNOME/Mutter paint, render-idle, and physical display power events; the API reserves deadline hints for a safely typed future reporter |
| Session state | Android screen/standby actions | logind active/idle/locked observation plus independent compositor-confirmed display blank; neither is inferred from the other |
| Modes | Device presets, including Android-specific aliases | Only `auto`, `powersave`, `balance`, and `performance`; scene patches remain orthogonal |
| Thermal | Device-specific controls may disable or bypass vendor behavior | Independent trusted sensors and non-bypassable hardware/admin/thermal caps; burst never bypasses safety |
| GPU | Platform-specific nodes and policies | Manual-only unless a future device profile supplies a trustworthy GPU-utilization source; CPU load is never used as a proxy |
| Interconnect/sysfs | Logical names may resolve to arbitrary Android paths | Root-owned typed scalar targets with closed domains, exact paths, readback, reverse rollback, and v6 recovery |
| Workload identity | Android package/process conventions | Client submits a PID; daemon captures PID, start time, UID, and revalidates against exit/reuse |
| Game discovery | Package/process heuristics may drive mode | Read-only candidate discovery only; explicit or compositor focus identity selects the workload |
| Foreground application | Activity/cpuset-derived state | Authenticated compositor peer with separate expiring reporter and focus leases; explicit workload still wins |
| Scheduler | Per-scene affinity/priority; optional FIFO in v3 | Per-scene affinity, nice, independent uclamp bounds, OTHER/BATCH/IDLE, typed leader selectors, and root-reviewed experimental FIFO |
| Real-time safety | Reference priorities include 97/98 candidates | Default disabled, hard cap 50/default cap 20, required housekeeping CPU, separate systemd opt-in; no RR |
| Cgroups | Android cgroup/process writers | Owned systemd-unit controls and per-task snapshots with stable identity rollback |
| Configuration | Save-and-restart behavior | Parent-directory inotify with atomic-rename support, 250 ms debounce, full validation, and old-generation retention on failure |
| Observability | Log level, trace markers, verbose hint transitions | Structured tracing/journald events, monotonic decision/reconcile IDs, bounded D-Bus trace, and health diagnostics |
| Competing controllers | Scripts commonly disable vendor services | Read-only detection and drift reporting for PPD, tuned, TLP, auto-cpufreq, and system76-power; never stops them |
| Public API | Android module configuration and paths | Versioned capabilities and logical IDs; clients cannot submit a sysfs path |
| GUI | Reference-specific UI/configuration | Signal-driven and capability-generated; no SoC hard-coding |
