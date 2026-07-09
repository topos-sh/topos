//! Per-verb `--json` `data` payloads — the agent's primary signal.
//!
//! The envelope keeps a generic `data: Value` (one stable wrapper); a consumer reads `command` and
//! deserializes `data` into the matching type here. Each type gets a committed JSON-Schema.
//!
//! **Pinned vs inferred.** Only `pull`, `list`, and `diff` have their `data` fields named by the
//! spec — those are authoritative. The other nine (`add` `follow` `unfollow` `log` `publish`
//! `revert` `review` `invite` + `publish --propose`) are marked **INFERRED**: derived from the
//! documented mechanics, additive-only, and liable to tighten as each verb is built. The envelope,
//! receipt, error, outcome, and action-code shapes are all fully pinned (see the crate root).

use crate::Generation;
use serde::{Deserialize, Serialize};

// =================================================================================================
// PINNED — `pull` (the four-state currency machine, per skill).
// =================================================================================================

/// `pull` result — per-skill currency state plus the reviewer-queue count. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PullData {
    pub skills: Vec<PullSkill>,
    /// Open proposals on your followed skills (v0 is single-approver — any rostered member may review, so
    /// the count is all open-non-stale proposals across what you follow, not a reviewer-assignment queue).
    pub proposals_awaiting: u32,
}

/// One followed skill's pull state. `observed`/`applied`/`action`/`offer`/`conflict` are PINNED by
/// name; the *value enums* (`PullAction`) and the `offer`/`conflict` field shapes are INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PullSkill {
    pub skill: String,
    /// The workspace this followed skill lives in, or `None` for a targeted go-back / local-only pull that
    /// has no follow entry. A pulled skill is normally followed (so `Some`), but `pull <skill>@<hash>` on an
    /// unfollowed copy has none — `Option` keeps that honest and stays symmetric with [`SkillEntry`]. Names
    /// the workspace so a session-start sweep does not show two same-named skills indistinguishably.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Highest authenticated `(epoch,seq)` seen — the anti-rollback floor.
    pub observed: Generation,
    /// Highest generation actually materialized to disk.
    pub applied: Generation,
    pub action: PullAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<Offer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Conflict>,
    /// Present for the author-merge outcomes (`merged` / `conflicted`) — the resolution disclosure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeReport>,
}

/// What `pull` did / offers for a skill. **INFERRED value set** — the four-state machine pins the
/// semantics (CURRENT / BEHIND / DRAFT / DIVERGED) but not these exact tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PullAction {
    /// State ① — already current; nothing to do.
    UpToDate,
    /// State ② clean — auto fast-forwarded to the new bytes.
    FastForwarded,
    /// State ② confirm-each / first-receive — a one-tap offer is waiting.
    Offered,
    /// State ④ — a local draft conflicts with a newer remote (surfaced, not yet resolved — e.g. a
    /// confirm-each follower's bare sweep, which offers the merge rather than running it).
    Diverged,
    /// State ④ resolved cleanly — a three-way merge (or the escape) landed a draft-on-current.
    Merged,
    /// State ④ resolved with conflicts — a complete conflict tree was materialized and publish is blocked
    /// until the author resolves (or escapes).
    Conflicted,
    /// A transient local hold (e.g. a local go-back is pinned).
    Held,
    /// A reused/replayed generation tuple was seen — a loud integrity alarm.
    Alarm,
}

/// The re-disclosed bytes a `pull` offers (confirm-each / first-receive). **INFERRED fields** — the
/// spec pins that the offer re-discloses + re-binds the digest, not its exact shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct Offer {
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
}

/// The DIVERGED panel (local draft vs newer remote). **INFERRED fields.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct Conflict {
    /// The remote version the draft diverged from.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub remote_version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub local_version_id: Option<String>,
}

/// The author-merge disclosure (the `merged` / `conflicted` outcomes of a diverged draft). **INFERRED
/// fields** — the spec pins the merge semantics (deterministic, author-only, conflict-blocks-publish),
/// not this exact shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct MergeReport {
    /// The three-way base (the draft's fork point).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    /// `current` (theirs) the draft was merged onto.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub theirs_version_id: String,
    /// The forward 1-parent commit carrying the merged (or conflict-marked) tree.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub result_version_id: String,
    /// The merged/conflict tree's `bundle_digest`.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub result_digest: String,
    /// Whether the merge was clean (`true` → draft-on-current, publishable) or blocked (`false`).
    pub clean: bool,
    /// The conflicting paths when `clean` is `false` — the agent's resolution checklist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictPathReport>,
    /// For the escape / no-base 2-way fallback: a unified diff of what the chosen side drops vs the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_diff: Option<String>,
}

/// One conflicting path in a [`MergeReport`]. **INFERRED** — `kind` reuses the persisted vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ConflictPathReport {
    pub path: String,
    pub kind: crate::persisted::ConflictPathKind,
}

// =================================================================================================
// PINNED — `list` (the four buckets + per-skill identity).
// =================================================================================================

/// `list` result — the four inventory buckets. **PINNED** (bucket set + per-entry identity).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ListData {
    pub followed: Vec<SkillEntry>,
    pub published_by_you: Vec<SkillEntry>,
    pub tracked: Vec<SkillEntry>,
    /// A real skill in a harness dir that topos doesn't manage yet (discovered, not adopted) — it has
    /// no topos `version_id`/`bundle_digest` yet, so it carries only what is knowable on disk.
    pub untracked: Vec<UntrackedEntry>,
    /// Only present under `--remote`: skills available in the followed workspaces' catalogs, annotated with
    /// this install's local follow-state — the "what could I follow next" surface. **INFERRED** (additive;
    /// empty/omitted unless `--remote`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_available: Vec<RemoteSkillEntry>,
    /// Only present under `--footprint`: topos-owned paths outside skill dirs. **INFERRED shape.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint: Option<Vec<String>>,
}

/// A skill available in a followed workspace's catalog (`list --remote`), annotated with this install's
/// follow-state so the agent can see what to `follow` (or `pull`) next. Metadata only — the catalog grants
/// no bytes. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoteSkillEntry {
    /// The skill id — the workspace-scoped handle a `follow` targets.
    pub skill_id: String,
    /// The workspace the skill lives in (its catalog scope).
    pub workspace_id: String,
    /// The advisory display name, when the plane discloses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The catalog `current` version id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The catalog `current` consent hash (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// Open, non-stale proposal count on the skill.
    pub open_proposals: u64,
    /// This install's follow-state for the skill.
    pub state: RemoteFollowState,
}

/// The local follow-state annotation on a `--remote` catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum RemoteFollowState {
    /// In the workspace catalog, not followed by this install — `follow` to adopt.
    Available,
    /// Followed, and the local version matches the catalog `current`.
    Following,
    /// Followed, but the catalog `current` is newer than the local version — `pull` to advance.
    FollowingBehind,
}

/// A discovered-but-unadopted skill — known only by where it lives, not by any topos version yet.
/// Discovery spans every harness in the baked registry, so `harness` is an open **slug** string (not the
/// closed [`crate::HarnessId`] — topos discovers far more harnesses than it has full adapters for).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct UntrackedEntry {
    pub name: String,
    /// The harness dir it was found in.
    pub path: String,
    /// The harness's registry slug (e.g. `claude-code`, `cursor`, `windsurf`).
    pub harness: String,
    /// The harness's human-readable name (e.g. `Claude Code`, `Cursor`).
    pub harness_name: String,
    /// True iff topos has a full adapter for this harness (so `add` can arm live currency). False = the
    /// skill is still adoptable (`topos add` tracks + shares its bytes), but auto-currency lands later.
    pub adapter_supported: bool,
    /// Where the skill dir was found: `user` (a global harness home) or `project` (the current repo).
    pub scope: String,
}

/// A skill row. `<skill>@<version_id>` identity + `draft` are PINNED; the other field names INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SkillEntry {
    pub skill: String,
    /// The workspace this skill is followed in (its signed-pointer scope), or `None` for a purely local,
    /// never-followed `add`'d skill. Provenance so two same-named skills from different workspaces are
    /// distinguishable; `--json` carries it flat, the TTY groups by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The approvable `@` token (the commit SHA-256).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The byte-exact consent hash, shown alongside as evidence.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// Local edits ahead of the version this entry is on.
    pub draft: bool,
    /// Open proposals, each as `<skill>@<version_id>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_proposals: Vec<String>,
}

// =================================================================================================
// PINNED — `diff` (source + version_id; body is a plain unified diff).
// =================================================================================================

/// `diff` result. `source` + `version_id` (+ the emitted digest) are **PINNED**; the diff *body*
/// representation ("a plain unified diff") is the only INFERRED part.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct DiffData {
    pub source: DiffSource,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// A plain unified diff.
    pub diff: String,
}

/// Where the compared bytes came from: the local sidecar, or a plane-held proposal. **PINNED.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DiffSource {
    Local,
    Plane,
}

// =================================================================================================
// INFERRED — the nine verbs whose `data` field list the spec does not enumerate. Shapes derived
// from the documented mechanics; additive-only; will tighten as each verb is built.
// =================================================================================================

/// Where an adopted skill was imported FROM, when `add` fetched it from a remote source (a GitHub repo).
/// All fields are public provenance — never a secret — and travel with the adopted skill so the agent (and
/// a later re-sync) can see the upstream it came from. `None` on `AddData` for a locally-adopted skill.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SkillOrigin {
    /// The `<host>/<owner>/<repo>` the skill was imported from (e.g. `github.com/vercel-labs/agent-skills`).
    pub source: String,
    /// The branch / tag / commit requested (`#<ref>` or a `/tree/<ref>/…` URL), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The resolved commit the bytes came from (best-effort — parsed from the fetched archive), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The skill's path within the repo (a monorepo subdir), if it was not the repo root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// A LICENSE file found at the skill root or repo root, recorded as provenance (never injected into the
    /// bundle — the adopted bytes stay byte-exact to the repo). `None` if the source carried no license.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

/// `add` (local, offline — no plane op, `receipt: null`). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AddData {
    pub skill_id: String,
    pub name: String,
    /// The base commit the local sidecar starts from.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    pub tracked: bool,
    /// The harness topos recognized the adopted directory as (e.g. Claude Code), or `None` for a plain
    /// directory tracked in place. Disclosed so the agent can see whether currency was armed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<crate::HarnessId>,
    /// The harness's registry slug the adopted dir was attributed to (e.g. `cursor`), even for a harness
    /// topos has no full adapter for (then `harness` is `None`). Provenance/disclosure only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_slug: Option<String>,
    /// The currency-trigger outcome, present when adopting into a recognized harness attempted a
    /// session-start trigger install — the honest disclosure of the (only) write `add` makes outside
    /// `~/.topos/`. `None` for a plain directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<crate::TriggerReport>,
    /// Where the skill was imported FROM, when `add` fetched it from a remote source. `None` for a
    /// locally-adopted skill (a path or a discovered name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<SkillOrigin>,
}

/// `follow` (enrollment + first-receive). Each offered skill is a TOFU offer, never auto-landed.
/// **INFERRED** (additive-only). The enrollment-disclosure fields (`deployment_mode` /
/// `workspace_display_name` / `verified_domain*`) and the two-call `pending` arm were added as the
/// enrollment surface landed; all are optional, so an old consumer ignores them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct FollowData {
    pub workspace_id: String,
    pub enrolled: bool,
    /// First-receive offers — empty when the link is membership-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<FollowOffer>,
    /// The workspace's deployment posture (disclosed from the bootstrap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_mode: Option<crate::bootstrap::DeploymentMode>,
    /// The workspace display name (disclosed from the bootstrap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_display_name: Option<String>,
    /// The workspace's org-domain claim, if any (disclosed from the bootstrap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_domain: Option<String>,
    /// The workspace's domain-verification state (disclosed from the bootstrap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_domain_status: Option<crate::bootstrap::VerifiedDomainStatus>,
    /// The plane API base URL this machine enrolls against (disclosed from the bootstrap — a share link
    /// may ride another host, e.g. a hosted team's web origin; this is where the device actually dials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane_base_url: Option<String>,
    /// Present when `follow` returned a pending device-authorization that needs a human verification step
    /// (the client's two-call enrollment surface — visit the URL, then re-run `follow`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<EnrollmentPending>,
    /// The currency-trigger outcome, present when completing the enrollment armed the session-start hook
    /// (a pure follower never runs `add`, so enrollment is where their currency gets armed — best-effort:
    /// a degraded config edit is disclosed here, never a rolled-back enrollment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<crate::TriggerReport>,
}

/// A pending device-authorization a `follow` surfaced — the human visits `verification_uri_complete` (which
/// embeds the `user_code`), then the client re-polls. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct EnrollmentPending {
    /// The verification URL with the `user_code` embedded — the human opens it to approve the session.
    pub verification_uri_complete: String,
    /// The short code shown for cross-checking on the verification page.
    pub user_code: String,
    /// 16-hex fingerprint of this device's public key; a human cross-checks it against the verification page.
    pub device_fingerprint: String,
    /// The session expiry as an RFC-3339 string, if it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// A single skill offered at `follow` — disclosed, awaiting a direct human yes (TOFU). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct FollowOffer {
    pub skill_id: String,
    pub name: String,
    pub offer: Offer,
}

/// `unfollow` (local — stop following `current`, keep the bytes as a frozen copy). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct UnfollowData {
    pub skill_id: String,
    pub following: bool,
    /// The local bytes are retained, not deleted.
    pub bytes_kept: bool,
}

/// `log` — local action events (and, with `--team`, partial plane records). The individual event
/// fields are **not pinned by the spec**, so events stay open JSON. **INFERRED.**
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct LogData {
    /// Local action-event envelopes from `log.jsonl` (field set intentionally open).
    pub events: Vec<serde_json::Value>,
    /// Plane-side records under `--team` (op-receipts ⋈ approvals ⋈ lineage) — honestly partial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<Vec<serde_json::Value>>,
}

/// `publish` (a direct publish that moves `current`). On a GENESIS (first) publish the client also folds in
/// a shareable `/i/` link pre-offering the skill — **best-effort + owner-gated** (minting it signs a
/// governance op the plane denies for a non-owner), so `invite_link` is `Some` only on a genesis publish by
/// an owner, and `None` otherwise. Under `review-required` a direct publish instead returns
/// `APPROVAL_REQUIRED` (with the `publish --propose` next-action) and carries no `data`. **INFERRED.**
///
/// An UN-ENROLLED direct publish on the hosted plane starts a workspace STANDUP instead of failing: the
/// envelope is still `ok = true`, but `data` carries the [`PublishPending`] block (sign in to approve) and
/// no version — `version_id` / `current_generation` are `None` at pending because nothing was published yet
/// (only the computed digest of the bytes being published can be honestly filled). Re-invoking the SAME
/// publish command (the `ENROLL_RESUME` next-action) resumes: once the sign-in is approved, the same command
/// completes enrollment AND the publish in one invocation, and the receipt carries the [`StandupReceipt`]
/// disclosure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PublishData {
    pub skill_id: String,
    /// The new commit — `None` while the publish is PENDING a workspace-standup sign-in (nothing was
    /// published yet; the version id is only knowable once the bytes actually ship).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: Option<String>,
    /// The byte-exact digest of the shipped (or, at pending, the scanned) bytes — always present: it is
    /// computed over the draft before any network call, and an optional `<skill>@<digest>` pin gates it.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The pointer's new generation after the move — `None` while the publish is PENDING a standup sign-in
    /// (no pointer moved yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<Generation>,
    /// A shareable `/i/<token>` invite pre-offering this skill — present ONLY on a genesis publish where the
    /// publisher could mint it (an owner); `None` on an ordinary publish or a denied/failed mint. The pointer
    /// move is the real outcome; the link is a convenience (the `invite` verb mints one independently).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_link: Option<String>,
    /// Present when this publish is WAITING on the workspace-standup sign-in (the un-enrolled first publish
    /// on a hosted plane): a human opens `verification_uri_complete` and approves; the agent then re-runs
    /// the SAME publish command (the `ENROLL_RESUME` next-action carries the argv).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PublishPending>,
    /// Present ONLY when THIS invocation completed a workspace standup before publishing — the disclosure
    /// that makes a hijacked approval visible ("workspace X — owner Y"): the human who approved the sign-in
    /// is the seated owner, so a name you don't recognize means someone else owns your workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standup: Option<StandupReceipt>,
    /// Present ONLY when THIS `publish` invocation ADDED the skill to topos first (the auto-add
    /// convenience: `publish <name>|<name>@<harness>|<dir>` adopts an untracked LOCAL skill, then ships it
    /// in one command). Discloses the one local `add` the publish folded in; `None` when the skill was
    /// already tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<AddedNote>,
}

/// The disclosure a `publish` attaches when it ADDED the skill to topos before shipping — the auto-add
/// convenience (`publish` accepts an untracked local source and adopts it first). Public disclosure only
/// (the same facts an explicit `topos add` would return); never a secret. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct AddedNote {
    /// The name the skill was adopted under (what `list` / `diff` / `publish` now resolve it by).
    pub name: String,
    /// The harness registry slug the adopted directory was attributed to (e.g. `claude-code`), or `None`
    /// for a plain directory adopted in place under no known harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_slug: Option<String>,
}

/// The workspace-standup sign-in a pending `publish` waits on. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PublishPending {
    /// Always `signin_required` — the one pending state this block discloses.
    pub status: PublishPendingStatus,
    /// The sign-in URL with the code already embedded — the ONE link a human opens to approve (served by
    /// the plane; the client uses it verbatim).
    pub verification_uri_complete: String,
    /// The code embedded in the URL, shown for cross-checking on the sign-in page.
    pub user_code: String,
    /// 16-hex fingerprint of this device's public key; a human cross-checks it against the sign-in page.
    pub device_fingerprint: String,
    /// The sign-in session's expiry as an RFC-3339 string, if it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// A pending publish's status — a CLOSED single-value set (snake_case): the standup sign-in is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum PublishPendingStatus {
    /// A human must sign in at `verification_uri_complete` and approve the workspace creation.
    SigninRequired,
}

/// The standup disclosure a workspace-creating publish carries: which workspace was stood up and who owns
/// it. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct StandupReceipt {
    /// The stood-up workspace's display name (chosen at the sign-in approval).
    pub workspace_display_name: String,
    /// The seated owner principal (the approver's confirmed email, or a device-rooted id) — the hijack
    /// tripwire: a principal you don't recognize means someone else approved (and owns) this workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_principal: Option<String>,
}

/// `publish --propose` (opens a PR; uploads a full candidate **without moving `current`**). Returns
/// `NEEDS_REVIEW`. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ProposeData {
    /// `<skill>@<version_id>` of the candidate.
    pub proposal: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Present ONLY when THIS `publish --propose` invocation ADDED the skill to topos first (the auto-add
    /// convenience — a proposal of an untracked local source adopts it before opening the PR). Discloses the
    /// one local `add` the propose folded in; `None` when the skill was already tracked. **INFERRED.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<AddedNote>,
}

/// `revert` (a **forward** git-revert restoring older bytes as a new, higher-`seq` version — never a
/// pointer rollback, never a delete). `--to` names the GOOD version. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RevertData {
    pub skill_id: String,
    /// The good version named by `--to` (the bytes being restored).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub reverted_to: String,
    /// The new forward-revert commit that carries those bytes.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub new_version_id: String,
    pub current_generation: Generation,
}

/// `review` (`--approve` / `--reject` a proposal). Approve is a compare-and-set on the base; a stale
/// base returns `CONFLICT`. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ReviewData {
    /// `<skill>@<version_id>` of the reviewed proposal.
    pub proposal: String,
    pub decision: ReviewDecision,
    /// The pointer's new generation when an approval moved `current`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<Generation>,
}

/// A review verdict. **INFERRED.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
}

/// `invite` (mint an `/i/` link + optionally seed the roster). A link never carries a role and never
/// enrolls on its own. **INFERRED.** Also the `POST /v1/invites` success `data` shape (the OpenAPI body),
/// hence the `utoipa::ToSchema` derive alongside `schemars`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct InviteData {
    /// `/i/<token>`.
    pub invite_link: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roster_added: Vec<String>,
    /// The skills a redeemer joins + follows (empty = a membership-only door).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_pull_shape_round_trips() {
        let data = PullData {
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                workspace_id: Some("w_acme".to_owned()),
                observed: Generation { epoch: 1, seq: 42 },
                applied: Generation { epoch: 1, seq: 42 },
                action: PullAction::UpToDate,
                offer: None,
                conflict: None,
                merge: None,
            }],
            proposals_awaiting: 0,
        };
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["skills"][0]["action"], "up_to_date");
        assert_eq!(v["skills"][0]["workspace_id"], "w_acme");
        assert_eq!(v["proposals_awaiting"], 0);
        let back: PullData = serde_json::from_value(v).unwrap();
        assert_eq!(back.skills[0].action, PullAction::UpToDate);
    }

    #[test]
    fn publish_data_pending_shape_is_additive_and_omits_the_absent_fields() {
        // The PENDING publish (a workspace standup awaiting sign-in): no version, no generation — only the
        // consent digest and the pending block ride the wire.
        let pending = PublishData {
            skill_id: "topos_t00".to_owned(),
            version_id: None,
            bundle_digest: "c".repeat(64),
            current_generation: None,
            invite_link: None,
            pending: Some(PublishPending {
                status: PublishPendingStatus::SigninRequired,
                verification_uri_complete: "https://topos.sh/verify/CODE".to_owned(),
                user_code: "CODE".to_owned(),
                device_fingerprint: "e4aaf52f5c391ce9".to_owned(),
                expires_at: Some("2026-07-03T00:15:00Z".to_owned()),
            }),
            standup: None,
            added: None,
        };
        let v = serde_json::to_value(&pending).unwrap();
        assert!(v.get("version_id").is_none(), "no version at pending");
        assert!(v.get("current_generation").is_none());
        assert_eq!(v["pending"]["status"], "signin_required");
        assert_eq!(v["pending"]["user_code"], "CODE");
        assert_eq!(v["pending"]["device_fingerprint"], "e4aaf52f5c391ce9");
        // A COMPLETED standup publish: version + generation present, plus the owner disclosure.
        let done = PublishData {
            skill_id: "topos_t00".to_owned(),
            version_id: Some("a".repeat(64)),
            bundle_digest: "c".repeat(64),
            current_generation: Some(Generation { epoch: 1, seq: 1 }),
            invite_link: None,
            pending: None,
            standup: Some(StandupReceipt {
                workspace_display_name: "robert's workspace".to_owned(),
                owner_principal: Some("robert@example.com".to_owned()),
            }),
            added: None,
        };
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["standup"]["workspace_display_name"], "robert's workspace");
        assert_eq!(v["standup"]["owner_principal"], "robert@example.com");
        assert!(v.get("pending").is_none());
        // An OLD-shape ordinary publish (no pending/standup fields) still deserializes (additive-compat).
        let old: PublishData = serde_json::from_value(serde_json::json!({
            "skill_id": "topos_t00",
            "version_id": "a".repeat(64),
            "bundle_digest": "c".repeat(64),
            "current_generation": { "epoch": 1, "seq": 1 },
        }))
        .unwrap();
        assert!(old.pending.is_none() && old.standup.is_none());
    }

    #[test]
    fn diff_source_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&DiffSource::Plane).unwrap(),
            "\"plane\""
        );
        assert_eq!(
            serde_json::to_string(&DiffSource::Local).unwrap(),
            "\"local\""
        );
    }
}
