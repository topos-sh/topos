//! The web-session roster leg — the orchestration half (outside the transaction).
//!
//! The hosted cloud's "manage the team in settings" surface: a composing plane whose WEB layer has
//! verified a session email calls these PRIVILEGED lib-level ops (there is no OSS HTTP route) to
//! invite members, remove them, and read the roster. Split from
//! [`crate::governance`] the way that module split from [`crate::enroll`]: this file models the
//! session ops and does the work OUTSIDE the one write transaction (posture gate, request-id parse,
//! email parse, the deterministic request identity); the raw SQL — and the `SERIALIZABLE`
//! (`run_serializable!`) transactions — live in [`crate::db`].
//!
//! An invitation is a ROSTER WRITE and nothing more: the tokened invite link (and its standing-door
//! rotation) is gone — the workspace ADDRESS is what these ops disclose, and it is just a name (links
//! carry nothing; the roster is the lock, so the address is safe to show any confirmed member).
//!
//! The trust shape mirrors [`Authority::create_workspace`](crate::Authority::create_workspace), not
//! the device governance ops: there is no signature to verify — the composing caller's own
//! session verification is the authentication, and the ACTING GATE (the acting email must hold a
//! confirmed **owner** seat, checked in-transaction) is the authorization. Every mutation is
//! `request_id`-idempotent through the same `workspace_events` slot the device lane uses, with a
//! FRESH domain-tagged request identity (the kernel governance preimage needs a signature frame no
//! session op has), so a device op id and a session request id can never replay each other — a
//! cross-leg id collision always fails closed as a key reuse. All ops (the read included) are
//! uniformly denied on a self-host plane: self-host joining stays the device lane.

use crate::authority::Authority;
use crate::enroll::{DeploymentMode, parse_op_id};
use crate::error::{AuthorityError, Result};
use crate::governance::{GovernanceOutcome, Role};
use crate::id::{Principal, WorkspaceId};

/// The domain tag of the session-leg request identity (`request_sha256`) — versioned, and distinct
/// from every kernel signing-frame tag, so a stored device-lane event can never byte-match a
/// session request (and vice versa).
const SESSION_REQUEST_TAG: &[u8] = b"TOPOS_SESSION_ROSTER_V1\0";

/// The most invited addresses one session invite accepts (a composing route should cap earlier;
/// this is the in-op belt).
const MAX_SESSION_INVITE_EMAILS: usize = 20;

/// The ONE uniform acting-gate denial: a non-member, a merely-invited seat, an absent workspace,
/// and a confirmed non-owner all read the same (the static reason is for server logs, never an
/// oracle — and only a CONFIRMED member's denial is ever recorded).
pub(crate) const SESSION_ACTING_DENIED: &str = "session roster ops require a confirmed owner";

/// The role a session invite may seat — owner is unrepresentable by construction (an owner-role
/// grant stays a web roster-set ceremony, never an invitation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionInviteRole {
    /// An ordinary member.
    Member,
    /// A reviewer (the review-gate role).
    Reviewer,
}

impl SessionInviteRole {
    /// Parse a wire discriminant (`"member"` / `"reviewer"`). `None` on anything else — including
    /// `"owner"`, which this type deliberately cannot express.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "member" => Some(SessionInviteRole::Member),
            "reviewer" => Some(SessionInviteRole::Reviewer),
            _ => None,
        }
    }

    /// The workspace role this seats.
    pub(crate) fn as_role(self) -> Role {
        match self {
            SessionInviteRole::Member => Role::Member,
            SessionInviteRole::Reviewer => Role::Reviewer,
        }
    }
}

/// The outcome of [`Authority::invite_members_session`]. The disclosed `address` is just the
/// workspace's name under the public link base — a plain fact, not a credential (the roster is the
/// lock), so nothing here needs redaction.
#[derive(Debug, Clone)]
pub enum SessionInviteOutcome {
    /// The seats are seeded (or the identical request replayed).
    Invited {
        /// The workspace ADDRESS the invitees join at (`follow <address>` + proof of the invited email).
        address: String,
        /// How many distinct addresses were seated.
        seated: usize,
    },
    /// The request was denied (a uniform denial; the static reason is for server logs).
    Denied(&'static str),
}

/// One workspace seat, as the roster read discloses it.
#[derive(Debug, Clone)]
pub struct RosterSeat {
    /// The seat's principal (the email).
    pub email: String,
    /// The stored role discriminant (`owner` / `reviewer` / `member`).
    pub role: String,
    /// The enrollment lifecycle (`invited` / `confirmed`).
    pub status: String,
    /// When the seat row was added (ISO-8601; the seat table's only timestamp).
    pub added_at: String,
}

/// The roster read's disclosure: every seat, plus the workspace ADDRESS — member-visible (it is a
/// name, not a door; joining still gates on the roster).
#[derive(Debug, Clone)]
pub struct RosterView {
    /// The workspace's seats (invited and confirmed), ordered by `added_at` then email.
    pub seats: Vec<RosterSeat>,
    /// The workspace's full address (`<link_base>/<name>`).
    pub address: String,
}

/// The server-trusted inputs to a session-leg transaction.
pub(crate) struct SessionInput<'a> {
    /// The target workspace.
    pub(crate) ws: &'a WorkspaceId,
    /// The caller-minted request id (the idempotency key — a canonical UUID, idempotency-ONLY,
    /// never a derivation input).
    pub(crate) request_id: &'a str,
    /// The fresh domain-tagged request identity (what the `workspace_events` slot binds).
    pub(crate) request_sha256: [u8; 32],
    /// The acting principal (the web-verified session email).
    pub(crate) acting: &'a Principal,
    /// The server-stamped creation timestamp.
    pub(crate) created_at: &'a str,
    /// The server clock (epoch ms) — the member-removal detach reconcile stamps `detached_at` with it.
    pub(crate) now: i64,
}

/// The session request identity: sha256 over the versioned domain tag + u64-be length-prefixed
/// parts (verb, workspace, acting email, then the op payload). Deterministic — a lost-ack retry
/// recomputes the identical identity; any divergent payload under a reused request id mismatches.
fn session_request_sha256(
    verb: &str,
    ws: &WorkspaceId,
    acting: &Principal,
    payload: &[&[u8]],
) -> [u8; 32] {
    let head = [
        verb.as_bytes(),
        ws.as_str().as_bytes(),
        acting.as_str().as_bytes(),
    ];
    let mut buf = Vec::with_capacity(
        SESSION_REQUEST_TAG.len()
            + head
                .iter()
                .chain(payload)
                .map(|p| p.len() + 8)
                .sum::<usize>(),
    );
    buf.extend_from_slice(SESSION_REQUEST_TAG);
    for part in head.iter().copied().chain(payload.iter().copied()) {
        buf.extend_from_slice(&(part.len() as u64).to_be_bytes());
        buf.extend_from_slice(part);
    }
    topos_core::digest::sha256(&buf)
}

/// Invite members from a verified owner session (the orchestration half of
/// [`Authority::invite_members_session`]). Parses everything INSIDE the op, dedupes the invited
/// set (the deterministic payload identity), and runs the one transaction (replay → acting gate →
/// seat → receipt). What comes back is the workspace ADDRESS, not a link: the seats are the
/// invitation.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn invite_members_session(
    authority: &Authority,
    ws: &WorkspaceId,
    request_id: &str,
    acting_email: &str,
    emails: &[String],
    role: SessionInviteRole,
    plane_mode: DeploymentMode,
    created_at: &str,
    now: i64,
) -> Result<SessionInviteOutcome> {
    if plane_mode == DeploymentMode::SelfHost {
        return Ok(SessionInviteOutcome::Denied(
            "session roster ops are cloud-only",
        ));
    }
    if parse_op_id(request_id).is_none() {
        return Ok(SessionInviteOutcome::Denied(
            "request_id is not a canonical UUID",
        ));
    }
    let Ok(acting) = Principal::parse(acting_email) else {
        return Ok(SessionInviteOutcome::Denied("invalid acting email"));
    };
    if emails.is_empty() {
        return Ok(SessionInviteOutcome::Denied("no invited emails"));
    }
    if emails.len() > MAX_SESSION_INVITE_EMAILS {
        return Ok(SessionInviteOutcome::Denied("too many invited emails"));
    }
    let mut invited = Vec::with_capacity(emails.len());
    for email in emails {
        match Principal::parse(email) {
            Ok(p) => invited.push(p),
            Err(_) => return Ok(SessionInviteOutcome::Denied("invalid invited email")),
        }
    }
    invited.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    invited.dedup_by(|a, b| a.as_str() == b.as_str());

    let joined = invited
        .iter()
        .map(Principal::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let role_byte = [role.as_role().derivation_byte()];
    let request_sha256 = session_request_sha256(
        "invite",
        ws,
        &acting,
        &[role_byte.as_slice(), joined.as_bytes()],
    );
    let link_base = authority.enrollment()?.config.link_base().to_owned();
    let input = SessionInput {
        ws,
        request_id,
        request_sha256,
        acting: &acting,
        created_at,
        now,
    };
    authority
        .db()
        .session_invite_txn(&input, &invited, role, &link_base)
        .await
}

/// Remove a member from a verified owner session (the orchestration half of
/// [`Authority::roster_remove_session`]). Reuses the device lane's last-owner-lockout guard and
/// its exact instant-revoke transaction shape (the lapse-detach reconcile + the seat drop in one txn).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn roster_remove_session(
    authority: &Authority,
    ws: &WorkspaceId,
    request_id: &str,
    acting_email: &str,
    target_email: &str,
    plane_mode: DeploymentMode,
    created_at: &str,
    now: i64,
) -> Result<GovernanceOutcome> {
    if plane_mode == DeploymentMode::SelfHost {
        return Ok(GovernanceOutcome::Denied(
            "session roster ops are cloud-only",
        ));
    }
    if parse_op_id(request_id).is_none() {
        return Ok(GovernanceOutcome::Denied(
            "request_id is not a canonical UUID",
        ));
    }
    let Ok(acting) = Principal::parse(acting_email) else {
        return Ok(GovernanceOutcome::Denied("invalid acting email"));
    };
    let Ok(target) = Principal::parse(target_email) else {
        return Ok(GovernanceOutcome::Denied("invalid target email"));
    };
    let request_sha256 =
        session_request_sha256("roster_remove", ws, &acting, &[target.as_str().as_bytes()]);
    let input = SessionInput {
        ws,
        request_id,
        request_sha256,
        acting: &acting,
        created_at,
        now,
    };
    authority.db().session_remove_txn(&input, &target).await
}

/// Read the workspace roster for a verified session (the orchestration half of
/// [`Authority::read_roster`]). A pure read — no receipt, no idempotency slot. Every miss (a
/// self-host plane, an absent workspace, an acting email that is not a confirmed member) is the
/// single indistinguishable [`AuthorityError::NotFound`]; the workspace ADDRESS is disclosed to any
/// confirmed member (it is a name, not a door).
pub(crate) async fn read_roster(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<RosterView> {
    if plane_mode == DeploymentMode::SelfHost {
        return Err(AuthorityError::NotFound);
    }
    let acting = Principal::parse(acting_email).map_err(|_| AuthorityError::NotFound)?;
    let link_base = authority.enrollment()?.config.link_base().to_owned();
    authority
        .db()
        .read_roster_view(ws, &acting, &link_base)
        .await
}

#[cfg(test)]
mod tests {
    use super::{SessionInviteRole, session_request_sha256};
    use crate::id::{Principal, WorkspaceId};

    #[test]
    fn owner_is_unrepresentable_as_a_session_invite_role() {
        assert_eq!(
            SessionInviteRole::parse("member"),
            Some(SessionInviteRole::Member)
        );
        assert_eq!(
            SessionInviteRole::parse("reviewer"),
            Some(SessionInviteRole::Reviewer)
        );
        assert_eq!(SessionInviteRole::parse("owner"), None);
        assert_eq!(SessionInviteRole::parse("Owner"), None);
        assert_eq!(SessionInviteRole::parse(""), None);
    }

    #[test]
    fn session_request_identity_is_deterministic_and_payload_bound() {
        let ws = WorkspaceId::parse("w_1234").expect("workspace id");
        let acting = Principal::parse("owner@acme.com").expect("principal");
        let a = session_request_sha256("invite", &ws, &acting, &[b"x"]);
        let b = session_request_sha256("invite", &ws, &acting, &[b"x"]);
        let c = session_request_sha256("invite", &ws, &acting, &[b"y"]);
        let d = session_request_sha256("roster_remove", &ws, &acting, &[b"x"]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn part_boundaries_cannot_be_shifted_between_payload_parts() {
        let ws = WorkspaceId::parse("w_1234").expect("workspace id");
        let acting = Principal::parse("owner@acme.com").expect("principal");
        // Length-prefixing means ["ab", "c"] and ["a", "bc"] hash differently.
        let a = session_request_sha256("invite", &ws, &acting, &[b"ab", b"c"]);
        let b = session_request_sha256("invite", &ws, &acting, &[b"a", b"bc"]);
        assert_ne!(a, b);
    }
}
