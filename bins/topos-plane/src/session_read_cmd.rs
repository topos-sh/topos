//! Session-read wrappers — the leak-free [`PlaneState`] surface for the PRIVILEGED web-session
//! member-scoped reads (the workspace skill index / current / version metadata / object bytes /
//! proposals listing).
//!
//! Deliberately LIB-ONLY, like [`roster_cmd`](crate::roster_cmd) (there is no OSS HTTP route for any
//! of these): a downstream composition's authenticated admin routes call them with a session-verified
//! acting email. Every signature carries only plain/owned types; each wrapper parses the plane's
//! deployment mode STRICTLY (fail closed) and threads it into the authority op, which uniformly
//! denies a self-host plane.
//!
//! **Wire parity by construction.** The current / version-metadata / proposals wrappers return
//! PRE-SERIALIZED wire JSON bytes built by the SAME mappers (and, for `current`, the same stored
//! record blob) the token-scoped `/v1` routes serve — a composing route relays the bytes verbatim, so
//! the two lanes cannot drift shape. Only the skills index (which has no `/v1` twin) is a structured
//! summary.

use plane_store::AuthorityError;
use plane_store::WorkspaceId;

use crate::state::PlaneState;
use crate::wire;

/// One catalog row of [`PlaneState::list_skills_session`]. Plain owned fields; ids hex-encoded.
#[derive(Debug, Clone)]
pub struct SkillIndexEntrySummary {
    pub skill_id: String,
    /// The `current` version id (hex64).
    pub version_id: String,
    pub epoch: u64,
    pub seq: u64,
    /// Epoch **milliseconds** (the server clock unit) of the last pointer move.
    pub updated_at_ms: i64,
    /// The pointed version's consent digest (hex64).
    pub bundle_digest: String,
    /// The skill's UNSIGNED advisory display name (the author's folder name), or `None` (show the skill
    /// id). Display only — never part of the digest or any signature.
    pub display_name: Option<String>,
    pub open_proposals: u64,
}

/// The outcome of [`PlaneState::list_skills_session`].
#[derive(Debug, Clone)]
pub enum SkillsIndexSummary {
    /// The workspace catalog (possibly empty — a member of a workspace with no published skill).
    Skills(Vec<SkillIndexEntrySummary>),
    /// The single uniform miss (self-host / malformed input / unknown workspace / not a confirmed
    /// member).
    NotFound,
}

/// The outcome of [`PlaneState::read_current_session`].
#[derive(Debug, Clone)]
pub enum SessionCurrentSummary {
    /// The stored `SignedCurrentRecord` JSON, byte-verbatim (what a follower verifies; what the
    /// token-scoped current route serves). The authority's `Ok(None)` — no signed pointer exists for
    /// this (ws, skill): a cataloged-but-never-signed skill and an unknown skill id are deliberately
    /// indistinguishable there — is FOLDED into the uniform `NotFound` here; this wrapper is that
    /// composing fold (pre-first-publish visibility would be a conscious new arm, not this fold).
    Current { signed_record: Vec<u8> },
    /// The single uniform miss.
    NotFound,
}

/// The outcome of [`PlaneState::read_version_session`].
#[derive(Debug, Clone)]
pub enum SessionVersionSummary {
    /// The wire version-metadata JSON (the `/v1` versions route's exact body shape).
    Body(Vec<u8>),
    /// The single uniform miss.
    NotFound,
}

/// The outcome of [`PlaneState::read_object_session`].
#[derive(Debug, Clone)]
pub enum SessionObjectSummary {
    /// The object's verified raw bytes.
    Bytes(Vec<u8>),
    /// The single uniform miss.
    NotFound,
}

/// The outcome of [`PlaneState::list_proposals_session`].
#[derive(Debug, Clone)]
pub enum SessionProposalsSummary {
    /// The wire proposals-list JSON (the `/v1` proposals route's exact body shape).
    Body(Vec<u8>),
    /// The single uniform miss.
    NotFound,
}

impl PlaneState {
    /// The workspace catalog for a session-verified confirmed member: every skill with a `current`
    /// row, its pointer generation + epoch-ms update time + consent digest, and its OPEN non-stale
    /// proposal count.
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault (a route
    /// maps it to a 500 — never a miss).
    pub async fn list_skills_session(
        &self,
        workspace_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<SkillsIndexSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(SkillsIndexSummary::NotFound);
        };
        match self
            .authority()
            .list_skills_session(&ws, acting_email, mode)
            .await
        {
            Ok(rows) => Ok(SkillsIndexSummary::Skills(
                rows.into_iter()
                    .map(|r| SkillIndexEntrySummary {
                        skill_id: r.skill_id,
                        version_id: hex::encode(r.version_id),
                        epoch: r.generation.epoch,
                        seq: r.generation.seq,
                        updated_at_ms: r.updated_at,
                        bundle_digest: hex::encode(r.bundle_digest),
                        display_name: r.display_name,
                        open_proposals: r.open_proposals,
                    })
                    .collect(),
            )),
            Err(AuthorityError::NotFound) => Ok(SkillsIndexSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("reading the skill index: {error}")),
        }
    }

    /// A skill's signed `current` pointer for a session-verified confirmed member — the stored
    /// record bytes verbatim (parity with the token-scoped current route by construction).
    ///
    /// # Errors
    /// An unparseable plane mode or a stringified authority fault.
    pub async fn read_current_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionCurrentSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(SessionCurrentSummary::NotFound);
        };
        match self
            .authority()
            .read_current_session(&ws, skill_id, acting_email, mode)
            .await
        {
            Ok(Some(pointer)) => Ok(SessionCurrentSummary::Current {
                signed_record: pointer.signed_record,
            }),
            // The deliberate fold: no signed pointer for this (ws, skill) reads as the uniform miss here.
            Ok(None) => Ok(SessionCurrentSummary::NotFound),
            Err(AuthorityError::NotFound) => Ok(SessionCurrentSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("reading the current pointer: {error}")),
        }
    }

    /// A version's authenticated metadata for a session-verified confirmed member, as the wire JSON
    /// the token-scoped versions route serves (same mapper, same serde — parity by construction).
    ///
    /// # Errors
    /// An unparseable plane mode, a wire-serialization fault, or a stringified authority fault.
    pub async fn read_version_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version_id_hex: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionVersionSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(SessionVersionSummary::NotFound);
        };
        match self
            .authority()
            .read_version_metadata_session(&ws, skill_id, version_id_hex, acting_email, mode)
            .await
        {
            Ok(meta) => Ok(SessionVersionSummary::Body(serde_json::to_vec(
                &wire::map::version_meta_to_wire(meta),
            )?)),
            Err(AuthorityError::NotFound) => Ok(SessionVersionSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("reading the version metadata: {error}")),
        }
    }

    /// One object's verified bytes for a session-verified confirmed member.
    ///
    /// # Errors
    /// An unparseable plane mode or a stringified authority fault (an Integrity alarm is a fault —
    /// a route maps it to a 500, never a miss).
    pub async fn read_object_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        object_id_hex: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionObjectSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(SessionObjectSummary::NotFound);
        };
        match self
            .authority()
            .serve_object_session(&ws, skill_id, object_id_hex, acting_email, mode)
            .await
        {
            Ok(bytes) => Ok(SessionObjectSummary::Bytes(bytes)),
            Err(AuthorityError::NotFound) => Ok(SessionObjectSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("reading the object: {error}")),
        }
    }

    /// A skill's OPEN, non-stale proposals for a session-verified confirmed member, as the wire JSON
    /// the token-scoped proposals route serves (same mapper, same serde — parity by construction).
    ///
    /// # Errors
    /// An unparseable plane mode, a wire-serialization fault, or a stringified authority fault.
    pub async fn list_proposals_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionProposalsSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(SessionProposalsSummary::NotFound);
        };
        match self
            .authority()
            .list_open_proposals_session(&ws, skill_id, acting_email, mode)
            .await
        {
            Ok(rows) => Ok(SessionProposalsSummary::Body(serde_json::to_vec(
                &wire::map::open_proposals_to_wire(rows),
            )?)),
            Err(AuthorityError::NotFound) => Ok(SessionProposalsSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("listing the proposals: {error}")),
        }
    }
}
