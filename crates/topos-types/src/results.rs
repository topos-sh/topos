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

use serde::{Deserialize, Serialize};

// =================================================================================================
// PINNED — `pull` (the four-state currency machine, per skill).
// =================================================================================================

/// `pull` result — per-skill currency state plus the reviewer-queue count. **PINNED** (the original
/// fields); `notices` + `sync` are ADDITIVE (the delivery-driven sweep's feed + freshness — absent
/// on a targeted pull and from an older producer).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PullData {
    pub skills: Vec<PullSkill>,
    /// Open proposals on your followed skills (v0 is single-approver — any rostered member may review, so
    /// the count is all open-non-stale proposals across what you follow, not a reviewer-assignment queue).
    pub proposals_awaiting: u32,
    /// The unacked, person-scoped notices the delivery answered (verdicts first) — narrated by an
    /// interactive `update`, which then acks exactly these ids; the quiet hook fetches without
    /// acking. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<crate::requests::WireNotice>,
    /// Per-workspace delivery/report freshness after the sweep — the staleness clock the hook
    /// warning and `auth status` read. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync: Vec<WorkspaceSyncReport>,
}

/// One workspace's sync freshness in a [`PullData`]. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct WorkspaceSyncReport {
    pub workspace_id: String,
    /// When the last successful delivery answered (epoch millis; absent if never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_at: Option<i64>,
    /// When the last successful applied-state report landed (epoch millis; absent if never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report_at: Option<i64>,
    /// The workspace's staleness window (ms).
    pub staleness_window_ms: u64,
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
    /// The generation the plane most recently served — the sync target.
    pub observed: u64,
    /// Highest generation actually materialized to disk.
    pub applied: u64,
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
    /// UPSTREAM withdrew the skill (archived, or its last delivering channel dropped it): the agent
    /// dirs were cleaned; the sidecar keeps the bytes + any draft delta ("keep it as yours" is a
    /// narration away).
    Withdrawn,
    /// The PERSON detached the skill (an unfollow, or a channel leave that lapsed it) on some
    /// device: this copy froze in place — bytes untouched, delivery ended.
    Detached,
    /// THIS DEVICE excludes the skill ("not on this device"): the agent dirs are clear here, the
    /// person keeps receiving it everywhere else, and following it here lifts the exclusion.
    Excluded,
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
    /// The catalog's bundle kind (`"skill"` for everything today) — display metadata, never branched
    /// on. Additive: an older plane omits it.
    #[serde(default = "default_bundle_kind")]
    pub kind: String,
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

/// The wire fallback for a plane predating the catalog `kind` (everything it serves is a skill).
fn default_bundle_kind() -> String {
    "skill".to_owned()
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
    /// The workspace this skill is followed in (its pointer scope), or `None` for a purely local,
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
    /// Where the bytes come from: the followed workspace's address name, an imported skill's origin
    /// host, or `local` for a purely local `add`. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The currency posture of the local copy: `current` / `behind` / `draft` / `detached`.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SkillStatus>,
    /// Why a `detached` row is no longer live: `unfollowed` / `excluded-here` / `removed-upstream` /
    /// `signed-out`. Absent when the row is live. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<DetachCause>,
}

/// A tracked skill's currency posture in [`SkillEntry`]. **INFERRED** (additive value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum SkillStatus {
    /// On the followed `current` (or a local skill at its own head), no local edits.
    Current,
    /// The followed workspace serves a newer `current` than this copy holds — `update` to advance.
    Behind,
    /// Local edits ahead of the version this copy is on.
    Draft,
    /// No longer live here (see [`SkillEntry::cause`]) — the bytes are a frozen copy.
    Detached,
}

/// Why a tracked skill is `detached` in [`SkillEntry`]. **INFERRED** (additive value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum DetachCause {
    /// The person unfollowed the skill (delivery stopped on every device).
    Unfollowed,
    /// `topos remove` excluded the skill on THIS device (other devices still receive it).
    ExcludedHere,
    /// Upstream withdrew the skill (archived, or its last delivering channel dropped it).
    RemovedUpstream,
    /// No stored workspace credential — signed out of the workspace this skill lives in.
    SignedOut,
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

/// The `keep it as yours` describe — an `add <name>` that re-forks a RETAINED withdrawn/detached copy
/// into a NEW local skill with no upstream. Bare `add <name>` returns this preview; `--yes` re-adopts the
/// bytes and returns an ordinary [`AddData`]. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct KeepAsYoursData {
    /// The skill name to re-fork.
    pub name: String,
    /// The workspace the retained copy was followed in (its former upstream), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Why the local copy is retained (and no longer delivering here).
    pub reason: KeepReason,
    /// Whether a local draft rides along into the fork (a snapshotted or on-disk edit ahead of the base).
    pub has_draft: bool,
}

/// Why a `keep-as-yours` copy is retained but no longer live. **INFERRED** (additive value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum KeepReason {
    /// Upstream withdrew the skill (archived, or its last delivering channel dropped it) — the agent dirs
    /// were cleaned; the sidecar kept the bytes + any draft.
    WithdrawnUpstream,
    /// The person unfollowed the skill (a detach) — its bytes are frozen in place here.
    Detached,
    /// `topos remove` excluded the skill on this device — the agent dirs were cleaned, the sidecar kept.
    RemovedHere,
}

/// `follow` (enrollment + first-receive). Each offered skill is a TOFU offer, never auto-landed.
/// **INFERRED** (additive-only). All disclosure fields are optional, so an old consumer ignores
/// them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct FollowData {
    pub workspace_id: String,
    pub enrolled: bool,
    /// First-receive offers — empty when the link is membership-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<FollowOffer>,
    /// The workspace display name (disclosed at enrollment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_display_name: Option<String>,
    /// The API base URL this machine enrolled against (disclosed from the protocol card — a share
    /// address may ride another host; this is where the device actually dials).
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
    /// The short human-facing code embedded in `verification_uri_complete` (a cross-check against the
    /// approval page — the human clicks the URL; the code is never typed as a secret).
    pub user_code: String,
    /// The session expiry as an RFC-3339 string, if it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The minimum poll interval, in seconds — a headless agent re-invokes no faster than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
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
    /// When the skill was resolved by a FREED base name (it has since been archived under a new name),
    /// the archived-successor hint: "`<base>` is archived as `<archived>`". **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_successor: Option<String>,
}

/// `publish` (a direct publish that moves `current`). Under a `reviewed` bundle a direct publish is
/// DOWNGRADED to a proposal (see [`ProposeData`]); an un-enrolled publish is refused typed (enroll with
/// `topos follow <workspace-address>` first). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PublishData {
    pub skill_id: String,
    /// The skill's NAME — the handle humans speak and the TTY success line leads with
    /// (`Published <name>@…`); the opaque `skill_id` above stays the machine key.
    pub name: String,
    /// The new commit (the shipped `version_id`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The byte-exact digest of the shipped bytes — computed over the draft before any network call;
    /// an optional `<skill>@<digest>` pin gates it.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The pointer's new generation after the move.
    pub current_generation: u64,
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

/// `revert` (a **forward** git-revert restoring older bytes as a new, higher-generation version —
/// never a pointer rollback, never a delete). `--to` names the GOOD version. **INFERRED.**
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
    pub current_generation: u64,
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
    pub current_generation: Option<u64>,
}

/// A review verdict — `approve` promotes, `reject` carries a reason back, `withdraw` is the author
/// retracting their own open proposal. **INFERRED.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
    Withdraw,
}

// =================================================================================================
// INFERRED — the adopted describe/apply payloads (`remove` / `channel` / `protect` / the review
// inbox+describe / `invite`'s read+describe / `update --reset` / `publish`'s describe). Each rides the
// two-phase envelope: a bare mutating verb returns the payload under `data.describe` (nothing changed),
// `--yes` returns it as `data` with `applied: true`. Additive-only.
// =================================================================================================

/// `remove` — take skills off THIS device. A followed skill becomes a per-device exclusion (other
/// devices keep receiving it); an untracked local copy (or a never-published tracked one) is deleted
/// permanently. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoveData {
    pub items: Vec<RemoveItem>,
    /// `true` on the `--yes` apply, `false` on the describe (nothing changed yet).
    pub applied: bool,
}

/// One skill in a [`RemoveData`]. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoveItem {
    pub name: String,
    /// How the removal behaves for this skill.
    pub kind: RemoveKind,
    /// The workspace the exclusion is recorded in (a followed skill); absent for a local copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The agent directories cleaned (or, on the describe, that would be cleaned).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_dirs: Vec<String>,
    /// Whether the sidecar bytes are kept (a followed exclusion / a tracked-local keeps the bytes as a
    /// frozen copy; an untracked-local delete removes the only copy there is).
    pub bytes_kept: bool,
}

/// How `remove` treats one skill. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum RemoveKind {
    /// A followed skill → a per-device exclusion (the server keeps delivering it to your other devices).
    FollowedExclusion,
    /// An untracked local copy in an agent dir → permanent delete (no other copy exists).
    UntrackedLocal,
    /// A tracked, never-published local skill → permanent delete (the sidecar entry drops too).
    TrackedLocalPermanent,
}

/// `channel add|remove <channel> <skill>...` — place / remove skill references in a channel. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ChannelData {
    pub channel: String,
    pub workspace_id: String,
    /// `add` or `remove`.
    pub action: ChannelAction,
    /// The channel's mode (`open` / `curated`) — the gate a placement passes.
    pub mode: String,
    /// `true` when this `add` would CREATE the channel (it does not exist yet).
    pub creates: bool,
    /// The per-skill placement/removal outcomes.
    pub items: Vec<ChannelItem>,
    /// `true` on the `--yes` apply, `false` on the describe.
    pub applied: bool,
}

/// `add` vs `remove` for a [`ChannelData`]. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChannelAction {
    Add,
    Remove,
}

/// One skill's placement/removal in a [`ChannelData`]. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ChannelItem {
    pub skill: String,
    pub skill_id: String,
    /// `pending` (describe), `placed` / `removed` (applied ok), or `failed` (a mid-flight refusal after
    /// an earlier one landed — reported honestly, per skill).
    pub outcome: ChannelItemOutcome,
    /// The refusal detail when `failed` (e.g. a curated-channel role refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One skill's channel-op outcome. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChannelItemOutcome {
    Pending,
    Placed,
    Removed,
    Failed,
}

/// `protect <target> [<level>]` — set a skill's or channel's protection level. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ProtectData {
    pub target: String,
    /// `skill` or `channel`.
    pub kind: String,
    pub workspace_id: String,
    /// The level being set (`reviewed` / `curated` / `open`).
    pub level: String,
    /// `true` when the level LOOSENS protection (`open`) — the owner-gated direction.
    pub loosening: bool,
    /// The audience this protection governs: the reach (people) for a skill, the member count for a
    /// channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<u64>,
    /// A standing note the describe carries (e.g. "pending proposals survive a loosening").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `true` on the `--yes` apply, `false` on the describe.
    pub applied: bool,
}

/// `review` (bare) — the review inbox/outbox across every enrolled workspace, author-message first.
/// **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ReviewIndexData {
    /// Proposals others opened that you can review (inbox).
    pub inbox: Vec<ReviewIndexEntry>,
    /// Your own open proposals (outbox).
    pub outbox: Vec<ReviewIndexEntry>,
}

/// One proposal in the review inbox/outbox. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ReviewIndexEntry {
    pub workspace_id: String,
    /// The workspace's address name (the inbox groups by it).
    pub workspace_name: String,
    pub skill: String,
    /// The review target handle, `<skill>@<version_id>`.
    pub proposal: String,
    pub proposer: String,
    /// The author's message — rendered FIRST.
    pub message: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    pub created_at: String,
    /// Whether `current` has moved past the proposal's base (a stale proposal needs a re-propose).
    pub stale: bool,
}

/// `review <target>` (bare, no verdict) — the target describe: who, what, base, staleness, and the
/// diff against current. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ReviewDescribeData {
    /// The review target handle, `<skill>@<version_id>`.
    pub proposal: String,
    pub skill: String,
    pub proposer: String,
    pub message: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    pub stale: bool,
    /// The unified diff of the proposal against current (`current..<proposal>`).
    pub diff: String,
}

/// `invite` (bare, no emails) — the no-mutation read of the workspace address + invite policy.
/// **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct InviteReadData {
    /// The workspace address teammates paste to join.
    pub address: String,
    /// The workspace's invite policy (`members` / `owners`) — who may invite.
    pub invite_policy: String,
    /// Always `false` — a bare read sends nothing and changes nothing.
    pub changed: bool,
}

/// `invite <email>...` (bare, no `--yes`) — the describe: who gets seated, the channel pre-placements,
/// and the mail-or-paste note. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct InviteDescribeData {
    pub address: String,
    pub invite_policy: String,
    /// The emails that would be seated (canonical form).
    pub seat: Vec<String>,
    /// The channels each invitee would be pre-placed into.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
}

/// `update --reset <skill>` — discard a local draft back to the followed `current` (or an imported
/// skill's last-fetched origin snapshot). The describe LEADS with what is lost. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ResetData {
    pub skill: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The version the reset lands on (the followed current / the origin snapshot).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub to_version: String,
    /// The unified diff of the draft that would be (describe) / was (apply) discarded.
    pub drop_diff: String,
    /// `true` on the `--yes` apply, `false` on the describe.
    pub applied: bool,
}

/// `publish` (bare, no `--yes`) — the describe: where it lands, the gate outcome, the audience, the
/// share line, and the undo path. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PublishDescribeData {
    pub skill: String,
    pub skill_id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_display_name: Option<String>,
    /// The byte-exact digest of the draft being published.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The channels the reference lands in (`--to`, or `everyone` for a brand-new skill).
    pub placements: Vec<String>,
    /// The gate outcome: an OPEN bundle lands directly; a REVIEWED one becomes a proposal.
    pub gate: PublishGate,
    /// Whether this publish restores an ancestor's bytes (a revert-shaped publish, same gate).
    pub is_revert: bool,
    /// The audience the change reaches (people entitled to the skill), when the plane discloses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<u64>,
    /// The paste-able share line (`<address>/skills/<name>`), when the workspace address is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_line: Option<String>,
    /// The undo path — the version `revert --to` restores to get back here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<String>,
    /// The origin-demotion disclosure for an imported skill (publishing makes the team copy the source
    /// of truth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_note: Option<String>,
}

/// The gate a `publish` describe predicts. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PublishGate {
    /// The bundle is open — publishing moves `current` directly.
    Lands,
    /// The bundle is reviewed — publishing opens a proposal instead of moving `current`.
    Proposal,
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
                observed: 42,
                applied: 42,
                action: PullAction::UpToDate,
                offer: None,
                conflict: None,
                merge: None,
            }],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
        };
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["skills"][0]["action"], "up_to_date");
        assert_eq!(v["skills"][0]["workspace_id"], "w_acme");
        assert_eq!(v["proposals_awaiting"], 0);
        // The additive fields OMIT when empty (an older consumer sees the unchanged pinned shape).
        assert!(v.get("notices").is_none() && v.get("sync").is_none());
        let back: PullData = serde_json::from_value(v).unwrap();
        assert_eq!(back.skills[0].action, PullAction::UpToDate);
    }

    #[test]
    fn publish_data_carries_the_move_and_omits_an_absent_added_note() {
        let done = PublishData {
            skill_id: "topos_t00".to_owned(),
            name: "pr-describe".to_owned(),
            version_id: "a".repeat(64),
            bundle_digest: "c".repeat(64),
            current_generation: 1,
            added: None,
        };
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["version_id"], "a".repeat(64));
        assert_eq!(v["current_generation"], 1);
        assert!(v.get("added").is_none(), "an absent added note omits");
        let back: PublishData = serde_json::from_value(v).unwrap();
        assert_eq!(back.bundle_digest, "c".repeat(64));
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
