# `topos-types` — the boundary DTOs (wire + persisted)

The serde structs/enums for the boundary: the `--json` envelope, every per-verb result payload
([`results`]), the frozen terminal-outcome enum, the unsigned `WireCurrentRecord` pointer body,
the error taxonomy, the HTTP wire request/response DTOs ([`requests`]: the write bodies + the
version metadata + the **enrollment** device-flow/passcode/redeem/admin-claim DTOs — intent-aware for
the workspace-standup flow; **reshaped by the workspace-credential clean break** (deliberate:
pre-1.0, no compatibility window): the write + governance bodies DROPPED `device_key_id` (the acting
device rides the `Authorization: Bearer` workspace credential, never a body field), `RedeemResponse`
dropped the per-skill `read_creds` (and `RedeemedSkillCred` with it) for the ONE plaintext
`credential`; the enrollment surface then went **token-less** with the adopted verbs:
`DeviceAuthorizeRequest` dropped `invite_token` for an optional `workspace` ADDRESS (an unknown name is
never disclosed), and the closed `SessionIntent` is now `enroll`/`standup`/`login`; the `/i/`-link invite
DTOs (`InviteRequest`/`InviteSkill`/`InviteData`) were DELETED — joining is `follow <address>`, and inviting
is a roster write (`InvitationRequest`/`InvitationData`). Earlier standup-era additions stay:
`DeviceAuthorizeResponse`'s optional `verification_uri_complete` + the standup `plane` block,
`DeviceTokenResponse`'s optional granted `workspace {workspace_id, display_name, address}`,
`RedeemResponse`'s optional seated `principal`, `VerificationContextResponse`'s optional `intent`; plus the
**governance** roster/revoke bodies and the **adopted member-lane surface**: `POST /v1/login`
(`LoginRedeemRequest` → `LoginData`/`LoginMembership`), the constant `WireProtocolCard`, the member-scoped
describe reads (`WireMe`, `WireChannelIndex`, `WireProposalIndex`, `WireSkillLog`, `WireReach`), the row-op
bodies (`ProtectionSetRequest`, `NoticeAckRequest`), and `WireDelivery`'s new `staleness_window_ms`), the
unauthenticated **bootstrap** payload ([`bootstrap`]: the
pre-enrollment read — workspace + the plane API base to dial; no trust root, no bytes, no role), and the on-disk
client documents ([`persisted`]: sync / lock / map / op / conflict — the last records an unresolved author
merge: the publish-block fact + the recovery journal). These are **deserialization shapes** — the app libs
parse them into `topos-core`'s validated domain types at the HTTP/CLI boundary (parse-don't-validate, so the
kernel never holds an invalid deserialized state). The wire request/response DTOs additionally derive
`utoipa::ToSchema` (they ride in the plane's OpenAPI, assembled where the routes live), independent of the
`schemars` JSON-Schema output. **Both schema-derive families are gated behind the default-off
`contract-derives` feature** — only the two contract producers (`xtask` for gen-schema, `topos-plane` for
its OpenAPI) enable it; every other consumer, above all the client, compiles pure-serde DTOs (check-arch
asserts the `topos` tree resolves neither `schemars` nor `utoipa`).

Per-verb `data` shapes: `pull`/`list`/`diff` are spec-PINNED; the rest are marked **INFERRED**
(additive-only, tightening as each verb is built) — including the adopted two-phase describe/apply payloads
(`RemoveData`, `ChannelData`, `ProtectData`, `ReviewIndexData`/`ReviewDescribeData`,
`InviteReadData`/`InviteDescribeData`, `ResetData`, `PublishDescribeData`, `KeepAsYoursData`), each carrying
an `applied` flag (a bare describe is `false`, `--yes` is `true`). `WireError.code` is an **open** string vocabulary by
design — the spec deliberately does not freeze a closed code set. `PublishData` widened for the
workspace-standup client: `version_id`/`current_generation` became `Option` (unknowable while a publish is
PENDING the standup sign-in) and it gained the optional `pending` (`PublishPending`, status
`signin_required`) + `standup` (`StandupReceipt` — the "workspace X — owner Y" hijack-visibility
disclosure) blocks.

## Frozen names (do not rename)

- `version_id` — the commit SHA-256; the user-facing `<skill>@<version_id>`.
- `bundle_digest` — the byte-exact consent hash over the bundle's files.
- The op/receipt identity carries `skill_id` + `version_id` + `bundle_digest`. These are **distinct
  values** — never call both "digest."

## No logic here

This is the shared leaf that every app lib, every fixture, and the contract generator link.

**The `///` doc comments on these types become the JSON-Schema field descriptions** (via `schemars`), and
those schemas are generated + committed under `contracts/schemas/`. Keep the descriptions accurate; after
changing a type or its docs, regenerate (`cargo run -p xtask -- gen-schema`) and review the diff.

Dependencies: `serde`, `serde_json`; `schemars` (JSON-Schema 2020-12) + `utoipa` are OPTIONAL, behind
`contract-derives`. (No `uuid` — `op_id` is a wire `String` with `format: uuid`; the `uuid` crate is the
client's, where ids are minted.)
