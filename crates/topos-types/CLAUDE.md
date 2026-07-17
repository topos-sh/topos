# `topos-types` — the boundary DTOs (wire + persisted)

The serde structs/enums for the boundary: the `--json` envelope, every per-verb result payload
([`results`]), the frozen terminal-outcome enum, the unsigned `WireCurrentRecord` pointer body, the
error taxonomy, the HTTP wire request/response DTOs ([`requests`]), and the on-disk client documents
([`persisted`]: sync / lock / map / op / conflict). `map.json` carries its OWN schema ceiling
(`PLACEMENT_MAP_SCHEMA_VERSION` = 2 — the per-placement `placement_state` shape; a v1 single-placement
document upgrades losslessly in memory on read); every other persisted doc dispatches on
`PERSISTED_SCHEMA_VERSION`.

**Generation is a bare `u64` on the wire** — the pointer's single monotonically advancing CAS
counter (the old `(epoch, seq)` pair is gone). `expected_generation` / `current_generation` /
`generation` fields are plain numbers everywhere; `ETag = "<generation>"`.

The [`requests`] module carries the PUBLIC device lane the product app serves:

- the **gh-style device-auth flow** — `DeviceAuthStartRequest { requested_name, workspace }` →
  `DeviceAuthStartResponse { device_code, user_code, verification_uri, verification_uri_complete,
  expires_in_secs, interval_secs }`, and `DeviceAuthPollRequest { device_code }` →
  `DeviceAuthPollResponse { status: pending|denied|expired|granted, credential?, device_id?,
  workspace? {workspace_id, name, display_name} }`. Design fact: on approval the `device_code` is
  PROMOTED to the device's ONE bearer credential server-side; the poll's `credential` field carries
  it back, so the CLI stores one secret from one field.
- the write bodies (`PublishRequest` / `ProposeRequest` / `RevertRequest` / `ReviewRequest` +
  `WireCandidate`/`WireFile`), the read bodies (`WireCurrentRecord`, `WireVersionMeta`,
  `WireProposalList`, `WireSkillIndex`, `WireDelivery` + `WireAppliedReport`, the describe reads
  `WireMe`/`WireChannelIndex`/`WireProposalIndex`/`WireSkillLog`/`WireReach`), the row-op bodies
  (`ProtectionSetRequest`, `NoticeAckRequest`, `InvitationRequest`/`InvitationData`), the constant
  `WireProtocolCard`, and `DeviceRevokeRequest` (the CLI logout wire).

The old enrollment surface (device/token/passcode/redeem/claim/login/roster DTOs and the bootstrap
module) is DELETED — enrollment and every identity ceremony live in the product app now; the vault
never sees them.

These are **deserialization shapes** — the app libs parse them into validated domain types at the
HTTP/CLI boundary (parse-don't-validate). The wire request/response DTOs additionally derive
`utoipa::ToSchema` (they ride in the committed OpenAPI). **Both schema-derive families are gated
behind the default-off `contract-derives` feature** — only the two contract producers (`xtask` for
gen-schema, `topos-plane` for its OpenAPI) enable it; every other consumer compiles pure-serde DTOs.

Per-verb `data` shapes: `pull`/`list`/`diff` are spec-PINNED; the rest are marked **INFERRED**
(additive-only). `WireError.code` is an **open** string vocabulary by design.

## Frozen names (do not rename)

- `version_id` — the commit SHA-256; the user-facing `<skill>@<version_id>`.
- `bundle_digest` — the byte-exact consent hash over the bundle's files.
- The op/receipt identity carries `skill_id` + `version_id` + `bundle_digest`. These are **distinct
  values** — never call both "digest."

## No logic here

This is the shared leaf that every app lib, every fixture, and the contract generator link.

**The `///` doc comments on these types become the JSON-Schema field descriptions** (via `schemars`),
and those schemas are generated + committed under `contracts/schemas/`. Keep the descriptions
accurate; after changing a type or its docs, regenerate (`cargo run -p xtask -- gen-schema`) and
review the diff.

Dependencies: `serde`, `serde_json`; `schemars` (JSON-Schema 2020-12) + `utoipa` are OPTIONAL,
behind `contract-derives`.
