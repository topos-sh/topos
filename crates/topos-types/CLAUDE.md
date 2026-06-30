# `topos-types` — the boundary DTOs (wire + persisted)

The serde structs/enums for the boundary: the `--json` envelope, every per-verb result payload
([`results`]), the frozen terminal-outcome enum, the closed `SignatureAlg`, the signed-`current`-record
envelope, the error taxonomy, the HTTP wire request/response DTOs ([`requests`]: the write bodies + the
version metadata + the **enrollment** device-flow/passcode/redeem/admin-claim DTOs + the **governance**
invite/roster/revoke bodies), the unauthenticated invite-**bootstrap** payload ([`bootstrap`]: the
pre-enrollment TOFU read — workspace + the plane signing root to pin; no bytes, no role), and the on-disk
client documents ([`persisted`]: sync / lock / map / op / conflict — the last records an unresolved author
merge: the publish-block fact + the recovery journal). These are **deserialization shapes** — the app libs
parse them into `topos-core`'s validated domain types at the HTTP/CLI boundary (parse-don't-validate, so the
kernel never holds an invalid deserialized state). The wire request/response DTOs additionally derive
`utoipa::ToSchema` (they ride in the plane's OpenAPI, assembled where the routes live), independent of the
`schemars` JSON-Schema output.

Per-verb `data` shapes: `pull`/`list`/`diff` are spec-PINNED; the other nine are marked **INFERRED**
(additive-only, will tighten as each verb is built). `WireError.code` is an **open** string vocabulary by
design — the spec deliberately does not freeze a closed code set.

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

Dependencies: `serde`, `serde_json`, `schemars` (JSON-Schema 2020-12). (No `uuid` — `op_id` is a wire
`String` with `format: uuid`; the `uuid` crate is the client's, where ids are minted.)
