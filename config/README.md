# Configuration assets

This directory contains the strict configuration bundle used by tests and
packaging:

- `devices/sm8550.json`: the first device-profile and certification target;
- `policy.json`: default profiles, scene patches, observers, and scheduler
  policy for that target;
- `apps.json`: an empty seed for daemon-managed application rules;
- `schema/*-v2.schema.json`: Draft 2020-12 schemas generated from the public
  `uperf-core` configuration types.

Installed mutable and immutable paths intentionally differ:

| Repository asset | Installed path |
| --- | --- |
| `devices/sm8550.json` | `/usr/share/uperf-linux/devices/sm8550.json` and initial `/etc/uperf-linux/device.json` |
| `policy.json` | `/usr/share/uperf-linux/defaults/policy.json` and initial `/etc/uperf-linux/policy.json` |
| `apps.json` | `/usr/share/uperf-linux/defaults/apps.json` only |
| `schema/*.json` | `/usr/share/uperf-linux/schema/` |

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
configuration type must regenerate all three schemas and review the resulting
contract diff.
