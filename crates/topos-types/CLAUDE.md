# `topos-types` — the boundary DTOs (wire + persisted)

The serde structs/enums for the boundary: the `--json` envelope, every per-verb result payload
(`results`), the frozen terminal-outcome enum, the unsigned `WireCurrentRecord` pointer body, the
error taxonomy, the HTTP wire request/response DTOs (`requests`), and the on-disk client documents
(`persisted`: sync / lock / map / op / conflict — each dispatching on its schema version,
fail-closed).

**No logic here.** These are deserialization shapes — the app libs parse them into validated
domain types at the boundary (parse-don't-validate). This is the shared leaf every app lib, every
fixture, and the contract generator link.

- The `requests` module carries the PUBLIC session lane the product app serves: the
  RFC-8628-shaped login flow (`DeviceAuthStart*`/`DeviceAuthPoll*` — on approval the flow code is
  promoted server-side to the SESSION's workspace-scoped bearer credential; the poll's
  `credential` field carries it back), the write bodies (`PublishRequest` / `ProposeRequest` /
  `RevertRequest` / `ReviewRequest` + `WireCandidate`/`WireFile`), the read bodies
  (`WireCurrentRecord`, `WireVersionMeta`, `WireProposalList`, `WireSkillIndex`, `WireDelivery` +
  `WireAppliedReport`, `WireMe`/`WireChannelIndex`/`WireProposalIndex`/`WireSkillLog`/
  `WireReach`), the row-op bodies (`ProtectionSetRequest`, `NoticeAckRequest`,
  `InvitationRequest`), and the constant `WireProtocolCard`. `session_status`
  ("active"/"pending") rides `WireMe`, `WireDelivery`, and the granted poll;
  `DELETE /v1/session` is the self-end.
- **Generation is a bare `u64`** — the pointer's single monotonic CAS counter;
  `ETag = "<generation>"`.
- Envelope additions are ADDITIVE-ONLY and omit when absent/false (truncation/pagination markers,
  `MergePreview`, `NextAction`'s optional safety fields + `needs` list — filled by the producer's
  one construction fn, `topos::actions::next_action`). `WireError.code` is an open string
  vocabulary by design.

## Frozen names (do not rename)

- `version_id` — the commit SHA-256; the user-facing `<skill>@<version_id>`.
- `bundle_digest` — the byte-exact consent hash over the bundle's files.
- These are **distinct values** — never call both "digest."

**The `///` doc comments become the JSON-Schema field descriptions** (via `schemars`), generated +
committed under `contracts/schemas/`. After changing a type or its docs: `cargo xtask gen-schema`,
review the diff. Both schema-derive families (`schemars` + `utoipa`) are gated behind the
default-off `contract-derives` feature — only the two contract producers (`xtask`, `topos-plane`)
enable it.

Dependencies: `serde`, `serde_json`; `schemars` + `utoipa` optional behind `contract-derives`.
