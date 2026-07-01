//! The enrollment + governance issuance SQL — the raw-`sqlx` half (every `query!` for the issuance core).
//!
//! Mirrors [`super::set_current`]: the `SERIALIZABLE` (`run_serializable!`) redeem/governance/claim transactions live here, the
//! orchestration ([`crate::enroll`]) hands in server-trusted values (the rehashed grant, the re-derived
//! device key id, the parsed op id) plus the enrollment secret and gets back domain outcomes. No `sqlx` type
//! crosses the module boundary. Every row is `workspace_id`-scoped; every opaque credential is matched only
//! by its stored sha256; the confirmed principal is read from a server-trusted row, never parsed from a claim.

use sqlx::{Postgres, Transaction};
use topos_core::digest;
use topos_core::sign::{
    EnrollFields, GovernanceOpFields, GovernanceOpKind, governance_op_preimage, verify_enroll,
    verify_governance_op,
};

use super::Db;
use crate::enroll::{
    self, ConfirmOutcome, DeviceAuthPoll, EnrollmentRedeemed, GovernanceInput, GovernanceOp,
    GovernanceOutcome, GrantIssued, MintedReadToken, PasscodeComplete, RedeemInput, RedeemOutcome,
    Role,
};
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, SkillId, WorkspaceId};

// ── pool reads (the orchestration classifies on these) ─────────────────────────────────────────────────

/// A `workspace` row (the deployment posture + display fields).
pub(crate) struct WorkspaceRow {
    pub(crate) display_name: String,
    pub(crate) verified_domain: Option<String>,
    pub(crate) verified_domain_status: String,
    pub(crate) deployment_mode: String,
}

/// A live `invites` row (resolved by the presented token's sha256).
pub(crate) struct InviteRow {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) expires_at: Option<i64>,
    pub(crate) revoked: bool,
}

impl Db {
    /// Read a workspace row (deployment posture + display fields). `None` if the workspace does not exist.
    pub(crate) async fn read_workspace(&self, ws: &WorkspaceId) -> Result<Option<WorkspaceRow>> {
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT display_name AS "display_name!", verified_domain AS "verified_domain?",
                      verified_domain_status AS "verified_domain_status!", deployment_mode AS "deployment_mode!"
               FROM workspace WHERE workspace_id = $1"#,
            ws_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| WorkspaceRow {
            display_name: r.display_name,
            verified_domain: r.verified_domain,
            verified_domain_status: r.verified_domain_status,
            deployment_mode: r.deployment_mode,
        }))
    }

    /// Resolve an invite by its token's sha256 — the live row, whatever its revoked/expiry state (the caller
    /// applies the `non-revoked ∧ non-expired` gate so a miss and a dead invite are the same `NotFound`).
    pub(crate) async fn read_invite(&self, token_sha256: &[u8; 32]) -> Result<Option<InviteRow>> {
        let key = token_sha256.as_slice();
        let row = sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!", expires_at AS "expires_at?: i64",
                      revoked AS "revoked!: i64"
               FROM invites WHERE token_sha256 = $1"#,
            key,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(InviteRow {
                workspace_id: WorkspaceId::parse(&r.workspace_id)
                    .map_err(AuthorityError::integrity)?,
                expires_at: r.expires_at,
                revoked: r.revoked != 0,
            })),
        }
    }

    /// The skills an invite offers (with optional display names).
    pub(crate) async fn read_invite_skills(
        &self,
        token_sha256: &[u8; 32],
    ) -> Result<Vec<(SkillId, Option<String>)>> {
        let key = token_sha256.as_slice();
        let rows = sqlx::query!(
            r#"SELECT skill_id AS "skill_id!", name AS "name?" FROM invite_skill WHERE token_sha256 = $1
               ORDER BY skill_id"#,
            key,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok((
                    SkillId::parse(&r.skill_id).map_err(AuthorityError::integrity)?,
                    r.name,
                ))
            })
            .collect()
    }

    /// Whether a LIVE (pending/confirmed) session already holds `user_code` (the start loop avoids a clash
    /// against the partial-unique index).
    pub(crate) async fn live_user_code_exists(&self, user_code: &str) -> Result<bool> {
        let row = sqlx::query!(
            r#"SELECT 1 AS "ok!: i64" FROM device_auth_sessions
               WHERE user_code = $1 AND status IN ('pending', 'confirmed') LIMIT 1"#,
            user_code,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }

    /// Insert a fresh device-auth session (cloud starts `pending`; self-host starts `confirmed` with a
    /// server-derived device-rooted principal). Stores ONLY the device code's sha256; the user code plaintext.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn insert_device_auth_session(
        &self,
        device_code_sha256: &[u8; 32],
        user_code: &str,
        ws: &WorkspaceId,
        invite_sha256: &[u8; 32],
        device_pubkey: &[u8; 32],
        device_key_id: &str,
        machine_name: &str,
        status: &str,
        confirmed_principal: Option<&str>,
        expires_at: i64,
        interval_secs: i64,
        created_at: &str,
    ) -> Result<()> {
        let (dc, ws_s, inv, pk) = (
            device_code_sha256.as_slice(),
            ws.as_str(),
            invite_sha256.as_slice(),
            device_pubkey.as_slice(),
        );
        sqlx::query!(
            "INSERT INTO device_auth_sessions \
               (device_code_sha256, user_code, workspace_id, invite_sha256, device_pubkey, device_key_id, \
                machine_name, status, confirmed_principal, expires_at, interval_secs, last_polled_at, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NULL, $12)",
            dc,
            user_code,
            ws_s,
            inv,
            pk,
            device_key_id,
            machine_name,
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
    /// device code is the indistinguishable `NotFound`.
    pub(crate) async fn poll_txn(
        &self,
        device_code_sha256: &[u8; 32],
        now: i64,
        created_at: &str,
        secret: &[u8; 32],
    ) -> Result<DeviceAuthPoll> {
        run_serializable!(self, tx, {
            poll_run(&mut tx, device_code_sha256, now, created_at, secret).await
        })
    }
}

struct SessionRow {
    user_code: String,
    workspace_id: String,
    invite_sha256: Option<Vec<u8>>,
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
        r#"SELECT user_code AS "user_code!", workspace_id AS "workspace_id!",
                  invite_sha256 AS "invite_sha256?: Vec<u8>", device_pubkey AS "device_pubkey!: Vec<u8>",
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
        invite_sha256: r.invite_sha256,
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
            let granted =
                issue_grant(tx, device_code_sha256, &session, now, created_at, secret).await?;
            Ok(DeviceAuthPoll::Granted(granted))
        }
        _ => Err(AuthorityError::integrity(EnrollCorrupt("session status"))),
    }
}

/// Issue (or re-derive) the single-use grant for a confirmed/issued session. The grant token is
/// deterministic in `(device_code_sha256, ws)`, so a re-poll re-derives it and the `INSERT OR IGNORE` is a
/// no-op — naturally idempotent. On the FIRST issue it binds the proven identity (principal, device, offered
/// skills) and flips the session to `issued`.
async fn issue_grant(
    tx: &mut Transaction<'_, Postgres>,
    device_code_sha256: &[u8; 32],
    session: &SessionRow,
    now: i64,
    created_at: &str,
    secret: &[u8; 32],
) -> Result<GrantIssued> {
    let ws = WorkspaceId::parse(&session.workspace_id).map_err(AuthorityError::integrity)?;
    let principal = session.confirmed_principal.as_deref().ok_or_else(|| {
        AuthorityError::integrity(EnrollCorrupt("confirmed session without principal"))
    })?;

    // The grant token: derive_token(b"grant", [device_code_sha256, ws]); store only its sha256.
    let grant_token = enroll::derive_token(
        secret,
        b"grant",
        &[device_code_sha256, ws.as_str().as_bytes()],
    );
    let grant_sha256 = enroll::sha256_token(&grant_token);

    // The offered skills = the session invite's offered skills (copied into the grant on first issue).
    let offered: Vec<SkillId> = match &session.invite_sha256 {
        Some(inv) => {
            let inv = inv.as_slice();
            let rows = sqlx::query!(
                r#"SELECT skill_id AS "skill_id!" FROM invite_skill WHERE token_sha256 = $1 ORDER BY skill_id"#,
                inv,
            )
            .fetch_all(&mut **tx)
            .await
            .map_err(AuthorityError::internal)?;
            rows.into_iter()
                .map(|r| SkillId::parse(&r.skill_id).map_err(AuthorityError::integrity))
                .collect::<Result<Vec<_>>>()?
        }
        None => Vec::new(),
    };

    let device_auth_id = session.user_code.clone();
    let device_key_id = session.device_key_id.clone();
    let expires_at = now.saturating_add(enroll::GRANT_TTL_MS);

    let (gs, ws_s, inv, pk) = (
        grant_sha256.as_slice(),
        ws.as_str(),
        session.invite_sha256.as_deref(),
        session.device_pubkey.as_slice(),
    );
    sqlx::query!(
        "INSERT INTO enrollment_grants \
           (grant_sha256, workspace_id, invite_sha256, principal, device_pubkey, device_key_id, \
            device_auth_id, expires_at, consumed_at, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9) \
         ON CONFLICT (grant_sha256) DO NOTHING",
        gs,
        ws_s,
        inv,
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

    for skill in &offered {
        let sk = skill.as_str();
        sqlx::query!(
            "INSERT INTO enrollment_grant_skill (grant_sha256, skill_id) VALUES ($1, $2) \
             ON CONFLICT (grant_sha256, skill_id) DO NOTHING",
            gs,
            sk,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }

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
        device_auth_id,
        device_key_id,
        offered_skills: offered,
        expires_at,
    })
}

// ── verification page: the live-session disclosure + the external-identity confirm ─────────────────────

/// A LIVE device-auth session resolved by `user_code` for the verification-page disclosure (no secret).
pub(crate) struct VerificationSessionRow {
    pub(crate) machine_name: String,
    pub(crate) device_pubkey: [u8; 32],
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) invite_sha256: Option<[u8; 32]>,
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
            r#"SELECT machine_name AS "machine_name!", device_pubkey AS "device_pubkey!: Vec<u8>",
                      workspace_id AS "workspace_id!", invite_sha256 AS "invite_sha256?: Vec<u8>"
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
                machine_name: r.machine_name,
                device_pubkey: blob32(&r.device_pubkey)?,
                workspace_id: WorkspaceId::parse(&r.workspace_id)
                    .map_err(AuthorityError::integrity)?,
                invite_sha256: r.invite_sha256.map(|b| blob32(&b)).transpose()?,
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
    // The live session this user code names (pending/confirmed), non-expired. Absent ⇒ the uniform miss.
    let row = sqlx::query!(
        r#"SELECT device_code_sha256 AS "device_code_sha256!: Vec<u8>" FROM device_auth_sessions
           WHERE user_code = $1 AND status IN ('pending', 'confirmed') AND expires_at >= $2 LIMIT 1"#,
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
    let dc = device_code_sha256.as_slice();
    let prin = principal.as_str();
    // Confirm the (externally-proven) principal — the device may now poll a grant. No code check: the OIDC
    // module already validated the id_token, so this is `complete_passcode`'s confirm half, minus the verify.
    sqlx::query!(
        "UPDATE device_auth_sessions SET status = 'confirmed', confirmed_principal = $2 \
         WHERE device_code_sha256 = $1",
        dc,
        prin,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(ConfirmOutcome::Confirmed)
}

// ── passcode: start (upsert) + complete (verify under cap), the verification-page second factor ────────

impl Db {
    /// Resolve the LIVE session a `user_code` names (pending/confirmed), returning its device-code sha256.
    /// `None` ⇒ no live session (the uniform miss).
    pub(crate) async fn live_session_device_code(
        &self,
        user_code: &str,
    ) -> Result<Option<[u8; 32]>> {
        let row = sqlx::query!(
            r#"SELECT device_code_sha256 AS "device_code_sha256!: Vec<u8>" FROM device_auth_sessions
               WHERE user_code = $1 AND status IN ('pending', 'confirmed') LIMIT 1"#,
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
    // The live, NON-EXPIRED session this user code names (pending/confirmed). Absent/expired ⇒ the uniform
    // miss — an expired session is the indistinguishable NotFound at every confirm entry point (matching the
    // poll + read_verification_session), not only at poll time.
    let row = sqlx::query!(
        r#"SELECT device_code_sha256 AS "device_code_sha256!: Vec<u8>" FROM device_auth_sessions
           WHERE user_code = $1 AND status IN ('pending', 'confirmed') AND expires_at >= $2 LIMIT 1"#,
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
        // Confirm the session's identity (the proven principal) — the device may now poll a grant.
        sqlx::query!(
            "UPDATE device_auth_sessions SET status = 'confirmed', confirmed_principal = $2 \
             WHERE device_code_sha256 = $1",
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

// ── redeem: the central possession-proof + gate + register + mint transaction ──────────────────────────

struct GrantRow {
    workspace_id: WorkspaceId,
    principal: String,
    device_pubkey: [u8; 32],
    device_key_id: String,
    device_auth_id: String,
    expires_at: i64,
}

impl Db {
    /// Redeem a grant into a registered device + minted read tokens. ONE `SERIALIZABLE` (`run_serializable!`) txn (the
    /// pointer-move's discipline). All `Denied` checks run BEFORE any write, so a denial has no side effect;
    /// only an all-checks-passed redeem confirms membership, registers the device, rosters the skills, and
    /// mints the (deterministic) read tokens — so a replay re-derives identical tokens with no extra creds.
    pub(crate) async fn redeem_txn(
        &self,
        input: &RedeemInput<'_>,
        secret: &[u8; 32],
    ) -> Result<RedeemOutcome> {
        run_serializable!(self, tx, redeem_run(&mut tx, input, secret).await)
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
    // (3) The grant binds exactly this device — the presented key + its server-derived id must match.
    if grant.device_pubkey != input.device_public_key
        || grant.device_key_id != input.server_device_key_id
    {
        return Ok(RedeemOutcome::Denied("device key mismatch"));
    }
    // The grant's offered skills (needed for the possession frame AND the mint loop).
    let offered = read_grant_skills(tx, gs).await?;

    // (4) POSSESSION PROOF: rebuild the enroll frame from SERVER-trusted values and verify against the
    // GRANT's bound public key (never a client-body key). A tampered key/skill-set changes the frame and fails.
    let offered_strs: Vec<&str> = offered.iter().map(SkillId::as_str).collect();
    let fields = EnrollFields {
        workspace_id: grant.workspace_id.as_str(),
        grant_hash: input.grant_sha256,
        device_auth_id: &grant.device_auth_id,
        device_key_id: input.server_device_key_id,
        device_public_key: input.device_public_key,
        offered_skill_ids: &offered_strs,
    };
    if !verify_enroll(&fields, input.enroll_sig, &grant.device_pubkey) {
        return Ok(RedeemOutcome::Denied("possession proof failed"));
    }

    // (5) THE GATE (deployment mode from the workspace row).
    let Some(workspace) = read_workspace_in_tx(tx, &grant.workspace_id).await? else {
        return Ok(RedeemOutcome::Denied("no such workspace"));
    };
    let cloud_invited = match workspace.deployment_mode.as_str() {
        "cloud" => {
            // Cloud requires a confirmed identity ALREADY on the roster (the invite carried no role).
            match read_member_status(tx, &grant.workspace_id, &grant.principal).await? {
                None => {
                    return Ok(RedeemOutcome::Denied(
                        "principal not on the workspace roster",
                    ));
                }
                Some(status) => status == "invited",
            }
        }
        // Self-host grants membership straight from the bearer.
        _ => false,
    };

    // (6) Anti-squat + revocation durability: a pre-existing device row must match (key, principal) exactly
    // AND must NOT be revoked. Without the revoked check, a revoked device could re-redeem its still-live
    // grant (a ~12-min TTL) and the deterministic mint loop below would RE-CREATE the read tokens the revoke
    // just deleted — undoing the kill switch within the grant window. A revoked device cannot re-enroll.
    if let Some((existing_pk, existing_principal, revoked)) =
        read_device(tx, &grant.workspace_id, input.server_device_key_id).await?
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
    let ws_s = grant.workspace_id.as_str();
    let prin = principal.as_str();

    // (5') Membership: cloud flips an `invited` row to `confirmed`; self-host inserts a confirmed member.
    if cloud_invited {
        sqlx::query!(
            "UPDATE workspace_member SET status = 'confirmed' \
             WHERE workspace_id = $1 AND principal = $2 AND status = 'invited'",
            ws_s,
            prin,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    } else if workspace.deployment_mode == "self_host" {
        sqlx::query!(
            "INSERT INTO workspace_member (workspace_id, principal, role, status, invited_by, added_at) \
             VALUES ($1, $2, 'member', 'confirmed', NULL, $3) \
             ON CONFLICT (workspace_id, principal) DO NOTHING",
            ws_s,
            prin,
            input.created_at,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }

    // (5'') REGISTER the device (idempotent — step 6 proved no conflicting row).
    let pk = input.device_public_key.as_slice();
    sqlx::query!(
        "INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, revoked) \
         VALUES ($1, $2, $3, $4, 0) \
         ON CONFLICT (workspace_id, device_key_id) DO NOTHING",
        ws_s,
        input.server_device_key_id,
        pk,
        prin,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    // (6') Per offered skill: roster the principal + mint the deterministic read token (store only its sha256).
    let mut read_tokens = Vec::with_capacity(offered.len());
    for skill in &offered {
        let sk = skill.as_str();
        sqlx::query!(
            "INSERT INTO roster (workspace_id, skill_id, principal) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, skill_id, principal) DO NOTHING",
            ws_s,
            sk,
            prin,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;

        let token = enroll::derive_token(secret, b"readtoken", &[gs, sk.as_bytes()]);
        let token_sha = enroll::sha256_token(&token);
        let ts = token_sha.as_slice();
        // Non-expiring (NULL) — the per-device revoke (DELETE these on revoke) is the kill switch. Bound to
        // the enrolling device so that revoke can find them. Deterministic ⇒ a replay re-derives the same row.
        sqlx::query!(
            "INSERT INTO read_token (workspace_id, skill_id, principal, token_sha256, device_key_id, expires_at) \
             VALUES ($1, $2, $3, $4, $5, NULL) \
             ON CONFLICT (token_sha256) DO UPDATE SET \
               workspace_id = excluded.workspace_id, skill_id = excluded.skill_id, \
               principal = excluded.principal, device_key_id = excluded.device_key_id, \
               expires_at = excluded.expires_at",
            ws_s,
            sk,
            prin,
            ts,
            input.server_device_key_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;

        read_tokens.push(MintedReadToken {
            skill_id: skill.clone(),
            token,
            expires_at: None,
        });
    }

    // (7) Audit marker (idempotent — a replay re-stamps, harmless).
    sqlx::query!(
        "UPDATE enrollment_grants SET consumed_at = $2 WHERE grant_sha256 = $1 AND consumed_at IS NULL",
        gs,
        input.now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    Ok(RedeemOutcome::Redeemed(EnrollmentRedeemed {
        workspace_id: grant.workspace_id,
        principal,
        device_key_id: input.server_device_key_id.to_owned(),
        read_tokens,
    }))
}

async fn read_grant(tx: &mut Transaction<'_, Postgres>, gs: &[u8]) -> Result<Option<GrantRow>> {
    let row = sqlx::query!(
        r#"SELECT workspace_id AS "workspace_id!", principal AS "principal!",
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
            workspace_id: WorkspaceId::parse(&r.workspace_id).map_err(AuthorityError::integrity)?,
            principal: r.principal,
            device_pubkey: blob32(&r.device_pubkey)?,
            device_key_id: r.device_key_id,
            device_auth_id: r.device_auth_id,
            expires_at: r.expires_at,
        })),
    }
}

async fn read_grant_skills(tx: &mut Transaction<'_, Postgres>, gs: &[u8]) -> Result<Vec<SkillId>> {
    let rows = sqlx::query!(
        r#"SELECT skill_id AS "skill_id!" FROM enrollment_grant_skill WHERE grant_sha256 = $1 ORDER BY skill_id"#,
        gs,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    rows.into_iter()
        .map(|r| SkillId::parse(&r.skill_id).map_err(AuthorityError::integrity))
        .collect()
}

async fn read_workspace_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
) -> Result<Option<WorkspaceRow>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT display_name AS "display_name!", verified_domain AS "verified_domain?",
                  verified_domain_status AS "verified_domain_status!", deployment_mode AS "deployment_mode!"
           FROM workspace WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| WorkspaceRow {
        display_name: r.display_name,
        verified_domain: r.verified_domain,
        verified_domain_status: r.verified_domain_status,
        deployment_mode: r.deployment_mode,
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
async fn read_device(
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

// ── governance: create-invite + roster/revoke mutations (owner-signed, in-txn authorized) ──────────────

/// The signing device resolved + verified — the shared governance preamble's success result.
struct GovernSigner {
    principal: Principal,
    role: Role,
    request_sha256: [u8; 32],
}

/// The governance preamble outcome (replay / authorized / authz-failure).
enum Preamble {
    /// A workspace_events hit — replay the stored outcome (or a key-reuse `Denied`).
    Replay(GovernanceOutcome),
    /// Authorized: the signer's confirmed principal + role + the request identity to record.
    Proceed(GovernSigner),
    /// A device/signature/role-resolution failure — record a DENIED event with this request identity.
    Fail(&'static str, [u8; 32]),
}

/// The shared in-transaction governance authorization: build the signing preimage, replay-check the op id,
/// resolve the SIGNING device to a non-revoked registered key + its bound principal, verify the governance
/// signature, and look up that principal's confirmed workspace role. The op-specific ROLE check (owner-only,
/// or owner-or-self) is the caller's — this returns the actor + role.
async fn govern_preamble(
    tx: &mut Transaction<'_, Postgres>,
    input: &GovernanceInput<'_>,
) -> Result<Preamble> {
    // Build the kernel governance frame from server-trusted values (the request scope + the typed op).
    let emails: Vec<&str>;
    let skills: Vec<&str>;
    let kind = match &input.signed.op {
        GovernanceOp::Invite {
            role,
            expires_at,
            emails: e,
            skills: s,
        } => {
            emails = e.iter().map(Principal::as_str).collect();
            skills = s.iter().map(|(id, _)| id.as_str()).collect();
            GovernanceOpKind::Invite {
                role: role.signing_byte(),
                expires_at: expires_to_u64(*expires_at),
                emails: &emails,
                skills: &skills,
            }
        }
        GovernanceOp::RosterSet { role, target } => GovernanceOpKind::RosterSet {
            role: role.signing_byte(),
            target: target.as_str(),
        },
        GovernanceOp::RosterRemove { target } => GovernanceOpKind::RosterRemove {
            target: target.as_str(),
        },
        GovernanceOp::DeviceRevoke {
            target_device_key_id,
        } => GovernanceOpKind::DeviceRevoke {
            target_device_key_id: target_device_key_id.as_str(),
        },
    };
    let fields = GovernanceOpFields {
        workspace_id: input.ws.as_str(),
        op_id: input.op_id_bytes,
        device_key_id: &input.signed.device_key_id,
        op: kind,
    };
    let preimage = governance_op_preimage(&fields)
        .map_err(|_| AuthorityError::internal(EnrollCorrupt("governance preimage")))?;
    let request_sha256 = digest::sha256(&preimage);

    // Replay BEFORE authz (mirrors the pointer-move): a since-revoked owner still replays its committed OK.
    if let Some((stored_req, outcome)) = read_event(tx, input.ws, input.op_id).await? {
        let replay = if stored_req == request_sha256 {
            match outcome.as_str() {
                "OK" => GovernanceOutcome::Ok,
                _ => GovernanceOutcome::Denied("replayed denial"),
            }
        } else {
            GovernanceOutcome::Denied("op id reused with a different request")
        };
        return Ok(Preamble::Replay(replay));
    }

    // Resolve the SIGNING device (non-revoked) + verify the governance signature.
    let Some((public_key, principal_s)) =
        read_active_device(tx, input.ws, &input.signed.device_key_id).await?
    else {
        return Ok(Preamble::Fail(
            "signing device unknown or revoked",
            request_sha256,
        ));
    };
    if !verify_governance_op(&fields, &input.signed.signature, &public_key) {
        return Ok(Preamble::Fail(
            "governance signature invalid",
            request_sha256,
        ));
    }
    let principal = Principal::parse(&principal_s).map_err(AuthorityError::integrity)?;
    // The signer must be a CONFIRMED member with a governance role.
    let Some((role_s, status)) = read_member_role(tx, input.ws, &principal).await? else {
        return Ok(Preamble::Fail(
            "signer is not a workspace member",
            request_sha256,
        ));
    };
    if status != "confirmed" {
        return Ok(Preamble::Fail(
            "signer is not a confirmed member",
            request_sha256,
        ));
    }
    let role = Role::parse(&role_s)
        .ok_or_else(|| AuthorityError::integrity(EnrollCorrupt("member role")))?;
    Ok(Preamble::Proceed(GovernSigner {
        principal,
        role,
        request_sha256,
    }))
}

impl Db {
    /// `create_invite`: the owner-signed invite mint. One `SERIALIZABLE` (`run_serializable!`) txn: governance authz (owner-only) →
    /// store the (orchestration-derived) invite + its skills → UPSERT the invited members → record the audit
    /// receipt. `invite_token_sha256` is the deterministic token's sha256 (the plaintext never reaches here).
    pub(crate) async fn create_invite_txn(
        &self,
        input: &GovernanceInput<'_>,
        invite_token_sha256: &[u8; 32],
    ) -> Result<GovernanceOutcome> {
        run_serializable!(
            self,
            tx,
            create_invite_run(&mut tx, input, invite_token_sha256).await
        )
    }

    /// A governance roster/revoke mutation (owner-only roster set/remove with the last-owner-lockout guard;
    /// owner-or-self device revoke that flips `revoked` AND purges the device's read tokens). One
    /// `SERIALIZABLE` (`run_serializable!`) txn per mutation.
    pub(crate) async fn governance_mutation_txn(
        &self,
        input: &GovernanceInput<'_>,
    ) -> Result<GovernanceOutcome> {
        run_serializable!(self, tx, governance_mutation_run(&mut tx, input).await)
    }
}

async fn create_invite_run(
    tx: &mut Transaction<'_, Postgres>,
    input: &GovernanceInput<'_>,
    invite_token_sha256: &[u8; 32],
) -> Result<GovernanceOutcome> {
    let GovernanceOp::Invite {
        role,
        expires_at,
        emails,
        skills,
    } = &input.signed.op
    else {
        return Ok(GovernanceOutcome::Denied("op is not an invite"));
    };
    let signer = match govern_preamble(tx, input).await? {
        Preamble::Replay(out) => return Ok(out),
        // A PRE-AUTHENTICATION failure (an unknown/revoked signing device or an invalid signature) is NOT
        // attributable to any verified actor, so it must NOT write a durable workspace_events row: recording it
        // would let an UNAUTHENTICATED network client forge audit entries (attacker-chosen actor/target for an
        // arbitrary workspace) and grow storage without bound. The authenticated-but-unauthorized denials below
        // (the role / last-owner guards, reached via Proceed) ARE recorded — they name a verified device.
        Preamble::Fail(reason, _req) => return Ok(GovernanceOutcome::Denied(reason)),
        Preamble::Proceed(s) => s,
    };
    // Owner-only.
    if signer.role != Role::Owner {
        record_event(tx, input, &signer.request_sha256, "DENIED", None).await?;
        return Ok(GovernanceOutcome::Denied("invite requires the owner role"));
    }

    let ws_s = input.ws.as_str();
    let actor = signer.principal.as_str();
    let tok = invite_token_sha256.as_slice();
    sqlx::query!(
        "INSERT INTO invites (token_sha256, workspace_id, expires_at, created_by, revoked, created_at) \
         VALUES ($1, $2, $3, $4, 0, $5) ON CONFLICT (token_sha256) DO NOTHING",
        tok,
        ws_s,
        *expires_at,
        actor,
        input.created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    for (skill, name) in skills {
        let sk = skill.as_str();
        sqlx::query!(
            "INSERT INTO invite_skill (token_sha256, skill_id, name) VALUES ($1, $2, $3) \
             ON CONFLICT (token_sha256, skill_id) DO UPDATE SET name = excluded.name",
            tok,
            sk,
            name.as_deref(),
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }
    let role_s = role.as_str();
    for email in emails {
        let em = email.as_str();
        // UPSERT the invited member, but NEVER downgrade an already-CONFIRMED one: keep both their status AND
        // their role. An invite is an ADD; re-inviting a member who already joined must not re-role them — and
        // in particular must not silently demote the last owner to a member (which would orphan the workspace,
        // the exact case roster_set guards with would_orphan_owner). Only a NEW/still-invited row takes the
        // invite's role.
        sqlx::query!(
            "INSERT INTO workspace_member (workspace_id, principal, role, status, invited_by, added_at) \
             VALUES ($1, $2, $3, 'invited', $4, $5) \
             ON CONFLICT (workspace_id, principal) DO UPDATE SET \
               role = CASE WHEN workspace_member.status = 'confirmed' THEN workspace_member.role ELSE excluded.role END, \
               invited_by = excluded.invited_by, \
               status = CASE WHEN workspace_member.status = 'confirmed' THEN 'confirmed' ELSE 'invited' END",
            ws_s,
            em,
            role_s,
            actor,
            input.created_at,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }
    let details = serde_json::json!({ "emails": emails.len(), "skills": skills.len() }).to_string();
    record_event(tx, input, &signer.request_sha256, "OK", Some(&details)).await?;
    Ok(GovernanceOutcome::Ok)
}

async fn governance_mutation_run(
    tx: &mut Transaction<'_, Postgres>,
    input: &GovernanceInput<'_>,
) -> Result<GovernanceOutcome> {
    let signer = match govern_preamble(tx, input).await? {
        Preamble::Replay(out) => return Ok(out),
        // Pre-authentication failure (unknown/revoked device or invalid signature): NOT attributable to a
        // verified actor, so record NOTHING (see create_invite_run) — an unauthenticated request can't forge
        // an audit row. Post-auth denials below (role / last-owner) are recorded against the verified device.
        Preamble::Fail(reason, _req) => return Ok(GovernanceOutcome::Denied(reason)),
        Preamble::Proceed(s) => s,
    };
    let ws_s = input.ws.as_str();

    let outcome = match &input.signed.op {
        GovernanceOp::RosterSet { role, target } => {
            if signer.role != Role::Owner {
                GovernanceOutcome::Denied("roster mutation requires the owner role")
            } else if would_orphan_owner(tx, input.ws, target.as_str(), Some(*role)).await? {
                GovernanceOutcome::Denied("would remove the last owner")
            } else {
                let (tgt, role_s) = (target.as_str(), role.as_str());
                sqlx::query!(
                    "INSERT INTO workspace_member (workspace_id, principal, role, status, invited_by, added_at) \
                     VALUES ($1, $2, $3, 'confirmed', $4, $5) \
                     ON CONFLICT (workspace_id, principal) DO UPDATE SET role = excluded.role",
                    ws_s,
                    tgt,
                    role_s,
                    ws_s, // invited_by: the workspace (a direct owner roster_set, not an invite chain)
                    input.created_at,
                )
                .execute(&mut **tx)
                .await
                .map_err(AuthorityError::internal)?;
                GovernanceOutcome::Ok
            }
        }
        GovernanceOp::RosterRemove { target } => {
            if signer.role != Role::Owner {
                GovernanceOutcome::Denied("roster mutation requires the owner role")
            } else if would_orphan_owner(tx, input.ws, target.as_str(), None).await? {
                GovernanceOutcome::Denied("would remove the last owner")
            } else {
                let tgt = target.as_str();
                // Remove the workspace membership AND, in the same transaction, revoke the principal's read
                // access instantly: drop every per-skill roster grant + read token they hold in this workspace
                // (the same instant-revoke discipline the device-revoke arm uses). Otherwise a removed member
                // would keep reading the workspace's skills through their surviving roster rows.
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
                GovernanceOutcome::Ok
            }
        }
        GovernanceOp::DeviceRevoke {
            target_device_key_id,
        } => {
            // Owner OR the device's own principal may revoke it.
            let target_principal = read_device(tx, input.ws, target_device_key_id)
                .await?
                .map(|(_, p, _)| p);
            let is_self = target_principal.as_deref() == Some(signer.principal.as_str());
            if signer.role != Role::Owner && !is_self {
                GovernanceOutcome::Denied(
                    "revoke requires the owner role or the device's own principal",
                )
            } else {
                // Instant per-device revoke: flip `revoked` AND drop the device's read tokens in one txn.
                sqlx::query!(
                    "UPDATE device_registry SET revoked = 1 WHERE workspace_id = $1 AND device_key_id = $2",
                    ws_s,
                    target_device_key_id,
                )
                .execute(&mut **tx)
                .await
                .map_err(AuthorityError::internal)?;
                sqlx::query!(
                    "DELETE FROM read_token WHERE workspace_id = $1 AND device_key_id = $2",
                    ws_s,
                    target_device_key_id,
                )
                .execute(&mut **tx)
                .await
                .map_err(AuthorityError::internal)?;
                GovernanceOutcome::Ok
            }
        }
        GovernanceOp::Invite { .. } => GovernanceOutcome::Denied("invite is not a roster mutation"),
    };

    let outcome_s = if matches!(outcome, GovernanceOutcome::Ok) {
        "OK"
    } else {
        "DENIED"
    };
    record_event(tx, input, &signer.request_sha256, outcome_s, None).await?;
    Ok(outcome)
}

/// Would setting `target` to `new_role` (or removing it, `new_role = None`) drop the confirmed-owner count to
/// zero? True only if `target` is CURRENTLY a confirmed owner, the change stops it being an owner, and it is
/// the LAST confirmed owner — the last-owner-lockout guard.
async fn would_orphan_owner(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    target: &str,
    new_role: Option<Role>,
) -> Result<bool> {
    let ws_s = ws.as_str();
    let target_is_owner = matches!(
        read_member_role(tx, ws, &Principal::parse(target).map_err(AuthorityError::internal)?).await?,
        Some((ref r, ref s)) if r == "owner" && s == "confirmed"
    );
    if !target_is_owner {
        return Ok(false);
    }
    if matches!(new_role, Some(Role::Owner)) {
        return Ok(false); // still an owner afterwards
    }
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM workspace_member
           WHERE workspace_id = $1 AND role = 'owner' AND status = 'confirmed'"#,
        ws_s,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.n <= 1)
}

// ── admin claim (self-host first-boot standup) ─────────────────────────────────────────────────────────

impl Db {
    /// Consume a one-time admin-claim token: stand up the workspace (self-host), seat its first owner, and
    /// register the claiming device. One `SERIALIZABLE` (`run_serializable!`) txn. An absent/consumed token is the uniform denial.
    pub(crate) async fn admin_claim_txn(
        &self,
        claim_sha256: &[u8; 32],
        server_device_key_id: &str,
        device_public_key: &[u8; 32],
        display_name: &str,
        now: i64,
        created_at: &str,
    ) -> Result<RedeemOutcome> {
        run_serializable!(self, tx, {
            admin_claim_run(
                &mut tx,
                claim_sha256,
                server_device_key_id,
                device_public_key,
                display_name,
                now,
                created_at,
            )
            .await
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn admin_claim_run(
    tx: &mut Transaction<'_, Postgres>,
    claim_sha256: &[u8; 32],
    server_device_key_id: &str,
    device_public_key: &[u8; 32],
    display_name: &str,
    now: i64,
    created_at: &str,
) -> Result<RedeemOutcome> {
    let cs = claim_sha256.as_slice();
    let claim = sqlx::query!(
        r#"SELECT workspace_id AS "workspace_id!", consumed_at AS "consumed_at?: i64"
           FROM admin_claim WHERE token_sha256 = $1"#,
        cs,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let Some(claim) = claim else {
        return Ok(RedeemOutcome::Denied("no such claim token"));
    };
    if claim.consumed_at.is_some() {
        return Ok(RedeemOutcome::Denied("claim token already consumed"));
    }
    let ws = WorkspaceId::parse(&claim.workspace_id).map_err(AuthorityError::integrity)?;
    let principal = enroll::device_rooted_principal(server_device_key_id)?;
    let (ws_s, prin) = (ws.as_str(), principal.as_str());

    sqlx::query!(
        "INSERT INTO workspace (workspace_id, display_name, verified_domain, verified_domain_status, deployment_mode, created_at) \
         VALUES ($1, $2, NULL, 'unverified', 'self_host', $3) \
         ON CONFLICT (workspace_id) DO NOTHING",
        ws_s,
        display_name,
        created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "INSERT INTO workspace_member (workspace_id, principal, role, status, invited_by, added_at) \
         VALUES ($1, $2, 'owner', 'confirmed', NULL, $3) ON CONFLICT (workspace_id, principal) DO NOTHING",
        ws_s,
        prin,
        created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    // Register the device (anti-squat + revocation as in redeem).
    if let Some((existing_pk, existing_principal, revoked)) =
        read_device(tx, &ws, server_device_key_id).await?
    {
        if &existing_pk != device_public_key || existing_principal != principal.as_str() {
            return Ok(RedeemOutcome::Denied("device key id already bound"));
        }
        if revoked {
            return Ok(RedeemOutcome::Denied("device is revoked"));
        }
    }
    let pk = device_public_key.as_slice();
    sqlx::query!(
        "INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, revoked) \
         VALUES ($1, $2, $3, $4, 0) ON CONFLICT (workspace_id, device_key_id) DO NOTHING",
        ws_s,
        server_device_key_id,
        pk,
        prin,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "UPDATE admin_claim SET consumed_at = $2 WHERE token_sha256 = $1 AND consumed_at IS NULL",
        cs,
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    Ok(RedeemOutcome::Redeemed(EnrollmentRedeemed {
        workspace_id: ws,
        principal,
        device_key_id: server_device_key_id.to_owned(),
        read_tokens: Vec::new(),
    }))
}

// ── shared in-txn helpers ──────────────────────────────────────────────────────────────────────────────

/// Resolve a NON-REVOKED registered device to `(public_key, principal)`. `None` ⇒ unknown or revoked.
async fn read_active_device(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    device_key_id: &str,
) -> Result<Option<([u8; 32], String)>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT public_key AS "public_key!: Vec<u8>", principal AS "principal!"
           FROM device_registry WHERE workspace_id = $1 AND device_key_id = $2 AND revoked = 0"#,
        ws_s,
        device_key_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some((blob32(&r.public_key)?, r.principal))),
    }
}

async fn read_member_role(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    principal: &Principal,
) -> Result<Option<(String, String)>> {
    let (ws_s, prin) = (ws.as_str(), principal.as_str());
    let row = sqlx::query!(
        r#"SELECT role AS "role!", status AS "status!" FROM workspace_member
           WHERE workspace_id = $1 AND principal = $2"#,
        ws_s,
        prin,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| (r.role, r.status)))
}

/// Read a workspace_events row's `(request_sha256, outcome)` for the op-id replay check.
async fn read_event(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &str,
) -> Result<Option<([u8; 32], String)>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT request_sha256 AS "request_sha256!: Vec<u8>", outcome AS "outcome!"
           FROM workspace_events WHERE workspace_id = $1 AND op_id = $2"#,
        ws_s,
        op_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some((blob32(&r.request_sha256)?, r.outcome))),
    }
}

/// Record the governance audit + idempotency row (one per op id; NO secret in `details`).
///
/// This is a **plain INSERT** (no `ON CONFLICT DO NOTHING`) on purpose: `workspace_events(workspace_id,
/// op_id)` is the idempotency slot that guards a NON-idempotent governance mutation (roster set/remove,
/// device revoke), which has already run earlier in this transaction over DISJOINT rows (so no co-located
/// CAS serializes it). Under SQLite the global writer lock made a concurrent same-`op_id` request see the
/// first's committed event and replay; under Postgres MVCC a silent `DO NOTHING` would let two fresh
/// same-`op_id` racers both mutate and only one record the event — an unreceipted second mutation. Failing
/// hard on the duplicate key (SQLSTATE 23505) instead aborts the loser's transaction, rolling its mutation
/// back; the loser's `op_id` retry then re-reads this committed row in [`govern_preamble`] and replays.
async fn record_event(
    tx: &mut Transaction<'_, Postgres>,
    input: &GovernanceInput<'_>,
    request_sha256: &[u8; 32],
    outcome: &str,
    details: Option<&str>,
) -> Result<()> {
    let ws_s = input.ws.as_str();
    let req = request_sha256.as_slice();
    let verb = input.signed.op.audit_verb();
    let target = input.signed.op.audit_target();
    // The actor is the SIGNING device key id (the confirmed principal is resolved per row; the audit "who"
    // is the device that signed — the request is bound to it).
    let actor = input.signed.device_key_id.as_str();
    sqlx::query!(
        "INSERT INTO workspace_events \
           (workspace_id, op_id, actor, gov_op_type, request_sha256, target, outcome, details, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        ws_s,
        input.op_id,
        actor,
        verb,
        req,
        target,
        outcome,
        details,
        input.created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// `expires_at` (epoch-ms; `None` = never) → the `u64` the governance frame binds (`None`/negative → 0).
fn expires_to_u64(expires_at: Option<i64>) -> u64 {
    u64::try_from(expires_at.unwrap_or(0)).unwrap_or(0)
}

/// Convert a stored 32-byte BLOB to a fixed array, or an integrity fault if the width is wrong (a CHECK
/// should forbid it; a violation is store corruption).
fn blob32(bytes: &[u8]) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(EnrollCorrupt("stored content id is not 32 bytes")))
}

/// A stored enrollment value violated an invariant (a width/range CHECK, an unparseable enum) — store corruption.
#[derive(Debug, thiserror::Error)]
#[error("enrollment store corruption: {0}")]
struct EnrollCorrupt(&'static str);
