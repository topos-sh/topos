//! Delivery — "what should this device have", and the fleet's applied-state report (the
//! orchestration half; since the door cutover the computation itself is ONE guarded SQL function
//! per op — `topos_delivery` / `topos_report_applied`, migration 0019 — called by the web tier
//! directly and by this crate through the thin statements in `db/directory/delivery.rs`).
//!
//! What stays here is the DEVICE lane's front door (credential sha256 → non-revoked row →
//! confirmed member; every miss the uniform `NotFound`) and the typed view over the function's
//! wire-shaped body — the in-crate suites drive the production SQL through it, so the one
//! implementation carries the whole behavioral suite whichever tier calls it.

use serde::Deserialize;
use topos_types::Generation;

use crate::Authority;
use crate::db::custody::witness::AccessWitness;
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, CommitId, WorkspaceId};

/// One skill this device should have — catalog identity, the pinned current version, the resolved
/// protection posture, and the `via` attribution the narration uses.
#[derive(Debug, Clone)]
pub struct DeliveredSkill {
    pub skill_id: String,
    /// The catalog's user-facing name (the on-disk directory name for a fresh install).
    pub name: String,
    /// The catalog's bundle kind (`"skill"` today) — display metadata, no reader branches on it.
    pub kind: String,
    pub display_name: Option<String>,
    /// `"open"` / `"reviewed"` — the resolved per-bundle cascade (the client's publish preflight
    /// posture; the server re-decides authoritatively on every write).
    pub protection: String,
    pub version_id: [u8; 32],
    pub generation: Generation,
    /// Epoch milliseconds of the last pointer move.
    pub updated_at: i64,
    pub bundle_digest: [u8; 32],
    /// The channels delivering it (names, sorted; `everyone` included when it does).
    pub via_channels: Vec<String>,
    /// Whether the person also follows it directly (survives every channel drop).
    pub direct: bool,
}

/// One unacked person-scoped notice (verdicts with reasons, circumstantial proposal closures — the
/// open `kind` vocabulary grows without a schema change). The silent hook fetches these without
/// acking; interactive narration acks by id.
#[derive(Debug, Clone)]
pub struct DeliveryNotice {
    pub id: String,
    pub kind: String,
    pub skill_id: Option<String>,
    /// The skill's current catalog name (joined for narration; `None` when the notice names none).
    pub skill_name: Option<String>,
    pub version_id: Option<[u8; 32]>,
    pub actor: Option<String>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub created_at: String,
}

/// The delivery response: the entitled set + the person's detached skills (freeze-in-place, never
/// clean — the who-acts principle needs the client to distinguish "you detached this" from
/// "upstream withdrew this", and absence alone cannot say which) + the notices feed + the
/// open-proposal count across the entitled set.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub skills: Vec<DeliveredSkill>,
    /// Skill ids the person detached (unfollowed, or lapsed via a channel leave / removal) and that
    /// are NOT currently re-entitled — every device freezes these in place.
    pub detached: Vec<String>,
    /// Skill ids THIS DEVICE excludes ("not on this device") — the third actor: the copy leaves this
    /// device, the person keeps receiving it everywhere else, and `follow` here lifts it.
    pub excluded: Vec<String>,
    pub notices: Vec<DeliveryNotice>,
    /// OPEN, non-stale proposals across the entitled skills (the review-inbox pressure gauge; the
    /// inbox detail is a separate surface).
    pub proposals_awaiting: u64,
    /// The workspace's staleness window in milliseconds — the ONE clock the fleet page and the
    /// client's currency hook both read (`topos_staleness_window`; a missing policy row COALESCEs to
    /// the default), so a device knows how long its last apply stays "current".
    pub staleness_window_ms: u64,
}

/// One applied-state report row: what this device holds for a skill after its reconcile.
#[derive(Debug, Clone)]
pub struct AppliedSkill {
    pub skill_id: BundleId,
    pub version_id: CommitId,
}

// ── The wire-shaped body `topos_delivery` returns, deserialized for the typed view. Field
//    omission mirrors the wire contract (`excluded` absent when empty; a notice's optional
//    fields absent, never null), so every `#[serde(default)]` below is a wire rule, not slack.
#[derive(Deserialize)]
struct WireDeliveryBody {
    skills: Vec<WireSkillEntry>,
    detached: Vec<String>,
    #[serde(default)]
    excluded: Vec<String>,
    notices: Vec<WireNoticeEntry>,
    proposals_awaiting: u64,
    staleness_window_ms: u64,
}

#[derive(Deserialize)]
struct WireSkillEntry {
    skill_id: String,
    name: String,
    #[serde(default = "default_bundle_kind")]
    kind: String,
    #[serde(default)]
    display_name: Option<String>,
    protection: String,
    version_id: String,
    bundle_digest: String,
    generation: Generation,
    updated_at: i64,
    via: WireViaEntry,
}

/// The wire fallback for a producer predating the catalog `kind` (everything it serves is a skill).
fn default_bundle_kind() -> String {
    "skill".to_owned()
}

#[derive(Deserialize)]
struct WireViaEntry {
    channels: Vec<String>,
    direct: bool,
}

#[derive(Deserialize)]
struct WireNoticeEntry {
    id: String,
    kind: String,
    #[serde(default)]
    skill_id: Option<String>,
    #[serde(default)]
    skill_name: Option<String>,
    #[serde(default)]
    version_id: Option<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
    created_at: String,
}

fn hex32(s: &str) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return Err(AuthorityError::integrity(BadDeliveryBody));
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| AuthorityError::integrity(BadDeliveryBody))?;
    }
    Ok(out)
}

pub(crate) async fn delivery(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
) -> Result<Delivery> {
    let credential_sha256 = topos_core::digest::sha256(credential.as_bytes());
    let identity = authority
        .db()
        .resolve_read_credential(ws, &credential_sha256)
        .await?
        .ok_or(AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &identity.principal).await? {
        return Err(AuthorityError::NotFound);
    }
    let body = authority
        .db()
        .delivery_body(ws, &identity.principal, &identity.device_key_id)
        .await?
        // The function re-runs the gate itself; a refusal past the front door is a revoke or
        // removal racing this read — the same uniform miss either way.
        .ok_or(AuthorityError::NotFound)?;
    let parsed: WireDeliveryBody =
        serde_json::from_value(body).map_err(AuthorityError::integrity)?;
    let skills = parsed
        .skills
        .into_iter()
        .map(|s| {
            Ok(DeliveredSkill {
                skill_id: s.skill_id,
                name: s.name,
                kind: s.kind,
                display_name: s.display_name,
                protection: s.protection,
                version_id: hex32(&s.version_id)?,
                generation: s.generation,
                updated_at: s.updated_at,
                bundle_digest: hex32(&s.bundle_digest)?,
                via_channels: s.via.channels,
                direct: s.via.direct,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let notices = parsed
        .notices
        .into_iter()
        .map(|n| {
            Ok(DeliveryNotice {
                id: n.id,
                kind: n.kind,
                skill_id: n.skill_id,
                skill_name: n.skill_name,
                version_id: n.version_id.as_deref().map(hex32).transpose()?,
                actor: n.actor,
                outcome: n.outcome,
                reason: n.reason,
                message: n.message,
                created_at: n.created_at,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Delivery {
        skills,
        detached: parsed.detached,
        excluded: parsed.excluded,
        notices,
        proposals_awaiting: parsed.proposals_awaiting,
        staleness_window_ms: parsed.staleness_window_ms,
    })
}

pub(crate) async fn report_applied(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    applied: &[AppliedSkill],
    now: i64,
) -> Result<()> {
    let credential_sha256 = topos_core::digest::sha256(credential.as_bytes());
    let identity = authority
        .db()
        .resolve_read_credential(ws, &credential_sha256)
        .await?
        .ok_or(AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &identity.principal).await? {
        return Err(AuthorityError::NotFound);
    }
    let skill_ids: Vec<String> = applied
        .iter()
        .map(|a| a.skill_id.as_str().to_owned())
        .collect();
    let commits: Vec<Vec<u8>> = applied.iter().map(|a| a.version_id.0.to_vec()).collect();
    let ok = authority
        .db()
        .report_applied_fn(
            ws,
            &identity.principal,
            &identity.device_key_id,
            &skill_ids,
            &commits,
            now,
        )
        .await?;
    if !ok {
        // The function's own gate refused past the front door — a revoke racing the report.
        return Err(AuthorityError::NotFound);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("topos_delivery returned a body outside the wire contract")]
struct BadDeliveryBody;
