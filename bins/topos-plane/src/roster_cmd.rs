//! Session-roster wrappers — the leak-free [`PlaneState`] surface for the PRIVILEGED web-session
//! membership ops (invite / remove / rotate-the-standing-door / read-roster).
//!
//! Deliberately LIB-ONLY (there is no OSS HTTP route for any of these): a downstream composition's
//! authenticated admin routes call them with a session-verified acting email. Like
//! [`standup_cmd`](crate::standup_cmd), every signature carries only plain/owned types — ids are
//! `&str`, outcomes are owned enums/structs, faults are stringified — so a composing plane never
//! names a `plane-store` type. Each wrapper parses the plane's deployment mode STRICTLY (fail
//! closed) and threads it into the authority op; the ops themselves uniformly deny a self-host
//! plane (self-host membership stays the device-signed invite chain).

use plane_store::{
    AuthorityError, GovernanceOutcome, SessionInviteOutcome, SessionInviteRole,
    SessionRotateOutcome, WorkspaceId,
};

use crate::state::PlaneState;
use crate::wire;

/// The outcome of [`PlaneState::invite_members`]. Plain owned fields only. `Debug` REDACTS
/// `invite_link` — the standing door is a live workspace-wide join credential (never logged).
#[derive(Clone)]
pub enum InviteMembersSummary {
    /// The seats are seeded (or the identical request replayed); `invite_link` is the standing
    /// workspace door.
    Invited {
        /// The standing door link (`<link_base>/i/<token>`).
        invite_link: String,
        /// How many distinct addresses were seated.
        seated: usize,
    },
    /// The request was denied (the uniform acting-gate denial, a malformed input, self-host).
    Denied {
        /// The static, typed reason (server-log fidelity; never an oracle).
        reason: String,
    },
}

impl std::fmt::Debug for InviteMembersSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InviteMembersSummary::Invited { seated, .. } => f
                .debug_struct("Invited")
                .field("invite_link", &"<redacted>")
                .field("seated", seated)
                .finish(),
            InviteMembersSummary::Denied { reason } => {
                f.debug_struct("Denied").field("reason", reason).finish()
            }
        }
    }
}

/// The outcome of [`PlaneState::remove_member`].
#[derive(Debug, Clone)]
pub enum RemoveMemberSummary {
    /// The seat is gone (idempotent — an absent principal removes to the same outcome) and the
    /// principal's read tokens are dropped.
    Removed,
    /// The request was denied (the acting gate, the last-owner lockout, a malformed input).
    Denied {
        /// The static, typed reason.
        reason: String,
    },
}

/// The outcome of [`PlaneState::rotate_join_link`]. `Debug` REDACTS the new door link.
#[derive(Clone)]
pub enum RotateJoinLinkSummary {
    /// The door rotated; `invite_link` is the NEW standing door.
    Rotated {
        /// The new standing door link.
        invite_link: String,
    },
    /// The request was denied.
    Denied {
        /// The static, typed reason.
        reason: String,
    },
}

impl std::fmt::Debug for RotateJoinLinkSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RotateJoinLinkSummary::Rotated { .. } => f
                .debug_struct("Rotated")
                .field("invite_link", &"<redacted>")
                .finish(),
            RotateJoinLinkSummary::Denied { reason } => {
                f.debug_struct("Denied").field("reason", reason).finish()
            }
        }
    }
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

/// The outcome of [`PlaneState::read_roster`]. `NotFound` is the UNIFORM miss (an absent
/// workspace, a non-member or merely-invited acting email, a self-host plane) — the caller must
/// render it indistinguishably. `Debug` REDACTS the owner-only door link.
#[derive(Clone)]
pub enum RosterSummary {
    /// The roster, plus the standing door link for a confirmed OWNER caller (`None` also when no
    /// door stands yet).
    Roster {
        /// The seats, invited and confirmed.
        seats: Vec<RosterSeatSummary>,
        /// The standing door link — owner-only disclosure.
        invite_link: Option<String>,
    },
    /// The uniform miss.
    NotFound,
}

impl std::fmt::Debug for RosterSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RosterSummary::Roster { seats, invite_link } => f
                .debug_struct("Roster")
                .field("seats", seats)
                .field("invite_link", &invite_link.as_ref().map(|_| "<redacted>"))
                .finish(),
            RosterSummary::NotFound => f.write_str("NotFound"),
        }
    }
}

impl PlaneState {
    /// Invite members from a session-verified OWNER email (the composing web surface proves the
    /// email; this wrapper never does). Seats `emails` at `role` (`"member"` / `"reviewer"` —
    /// anything else, owner included, is a typed denial) and returns the standing workspace door
    /// link. Idempotent per `request_id` (a canonical UUID).
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
        let (created_at, _now) = wire::now_utc();
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
            )
            .await
            .map_err(|error| anyhow::anyhow!("inviting members: {error}"))?;
        Ok(match outcome {
            SessionInviteOutcome::Invited {
                invite_token,
                seated,
            } => {
                let link_base = self.authority().enrollment_disclosure()?.link_base;
                InviteMembersSummary::Invited {
                    invite_link: format!("{link_base}/i/{invite_token}"),
                    seated,
                }
            }
            SessionInviteOutcome::Denied(reason) => InviteMembersSummary::Denied {
                reason: reason.to_owned(),
            },
        })
    }

    /// Remove a member from a session-verified OWNER email. Idempotent per `request_id`; the
    /// last-owner lockout denies typed; the removed principal's read tokens drop in the same
    /// transaction.
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
        let (created_at, _now) = wire::now_utc();
        let outcome = self
            .authority()
            .roster_remove_session(
                &ws,
                request_id,
                acting_email,
                target_email,
                mode,
                &created_at,
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

    /// Rotate the standing workspace door ("reset link") from a session-verified OWNER email.
    /// Blocks future redemption only; device-leg invite links are untouched. Idempotent per
    /// `request_id` (a replay re-derives the epoch it originally minted).
    ///
    /// # Errors
    /// An unparseable plane mode / workspace id (typed, fail closed) or a stringified authority
    /// fault.
    pub async fn rotate_join_link(
        &self,
        workspace_id: &str,
        request_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<RotateJoinLinkSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(RotateJoinLinkSummary::Denied {
                reason: "invalid workspace id".to_owned(),
            });
        };
        let (created_at, _now) = wire::now_utc();
        let outcome = self
            .authority()
            .rotate_join_link_session(&ws, request_id, acting_email, mode, &created_at)
            .await
            .map_err(|error| anyhow::anyhow!("rotating the join link: {error}"))?;
        Ok(match outcome {
            SessionRotateOutcome::Rotated { invite_token } => {
                let link_base = self.authority().enrollment_disclosure()?.link_base;
                RotateJoinLinkSummary::Rotated {
                    invite_link: format!("{link_base}/i/{invite_token}"),
                }
            }
            SessionRotateOutcome::Denied(reason) => RotateJoinLinkSummary::Denied {
                reason: reason.to_owned(),
            },
        })
    }

    /// Read the workspace roster for a session-verified email — seats for any confirmed member,
    /// the standing door link ONLY when the caller is a confirmed owner. Every miss is the
    /// uniform [`RosterSummary::NotFound`].
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
            Ok(view) => {
                let invite_link = match view.invite_token {
                    Some(token) => {
                        let link_base = self.authority().enrollment_disclosure()?.link_base;
                        Some(format!("{link_base}/i/{token}"))
                    }
                    None => None,
                };
                Ok(RosterSummary::Roster {
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
                    invite_link,
                })
            }
            Err(AuthorityError::NotFound) => Ok(RosterSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("reading the roster: {error}")),
        }
    }
}
