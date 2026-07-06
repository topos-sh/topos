//! The WEB-SESSION read lane — PRIVILEGED lib-level member-scoped reads (no OSS HTTP route). The read
//! twin of [`crate::session_roster`]: a hosted composition's authenticated admin routes call these with
//! the session's verified email; the composing caller's session verification IS the authentication (no
//! signature, no token). Pool reads only — no `run_serializable!`, no `op_id`, no `workspace_events`,
//! no receipts (reads mint no durable rows).
//!
//! **Deliberately BROADER than the device lane, by decision:** any CONFIRMED workspace member — any
//! role, with or without per-skill `roster` rows — reads the workspace's full catalog and every skill's
//! current/metadata/bytes/proposals. Catalog visibility IS workspace membership; per-skill `roster`
//! stays the device lane's (read-token) gate. Cloud-only: a self-host plane denies uniformly (bearer +
//! invite-chain remain the self-host story).
//!
//! **NotFound uniformity.** Every pre-gate miss — self-host, malformed email, malformed skill id,
//! unknown workspace, non-member, invited-but-unconfirmed — is the single indistinguishable
//! [`AuthorityError::NotFound`]: [`member_gate`] is the ONE session entry, so the uniformity cannot
//! drift per-op. The only post-gate non-uniform outcomes are member-entitled: `read_current_session`'s
//! `Ok(None)` (no signed pointer exists for this (ws, skill) — a cataloged-but-never-signed skill and an
//! unknown skill id are deliberately indistinguishable here; the composing wrapper folds both into the
//! uniform miss) and the empty lists. Two benign
//! mid-flight-revocation shapes, named so neither is ever "fixed" into an oracle: the skill-scoped
//! reads re-run the member gate per statement (a just-removed member's in-flight read completes or
//! misses uniformly — the same accepted gate-to-reach window the token lane carries), while
//! [`list_skills_session`]'s delegated index reads are principal-free, so a removal mid-call returns
//! that one in-flight catalog whole.

use topos_types::Generation;

use crate::authority::Authority;
use crate::db::session_read::SkillIndexDbRow;
use crate::enroll::DeploymentMode;
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, SkillId, WorkspaceId};
use crate::read::{CurrentPointer, OpenProposalSummary, ReadScope, VersionMeta};

/// One skill of the workspace catalog, as [`Authority::list_skills_session`] returns it: the skill, its
/// `current` pointer facts (version id, generation, epoch-ms update time), the pointed version's consent
/// `bundle_digest`, the skill's advisory display name, and the OPEN non-stale proposal count. NO bytes, NO
/// signed record (that stays on [`Authority::read_current_session`]), NO proposer identities.
#[derive(Debug, Clone)]
pub struct SkillIndexRow {
    pub skill_id: String,
    pub version_id: [u8; 32],
    pub generation: Generation,
    /// Epoch **milliseconds** (the server clock unit) of the last pointer move.
    pub updated_at: i64,
    pub bundle_digest: [u8; 32],
    /// The skill's UNSIGNED advisory display name (the author's folder name), or `None` (show the skill
    /// id). Display only — never part of the digest or any signature.
    pub display_name: Option<String>,
    pub open_proposals: u64,
}

/// The ONE session entry: self-host denial → canonical principal parse → confirmed-member probe. Every
/// session op runs this first; each miss is the same indistinguishable [`AuthorityError::NotFound`].
async fn member_gate(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<Principal> {
    if plane_mode == DeploymentMode::SelfHost {
        return Err(AuthorityError::NotFound);
    }
    let acting = Principal::parse(acting_email).map_err(|_| AuthorityError::NotFound)?;
    if !authority.db().confirmed_member(ws, &acting).await? {
        return Err(AuthorityError::NotFound);
    }
    Ok(acting)
}

/// Parse a skill-scoped session op's skill id and build the member-lane scope. A malformed skill id is
/// the uniform miss (never a distinguishable 400 from this layer).
fn member_scope(ws: &WorkspaceId, skill: &str, acting: Principal) -> Result<ReadScope> {
    let skill = SkillId::parse(skill).map_err(|_| AuthorityError::NotFound)?;
    Ok(ReadScope::for_member(ws.clone(), skill, acting))
}

/// The workspace catalog: every skill holding a `current` row, each with its open-proposal count. The
/// count delegates per skill to the SAME `open_proposal_rows` statement the proposals listing serves, so
/// `count == list.len()` by construction and the staleness predicate keeps its one listing home (a
/// deliberate O(skills) fan-out on a cold route; a joined count would be a sixth predicate copy).
pub(crate) async fn list_skills_session(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<Vec<SkillIndexRow>> {
    member_gate(authority, ws, acting_email, plane_mode).await?;
    let rows = authority.db().list_skill_index(ws).await?;
    let mut out = Vec::with_capacity(rows.len());
    for SkillIndexDbRow {
        skill_id,
        commit,
        generation,
        updated_at,
        bundle_digest,
        display_name,
    } in rows
    {
        // A stored skill_id was validated on the way in; a re-parse failure is store corruption (the
        // `commit_owners` convention), never a not-found.
        let skill = SkillId::parse(&skill_id).map_err(AuthorityError::integrity)?;
        let open_proposals = authority.db().open_proposal_rows(ws, &skill).await?.len() as u64;
        out.push(SkillIndexRow {
            skill_id,
            version_id: commit,
            generation,
            updated_at,
            bundle_digest,
            display_name,
            open_proposals,
        });
    }
    Ok(out)
}

/// A skill's signed `current` pointer for a confirmed member — [`crate::read::read_current`] verbatim
/// over the member-lane scope. `Ok(None)` means no signed pointer exists for this (ws, skill) — a
/// cataloged-but-never-signed skill and an unknown skill id are deliberately indistinguishable here; the
/// composing wrapper folds both into the uniform miss. It is a member-entitled post-gate outcome,
/// deliberately distinct from this layer's uniform `NotFound`.
pub(crate) async fn read_current_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<Option<CurrentPointer>> {
    let acting = member_gate(authority, ws, acting_email, plane_mode).await?;
    let scope = member_scope(ws, skill, acting)?;
    crate::read::read_current(authority, &scope).await
}

/// One object's bytes for a confirmed member — [`crate::read::serve_object`] verbatim (the scope/path
/// assert, the hex parse, the gate/reach authorization, the verify-on-read fetch, and the
/// re-authorize-on-miss guard all reused; the guard re-gates on the MEMBER lane, so a reclaimed object
/// is 404 and genuine corruption stays an Integrity alarm for this lane exactly as for the token lane).
pub(crate) async fn serve_object_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &str,
    object_id_hex: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<Vec<u8>> {
    let acting = member_gate(authority, ws, acting_email, plane_mode).await?;
    let scope = member_scope(ws, skill, acting)?;
    crate::read::serve_object(
        authority,
        &scope,
        ws.as_str(),
        scope.skill().as_str(),
        object_id_hex,
    )
    .await
}

/// A version's authenticated metadata for a confirmed member — [`crate::read::read_version_metadata`]
/// verbatim over the member-lane scope (same R1 authorization shape, gate swapped by the lane).
pub(crate) async fn read_version_metadata_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &str,
    version_id_hex: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<VersionMeta> {
    let acting = member_gate(authority, ws, acting_email, plane_mode).await?;
    let scope = member_scope(ws, skill, acting)?;
    crate::read::read_version_metadata(
        authority,
        &scope,
        ws.as_str(),
        scope.skill().as_str(),
        version_id_hex,
    )
    .await
}

/// The OPEN, non-stale proposals on one skill for a confirmed member —
/// [`crate::read::list_open_proposals`] verbatim over the member-lane scope. The gate folds a denial to
/// the preamble's NotFound BEFORE the list runs; a mid-flight revocation between the preamble and the
/// statement's own re-gate folds to `Ok(empty)` — both named above, neither an oracle.
pub(crate) async fn list_open_proposals_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<Vec<OpenProposalSummary>> {
    let acting = member_gate(authority, ws, acting_email, plane_mode).await?;
    let scope = member_scope(ws, skill, acting)?;
    crate::read::list_open_proposals(authority, &scope, ws.as_str(), scope.skill().as_str()).await
}
