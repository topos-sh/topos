//! Session-roster wrappers — the leak-free [`PlaneState`] surface for the PRIVILEGED web-session
//! membership ops (invite / remove / read-roster).
//!
//! Deliberately LIB-ONLY (there is no OSS HTTP route for any of these): a downstream composition's
//! authenticated admin routes call them with a session-verified acting email. Like
//! [`standup_cmd`](crate::standup_cmd), every signature carries only plain/owned types — ids are
//! `&str`, outcomes are owned enums/structs, faults are stringified — so a composing plane never
//! names a `plane-store` type. Each wrapper parses the plane's deployment mode STRICTLY (fail
//! closed) and threads it into the authority op — though the mode no longer gates these ops: the
//! acting gate is the confirmed-owner seat check, the same on a self-host plane and a hosted one
//! (the product app serves self-hosted deployments through this session lane).
//!
//! An invitation is a ROSTER WRITE and nothing more: what comes back is the workspace ADDRESS
//! (`<link_base>/<name>`), never a tokened link — links carry nothing, the roster is the lock. The
//! standing-door machinery (rotation, door links) died with the invite tables, so there is nothing to
//! rotate and no secret to redact here.

use plane_store::{
    AuthorityError, GovernanceOutcome, SessionInviteOutcome, SessionInviteRole, WorkspaceId,
};

use crate::state::PlaneState;
use crate::wire;

/// The outcome of [`PlaneState::invite_members`]. Plain owned fields only (the ADDRESS is a public fact,
/// not a credential, so nothing here is redacted).
#[derive(Debug, Clone)]
pub enum InviteMembersSummary {
    /// The seats are seeded (or the identical request replayed); `address` is the workspace share address.
    Invited {
        /// The workspace ADDRESS the invitees join at (`follow <address>` + proof of the invited email).
        address: String,
        /// How many distinct addresses were seated.
        seated: usize,
    },
    /// The request was denied (the uniform acting-gate denial, or a malformed input).
    Denied {
        /// The static, typed reason (server-log fidelity; never an oracle).
        reason: String,
    },
}

/// The outcome of [`PlaneState::remove_member`].
#[derive(Debug, Clone)]
pub enum RemoveMemberSummary {
    /// The seat is gone (idempotent — an absent principal removes to the same outcome); the removed
    /// principal's devices stop authorizing the instant the seat drops.
    Removed,
    /// The request was denied (the acting gate, the last-owner lockout, a malformed input).
    Denied {
        /// The static, typed reason.
        reason: String,
    },
}

/// One seat, as [`PlaneState::read_roster`] discloses it. Plain owned fields only.
#[derive(Debug, Clone)]
pub struct RosterSeatSummary {
    /// The seat's email.
    pub email: String,
    /// The role discriminant (`owner` / `reviewer` / `member`).
    pub role: String,
    /// The lifecycle (`invited` / `confirmed`).
    pub status: String,
    /// When the seat was added (ISO-8601).
    pub added_at: String,
}

/// The outcome of [`PlaneState::read_roster`]. `NotFound` is the UNIFORM miss (an absent workspace, a
/// non-member or merely-invited acting email) — the caller must render it indistinguishably.
#[derive(Debug, Clone)]
pub enum RosterSummary {
    /// The roster, plus the workspace ADDRESS (member-visible — a name, not a door; joining still gates
    /// on the roster).
    Roster {
        /// The seats, invited and confirmed.
        seats: Vec<RosterSeatSummary>,
        /// The workspace's full address (`<link_base>/<name>`).
        address: String,
    },
    /// The uniform miss.
    NotFound,
}

impl PlaneState {
    /// Invite members from a session-verified OWNER email (the composing web surface proves the
    /// email; this wrapper never does). Seats `emails` at `role` (`"member"` / `"reviewer"` —
    /// anything else, owner included, is a typed denial) and returns the workspace ADDRESS the
    /// invitees join at. Idempotent per `request_id` (a canonical UUID).
    ///
    /// # Errors
    /// An unparseable plane mode / workspace id (typed, fail closed) or a stringified authority
    /// fault; a protocol-level refusal is the typed [`InviteMembersSummary::Denied`].
    pub async fn invite_members(
        &self,
        workspace_id: &str,
        request_id: &str,
        acting_email: &str,
        emails: &[String],
        role: &str,
    ) -> anyhow::Result<InviteMembersSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(InviteMembersSummary::Denied {
                reason: "invalid workspace id".to_owned(),
            });
        };
        let Some(role) = SessionInviteRole::parse(role) else {
            return Ok(InviteMembersSummary::Denied {
                reason: "role must be member or reviewer".to_owned(),
            });
        };
        let (created_at, now) = wire::now_utc();
        let outcome = self
            .authority()
            .invite_members_session(
                &ws,
                request_id,
                acting_email,
                emails,
                role,
                mode,
                &created_at,
                now,
            )
            .await
            .map_err(|error| anyhow::anyhow!("inviting members: {error}"))?;
        Ok(match outcome {
            SessionInviteOutcome::Invited { address, seated } => {
                InviteMembersSummary::Invited { address, seated }
            }
            SessionInviteOutcome::Denied(reason) => InviteMembersSummary::Denied {
                reason: reason.to_owned(),
            },
        })
    }

    /// Remove a member from a session-verified OWNER email. Idempotent per `request_id`; the
    /// last-owner lockout denies typed; the removed principal's seat drops in the same transaction.
    ///
    /// # Errors
    /// An unparseable plane mode / workspace id (typed, fail closed) or a stringified authority
    /// fault.
    pub async fn remove_member(
        &self,
        workspace_id: &str,
        request_id: &str,
        acting_email: &str,
        target_email: &str,
    ) -> anyhow::Result<RemoveMemberSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(RemoveMemberSummary::Denied {
                reason: "invalid workspace id".to_owned(),
            });
        };
        let (created_at, now) = wire::now_utc();
        let outcome = self
            .authority()
            .roster_remove_session(
                &ws,
                request_id,
                acting_email,
                target_email,
                mode,
                &created_at,
                now,
            )
            .await
            .map_err(|error| anyhow::anyhow!("removing the member: {error}"))?;
        Ok(match outcome {
            GovernanceOutcome::Ok => RemoveMemberSummary::Removed,
            GovernanceOutcome::Denied(reason) => RemoveMemberSummary::Denied {
                reason: reason.to_owned(),
            },
        })
    }

    /// Read the workspace roster for a session-verified email — seats for any confirmed member, plus
    /// the workspace ADDRESS (member-visible). Every miss is the uniform [`RosterSummary::NotFound`].
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault.
    pub async fn read_roster(
        &self,
        workspace_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<RosterSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(RosterSummary::NotFound);
        };
        match self.authority().read_roster(&ws, acting_email, mode).await {
            Ok(view) => Ok(RosterSummary::Roster {
                seats: view
                    .seats
                    .into_iter()
                    .map(|s| RosterSeatSummary {
                        email: s.email,
                        role: s.role,
                        status: s.status,
                        added_at: s.added_at,
                    })
                    .collect(),
                address: view.address,
            }),
            Err(AuthorityError::NotFound) => Ok(RosterSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("reading the roster: {error}")),
        }
    }
}
