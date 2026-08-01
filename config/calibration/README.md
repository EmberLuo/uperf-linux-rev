# Hardware energy calibration

This directory documents the first-party measurement workflow for
`uperf-linux`. Calibration worksheets and reports are review artifacts, not
runtime configuration. Do not copy an unfinished artifact into
`/etc/uperf-linux` or activate an unmeasured energy model.

## 1. Capture the target

Run the read-only probe on the exact board and software build being calibrated:

```bash
uperf-probe --pretty > probe.json
uperf-probe --energy-draft > energy-draft.json
```

Archive both files with the board revision, kernel, firmware, boot ID, ambient
temperature, cooling setup, power-meter identity, and the commands used for
the workload. Repeat calibration when any of those inputs changes materially.

The energy draft enumerates the live CPU policies and OPPs. Its
`relative_capacity_inferred` values are starting estimates and every
`power_mw_per_core_measured` field is deliberately `null`; the draft is not an
activatable energy model.

## 2. Measure each policy

Use a reproducible CPU-bound workload pinned to the policy under test. Keep
unrelated workloads, thermal state, cooling, and measurement duration
consistent. At every OPP that the planner may select:

1. Measure an idle baseline under the same conditions.
2. Measure steady-state loaded power after frequency and temperature settle.
3. Repeat the run and retain the samples, median, spread, and any rejected run.
4. Compute incremental per-core power as
   `(loaded_power - idle_baseline) / active_cores`.
5. Measure or justify relative capacity using the same workload and a declared
   normalization point; do not silently promote the probe's frequency-based
   estimate to measured capacity.

Record temperatures throughout the run. Discard measurements affected by
thermal throttling, an unexpected OPP, CPU migration, or another power-policy
controller. Power and capacity must be positive and monotonic after review;
an anomaly requires investigation rather than automatic smoothing.

## 3. Build and validate the model

Translate the reviewed rows into a `measured-opp-v1` model in a device profile:

```json
{
  "energy_model": {
    "kind": "measured-opp-v1",
    "points": [
      {
        "frequency_hz": 1000000000,
        "relative_capacity": 320,
        "power_mw_per_core": 180
      }
    ]
  }
}
```

The example shows only the field shape; its numbers are not calibration data.
Include every OPP the policy may use, select the policy by its exact
`related_cpus`, and keep the measurement report beside the proposed profile
during review.

Validate the complete configuration bundle:

```bash
uperfctl config validate /path/to/review-bundle
```

## 4. Certify before installation

Only install a root-reviewed, hardware-specific profile after completing the
relevant hardware certification checklist, including fault-injection recovery,
event-to-verified-apply latency, a long soak, and an A/B result showing either
lower energy at equivalent performance or better performance at equivalent
energy. An incomplete model is rejected instead of falling back to another
planner.
