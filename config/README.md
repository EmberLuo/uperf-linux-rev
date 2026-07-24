# Configuration assets

This directory contains the strict configuration bundle used by tests and
packaging:

- `devices/*.json`: independently installable device profiles selected by exact
  device-tree identity;
- `policy.json`: device-neutral profiles, scene patches, observers, and
  scheduler policy using logical CPU groups;
- `apps.json`: an empty seed for daemon-managed application rules;
- `schema/*-v2.schema.json`: Draft 2020-12 schemas generated from the public
  `uperf-core` configuration types.

Installed mutable and immutable paths intentionally differ:

| Repository asset | Installed path |
| --- | --- |
| `devices/*.json` | `/usr/share/uperf-linux/devices/*.json` |
| `policy.json` | `/usr/share/uperf-linux/defaults/policy.json` and initial `/etc/uperf-linux/policy.json` |
| `apps.json` | `/usr/share/uperf-linux/defaults/apps.json` only |
| `schema/*.json` | `/usr/share/uperf-linux/schema/` |

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
