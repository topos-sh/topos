# `topos-types` — the wire DTOs

The serde structs/enums for the boundary: the `--json` envelope, every per-verb result shape, the frozen
terminal-outcome code enum, the signed-`current`-record envelope, and the error taxonomy. These are
**deserialization shapes** — the app libs parse them into `topos-core`'s validated domain types at the
HTTP/CLI boundary (parse-don't-validate, so the kernel never holds an invalid deserialized state).

## Frozen names (do not rename)

- `version_id` — the commit SHA-256; the user-facing `<skill>@<version_id>`.
- `bundle_digest` — the byte-exact consent hash over the bundle's files.
- The signing/approval preimage binds `skill_id` + `version_id` + `bundle_digest`. These are **distinct
  values** — never call both "digest."

## No logic here

This is the shared leaf that every app lib, every fixture, and the contract generator link.

**The `///` doc comments on these types become the JSON-Schema field descriptions** (via `schemars`), and
those schemas are generated + committed under `contracts/schemas/`. Keep the descriptions accurate; after
changing a type or its docs, regenerate (`cargo run -p xtask -- gen-schema`) and review the diff.

Dependencies: `serde`, `serde_json`, `schemars` (JSON-Schema), `utoipa` (the plane OpenAPI), `uuid`.
