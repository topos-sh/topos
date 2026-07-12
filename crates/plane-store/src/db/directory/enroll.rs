//! The enrollment issuance SQL — the raw-`sqlx` half (every `query!` for the issuance core).
//!
//! Mirrors [`crate::db::custody::set_current`]: the `SERIALIZABLE` (`run_serializable!`) poll/confirm/redeem transactions live here, the
//! orchestration ([`crate::enroll`]) hands in server-trusted values (the rehashed grant, the re-derived
//! device key id) plus the enrollment secret and gets back domain outcomes. No `sqlx` type crosses the
//! module boundary. Every row is `workspace_id`-scoped; every opaque credential is matched only by its
//! stored sha256; the confirmed principal is read from a server-trusted row, never parsed from a claim.
//! The governance + admin-claim SQL is split into [`super::governance`] (which shares this file's
//! device-row / 32-byte-blob helpers).

use sqlx::{Postgres, Transaction};
use topos_core::digest;

use crate::db::{Db, blob32};
use crate::enroll::{
    self, ConfirmOutcome, DeviceAuthPoll, EnrollmentRedeemed, GrantIssued, LoginOutcome,
    LoginRedeemed, LoginSeat, PasscodeComplete, RedeemInput, RedeemOutcome,
};
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, WorkspaceId};

// ── pool reads (the orchestration classifies on these) ─────────────────────────────────────────────────

/// A `workspace` row (the address name + deployment posture + display fields).
pub(crate) struct WorkspaceRow {
    pub(crate) name: String,
    pub(crate) display_name: String,
    pub(crate) verified_domain: Option<String>,
    pub(crate) verified_domain_status: String,
}

impl Db {
    /// Read a workspace row (address name + posture + display fields). `None` if the workspace does not exist.
    pub(crate) async fn read_workspace(&self, ws: &WorkspaceId) -> Result<Option<WorkspaceRow>> {
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT name AS "name!", display_name AS "display_name!", verified_domain AS "verified_domain?",
                      verified_domain_status AS "verified_domain_status!"
               FROM workspace WHERE workspace_id = $1"#,
            ws_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| WorkspaceRow {
            name: r.name,
            display_name: r.display_name,
            verified_domain: r.verified_domain,
            verified_domain_status: r.verified_domain_status,
        }))
    }

    /// Resolve a workspace ADDRESS name to its id (the enroll-by-address lookup). `None` when no
    /// workspace holds the name — the caller opens the session anyway (resolution is never disclosed
    /// on the authorize route).
    pub(crate) async fn workspace_id_by_name(&self, name: &str) -> Result<Option<WorkspaceId>> {
        let row = sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!" FROM workspace WHERE name = $1"#,
            name,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| WorkspaceId::parse(&r.workspace_id).map_err(AuthorityError::integrity))
            .transpose()
    }

    /// Whether a LIVE (pending/confirmed) session already holds `user_code` (the start loop avoids a clash
    /// against the partial-unique index).
    pub(crate) async fn live_user_code_exists(&self, user_code: &str) -> Result<bool> {
        let row = sqlx::query!(
            r#"SELECT 1::int8 AS "ok!: i64" FROM device_auth_sessions
               WHERE user_code = $1 AND status IN ('pending', 'confirmed') LIMIT 1"#,
            user_code,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }

    /// Insert a fresh device-auth session — always born `pending` (identity proof is a passcode or a
    /// web-session approval on every posture). An ENROLL session records the REQUESTED address name
    /// verbatim plus the resolved workspace id when the name resolved; a STANDUP/LOGIN session carries
    /// neither. Stores ONLY the device code's sha256; the user code plaintext.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn insert_device_auth_session(
        &self,
        device_code_sha256: &[u8; 32],
        user_code: &str,
        ws: Option<&WorkspaceId>,
        requested_workspace: Option<&str>,
        device_pubkey: &[u8; 32],
        device_key_id: &str,
        machine_name: &str,
        intent: &str,
        status: &str,
        confirmed_principal: Option<&str>,
        expires_at: i64,
        interval_secs: i64,
        created_at: &str,
    ) -> Result<()> {
        let (dc, ws_s, pk) = (
            device_code_sha256.as_slice(),
            ws.map(WorkspaceId::as_str),
            device_pubkey.as_slice(),
        );
        sqlx::query!(
            "INSERT INTO device_auth_sessions \
               (device_code_sha256, user_code, workspace_id, requested_workspace, device_pubkey, device_key_id, \
                machine_name, intent, status, confirmed_principal, expires_at, interval_secs, last_polled_at, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, NULL, $13)",
            dc,
            user_code,
            ws_s,
            requested_workspace,
            pk,
            device_key_id,
            machine_name,
            intent,
            status,
            confirmed_principal,
            expires_at,
            interval_secs,
            created_at,
        )
        .execute(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }
}

// ── poll: classify + (touch | issue grant), all in one transaction ─────────────────────────────────────

impl Db {
    /// Poll a device-auth session: classify its status and either touch `last_polled_at` (pending) or issue
    /// the single-use grant (confirmed/issued). One `SERIALIZABLE` (`run_serializable!`) txn. The grant is HMAC-derived from
    /// `(device_code_sha256, ws)` so a re-poll re-derives the SAME grant (idempotent issue). An unknown
    /// device code is the indistinguishable `NotFound`. `link_base` composes the granted context's
    /// workspace ADDRESS.
    pub(crate) async fn poll_txn(
        &self,
        device_code_sha256: &[u8; 32],
        now: i64,
        created_at: &str,
        secret: &[u8; 32],
        link_base: &str,
    ) -> Result<DeviceAuthPoll> {
        run_serializable!(self, tx, {
            poll_run(
                &mut tx,
                device_code_sha256,
                now,
                created_at,
                secret,
                link_base,
            )
            .await
        })
    }
}

struct SessionRow {
    user_code: String,
    /// `None` for a not-yet-approved STANDUP session, for a LOGIN session (workspace-less by design),
    /// and for an ENROLL session whose requested address never resolved (the grant is issued anyway;
    /// the redeem answers the one uniform denial).
    workspace_id: Option<String>,
    /// The ADDRESS name an enroll session was opened against — echoed (never the real workspace's
    /// facts) at the verify page and the granted poll, so neither becomes a workspace-existence
    /// oracle. `None` for standup/login sessions.
    requested_workspace: Option<String>,
    intent: String,
    device_pubkey: Vec<u8>,
    device_key_id: String,
    status: String,
    confirmed_principal: Option<String>,
    expires_at: i64,
    interval_secs: i64,
    last_polled_at: Option<i64>,
}

async fn read_session(
    tx: &mut Transaction<'_, Postgres>,
    device_code_sha256: &[u8; 32],
) -> Result<Option<SessionRow>> {
    let dc = device_code_sha256.as_slice();
    let row = sqlx::query!(
        r#"SELECT user_code AS "user_code!", workspace_id AS "workspace_id?",
                  requested_workspace AS "requested_workspace?",
                  intent AS "intent!", device_pubkey AS "device_pubkey!: Vec<u8>",
                  device_key_id AS "device_key_id!", status AS "status!",
                  confirmed_principal AS "confirmed_principal?", expires_at AS "expires_at!: i64",
                  interval_secs AS "interval_secs!: i64", last_polled_at AS "last_polled_at?: i64"
           FROM device_auth_sessions WHERE device_code_sha256 = $1"#,
        dc,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| SessionRow {
        user_code: r.user_code,
        workspace_id: r.workspace_id,
        requested_workspace: r.requested_workspace,
        intent: r.intent,
        device_pubkey: r.device_pubkey,
        device_key_id: r.device_key_id,
        status: r.status,
        confirmed_principal: r.confirmed_principal,
        expires_at: r.expires_at,
        interval_secs: r.interval_secs,
        last_polled_at: r.last_polled_at,
    }))
}

async fn poll_run(
    tx: &mut Transaction<'_, Postgres>,
    device_code_sha256: &[u8; 32],
    now: i64,
    created_at: &str,
    secret: &[u8; 32],
    link_base: &str,
) -> Result<DeviceAuthPoll> {
    let Some(session) = read_session(tx, device_code_sha256).await? else {
        return Err(AuthorityError::NotFound);
    };
    let dc = device_code_sha256.as_slice();

    // Expiry is terminal for a not-yet-issued session (an issued one stays redeemable until the GRANT expires).
    if now > session.expires_at && matches!(session.status.as_str(), "pending" | "confirmed") {
        sqlx::query!(
            "UPDATE device_auth_sessions SET status = 'expired' WHERE device_code_sha256 = $1",
            dc,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        return Ok(DeviceAuthPoll::Expired);
    }

    match session.status.as_str() {
        "expired" => Ok(DeviceAuthPoll::Expired),
        "denied" => Ok(DeviceAuthPoll::Denied),
        "pending" => {
            let interval_ms = session.interval_secs.saturating_mul(1000);
            let too_fast = session
                .last_polled_at
                .is_some_and(|t| now.saturating_sub(t) < interval_ms);
            sqlx::query!(
                "UPDATE device_auth_sessions SET last_polled_at = $2 WHERE device_code_sha256 = $1",
                dc,
                now,
            )
            .execute(&mut **tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok(if too_fast {
                DeviceAuthPoll::SlowDown
            } else {
                DeviceAuthPoll::Pending
            })
        }
        "confirmed" | "issued" => {
            let granted = issue_grant(
                tx,
                device_code_sha256,
                &session,
                now,
                created_at,
                secret,
                link_base,
            )
            .await?;
            Ok(DeviceAuthPoll::Granted(granted))
        }
        _ => Err(AuthorityError::integrity(EnrollCorrupt("session status"))),
    }
}

/// Issue (or re-derive) the single-use grant for a confirmed/issued session. The grant token is
/// deterministic in `(device_code_sha256, ws-or-empty)`, so a re-poll re-derives it and the
/// `ON CONFLICT DO NOTHING` is a no-op — naturally idempotent. On the FIRST issue it binds the proven
/// identity (principal, device) and flips the session to `issued`. The grant records the session's
/// INTENT — a standup session issues an `enroll`-intent grant scoped to its created workspace, so the
/// two redeem doors can never cross — and a workspace only when the session has one (a login grant is
/// workspace-less by design; an enroll grant against an unresolved address is too, and the redeem
/// answers the one uniform denial).
/// A deterministic, UNRESOLVABLE workspace id (the real `w_<32 hex>` shape) for an enroll address that
/// did not resolve. The granted poll then carries the same id shape it would for a real workspace, so
/// it cannot become an existence oracle; the id is never a workspace row, so the redeem reads no
/// workspace and answers the one uniform denial. Derived from the enrollment secret so a client cannot
/// recompute it and probe (and it never collides with a real CSPRNG id).
fn placeholder_workspace_id(secret: &[u8; 32], requested: &str) -> Result<WorkspaceId> {
    let mac = enroll::sha256_token(&enroll::derive_token(
        secret,
        b"enroll-noresolve",
        &[requested.as_bytes()],
    ));
    let mut hex = String::with_capacity(34);
    hex.push_str("w_");
    for b in &mac[..16] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    WorkspaceId::parse(&hex).map_err(AuthorityError::internal)
}

#[allow(clippy::too_many_arguments)]
async fn issue_grant(
    tx: &mut Transaction<'_, Postgres>,
    device_code_sha256: &[u8; 32],
    session: &SessionRow,
    now: i64,
    created_at: &str,
    secret: &[u8; 32],
    link_base: &str,
) -> Result<GrantIssued> {
    let principal = session.confirmed_principal.as_deref().ok_or_else(|| {
        AuthorityError::integrity(EnrollCorrupt("confirmed session without principal"))
    })?;
    // The workspace context rides with the grant — the client learns what it is joining from the poll.
    // An ENROLL-by-address poll must be a NON-oracle: it discloses the SAME shape whether or not the
    // requested name resolved (an unresolved address is only ever denied at the redeem, uniformly). So
    // the display name + address ECHO the requested name (never the real workspace row), and the bound
    // id is the resolved id when it exists, else a deterministic, UNRESOLVABLE placeholder of the same
    // `w_<hex>` shape — indistinguishable from a real id at the poll (a bare id reveals nothing without
    // a membership credential), and the redeem's read of a placeholder finds no workspace and answers
    // the one uniform denial. STANDUP (its approval created a real workspace) and LOGIN (workspace-less)
    // keep their existing context.
    let (ws, workspace_display_name, workspace_address) = if session.intent == "enroll" {
        let requested = session.requested_workspace.clone().unwrap_or_default();
        let ws = match session.workspace_id.as_deref() {
            Some(id) => WorkspaceId::parse(id).map_err(AuthorityError::integrity)?,
            None => placeholder_workspace_id(secret, &requested)?,
        };
        let address = Some(format!("{link_base}/{requested}"));
        (Some(ws), requested, address)
    } else {
        let ws = session
            .workspace_id
            .as_deref()
            .map(WorkspaceId::parse)
            .transpose()
            .map_err(AuthorityError::integrity)?;
        let (display_name, address) = match &ws {
            Some(ws) => match read_workspace_in_tx(tx, ws).await? {
                Some(w) => (w.display_name, Some(format!("{link_base}/{}", w.name))),
                None => (String::new(), None),
            },
            None => (String::new(), None),
        };
        (ws, display_name, address)
    };

    // The grant's intent: a standup session's approval created its workspace, so its grant redeems at
    // the ENROLL door; enroll/login pass through. (The CHECK on `enrollment_grants.intent` pins the set.)
    let grant_intent = match session.intent.as_str() {
        "standup" => "enroll",
        other => other,
    };

    // The grant token: derive_token(b"grant", [device_code_sha256, ws-or-empty]); store only its
    // sha256. The empty part for a workspace-less session is unambiguous under the length-prefixed
    // framing (no workspace id is empty), and keeps the derivation exactly as before whenever a
    // workspace is bound.
    let ws_part: &[u8] = ws.as_ref().map_or(b"", |w| w.as_str().as_bytes());
    let grant_token = enroll::derive_token(secret, b"grant", &[device_code_sha256, ws_part]);
    let grant_sha256 = enroll::sha256_token(&grant_token);

    let device_auth_id = session.user_code.clone();
    let device_key_id = session.device_key_id.clone();
    let expires_at = now.saturating_add(enroll::GRANT_TTL_MS);

    let (gs, ws_s, pk) = (
        grant_sha256.as_slice(),
        ws.as_ref().map(WorkspaceId::as_str),
        session.device_pubkey.as_slice(),
    );
    sqlx::query!(
        "INSERT INTO enrollment_grants \
           (grant_sha256, workspace_id, intent, principal, device_pubkey, device_key_id, \
            device_auth_id, expires_at, consumed_at, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9) \
         ON CONFLICT (grant_sha256) DO NOTHING",
        gs,
        ws_s,
        grant_intent,
        principal,
        pk,
        device_key_id,
        device_auth_id,
        expires_at,
        created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    let dc = device_code_sha256.as_slice();
    sqlx::query!(
        "UPDATE device_auth_sessions SET status = 'issued' WHERE device_code_sha256 = $1",
        dc,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    Ok(GrantIssued {
        grant_token,
        workspace_id: ws,
        workspace_display_name,
        workspace_address,
        device_auth_id,
        device_key_id,
        expires_at,
    })
}

// ── verification page: the live-session disclosure + the external-identity confirm ─────────────────────

/// A LIVE device-auth session resolved by `user_code` for the verification-page disclosure (no secret).
pub(crate) struct VerificationSessionRow {
    pub(crate) intent: String,
    pub(crate) machine_name: String,
    pub(crate) device_pubkey: [u8; 32],
    /// `None` for a not-yet-approved STANDUP session, a LOGIN session, or an enroll session whose
    /// requested address never resolved.
    pub(crate) workspace_id: Option<WorkspaceId>,
    /// The address name an enroll session asked for, verbatim (charset-validated at authorize).
    pub(crate) requested_workspace: Option<String>,
}

impl Db {
    /// Resolve the LIVE (pending/confirmed), NON-EXPIRED session a `user_code` names — for the verification
    /// page. `None` ⇒ no such live session (an unknown code, a non-live/expired one — the uniform miss). A
    /// pure pool read: no mutation, no secret (the device code's sha256 is never returned here).
    pub(crate) async fn read_live_verification_session(
        &self,
        user_code: &str,
        now: i64,
    ) -> Result<Option<VerificationSessionRow>> {
        let row = sqlx::query!(
            r#"SELECT intent AS "intent!", machine_name AS "machine_name!",
                      device_pubkey AS "device_pubkey!: Vec<u8>",
                      workspace_id AS "workspace_id?", requested_workspace AS "requested_workspace?"
               FROM device_auth_sessions
               WHERE user_code = $1 AND status IN ('pending', 'confirmed') AND expires_at >= $2
               LIMIT 1"#,
            user_code,
            now,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(VerificationSessionRow {
                intent: r.intent,
                machine_name: r.machine_name,
                device_pubkey: blob32(&r.device_pubkey)?,
                workspace_id: r
                    .workspace_id
                    .as_deref()
                    .map(WorkspaceId::parse)
                    .transpose()
                    .map_err(AuthorityError::integrity)?,
                requested_workspace: r.requested_workspace,
            })),
        }
    }

    /// Confirm a live session's identity from an externally-proven principal (the OIDC callback's write half).
    /// One `SERIALIZABLE` (`run_serializable!`) txn. Mirrors [`complete_passcode_run`]'s success branch — set status `confirmed` +
    /// `confirmed_principal` — minus the code check (the CALLER proved the email via a validated id_token). An
    /// unknown / non-live / expired `user_code` is the uniform `NotFound`.
    pub(crate) async fn confirm_external_identity_txn(
        &self,
        user_code: &str,
        principal: &Principal,
        now: i64,
    ) -> Result<ConfirmOutcome> {
        run_serializable!(self, tx, {
            confirm_external_identity_run(&mut tx, user_code, principal, now).await
        })
    }
}

async fn confirm_external_identity_run(
    tx: &mut Transaction<'_, Postgres>,
    user_code: &str,
    principal: &Principal,
    now: i64,
) -> Result<ConfirmOutcome> {
    // The live ENROLL-or-LOGIN session this user code names (pending/confirmed), non-expired. Absent ⇒
    // the uniform miss. The intent guard keeps a STANDUP session out of this path entirely: a standup
    // session is only ever advanced by the approve op (which creates the workspace it confirms into);
    // an enroll and a login session are confirmed the same way (the identity proof IS the flow).
    let row = sqlx::query!(
        r#"SELECT device_code_sha256 AS "device_code_sha256!: Vec<u8>", status AS "status!",
                  confirmed_principal AS "confirmed_principal?"
           FROM device_auth_sessions
           WHERE user_code = $1 AND intent IN ('enroll', 'login')
             AND status IN ('pending', 'confirmed') AND expires_at >= $2 LIMIT 1"#,
        user_code,
        now,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let Some(row) = row else {
        return Err(AuthorityError::NotFound);
    };
    let device_code_sha256 = blob32(&row.device_code_sha256)?;
    // FIRST-WRITER-WINS: a confirmation is a pending→confirmed CAS, never an overwrite. A session already
    // confirmed for the SAME principal replays idempotently; one confirmed for a DIFFERENT principal is the
    // uniform miss — a later identity leg (a second passcode/OIDC round) can never re-bind the session.
    if row.status == "confirmed" {
        return if row.confirmed_principal.as_deref() == Some(principal.as_str()) {
            Ok(ConfirmOutcome::Confirmed)
        } else {
            Err(AuthorityError::NotFound)
        };
    }
    let dc = device_code_sha256.as_slice();
    let prin = principal.as_str();
    // Confirm the (externally-proven) principal — the device may now poll a grant. No code check: the OIDC
    // module already validated the id_token, so this is `complete_passcode`'s confirm half, minus the verify.
    // The `status = 'pending'` arm makes the write itself the CAS (a raced confirm loses cleanly).
    let updated = sqlx::query!(
        "UPDATE device_auth_sessions SET status = 'confirmed', confirmed_principal = $2 \
         WHERE device_code_sha256 = $1 AND status = 'pending'",
        dc,
        prin,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if updated.rows_affected() == 0 {
        return Err(AuthorityError::NotFound);
    }
    Ok(ConfirmOutcome::Confirmed)
}

// ── passcode: start (upsert) + complete (verify under cap), the verification-page second factor ────────

impl Db {
    /// Resolve the LIVE **enroll-or-login** session a `user_code` names (pending/confirmed), returning its
    /// device-code sha256. `None` ⇒ no live such session (the uniform miss). The intent guard keeps a
    /// STANDUP session out of the passcode flow — its only identity leg is the web approve.
    pub(crate) async fn live_session_device_code(
        &self,
        user_code: &str,
    ) -> Result<Option<[u8; 32]>> {
        let row = sqlx::query!(
            r#"SELECT device_code_sha256 AS "device_code_sha256!: Vec<u8>" FROM device_auth_sessions
               WHERE user_code = $1 AND intent IN ('enroll', 'login') AND status IN ('pending', 'confirmed') LIMIT 1"#,
            user_code,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(blob32(&r.device_code_sha256)?)),
        }
    }

    /// Upsert a passcode for `(session, principal)` — a fresh code resets `attempts` to 0. Stores only sha256.
    pub(crate) async fn upsert_passcode(
        &self,
        device_code_sha256: &[u8; 32],
        principal: &Principal,
        passcode_sha256: &[u8; 32],
        expires_at: i64,
        created_at: &str,
    ) -> Result<()> {
        let (dc, prin, ps) = (
            device_code_sha256.as_slice(),
            principal.as_str(),
            passcode_sha256.as_slice(),
        );
        sqlx::query!(
            "INSERT INTO passcodes (device_code_sha256, principal, passcode_sha256, expires_at, attempts, created_at) \
             VALUES ($1, $2, $3, $4, 0, $5) \
             ON CONFLICT (device_code_sha256, principal) DO UPDATE SET \
               passcode_sha256 = excluded.passcode_sha256, expires_at = excluded.expires_at, \
               attempts = 0, created_at = excluded.created_at",
            dc,
            prin,
            ps,
            expires_at,
            created_at,
        )
        .execute(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Verify a passcode under the TTL + attempt cap; on success confirm the session's identity. One
    /// `SERIALIZABLE` (`run_serializable!`) txn. An unknown user code is the uniform `NotFound`; a missing passcode for a known
    /// session is an indistinguishable `WrongCode` (never a per-email existence oracle).
    pub(crate) async fn complete_passcode_txn(
        &self,
        user_code: &str,
        principal: &Principal,
        code: &str,
        now: i64,
    ) -> Result<PasscodeComplete> {
        run_serializable!(self, tx, {
            complete_passcode_run(&mut tx, user_code, principal, code, now).await
        })
    }
}

async fn complete_passcode_run(
    tx: &mut Transaction<'_, Postgres>,
    user_code: &str,
    principal: &Principal,
    code: &str,
    now: i64,
) -> Result<PasscodeComplete> {
    // The live, NON-EXPIRED **enroll-or-login** session this user code names (pending/confirmed).
    // Absent/expired ⇒ the uniform miss — an expired session is the indistinguishable NotFound at every
    // confirm entry point (matching the poll + read_verification_session), not only at poll time. The
    // intent guard keeps a STANDUP session out (its only identity leg is the web approve).
    let row = sqlx::query!(
        r#"SELECT device_code_sha256 AS "device_code_sha256!: Vec<u8>", status AS "status!",
                  confirmed_principal AS "confirmed_principal?"
           FROM device_auth_sessions
           WHERE user_code = $1 AND intent IN ('enroll', 'login')
             AND status IN ('pending', 'confirmed') AND expires_at >= $2 LIMIT 1"#,
        user_code,
        now,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let Some(row) = row else {
        return Err(AuthorityError::NotFound);
    };
    // FIRST-WRITER-WINS: a session already confirmed for a DIFFERENT principal is the uniform miss BEFORE
    // any code is checked — a second identity leg can never re-bind it (nor probe its passcode).
    if row.status == "confirmed" && row.confirmed_principal.as_deref() != Some(principal.as_str()) {
        return Err(AuthorityError::NotFound);
    }
    // A session already confirmed for THIS SAME principal replays idempotently — BEFORE any passcode-row
    // consultation. The first writer won; the passcode row's later fate (expired, locked, gone) must not
    // turn a lost-ack retry or refresh into a WrongCode/Expired/TooManyAttempts failure.
    if row.status == "confirmed" {
        return Ok(PasscodeComplete::Confirmed);
    }
    let device_code_sha256 = blob32(&row.device_code_sha256)?;
    let dc = device_code_sha256.as_slice();
    let prin = principal.as_str();

    let pc = sqlx::query!(
        r#"SELECT passcode_sha256 AS "passcode_sha256!: Vec<u8>", expires_at AS "expires_at!: i64",
                  attempts AS "attempts!: i64"
           FROM passcodes WHERE device_code_sha256 = $1 AND principal = $2"#,
        dc,
        prin,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    // No passcode for this (session, email) ⇒ indistinguishable from a wrong guess with full attempts left.
    let Some(pc) = pc else {
        return Ok(PasscodeComplete::WrongCode {
            remaining: enroll::PASSCODE_MAX_ATTEMPTS,
        });
    };
    if now > pc.expires_at {
        return Ok(PasscodeComplete::Expired);
    }
    if pc.attempts >= enroll::PASSCODE_MAX_ATTEMPTS {
        return Ok(PasscodeComplete::TooManyAttempts);
    }
    let stored = blob32(&pc.passcode_sha256)?;
    if digest::sha256(code.as_bytes()) == stored {
        // Confirm the session's identity (the proven principal) — the device may now poll a grant. The
        // pending→confirmed CAS is the first-writer-wins write (the session is `pending` here: a
        // confirmed-same session already replayed above, a confirmed-different one was the uniform miss).
        sqlx::query!(
            "UPDATE device_auth_sessions SET status = 'confirmed', confirmed_principal = $2 \
             WHERE device_code_sha256 = $1 AND status = 'pending'",
            dc,
            prin,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(PasscodeComplete::Confirmed)
    } else {
        let attempts = pc.attempts + 1;
        sqlx::query!(
            "UPDATE passcodes SET attempts = $3 WHERE device_code_sha256 = $1 AND principal = $2",
            dc,
            prin,
            attempts,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(PasscodeComplete::WrongCode {
            remaining: (enroll::PASSCODE_MAX_ATTEMPTS - attempts).max(0),
        })
    }
}

// ── redeem: the central grant-redemption + gate + register + mint transaction ──────────────────────────

struct GrantRow {
    /// `None` for a login grant (workspace-less by design) and an enroll grant whose requested
    /// address never resolved.
    workspace_id: Option<WorkspaceId>,
    intent: String,
    principal: String,
    device_pubkey: [u8; 32],
    device_key_id: String,
    expires_at: i64,
}

impl Db {
    /// Redeem a grant into a registered device + its ONE minted workspace credential. ONE `SERIALIZABLE`
    /// (`run_serializable!`) txn (the pointer-move's discipline). All `Denied` checks run BEFORE any write,
    /// so a denial has no side effect; only an all-checks-passed redeem confirms membership and registers
    /// the credentialed device — the credential is deterministic in the grant, so a replay re-derives the
    /// identical value with no extra creds.
    pub(crate) async fn redeem_txn(
        &self,
        input: &RedeemInput<'_>,
        secret: &[u8; 32],
    ) -> Result<RedeemOutcome> {
        run_serializable!(self, tx, redeem_run(&mut tx, input, secret).await)
    }

    /// Redeem a LOGIN grant: register this device + re-mint its workspace credential in EVERY workspace
    /// where the proven identity holds a confirmed seat. ONE `SERIALIZABLE` (`run_serializable!`) txn.
    /// All `Denied` checks run before any write; a `blocked` seat (revoked device, squatted key id) is
    /// skipped without a side effect while the rest still mint. Deterministic per `(grant, workspace)`,
    /// so a lost-ack replay re-returns identical plaintexts.
    pub(crate) async fn redeem_login_txn(
        &self,
        grant_sha256: &[u8; 32],
        device_public_key: &[u8; 32],
        server_device_key_id: &str,
        now: i64,
        secret: &[u8; 32],
    ) -> Result<LoginOutcome> {
        run_serializable!(self, tx, {
            login_run(
                &mut tx,
                grant_sha256,
                device_public_key,
                server_device_key_id,
                now,
                secret,
            )
            .await
        })
    }
}

async fn redeem_run(
    tx: &mut Transaction<'_, Postgres>,
    input: &RedeemInput<'_>,
    secret: &[u8; 32],
) -> Result<RedeemOutcome> {
    // (1) Resolve the grant. Absent ⇒ DENIED (the uniform miss).
    let gs = input.grant_sha256.as_slice();
    let Some(grant) = read_grant(tx, gs).await? else {
        return Ok(RedeemOutcome::Denied("no such grant"));
    };
    // (2) Expiry.
    if input.now > grant.expires_at {
        return Ok(RedeemOutcome::Denied("grant expired"));
    }
    // (3) Intent: this is the ENROLL door. A login grant presented here reads exactly like a
    // membership miss — the uniform denial, never a hint that the grant is otherwise live.
    if grant.intent != "enroll" {
        return Ok(RedeemOutcome::Denied(enroll::ENROLL_UNAVAILABLE));
    }
    // (4) The grant binds exactly this device — the presented key + its server-derived id must match.
    if grant.device_pubkey != input.device_public_key
        || grant.device_key_id != input.server_device_key_id
    {
        return Ok(RedeemOutcome::Denied("device key mismatch"));
    }

    // (5) THE MEMBERSHIP GATE — the roster is the lock, on EVERY deployment posture (the self-host
    // bearer-grants-membership arm died with the invite token). ONE uniform denial covers every
    // not-yours case: an address that never resolved (a workspace-less grant), a grant scoped to a
    // DIFFERENT workspace than the path names, a vanished workspace row, and an identity with no seat
    // — indistinguishable, so a redeem is no workspace-existence or roster-enumeration oracle.
    let Some(grant_ws) = &grant.workspace_id else {
        return Ok(RedeemOutcome::Denied(enroll::ENROLL_UNAVAILABLE));
    };
    if grant_ws != input.ws {
        return Ok(RedeemOutcome::Denied(enroll::ENROLL_UNAVAILABLE));
    }
    if read_workspace_in_tx(tx, grant_ws).await?.is_none() {
        return Ok(RedeemOutcome::Denied(enroll::ENROLL_UNAVAILABLE));
    }
    let invited = match read_member_status(tx, grant_ws, &grant.principal).await? {
        None => return Ok(RedeemOutcome::Denied(enroll::ENROLL_UNAVAILABLE)),
        Some(status) => status == "invited",
    };

    // (6) Anti-squat + revocation durability (post-membership, deliberately typed — these are not
    // existence oracles): a pre-existing device row must match (key, principal) exactly AND must NOT
    // be revoked. Without the revoked check, a revoked device could re-redeem its still-live grant
    // (a ~12-min TTL) and the mint below would re-credential the row the revoke just killed —
    // undoing the kill switch within the grant window. A revoked device cannot re-enroll.
    if let Some((existing_pk, existing_principal, revoked)) =
        read_device(tx, grant_ws, input.server_device_key_id).await?
    {
        if existing_pk != input.device_public_key || existing_principal != grant.principal {
            return Ok(RedeemOutcome::Denied(
                "device key id already bound to a different key/principal",
            ));
        }
        if revoked {
            return Ok(RedeemOutcome::Denied("device is revoked"));
        }
    }

    // ── all checks passed — WRITES only from here (so a DENIED above had no side effect) ──
    let principal = Principal::parse(&grant.principal).map_err(AuthorityError::integrity)?;
    let ws_s = grant_ws.as_str();
    let prin = principal.as_str();

    // (5') Membership: an `invited` seat flips to `confirmed` (the redeem IS the join ceremony's end).
    if invited {
        sqlx::query!(
            "UPDATE workspace_member SET status = 'confirmed' \
             WHERE workspace_id = $1 AND principal = $2 AND status = 'invited'",
            ws_s,
            prin,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }

    // (6') REGISTER the device WITH its one workspace credential (idempotent — step 6 proved no
    // conflicting row). The credential is deterministic in the grant (`derive_token(b"wscred", [gs])`),
    // so a lost-ack replay re-derives the IDENTICAL value; only its sha256 is stored, ON the registry
    // row — one row, one device, one credential. A re-redeem through a FRESH grant (re-invite after a
    // member removal) derives a NEW credential, and the upsert rotates the column: the old plaintext
    // stops resolving the moment this commits. No per-skill roster rows and no per-skill tokens are
    // written — access is the membership join from here on.
    let credential = enroll::derive_token(secret, b"wscred", &[gs]);
    let credential_sha = enroll::sha256_token(&credential);
    let cs = credential_sha.as_slice();
    let pk = input.device_public_key.as_slice();
    sqlx::query!(
        "INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, revoked, credential_sha256) \
         VALUES ($1, $2, $3, $4, 0, $5) \
         ON CONFLICT (workspace_id, device_key_id) DO UPDATE SET credential_sha256 = excluded.credential_sha256",
        ws_s,
        input.server_device_key_id,
        pk,
        prin,
        cs,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    // (7) Audit marker (idempotent — a replay re-stamps, harmless).
    consume_grant(tx, gs, input.now).await?;

    Ok(RedeemOutcome::Redeemed(EnrollmentRedeemed {
        workspace_id: grant_ws.clone(),
        principal,
        device_key_id: input.server_device_key_id.to_owned(),
        credential,
    }))
}

/// The LOGIN door's transaction body: prove the grant, then walk the identity's confirmed seats.
async fn login_run(
    tx: &mut Transaction<'_, Postgres>,
    grant_sha256: &[u8; 32],
    device_public_key: &[u8; 32],
    server_device_key_id: &str,
    now: i64,
    secret: &[u8; 32],
) -> Result<LoginOutcome> {
    // (1)–(4) mirror the enroll redeem: resolve → expiry → intent → binding.
    let gs = grant_sha256.as_slice();
    let Some(grant) = read_grant(tx, gs).await? else {
        return Ok(LoginOutcome::Denied("no such grant"));
    };
    if now > grant.expires_at {
        return Ok(LoginOutcome::Denied("grant expired"));
    }
    // This is the LOGIN door: an enroll grant redeems at the workspace's enroll door, never here.
    if grant.intent != "login" {
        return Ok(LoginOutcome::Denied("not a login grant"));
    }
    if grant.device_pubkey != *device_public_key || grant.device_key_id != server_device_key_id {
        return Ok(LoginOutcome::Denied("device key mismatch"));
    }

    let principal = Principal::parse(&grant.principal).map_err(AuthorityError::integrity)?;
    let prin = principal.as_str();

    // (5) Every confirmed seat of the proven identity, with its workspace's address facts — ordered by
    // workspace id, so a replay's list is byte-stable.
    let seats = sqlx::query!(
        r#"SELECT wm.workspace_id AS "workspace_id!", wm.role AS "role!",
                  w.name AS "name!", w.display_name AS "display_name!"
           FROM workspace_member wm
           JOIN workspace w ON w.workspace_id = wm.workspace_id
           WHERE wm.principal = $1 AND wm.status = 'confirmed'
           ORDER BY wm.workspace_id"#,
        prin,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    let mut memberships = Vec::with_capacity(seats.len());
    for seat in seats {
        let ws = WorkspaceId::parse(&seat.workspace_id).map_err(AuthorityError::integrity)?;
        // The SAME register the enroll redeem runs, per seat: an existing row must match this exact
        // (key, principal) and not be revoked — else the seat comes back BLOCKED with no credential
        // and no side effect there (the other seats still mint).
        let blocked: Option<&'static str> = match read_device(tx, &ws, server_device_key_id).await?
        {
            Some((existing_pk, existing_principal, _))
                if existing_pk != *device_public_key || existing_principal != grant.principal =>
            {
                Some("device key id already bound to a different key/principal")
            }
            Some((_, _, true)) => {
                Some("device revoked in this workspace — enroll a fresh device or ask an owner")
            }
            _ => None,
        };
        let credential = if blocked.is_none() {
            // Deterministic per (grant, workspace) — the login grant is ONE bearer for N workspaces,
            // so the workspace id joins the mint (each workspace gets a DISTINCT credential) while a
            // lost-ack replay of the same grant re-derives identical plaintexts.
            let credential = enroll::derive_token(secret, b"wscred", &[gs, ws.as_str().as_bytes()]);
            let credential_sha = enroll::sha256_token(&credential);
            let (ws_s, cs, pk) = (ws.as_str(), credential_sha, device_public_key.as_slice());
            let cs = cs.as_slice();
            sqlx::query!(
                "INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, revoked, credential_sha256) \
                 VALUES ($1, $2, $3, $4, 0, $5) \
                 ON CONFLICT (workspace_id, device_key_id) DO UPDATE SET credential_sha256 = excluded.credential_sha256",
                ws_s,
                server_device_key_id,
                pk,
                prin,
                cs,
            )
            .execute(&mut **tx)
            .await
            .map_err(AuthorityError::internal)?;
            Some(credential)
        } else {
            None
        };
        memberships.push(LoginSeat {
            workspace_id: ws,
            name: seat.name,
            display_name: seat.display_name,
            role: seat.role,
            device_key_id: server_device_key_id.to_owned(),
            credential,
            blocked,
        });
    }

    // Consumption mirrors the enroll redeem exactly: an idempotent audit stamp, never a replay block —
    // the deterministic mints make a lost-ack retry re-return the identical outcome.
    consume_grant(tx, gs, now).await?;

    Ok(LoginOutcome::Redeemed(LoginRedeemed {
        principal,
        memberships,
    }))
}

/// Stamp a grant consumed (idempotent — a replay re-stamps nothing; the first consumption's time stands).
async fn consume_grant(tx: &mut Transaction<'_, Postgres>, gs: &[u8], now: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE enrollment_grants SET consumed_at = $2 WHERE grant_sha256 = $1 AND consumed_at IS NULL",
        gs,
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

async fn read_grant(tx: &mut Transaction<'_, Postgres>, gs: &[u8]) -> Result<Option<GrantRow>> {
    let row = sqlx::query!(
        r#"SELECT workspace_id AS "workspace_id?", intent AS "intent!", principal AS "principal!",
                  device_pubkey AS "device_pubkey!: Vec<u8>", device_key_id AS "device_key_id!",
                  device_auth_id AS "device_auth_id!", expires_at AS "expires_at!: i64"
           FROM enrollment_grants WHERE grant_sha256 = $1"#,
        gs,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(GrantRow {
            workspace_id: r
                .workspace_id
                .as_deref()
                .map(WorkspaceId::parse)
                .transpose()
                .map_err(AuthorityError::integrity)?,
            intent: r.intent,
            principal: r.principal,
            device_pubkey: blob32(&r.device_pubkey)?,
            device_key_id: r.device_key_id,
            expires_at: r.expires_at,
        })),
    }
}

async fn read_workspace_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
) -> Result<Option<WorkspaceRow>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT name AS "name!", display_name AS "display_name!", verified_domain AS "verified_domain?",
                  verified_domain_status AS "verified_domain_status!"
           FROM workspace WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| WorkspaceRow {
        name: r.name,
        display_name: r.display_name,
        verified_domain: r.verified_domain,
        verified_domain_status: r.verified_domain_status,
    }))
}

async fn read_member_status(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    principal: &str,
) -> Result<Option<String>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT status AS "status!" FROM workspace_member WHERE workspace_id = $1 AND principal = $2"#,
        ws_s,
        principal,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| r.status))
}

/// Read a device's `(public_key, principal)` if registered (any revoked state). `None` ⇒ unregistered.
/// `pub(super)`: shared with [`super::governance`] (the revoke arm + the admin claim resolve against it).
pub(super) async fn read_device(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    device_key_id: &str,
) -> Result<Option<([u8; 32], String, bool)>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT public_key AS "public_key!: Vec<u8>", principal AS "principal!", revoked AS "revoked!"
           FROM device_registry WHERE workspace_id = $1 AND device_key_id = $2"#,
        ws_s,
        device_key_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some((blob32(&r.public_key)?, r.principal, r.revoked != 0))),
    }
}

// ── shared in-txn helpers (also used by [`super::governance`]) ─────────────────────────────────────────

/// A stored enrollment value violated an invariant (a width/range CHECK, an unparseable enum) — store corruption.
#[derive(Debug, thiserror::Error)]
#[error("enrollment store corruption: {0}")]
pub(super) struct EnrollCorrupt(pub(super) &'static str);
