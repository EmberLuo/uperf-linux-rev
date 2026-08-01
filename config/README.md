# Configuration assets

This directory contains the strict configuration bundle used by tests and
packaging:

- `devices/*.json`: independently installable device profiles selected by exact
  device-tree identity;
- `policy.json`: device-neutral profiles, scene patches, observers, and
  scheduler policy using logical CPU groups;
- `apps.json`: an empty seed for daemon-managed application rules;
- `calibration/*.json`: non-installable, provenance-rich measurement seeds
  that remain `requires-calibration`;
- `schema/*-v2.schema.json`: Draft 2020-12 schemas generated from the public
  `uperf-core` configuration types.

Installed mutable and immutable paths intentionally differ:

| Repository asset | Installed path |
| --- | --- |
| `devices/*.json` | `/usr/share/uperf-linux/devices/*.json` |
| `policy.json` | `/usr/share/uperf-linux/defaults/policy.json` and initial `/etc/uperf-linux/policy.json` |
| `apps.json` | `/usr/share/uperf-linux/defaults/apps.json` only |
| `schema/*.json` | `/usr/share/uperf-linux/schema/` |

`calibration/` is intentionally absent from the installation table. Its files
are offline review inputs, not daemon configuration; see
[`calibration/README.md`](calibration/README.md).

The daemon scans every `*.json` in the installed device directory and requires
exactly one `device_match`. `/etc/uperf-linux/device.json` is an optional
administrator override, not a package-generated default. Device profiles map
logical groups such as `efficient`, `balanced`, `performance`, and `all` to
the concrete CPU IDs referenced by the shared policy.

`/var/lib/uperf-linux/apps.json` is intentionally not package-owned. The
daemon treats a missing file as an empty rule set and creates it when an
administrator makes the first persistent rule change.

Application rules currently use `executable` (the full `/proc/<pid>/exe`
path), `comm_regex` (a regex over the kernel `comm` name), or both as an AND
matcher. The structurally reserved `desktop_id` field is rejected by semantic
validation until a trusted desktop adapter can populate that identity.

Do not add a `$schema` member to runtime JSON files: unknown fields are
rejected. Configure an editor to associate each schema with its filename
out-of-band. JSON Schema checks structure, while `uperfctl config validate`
also performs semantic and cross-file validation.

The committed schemas are release artifacts. Any pull request that changes a
configuration type must run
`cargo run --package uperf-core --example generate_schemas` and review the
resulting contract diff.

## Energy models

An energy model belongs to a CPU policy selected by its exact
`related_cpus`, never by a transient `policyN` directory. Reference estimates
use this curve shape:

```json
{
  "energy_model": {
    "kind": "reference-curve-v1",
    "relative_performance": 320,
    "typical_power_mw_per_core": 1900,
    "typical_frequency_hz": 2800000000,
    "sweet_frequency_hz": 1800000000,
    "plain_frequency_hz": 1000000000,
    "free_frequency_hz": 700000000
  }
}
```

Those numbers illustrate the wire shape; copying them does not calibrate a
machine. Core count is derived from live `related_cpus`. Reference curves must
satisfy `free <= plain <= sweet <= typical`, matching the ordering required by
the sparse control-OPP table.

Linux measurements should use an explicit OPP table:

```json
{
  "energy_model": {
    "kind": "measured-opp-v1",
    "points": [
      {
        "frequency_hz": 499200000,
        "relative_capacity": 180,
        "power_mw_per_core": 90
      },
      {
        "frequency_hz": 1920000000,
        "relative_capacity": 700,
        "power_mw_per_core": 850
      }
    ]
  }
}
```

Every value must come from a documented measurement run; a two-row example is
not a production calibration. `uperf-probe --energy-draft` supplies the live
frequency rows but deliberately leaves power null.

Every automatic CPU policy requires an energy model, and every profile requires
a complete power budget. Governor timing and prediction thresholds are shared
policy:

```json
{
  "governor": {
    "active_sample_ms": 20,
    "idle_sample_ms": 80,
    "active_load_threshold": 0.30,
    "idle_load_threshold": 0.15,
    "predict_threshold": 0.15,
    "ramp_latency_ms": 100,
    "min_opp_residency_ms": 10
  }
}
```

The active cadence must be 10–40 ms. Load above the active threshold enters
that cadence, load below the idle threshold enters the 80 ms cadence, and the
region between them preserves the current state. This sampler is separate from
`load.*`, whose longer EMA/dwell drives the heavy-load scene, and from the
independent thermal sampler. The governor uses fixed 0.98/0.97 asymmetric
predictor coefficients in previous-final-OPP performance units;
`predict_threshold` controls the raw-load delta bypass.

The shipped SM8550 policy uses the reference balance-oriented 40 ms active and
80 ms slack cadence as one shared Linux timing. Desktop input and compositor
events still cause immediate evaluation; they do not wait for the next load
sample. This is not a scene-by-scene reproduction of every Android preset's
timing overrides.

For `reference-curve-v1`, the runtime mirrors Uperf v3's sparse control table:
`free`, `plain`, thirds from `plain` to `sweet`, then quarters from `sweet` to
the hardware maximum, each snapped to a real OPP and deduplicated. Package
power uses summed per-CPU load with the reference multi-core correction.
Frequency demand is capacity-scaled by the previous final OPP, then applies the
reference asymmetric predictor and bounded margin/burst growth. Burst leaves
the lowest-performance cluster on normal demand and overrides only the higher
clusters, as in the reference guide-slot path. A shared persistent cursor moves
by at most one absolute-cost-ordered transition when modeled power leaves the
0.9–1.1 budget band; it does not turn a 1W/2W budget directly into a fixed
frequency cap. Linux readback power estimation, safety caps, and ownership
remain deliberate integration differences rather than a byte-for-byte Android
daemon clone.

Each profile-level budget uses exact milliwatts and millijoules:

```json
{
  "power_budget": {
    "slow_limit_power_mw": 4000,
    "fast_limit_power_mw": 8000,
    "fast_limit_capacity_mj": 12000,
    "fast_limit_recover_scale": 0.3
  }
}
```

A scene may partially override that budget, but a partial scene budget
requires a complete profile-level base. As in Uperf v3, nonzero burst bypasses
the ramp and power budget; hardware, administrator, thermal, recovery, and
degraded caps always remain absolute.

## Scene-aware task plans

Task-profile `scenes` are partial `TaskPlan` patches. Missing fields inherit
the base plan, including independent `uclamp_min` and `uclamp_max`:

```json
{
  "id": "foreground",
  "plan": {
    "scheduling_class": "other",
    "uclamp_min": 128
  },
  "scenes": {
    "idle": {
      "uclamp_min": 64
    },
    "touch": {
      "uclamp_min": 384
    },
    "boost": {
      "nice": -5,
      "uclamp_min": 512
    }
  }
}
```

The scheduler scene mapping is fixed: policy Idle maps to `idle`;
Touch/Trigger/Gesture/Junk map to `touch`; Boost/Switch/Wake map to `boost`;
otherwise foreground/background selection applies. Scene changes invalidate
the scheduler immediately, while the active workload's thread directory is
also refreshed every 250 ms.

New thread rules use typed selectors:

```json
{
  "selector": {"kind": "leader"},
  "task_profile": "latency"
}
```

or:

```json
{
  "selector": {
    "kind": "comm-regex",
    "pattern": "^(RenderThread|GPU completion)$"
  },
  "task_profile": "latency"
}
```

`leader` means exactly `tid == pid`; it does not guess from a truncated process
name.

## Typed scalar resources

LLCC/L3/DDR and similar single-node controls can be declared only in the
root-owned device profile:

```json
{
  "id": "ddr.hw-max",
  "path": "/sys/devices/platform/example/hw_max_freq",
  "domain": {
    "kind": "integer-enum",
    "values": [400000000, 800000000, 1200000000]
  }
}
```

Supported domains are `integer-range`, `integer-enum`, `string-enum`, and
`cpu-list`. Profiles and scene patches refer only to the logical ID through
tagged `scalar_values`; they cannot supply a path or extend its domain:

```json
{
  "scalar_values": {
    "ddr.hw-max": {
      "kind": "integer",
      "value": 800000000
    }
  }
}
```

Scalar writes use exact original-value capture, domain validation, readback,
reverse-order rollback, and the current v6 recovery journal. Do not add a scalar
target until its live path, accepted values, units, permissions, and behavior
have been probed on the exact board. GPU remains `manual_only` unless a future
profile has a separately trusted utilization source.

## Optional session and real-time policy

`policy.session` may name `display_blanked_profile` and `locked_profile`.
Both are optional profile references; session observation and precedence live
in the trusted daemon runtime, and locked state is not treated as proof that a
physical display is blank.

Experimental FIFO plans require an explicit policy envelope:

```json
{
  "scheduler": {
    "enabled": true,
    "realtime": {
      "enabled": true,
      "max_priority": 20,
      "housekeeping_cpus": [0]
    },
    "task_profiles": [{
      "id": "render-fifo",
      "affinity_group": "performance",
      "plan": {
        "scheduling_class": "fifo",
        "rt_priority": 20
      }
    }]
  }
}
```

At least one housekeeping CPU is required, every effective FIFO affinity must
exclude it, priorities must be in `1..=max_priority`, and the configured
maximum cannot exceed 50. Policy validation is only the first opt-in: the
default service still has `RestrictRealtime=yes`. See
[`docs/packaging.md`](../docs/packaging.md#experimental-fifo-scheduling-opt-in)
for the separate administrator-controlled drop-in. `SCHED_RR` is unsupported.
