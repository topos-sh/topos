//! The web-session roster SQL — the raw-`sqlx` half (invite / remove / rotate / the roster read).
//!
//! Mirrors [`crate::session_roster`] exactly as [`super::governance`] mirrors [`crate::governance`]:
//! the `SERIALIZABLE` (`run_serializable!`) transactions live here, the orchestration hands in
//! server-trusted values (the parsed principals, the fresh session request identity, the enrollment
//! secret for the in-transaction door derivations) and gets back domain outcomes. No `sqlx` type
//! crosses the module boundary; every row touched is `workspace_id`-scoped.
//!
//! The shared session preamble ([`session_gate`]) is the signature-FREE analogue of
//! [`super::governance`]'s `govern_preamble`: replay BEFORE authorization (through the same
//! `workspace_events` slot, so a device op id and a session request id fail closed against each
//! other), then the acting gate — the acting email must hold a CONFIRMED **owner** seat, read
//! in-transaction. Only a confirmed member's denial is ever recorded: a web-verified email proves
//! nothing about membership in the TARGET workspace, and recording a stranger's denial would let
//! any account grow an arbitrary workspace's ledger and squat its `(workspace_id, op_id)` slots.
//!
//! THE DOOR FAMILY. The standing membership door is deterministic — `door_token(secret, ws,
//! link_epoch)` — so it is re-showable without ever storing plaintext. A create-page-born
//! workspace's door at epoch 0 is its GENESIS SELF-INVITE (re-derived through `genesis_requests`);
//! a standup/claim-born workspace has no door until a session op mints `door(0)` lazily. Rotation
//! revokes the whole standing family (the epoch door AND the genesis row) and bumps the epoch —
//! it deliberately does NOT touch invite links minted on the device leg, which are separate
//! invites managed there.

use sqlx::{Postgres, Transaction};

use super::Db;
use super::governance::{
    EventRecord, mint_invite_row, read_event, read_member_role, record_event_raw,
    self_invite_token, would_orphan_owner,
};
use crate::enroll::{self};
use crate::error::{AuthorityError, Result};
use crate::governance::{GovernanceOutcome, Role};
use crate::id::{Principal, WorkspaceId};
use crate::session_roster::{
    RosterSeat, RosterView, SESSION_ACTING_DENIED, SessionInput, SessionInviteOutcome,
    SessionInviteRole, SessionRotateOutcome,
};

/// The standing-door token for `(workspace, link_epoch)` — the one derivation `invite`, `rotate`,
/// and the roster read all call, so every surface agrees on the door by construction. The `door`
/// domain tag is prefix-free against the existing `invite`/`grant`/`readtoken` tags.
fn door_token(secret: &[u8; 32], ws: &WorkspaceId, link_epoch: i64) -> String {
    let epoch_be = link_epoch.to_be_bytes();
    enroll::derive_token(
        secret,
        b"door",
        &[ws.as_str().as_bytes(), epoch_be.as_slice()],
    )
}

/// Which standing door a workspace currently has (the receipt `details` marker a replay
/// re-derives from).
enum DoorKind {
    /// The epoch-derived door.
    Epoch(i64),
    /// The create-page genesis self-invite (epoch 0, pre-rotation).
    Genesis,
}

impl DoorKind {
    fn marker(&self) -> String {
        match self {
            DoorKind::Epoch(n) => n.to_string(),
            DoorKind::Genesis => "genesis".to_owned(),
        }
    }
}

/// A session-corrupt stored state (a ledger/row invariant the schema cannot express failed).
#[derive(Debug, thiserror::Error)]
#[error("session roster state corrupt: {0}")]
struct SessionCorrupt(&'static str);

/// Read a workspace's `link_epoch`. `None` ⇒ no workspace row (folded into the uniform
/// acting-gate denial by the callers — indistinguishable from a non-member caller).
async fn read_link_epoch(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
) -> Result<Option<i64>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT link_epoch AS "link_epoch!: i64" FROM workspace WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| r.link_epoch))
}

/// Is there an UNREVOKED invites row for this token sha in this workspace?
async fn door_row_stands(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    token_sha256: &[u8; 32],
) -> Result<bool> {
    let (ws_s, tok) = (ws.as_str(), token_sha256.as_slice());
    let row = sqlx::query!(
        r#"SELECT revoked AS "revoked!: i64" FROM invites
           WHERE token_sha256 = $1 AND workspace_id = $2"#,
        tok,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some_and(|r| r.revoked == 0))
}

/// The workspace's genesis request identity, when it was born through `create_workspace` (the
/// create page). Standup- and claim-born workspaces have none.
async fn read_genesis_request(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
) -> Result<Option<[u8; 32]>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT request_sha256 AS "request_sha256!: Vec<u8>" FROM genesis_requests
           WHERE workspace_id = $1 LIMIT 1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(super::blob32(&r.request_sha256)?)),
    }
}

/// Resolve the CURRENT standing door, if one stands: the epoch door when its row exists unrevoked,
/// else (at epoch 0 only) the genesis self-invite. At most one of the two can stand — rotation
/// revokes both and bumps the epoch in the same transaction.
async fn current_door(
    tx: &mut Transaction<'_, Postgres>,
    secret: &[u8; 32],
    ws: &WorkspaceId,
    link_epoch: i64,
) -> Result<Option<(String, DoorKind)>> {
    let epoch_token = door_token(secret, ws, link_epoch);
    if door_row_stands(tx, ws, &enroll::sha256_token(&epoch_token)).await? {
        return Ok(Some((epoch_token, DoorKind::Epoch(link_epoch))));
    }
    if link_epoch == 0
        && let Some(genesis_sha) = read_genesis_request(tx, ws).await?
    {
        let genesis_token = self_invite_token(secret, &genesis_sha, ws);
        if door_row_stands(tx, ws, &enroll::sha256_token(&genesis_token)).await? {
            return Ok(Some((genesis_token, DoorKind::Genesis)));
        }
    }
    Ok(None)
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
/// from (the door marker names WHICH door the original call returned). Encoded/decoded through
/// `serde_json::Value` (this crate carries no serde-derive).
struct InviteDetails {
    door: String,
    emails: usize,
    role: String,
}

impl InviteDetails {
    fn to_json(&self) -> String {
        serde_json::json!({ "door": self.door, "emails": self.emails, "role": self.role })
            .to_string()
    }

    fn parse(details: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(details).ok()?;
        Some(InviteDetails {
            door: v.get("door")?.as_str()?.to_owned(),
            emails: usize::try_from(v.get("emails")?.as_u64()?).ok()?,
            role: v.get("role")?.as_str()?.to_owned(),
        })
    }
}

/// The stored rotate receipt details.
struct RotateDetails {
    link_epoch: i64,
}

impl RotateDetails {
    fn to_json(&self) -> String {
        serde_json::json!({ "link_epoch": self.link_epoch }).to_string()
    }

    fn parse(details: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(details).ok()?;
        Some(RotateDetails {
            link_epoch: v.get("link_epoch")?.as_i64()?,
        })
    }
}

/// Re-derive a door token from its receipt marker (`"genesis"` or the epoch digits).
async fn door_from_marker(
    tx: &mut Transaction<'_, Postgres>,
    secret: &[u8; 32],
    ws: &WorkspaceId,
    marker: &str,
) -> Result<String> {
    if marker == "genesis" {
        let genesis_sha = read_genesis_request(tx, ws).await?.ok_or_else(|| {
            AuthorityError::integrity(SessionCorrupt(
                "receipt names a genesis door but no genesis request exists",
            ))
        })?;
        return Ok(self_invite_token(secret, &genesis_sha, ws));
    }
    let epoch: i64 = marker.parse().map_err(|_| {
        AuthorityError::integrity(SessionCorrupt("receipt door marker is not an epoch"))
    })?;
    Ok(door_token(secret, ws, epoch))
}

impl Db {
    /// `invite_members_session`: ONE `SERIALIZABLE` (`run_serializable!`) txn — replay → acting
    /// gate → ensure the standing door exists (lazy epoch mint for door-less workspaces) → seed
    /// the invited seats through the shared never-demote row-writer → receipt.
    pub(crate) async fn session_invite_txn(
        &self,
        input: &SessionInput<'_>,
        emails: &[Principal],
        role: SessionInviteRole,
        secret: &[u8; 32],
    ) -> Result<SessionInviteOutcome> {
        run_serializable!(
            self,
            tx,
            session_invite_run(&mut tx, input, emails, role, secret).await
        )
    }

    /// `roster_remove_session`: ONE txn — replay → acting gate → last-owner-lockout guard → the
    /// device lane's exact instant-revoke shape (membership + per-skill roster + read tokens) →
    /// receipt.
    pub(crate) async fn session_remove_txn(
        &self,
        input: &SessionInput<'_>,
        target: &Principal,
    ) -> Result<GovernanceOutcome> {
        run_serializable!(self, tx, session_remove_run(&mut tx, input, target).await)
    }

    /// `rotate_join_link_session`: ONE txn — replay → acting gate → revoke the standing door
    /// family (the epoch door AND the genesis self-invite, whichever stand) → bump `link_epoch` →
    /// mint the new door → receipt.
    pub(crate) async fn session_rotate_txn(
        &self,
        input: &SessionInput<'_>,
        secret: &[u8; 32],
    ) -> Result<SessionRotateOutcome> {
        run_serializable!(self, tx, session_rotate_run(&mut tx, input, secret).await)
    }

    /// The roster read — one read-only snapshot (a plain transaction; nothing is written): the
    /// seats for any confirmed member, the standing door token for a confirmed OWNER only. Every
    /// miss is the single indistinguishable [`AuthorityError::NotFound`].
    pub(crate) async fn read_roster_view(
        &self,
        ws: &WorkspaceId,
        acting: &Principal,
        secret: &[u8; 32],
    ) -> Result<RosterView> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(AuthorityError::internal)?;
        let Some(link_epoch) = read_link_epoch(&mut tx, ws).await? else {
            return Err(AuthorityError::NotFound);
        };
        let Some((role, status)) = read_member_role(&mut tx, ws, acting).await? else {
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
        let invite_token = if role == Role::Owner.as_str() {
            current_door(&mut tx, secret, ws, link_epoch)
                .await?
                .map(|(token, _)| token)
        } else {
            None
        };
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(RosterView {
            seats,
            invite_token,
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
    // `mint_invite_row` call site below.
    role: SessionInviteRole,
    secret: &[u8; 32],
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
            let token = door_from_marker(tx, secret, input.ws, &parsed.door).await?;
            return Ok(SessionInviteOutcome::Invited {
                invite_token: token,
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
    let Some(link_epoch) = read_link_epoch(tx, input.ws).await? else {
        return Ok(SessionInviteOutcome::Denied(SESSION_ACTING_DENIED));
    };
    let (token, kind) = match current_door(tx, secret, input.ws, link_epoch).await? {
        Some(door) => door,
        // No door stands (a standup/claim-born workspace before its first session op): mint the
        // epoch door lazily. The mint below is the ensure (ON CONFLICT DO NOTHING).
        None => (
            door_token(secret, input.ws, link_epoch),
            DoorKind::Epoch(link_epoch),
        ),
    };
    let door_sha = enroll::sha256_token(&token);
    mint_invite_row(
        tx,
        input.ws,
        &door_sha,
        None,
        input.acting.as_str(),
        role,
        emails,
        &[],
        input.created_at,
    )
    .await?;
    let details = InviteDetails {
        door: kind.marker(),
        emails: emails.len(),
        role: role.as_str().to_owned(),
    }
    .to_json();
    record_session_event(tx, input, "invite", None, "OK", Some(&details)).await?;
    Ok(SessionInviteOutcome::Invited {
        invite_token: token,
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
    // The device lane's exact instant-revoke shape: drop the membership AND, in the same
    // transaction, every per-skill roster grant + read token the principal holds here.
    let (ws_s, tgt) = (input.ws.as_str(), target.as_str());
    sqlx::query!(
        "DELETE FROM workspace_member WHERE workspace_id = $1 AND principal = $2",
        ws_s,
        tgt,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "DELETE FROM roster WHERE workspace_id = $1 AND principal = $2",
        ws_s,
        tgt,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "DELETE FROM read_token WHERE workspace_id = $1 AND principal = $2",
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

async fn session_rotate_run(
    tx: &mut Transaction<'_, Postgres>,
    input: &SessionInput<'_>,
    secret: &[u8; 32],
) -> Result<SessionRotateOutcome> {
    match session_gate(tx, input).await? {
        SessionGate::Replay {
            matches,
            outcome,
            details,
        } => {
            if !matches {
                return Ok(SessionRotateOutcome::Denied(
                    "op id reused with a different request",
                ));
            }
            if outcome != "OK" {
                return Ok(SessionRotateOutcome::Denied("replayed denial"));
            }
            let details = details.ok_or_else(|| {
                AuthorityError::integrity(SessionCorrupt("rotate receipt has no details"))
            })?;
            let parsed = RotateDetails::parse(&details).ok_or_else(|| {
                AuthorityError::integrity(SessionCorrupt("rotate receipt details unreadable"))
            })?;
            // The epoch-PINNED replay: byte-identical to the original response, even if later
            // rotations have since revoked that door (correct lost-ack semantics).
            return Ok(SessionRotateOutcome::Rotated {
                invite_token: door_token(secret, input.ws, parsed.link_epoch),
            });
        }
        SessionGate::Denied { record } => {
            if record {
                record_session_event(tx, input, "link_rotate", None, "DENIED", None).await?;
            }
            return Ok(SessionRotateOutcome::Denied(SESSION_ACTING_DENIED));
        }
        SessionGate::Proceed => {}
    }
    let Some(link_epoch) = read_link_epoch(tx, input.ws).await? else {
        return Ok(SessionRotateOutcome::Denied(SESSION_ACTING_DENIED));
    };
    // Revoke the WHOLE standing family — the epoch door and the genesis self-invite, whichever
    // stand (idempotent row flips; blocking future redemption only, never severing an
    // already-exchanged credential). Device-leg invite links are deliberately untouched.
    let epoch_sha = enroll::sha256_token(&door_token(secret, input.ws, link_epoch));
    revoke_invite_row(tx, input.ws, &epoch_sha).await?;
    if let Some(genesis_sha) = read_genesis_request(tx, input.ws).await? {
        let genesis_door_sha =
            enroll::sha256_token(&self_invite_token(secret, &genesis_sha, input.ws));
        revoke_invite_row(tx, input.ws, &genesis_door_sha).await?;
    }
    let new_epoch = link_epoch
        .checked_add(1)
        .ok_or_else(|| AuthorityError::integrity(SessionCorrupt("link epoch overflow")))?;
    let ws_s = input.ws.as_str();
    let updated = sqlx::query!(
        "UPDATE workspace SET link_epoch = $2 WHERE workspace_id = $1",
        ws_s,
        new_epoch,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if updated.rows_affected() != 1 {
        return Err(AuthorityError::integrity(SessionCorrupt(
            "rotate found no workspace row to bump",
        )));
    }
    let new_token = door_token(secret, input.ws, new_epoch);
    mint_invite_row(
        tx,
        input.ws,
        &enroll::sha256_token(&new_token),
        None,
        input.acting.as_str(),
        Role::Member,
        &[],
        &[],
        input.created_at,
    )
    .await?;
    let details = RotateDetails {
        link_epoch: new_epoch,
    }
    .to_json();
    record_session_event(tx, input, "link_rotate", None, "OK", Some(&details)).await?;
    Ok(SessionRotateOutcome::Rotated {
        invite_token: new_token,
    })
}

/// Flip an invites row's `revoked` kill switch (workspace-bound; a no-op when the row is absent).
async fn revoke_invite_row(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    token_sha256: &[u8; 32],
) -> Result<()> {
    let (ws_s, tok) = (ws.as_str(), token_sha256.as_slice());
    sqlx::query!(
        "UPDATE invites SET revoked = 1 WHERE token_sha256 = $1 AND workspace_id = $2",
        tok,
        ws_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}
