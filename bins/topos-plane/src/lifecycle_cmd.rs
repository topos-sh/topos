//! Skill-lifecycle wrappers — the leak-free [`PlaneState`] surface for the PRIVILEGED web-session
//! lifecycle ops (archive / unarchive / delete / version purge / rename).
//!
//! These are OWNER ceremonies of the web-surface class: a downstream composition's authenticated
//! admin pages call them with a session-verified acting email (the step-up/type-the-name confirm is
//! the calling page's ceremony, never this layer's). Like [`roster_cmd`](crate::roster_cmd), every
//! signature carries only plain/owned types — ids are `&str`, outcomes are owned enums, faults are
//! stringified — so a composing plane never names a `plane_store` type. Each wrapper parses the
//! plane's deployment mode STRICTLY (fail closed) — though the mode no longer gates these ops: the
//! acting gate is the confirmed-seat check, the same on a self-host plane and a hosted one, and the
//! OWNER gate is answered inside the guarded function (a confirmed non-owner member gets the typed
//! refusal).
//!
//! Every op keys on the IMMUTABLE skill id, never the mutable catalog name: the composing surface
//! resolves the name to the id in its own loader, so a concurrent rename makes a stale reference a
//! harmless miss instead of a wrong-target act.
//!
//! CLASSIFICATION POSTURE: the uniform miss (a malformed workspace id, an unproven caller, an
//! unknown skill id or version) is the typed `NotFound`; a member-entitled refusal is a typed
//! `Denied` whose `reason` is the guarded function's outcome code VERBATIM (`owner_role_required`,
//! `not_active`, `not_archived`, `name_taken`, `bad_name`, `is_current`, `already_purged`) — a
//! plane→composition byte contract, never an oracle.

use plane_store::{AuthorityError, LifecycleOutcome, PurgeOutcome, RenameOutcome, WorkspaceId};

use crate::session_review_cmd::parse_version_hex;
use crate::state::PlaneState;
use crate::wire;

/// The outcome of [`PlaneState::archive_skill_session`]. Plain owned fields only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveSkillSummary {
    /// Archived — renamed `<name>-archived-<date>` (the disclosed spelling), the base name freed,
    /// every channel placement removed, open proposals closed with author notices.
    Archived {
        /// The catalog name the skill now carries.
        archived_name: String,
    },
    /// A member-entitled typed refusal; `reason` is the guarded function's outcome code verbatim.
    Denied {
        /// The static outcome code (`owner_role_required` | `not_active`).
        reason: String,
    },
    /// The uniform miss (malformed ids, an unproven caller, an unknown skill id).
    NotFound,
}

/// The outcome of [`PlaneState::unarchive_skill_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnarchiveSkillSummary {
    /// Unarchived — renamed back to the base name.
    Unarchived {
        /// The restored catalog name.
        name: String,
    },
    /// A member-entitled typed refusal (`owner_role_required` | `not_archived` | `name_taken`).
    Denied {
        /// The static outcome code, verbatim.
        reason: String,
    },
    /// The uniform miss.
    NotFound,
}

/// The outcome of [`PlaneState::delete_skill_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteSkillSummary {
    /// Deleted — the catalog row is the tombstone; content is un-rooted for the GC. Deletion cannot
    /// recall device copies.
    Deleted,
    /// A member-entitled typed refusal (`owner_role_required` | `not_archived` — delete is
    /// archive-first).
    Denied {
        /// The static outcome code, verbatim.
        reason: String,
    },
    /// The uniform miss.
    NotFound,
}

/// The outcome of [`PlaneState::purge_version_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeVersionSummary {
    /// The version's bytes are un-rooted (the next GC pass reclaims what no live version shares);
    /// its hash stays in history as a tombstone.
    Purged,
    /// A member-entitled typed refusal (`owner_role_required` | `is_current` | `already_purged`).
    Denied {
        /// The static outcome code, verbatim.
        reason: String,
    },
    /// The uniform miss (malformed ids, an unproven caller, an unknown skill or version).
    NotFound,
}

/// The outcome of [`PlaneState::rename_skill_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameSkillSummary {
    /// Renamed — the old name stays a resolving redirect until a new identity claims it.
    Renamed {
        /// The catalog name the skill now carries.
        name: String,
    },
    /// A member-entitled typed refusal (`owner_role_required` | `not_active` | `bad_name` |
    /// `name_taken`).
    Denied {
        /// The static outcome code, verbatim.
        reason: String,
    },
    /// The uniform miss.
    NotFound,
}

/// The [`LifecycleOutcome`] refusals as their verbatim outcome codes; `None` for the success arms
/// (each wrapper matches its own success variant first, so a leftover success here is a
/// cross-verb contract breach the caller turns into a fault).
fn lifecycle_denial(outcome: &LifecycleOutcome) -> Option<&'static str> {
    match outcome {
        LifecycleOutcome::OwnerRoleRequired => Some("owner_role_required"),
        LifecycleOutcome::NotActive => Some("not_active"),
        LifecycleOutcome::NotArchived => Some("not_archived"),
        LifecycleOutcome::NameTaken => Some("name_taken"),
        LifecycleOutcome::Archived { .. }
        | LifecycleOutcome::Unarchived { .. }
        | LifecycleOutcome::Deleted => None,
    }
}

impl PlaneState {
    /// Archive a skill from a session-verified OWNER email (the composing web surface proves the
    /// email; this wrapper never does): renames the catalog entry FREEING the base name, unplaces
    /// it everywhere, closes open proposals with author notices, and drops it out of delivery.
    /// Keyed on the immutable skill id.
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault; every
    /// protocol refusal is a typed summary, never an error.
    pub async fn archive_skill_session(
        &self,
        workspace_id: &str,
        acting_email: &str,
        skill_id: &str,
    ) -> anyhow::Result<ArchiveSkillSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(ArchiveSkillSummary::NotFound);
        };
        let (created_at, now) = wire::now_utc();
        match self
            .authority()
            .archive_skill_session(&ws, acting_email, skill_id, mode, &created_at, now)
            .await
        {
            Ok(LifecycleOutcome::Archived { archived_name }) => {
                Ok(ArchiveSkillSummary::Archived { archived_name })
            }
            Ok(other) => match lifecycle_denial(&other) {
                Some(reason) => Ok(ArchiveSkillSummary::Denied {
                    reason: reason.to_owned(),
                }),
                None => Err(anyhow::anyhow!(
                    "archive answered out of contract: {other:?}"
                )),
            },
            Err(AuthorityError::NotFound) => Ok(ArchiveSkillSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("archiving the skill: {error}")),
        }
    }

    /// Unarchive a skill — renames back to the base name if it is still free, else the typed
    /// `name_taken` refusal. Keyed on the immutable skill id.
    ///
    /// # Errors
    /// As [`archive_skill_session`](Self::archive_skill_session).
    pub async fn unarchive_skill_session(
        &self,
        workspace_id: &str,
        acting_email: &str,
        skill_id: &str,
    ) -> anyhow::Result<UnarchiveSkillSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(UnarchiveSkillSummary::NotFound);
        };
        match self
            .authority()
            .unarchive_skill_session(&ws, acting_email, skill_id, mode)
            .await
        {
            Ok(LifecycleOutcome::Unarchived { name }) => {
                Ok(UnarchiveSkillSummary::Unarchived { name })
            }
            Ok(other) => match lifecycle_denial(&other) {
                Some(reason) => Ok(UnarchiveSkillSummary::Denied {
                    reason: reason.to_owned(),
                }),
                None => Err(anyhow::anyhow!(
                    "unarchive answered out of contract: {other:?}"
                )),
            },
            Err(AuthorityError::NotFound) => Ok(UnarchiveSkillSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("unarchiving the skill: {error}")),
        }
    }

    /// Delete an ARCHIVED skill (archive-first; the catalog row is the tombstone): un-roots every
    /// version's content for the GC. Deletion cannot recall device copies. Keyed on the immutable
    /// skill id.
    ///
    /// # Errors
    /// As [`archive_skill_session`](Self::archive_skill_session).
    pub async fn delete_skill_session(
        &self,
        workspace_id: &str,
        acting_email: &str,
        skill_id: &str,
    ) -> anyhow::Result<DeleteSkillSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(DeleteSkillSummary::NotFound);
        };
        let (_created_at, now) = wire::now_utc();
        match self
            .authority()
            .delete_skill_session(&ws, acting_email, skill_id, mode, now)
            .await
        {
            Ok(LifecycleOutcome::Deleted) => Ok(DeleteSkillSummary::Deleted),
            Ok(other) => match lifecycle_denial(&other) {
                Some(reason) => Ok(DeleteSkillSummary::Denied {
                    reason: reason.to_owned(),
                }),
                None => Err(anyhow::anyhow!(
                    "delete answered out of contract: {other:?}"
                )),
            },
            Err(AuthorityError::NotFound) => Ok(DeleteSkillSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("deleting the skill: {error}")),
        }
    }

    /// Purge ONE version's bytes (the leak tool): refused while it is `current`; the hash stays in
    /// history as a tombstone; the reclaim rides the next GC pass. `version_id_hex` is the 64-char
    /// lowercase-hex version id — any other shape is the uniform miss (the composing route
    /// pre-validates the shape and answers its own 400 for a malformed body).
    ///
    /// # Errors
    /// As [`archive_skill_session`](Self::archive_skill_session).
    pub async fn purge_version_session(
        &self,
        workspace_id: &str,
        acting_email: &str,
        skill_id: &str,
        version_id_hex: &str,
    ) -> anyhow::Result<PurgeVersionSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(PurgeVersionSummary::NotFound);
        };
        let Some(version) = parse_version_hex(version_id_hex) else {
            return Ok(PurgeVersionSummary::NotFound);
        };
        let (created_at, now) = wire::now_utc();
        match self
            .authority()
            .purge_version_session(&ws, acting_email, skill_id, version, mode, &created_at, now)
            .await
        {
            Ok(PurgeOutcome::Purged) => Ok(PurgeVersionSummary::Purged),
            Ok(PurgeOutcome::IsCurrent) => Ok(PurgeVersionSummary::Denied {
                reason: "is_current".to_owned(),
            }),
            Ok(PurgeOutcome::AlreadyPurged) => Ok(PurgeVersionSummary::Denied {
                reason: "already_purged".to_owned(),
            }),
            Ok(PurgeOutcome::OwnerRoleRequired) => Ok(PurgeVersionSummary::Denied {
                reason: "owner_role_required".to_owned(),
            }),
            Err(AuthorityError::NotFound) => Ok(PurgeVersionSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("purging the version: {error}")),
        }
    }

    /// Rename an ACTIVE skill: the identity (and every id-keyed reference, placement, and follow)
    /// is untouched — only the user-facing catalog name moves, and the old name keeps resolving as
    /// a redirect until a new identity claims it.
    ///
    /// # Errors
    /// As [`archive_skill_session`](Self::archive_skill_session).
    pub async fn rename_skill_session(
        &self,
        workspace_id: &str,
        acting_email: &str,
        skill_id: &str,
        new_name: &str,
    ) -> anyhow::Result<RenameSkillSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(RenameSkillSummary::NotFound);
        };
        let (created_at, _now) = wire::now_utc();
        match self
            .authority()
            .rename_skill_session(&ws, acting_email, skill_id, new_name, mode, &created_at)
            .await
        {
            Ok(RenameOutcome::Renamed { name }) => Ok(RenameSkillSummary::Renamed { name }),
            Ok(RenameOutcome::NameTaken) => Ok(RenameSkillSummary::Denied {
                reason: "name_taken".to_owned(),
            }),
            Ok(RenameOutcome::BadName) => Ok(RenameSkillSummary::Denied {
                reason: "bad_name".to_owned(),
            }),
            Ok(RenameOutcome::NotActive) => Ok(RenameSkillSummary::Denied {
                reason: "not_active".to_owned(),
            }),
            Ok(RenameOutcome::OwnerRoleRequired) => Ok(RenameSkillSummary::Denied {
                reason: "owner_role_required".to_owned(),
            }),
            Err(AuthorityError::NotFound) => Ok(RenameSkillSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("renaming the skill: {error}")),
        }
    }
}
