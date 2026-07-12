//! The web-session roster SQL — the raw-`sqlx` half (invite / remove / the roster read).
//!
//! Mirrors [`crate::session_roster`] exactly as [`super::governance`] mirrors [`crate::governance`]:
//! the `SERIALIZABLE` (`run_serializable!`) transactions live here, the orchestration hands in
//! server-trusted values (the parsed principals, the fresh session request identity, the public link
//! base for the disclosed address) and gets back domain outcomes. No `sqlx` type crosses the module
//! boundary; every row touched is `workspace_id`-scoped.
//!
//! The shared session preamble ([`session_gate`]) is the signature-FREE analogue of
//! [`super::governance`]'s `govern_preamble`: replay BEFORE authorization (through the same
//! `workspace_events` slot, so a device op id and a session request id fail closed against each
//! other), then the acting gate — the acting email must hold a CONFIRMED **owner** seat, read
//! in-transaction. Only a confirmed member's denial is ever recorded: a web-verified email proves
//! nothing about membership in the TARGET workspace, and recording a stranger's denial would let
//! any account grow an arbitrary workspace's ledger and squat its `(workspace_id, op_id)` slots.
//!
//! There is NO door here any more. An invitation is a roster write; what the ops disclose is the
//! workspace ADDRESS (`<link_base>/<name>`) — a plain name whose lock is the roster itself, so it is
//! safe to show any confirmed member and stable across replays (it re-reads the stored name).

use sqlx::{Postgres, Transaction};

use super::governance::{
    EventRecord, read_event, read_member_role, record_event_raw, seat_invited_members,
    would_orphan_owner,
};
use crate::db::Db;
use crate::error::{AuthorityError, Result};
use crate::governance::{GovernanceOutcome, Role, compose_address};
use crate::id::{Principal, WorkspaceId};
use crate::session_roster::{
    RosterSeat, RosterView, SESSION_ACTING_DENIED, SessionInput, SessionInviteOutcome,
    SessionInviteRole,
};

/// A session-corrupt stored state (a ledger/row invariant the schema cannot express failed).
#[derive(Debug, thiserror::Error)]
#[error("session roster state corrupt: {0}")]
struct SessionCorrupt(&'static str);

/// Read a workspace's ADDRESS name. `None` ⇒ no workspace row (folded into the uniform
/// acting-gate denial by the callers — indistinguishable from a non-member caller).
async fn read_workspace_name(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
) -> Result<Option<String>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT name AS "name!" FROM workspace WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| r.name))
}

/// The shared session gate: replay-check the request id, then resolve + authorize the acting
/// principal. `Replay` carries the stored row for the caller's per-op response re-derivation;
/// `Denied` carries whether the denial may be RECORDED (true only for a confirmed member).
enum SessionGate {
    /// A workspace_events hit for this request id.
    Replay {
        /// Did the stored request identity match this request's?
        matches: bool,
        /// The stored outcome (`"OK"` / `"DENIED"`).
        outcome: String,
        /// The stored details (what an OK replay re-derives its response from).
        details: Option<String>,
    },
    /// Authorized: the acting email holds a confirmed owner seat.
    Proceed,
    /// Denied. `record` is true ONLY for a confirmed (non-owner) member — never for a stranger.
    Denied { record: bool },
}

async fn session_gate(
    tx: &mut Transaction<'_, Postgres>,
    input: &SessionInput<'_>,
) -> Result<SessionGate> {
    // Replay BEFORE authorization (mirrors the device lane): a since-removed owner still replays
    // its committed OK; a cross-leg or divergent-payload id reuse fails closed on the identity.
    if let Some(stored) = read_event(tx, input.ws, input.request_id).await? {
        return Ok(SessionGate::Replay {
            matches: stored.request_sha256 == input.request_sha256,
            outcome: stored.outcome,
            details: stored.details,
        });
    }
    let Some((role, status)) = read_member_role(tx, input.ws, input.acting).await? else {
        return Ok(SessionGate::Denied { record: false });
    };
    if status != "confirmed" {
        return Ok(SessionGate::Denied { record: false });
    }
    if role != Role::Owner.as_str() {
        return Ok(SessionGate::Denied { record: true });
    }
    Ok(SessionGate::Proceed)
}

/// Record a session-leg receipt (actor = the acting principal's email, method `web_session`).
async fn record_session_event(
    tx: &mut Transaction<'_, Postgres>,
    input: &SessionInput<'_>,
    verb: &str,
    target: Option<&str>,
    outcome: &str,
    details: Option<&str>,
) -> Result<()> {
    record_event_raw(
        tx,
        &EventRecord {
            ws: input.ws,
            op_id: input.request_id,
            actor: input.acting.as_str(),
            gov_op_type: verb,
            request_sha256: &input.request_sha256,
            target,
            outcome,
            details,
            method: "web_session",
            created_at: input.created_at,
        },
    )
    .await
}

/// The stored invite receipt details — what an OK replay re-derives its byte-identical response
/// from (the address itself is NOT stored: it re-reads the live workspace name, so a rename never
/// serves a stale address). Encoded/decoded through `serde_json::Value` (this crate carries no
/// serde-derive).
struct InviteDetails {
    emails: usize,
    role: String,
}

impl InviteDetails {
    fn to_json(&self) -> String {
        serde_json::json!({ "emails": self.emails, "role": self.role }).to_string()
    }

    fn parse(details: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(details).ok()?;
        Some(InviteDetails {
            emails: usize::try_from(v.get("emails")?.as_u64()?).ok()?,
            role: v.get("role")?.as_str()?.to_owned(),
        })
    }
}

impl Db {
    /// `invite_members_session`: ONE `SERIALIZABLE` (`run_serializable!`) txn — replay → acting
    /// gate → seed the invited seats through the shared never-demote row-writer → receipt. The
    /// response discloses the workspace ADDRESS (re-read per call — it is a name, not a minted
    /// credential).
    pub(crate) async fn session_invite_txn(
        &self,
        input: &SessionInput<'_>,
        emails: &[Principal],
        role: SessionInviteRole,
        link_base: &str,
    ) -> Result<SessionInviteOutcome> {
        run_serializable!(
            self,
            tx,
            session_invite_run(&mut tx, input, emails, role, link_base).await
        )
    }

    /// `roster_remove_session`: ONE txn — replay → acting gate → last-owner-lockout guard → the
    /// device lane's exact instant-revoke shape (the lapse-detach reconcile + the seat drop) →
    /// receipt.
    pub(crate) async fn session_remove_txn(
        &self,
        input: &SessionInput<'_>,
        target: &Principal,
    ) -> Result<GovernanceOutcome> {
        run_serializable!(self, tx, session_remove_run(&mut tx, input, target).await)
    }

    /// The roster read — one read-only snapshot (a plain transaction; nothing is written): the
    /// seats + the workspace address for any confirmed member. Every miss is the single
    /// indistinguishable [`AuthorityError::NotFound`].
    pub(crate) async fn read_roster_view(
        &self,
        ws: &WorkspaceId,
        acting: &Principal,
        link_base: &str,
    ) -> Result<RosterView> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(AuthorityError::internal)?;
        let Some(name) = read_workspace_name(&mut tx, ws).await? else {
            return Err(AuthorityError::NotFound);
        };
        let Some((_, status)) = read_member_role(&mut tx, ws, acting).await? else {
            return Err(AuthorityError::NotFound);
        };
        if status != "confirmed" {
            return Err(AuthorityError::NotFound);
        }
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT principal AS "principal!", role AS "role!", status AS "status!",
                      added_at AS "added_at!"
               FROM workspace_member WHERE workspace_id = $1
               ORDER BY added_at, principal"#,
            ws_s,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        let seats = rows
            .into_iter()
            .map(|r| RosterSeat {
                email: r.principal,
                role: r.role,
                status: r.status,
                added_at: r.added_at,
            })
            .collect();
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(RosterView {
            seats,
            address: compose_address(link_base, &name),
        })
    }
}

async fn session_invite_run(
    tx: &mut Transaction<'_, Postgres>,
    input: &SessionInput<'_>,
    emails: &[Principal],
    // Threaded as the OWNER-LESS `SessionInviteRole`, not the full `Role`, so `Role::Owner` is
    // unrepresentable across the whole session-invite SQL path — the `member | reviewer` invariant
    // is enforced by the type, not by a single caller's discipline. Narrowed to `Role` only at the
    // `seat_invited_members` call site below.
    role: SessionInviteRole,
    link_base: &str,
) -> Result<SessionInviteOutcome> {
    let role = role.as_role();
    match session_gate(tx, input).await? {
        SessionGate::Replay {
            matches,
            outcome,
            details,
        } => {
            if !matches {
                return Ok(SessionInviteOutcome::Denied(
                    "op id reused with a different request",
                ));
            }
            if outcome != "OK" {
                return Ok(SessionInviteOutcome::Denied("replayed denial"));
            }
            let details = details.ok_or_else(|| {
                AuthorityError::integrity(SessionCorrupt("invite receipt has no details"))
            })?;
            let parsed = InviteDetails::parse(&details).ok_or_else(|| {
                AuthorityError::integrity(SessionCorrupt("invite receipt details unreadable"))
            })?;
            let name = read_workspace_name(tx, input.ws).await?.ok_or_else(|| {
                AuthorityError::integrity(SessionCorrupt("receipt names a missing workspace"))
            })?;
            return Ok(SessionInviteOutcome::Invited {
                address: compose_address(link_base, &name),
                seated: parsed.emails,
            });
        }
        SessionGate::Denied { record } => {
            if record {
                record_session_event(tx, input, "invite", None, "DENIED", None).await?;
            }
            return Ok(SessionInviteOutcome::Denied(SESSION_ACTING_DENIED));
        }
        SessionGate::Proceed => {}
    }
    // The acting gate passed but the workspace row itself must exist (nothing FKs `workspace`, so
    // a seeded roster without one is representable) — fold the miss into the same uniform denial.
    let Some(name) = read_workspace_name(tx, input.ws).await? else {
        return Ok(SessionInviteOutcome::Denied(SESSION_ACTING_DENIED));
    };
    seat_invited_members(
        tx,
        input.ws,
        emails,
        role,
        input.acting.as_str(),
        input.created_at,
    )
    .await?;
    let details = InviteDetails {
        emails: emails.len(),
        role: role.as_str().to_owned(),
    }
    .to_json();
    record_session_event(tx, input, "invite", None, "OK", Some(&details)).await?;
    Ok(SessionInviteOutcome::Invited {
        address: compose_address(link_base, &name),
        seated: emails.len(),
    })
}

async fn session_remove_run(
    tx: &mut Transaction<'_, Postgres>,
    input: &SessionInput<'_>,
    target: &Principal,
) -> Result<GovernanceOutcome> {
    match session_gate(tx, input).await? {
        SessionGate::Replay {
            matches, outcome, ..
        } => {
            let replay = if !matches {
                GovernanceOutcome::Denied("op id reused with a different request")
            } else if outcome == "OK" {
                GovernanceOutcome::Ok
            } else {
                GovernanceOutcome::Denied("replayed denial")
            };
            return Ok(replay);
        }
        SessionGate::Denied { record } => {
            if record {
                record_session_event(
                    tx,
                    input,
                    "roster_remove",
                    Some(target.as_str()),
                    "DENIED",
                    None,
                )
                .await?;
            }
            return Ok(GovernanceOutcome::Denied(SESSION_ACTING_DENIED));
        }
        SessionGate::Proceed => {}
    }
    if would_orphan_owner(tx, input.ws, target.as_str(), None).await? {
        record_session_event(
            tx,
            input,
            "roster_remove",
            Some(target.as_str()),
            "DENIED",
            None,
        )
        .await?;
        return Ok(GovernanceOutcome::Denied("would remove the last owner"));
    }
    // The device lane's exact removal shape: drop the membership — the row every read/write gate
    // joins against, so access dies the moment this commits (the person's devices and their
    // credentials stay: re-adding the member re-enables them, the git/GitHub model) — and, in the
    // same transaction, the lapse-detach reconcile — which runs FIRST, since the entitlement union
    // is membership-gated and reads empty once the seat is gone: everything the person received
    // lapses at once, writing their detachment records + freezing their devices' fleet rows at
    // last-applied ("removed — last known state", the fleet page's blind-spot list).
    let (ws_s, tgt) = (input.ws.as_str(), target.as_str());
    sqlx::query!(
        r#"SELECT topos_detach_on_removal($1, $2, $3, $4) AS "n!""#,
        ws_s,
        tgt,
        input.now,
        input.created_at,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "DELETE FROM workspace_member WHERE workspace_id = $1 AND principal = $2",
        ws_s,
        tgt,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    record_session_event(
        tx,
        input,
        "roster_remove",
        Some(target.as_str()),
        "OK",
        None,
    )
    .await?;
    Ok(GovernanceOutcome::Ok)
}
