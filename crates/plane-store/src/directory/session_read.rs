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

use topos_core::sign::{CatalogReadFields, verify_catalog_read};
use topos_types::Generation;

use crate::authority::Authority;
use crate::db::directory::session_read::SkillIndexDbRow;
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
    build_skill_index(authority, ws).await
}

/// The workspace catalog BODY — the SHARED index build both read lanes call AFTER their own gate, so the
/// session lane and the device lane emit byte-identical `SkillIndexRow`s: [`Db::list_skill_index`](crate::db::Db)
/// (the one index join) plus the per-skill OPEN non-stale proposal count delegated to the SAME
/// `open_proposal_rows` listing statement (so `count == list.len()` by construction; a deliberate O(skills)
/// fan-out on a cold route). Principal-free — the caller's gate has already run.
async fn build_skill_index(authority: &Authority, ws: &WorkspaceId) -> Result<Vec<SkillIndexRow>> {
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

/// The DEVICE-signed catalog read (`list --remote`) — the catalog-visibility twin of
/// [`list_skills_session`] authorized WITHOUT a web session, on BOTH cloud and self-host: device auth IS
/// the self-host membership story, so this lane does NOT take or consult a [`DeploymentMode`]. Three gates,
/// every failure the ONE uniform [`AuthorityError::NotFound`] (mirroring [`member_gate`]'s
/// indistinguishability):
/// 1. resolve the NON-REVOKED device → its registered public key + bound principal (miss ⇒ NotFound);
/// 2. verify the catalog-read signature over `(workspace_id, device_key_id)` against that key (false ⇒
///    NotFound) — the frame binds the workspace, so no cross-workspace replay;
/// 3. the device's bound principal must be a CONFIRMED workspace member (catalog visibility == membership).
///
/// Then the SAME [`build_skill_index`] body the session lane builds. A pool read only — no transaction, no
/// receipt, no op id. `_now` is accepted for signature parity with the token-lane reads; device
/// registration carries no expiry, so this lane never consults a clock.
pub(crate) async fn list_skills_device(
    authority: &Authority,
    ws: &WorkspaceId,
    device_key_id: &str,
    signature: &[u8; 64],
    _now: i64,
) -> Result<Vec<SkillIndexRow>> {
    let Some((public_key, principal_s)) =
        authority.db().read_active_device(ws, device_key_id).await?
    else {
        return Err(AuthorityError::NotFound);
    };
    let fields = CatalogReadFields {
        workspace_id: ws.as_str(),
        device_key_id,
    };
    if !verify_catalog_read(&fields, signature, &public_key) {
        return Err(AuthorityError::NotFound);
    }
    // A device_registry principal was validated at registration, so a re-parse failure is store corruption
    // (Integrity), never a not-found — the same convention `govern_preamble` follows for a signing device.
    let principal = Principal::parse(&principal_s).map_err(AuthorityError::integrity)?;
    if !authority.db().confirmed_member(ws, &principal).await? {
        return Err(AuthorityError::NotFound);
    }
    build_skill_index(authority, ws).await
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

/// One proposal's detail for a confirmed member — the review surface's read: status + base + proposer
/// (deliberately session-lane-only: the thin `/v1` listing stays proposer-free) + the resolution facts +
/// the workspace's `review_required` policy at read time (display-only; the in-txn gate is the
/// authority). `Ok(None)` means no proposal of this candidate was ever opened on this skill — a
/// member-entitled post-gate outcome the composing wrapper folds into its uniform miss.
#[derive(Debug, Clone)]
pub struct ProposalDetailSession {
    /// The candidate commit (the proposal's `@hash`).
    pub version_id: [u8; 32],
    /// The STORED status (`open` / `accepted` / `rejected`) — `stale` stays derived, by the reader,
    /// from `open` + a base that no longer equals the live current generation.
    pub status: String,
    /// The base generation the proposal was opened against.
    pub base: Generation,
    /// When the proposal was opened (ISO-8601).
    pub created_at: String,
    /// The proposer's canonical email (the four-eyes surface).
    pub proposer: String,
    /// The workspace's review-required policy at read time (display-only).
    pub review_required: bool,
    /// Who resolved it (`None` while open; pre-lane resolved rows may carry `None` timestamps/reasons).
    pub resolved_by: Option<String>,
    /// The session reject's mandatory reason (`None` on accepts, device rejects, and pre-lane rows).
    pub resolved_reason: Option<String>,
    /// When it was resolved (ISO-8601; `None` on open and pre-lane rows).
    pub resolved_at: Option<String>,
}

/// The proposal-detail read (see [`ProposalDetailSession`]): the shared member gate, then the ONE
/// preference-ordered row (open > accepted > latest rejected) for `(skill, candidate)`.
pub(crate) async fn read_proposal_detail_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &str,
    version_id_hex: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<Option<ProposalDetailSession>> {
    let acting = member_gate(authority, ws, acting_email, plane_mode).await?;
    let scope = member_scope(ws, skill, acting)?;
    let commit = crate::read::parse_hex32(version_id_hex).ok_or(AuthorityError::NotFound)?;
    let Some(row) = authority
        .db()
        .read_proposal_detail(ws, scope.skill(), crate::id::CommitId(commit))
        .await?
    else {
        return Ok(None);
    };
    let review_required = authority.db().workspace_review_required(ws).await?;
    Ok(Some(ProposalDetailSession {
        version_id: commit,
        status: row.status.as_str().to_owned(),
        base: row.base,
        created_at: row.created_at,
        proposer: row.proposer,
        review_required,
        resolved_by: row.resolved_by,
        resolved_reason: row.resolved_reason,
        resolved_at: row.resolved_at,
    }))
}
