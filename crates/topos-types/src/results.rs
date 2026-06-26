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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PullData {
    pub skills: Vec<PullSkill>,
    /// Proposals awaiting *me* as a reviewer.
    pub proposals_awaiting: u32,
}

/// One followed skill's pull state. `observed`/`applied`/`action`/`offer`/`conflict` are PINNED by
/// name; the *value enums* (`PullAction`) and the `offer`/`conflict` field shapes are INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PullSkill {
    pub skill: String,
    /// Highest authenticated `(epoch,seq)` seen — the anti-rollback floor.
    pub observed: Generation,
    /// Highest generation actually materialized to disk.
    pub applied: Generation,
    pub action: PullAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<Offer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Conflict>,
}

/// What `pull` did / offers for a skill. **INFERRED value set** — the four-state machine pins the
/// semantics (CURRENT / BEHIND / DRAFT / DIVERGED) but not these exact tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullAction {
    /// State ① — already current; nothing to do.
    UpToDate,
    /// State ② clean — auto fast-forwarded to the new bytes.
    FastForwarded,
    /// State ② confirm-each / first-receive — a one-tap offer is waiting.
    Offered,
    /// State ④ — a local draft conflicts with a newer remote.
    Diverged,
    /// A transient local hold (e.g. a local go-back is pinned).
    Held,
    /// A reused/replayed generation tuple was seen — a loud integrity alarm.
    Alarm,
}

/// The re-disclosed bytes a `pull` offers (confirm-each / first-receive). **INFERRED fields** — the
/// spec pins that the offer re-discloses + re-binds the digest, not its exact shape.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Offer {
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub version_id: String,
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub bundle_digest: String,
}

/// The DIVERGED panel (local draft vs newer remote). **INFERRED fields.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Conflict {
    /// The remote version the draft diverged from.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub remote_version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub local_version_id: Option<String>,
}

// =================================================================================================
// PINNED — `list` (the four buckets + per-skill identity).
// =================================================================================================

/// `list` result — the four inventory buckets. **PINNED** (bucket set + per-entry identity).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListData {
    pub followed: Vec<SkillEntry>,
    pub published_by_you: Vec<SkillEntry>,
    pub tracked: Vec<SkillEntry>,
    /// A real skill in a harness dir that topos doesn't manage yet (discovered, not adopted) — it has
    /// no topos `version_id`/`bundle_digest` yet, so it carries only what is knowable on disk.
    pub untracked: Vec<UntrackedEntry>,
    /// Only present under `--footprint`: topos-owned paths outside skill dirs. **INFERRED shape.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint: Option<Vec<String>>,
}

/// A discovered-but-unadopted skill — known only by where it lives, not by any topos version yet.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UntrackedEntry {
    pub name: String,
    /// The harness dir it was found in.
    pub path: String,
    pub harness: crate::HarnessId,
}

/// A skill row. `<skill>@<version_id>` identity + `draft` are PINNED; the other field names INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SkillEntry {
    pub skill: String,
    /// The approvable `@` token (the commit SHA-256).
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub version_id: String,
    /// The byte-exact consent hash, shown alongside as evidence.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffData {
    pub source: DiffSource,
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub version_id: String,
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    /// A plain unified diff.
    pub diff: String,
}

/// Where the compared bytes came from: the local sidecar, or a plane-held proposal. **PINNED.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiffSource {
    Local,
    Plane,
}

// =================================================================================================
// INFERRED — the nine verbs whose `data` field list the spec does not enumerate. Shapes derived
// from the documented mechanics; additive-only; will tighten as each verb is built.
// =================================================================================================

/// `add` (local, offline — no plane op, `receipt: null`). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AddData {
    pub skill_id: String,
    pub name: String,
    /// The base commit the local sidecar starts from.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub version_id: String,
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    pub tracked: bool,
}

/// `follow` (enrollment + first-receive). Each offered skill is a TOFU offer, never auto-landed.
/// **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FollowData {
    pub workspace_id: String,
    pub enrolled: bool,
    /// First-receive offers — empty when the link is membership-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<FollowOffer>,
}

/// A single skill offered at `follow` — disclosed, awaiting a direct human yes (TOFU). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FollowOffer {
    pub skill_id: String,
    pub name: String,
    pub offer: Offer,
}

/// `unfollow` (local — stop following `current`, keep the bytes as a frozen copy). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UnfollowData {
    pub skill_id: String,
    pub following: bool,
    /// The local bytes are retained, not deleted.
    pub bytes_kept: bool,
}

/// `log` — local action events (and, with `--team`, partial plane records). The individual event
/// fields are **not pinned by the spec**, so events stay open JSON. **INFERRED.**
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogData {
    /// Local action-event envelopes from `log.jsonl` (field set intentionally open).
    pub events: Vec<serde_json::Value>,
    /// Plane-side records under `--team` (op-receipts ⋈ approvals ⋈ lineage) — honestly partial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<Vec<serde_json::Value>>,
}

/// `publish` (a direct publish that moves `current`). On the FIRST publish the `/i/` link is
/// returned. Under `review-required` a direct publish instead returns `APPROVAL_REQUIRED` (with the
/// `publish --propose` next-action) and carries no `data`. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PublishData {
    pub skill_id: String,
    /// The new commit.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub version_id: String,
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    /// The pointer's new generation after the move.
    pub current_generation: Generation,
    /// Returned only on the first publish (which stands up the workspace): `/i/<token>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_link: Option<String>,
}

/// `publish --propose` (opens a PR; uploads a full candidate **without moving `current`**). Returns
/// `NEEDS_REVIEW`. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProposeData {
    /// `<skill>@<version_id>` of the candidate.
    pub proposal: String,
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub base_version_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// `revert` (a **forward** git-revert restoring older bytes as a new, higher-`seq` version — never a
/// pointer rollback, never a delete). `--to` names the GOOD version. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RevertData {
    pub skill_id: String,
    /// The good version named by `--to` (the bytes being restored).
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub reverted_to: String,
    /// The new forward-revert commit that carries those bytes.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub new_version_id: String,
    pub current_generation: Generation,
}

/// `review` (`--approve` / `--reject` a proposal). Approve is a compare-and-set on the base; a stale
/// base returns `CONFLICT`. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewData {
    /// `<skill>@<version_id>` of the reviewed proposal.
    pub proposal: String,
    pub decision: ReviewDecision,
    /// The pointer's new generation when an approval moved `current`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<Generation>,
}

/// A review verdict. **INFERRED.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
}

/// `invite` (mint an `/i/` link + optionally seed the roster). A link never carries a role and never
/// enrolls on its own. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
                observed: Generation { epoch: 1, seq: 42 },
                applied: Generation { epoch: 1, seq: 42 },
                action: PullAction::UpToDate,
                offer: None,
                conflict: None,
            }],
            proposals_awaiting: 0,
        };
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["skills"][0]["action"], "up_to_date");
        assert_eq!(v["proposals_awaiting"], 0);
        let back: PullData = serde_json::from_value(v).unwrap();
        assert_eq!(back.skills[0].action, PullAction::UpToDate);
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
