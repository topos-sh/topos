//! The governance + admin-claim SQL — the raw-`sqlx` half (owner-signed create-invite, the roster/revoke
//! mutations, and the self-host first-boot admin claim).
//!
//! Split from [`super::enroll`] (the enrollment issuance SQL) so the role-gated governance surface — the
//! part with the owner-role matrix, the last-owner-lockout guard, and the `workspace_events` audit +
//! idempotency discipline — reads on its own. Mirrors [`crate::governance`] exactly as `enroll` mirrors
//! [`crate::enroll`]: the `SERIALIZABLE` (`run_serializable!`) governance/claim transactions live here, the
//! orchestration hands in server-trusted values (the parsed op id, the derived invite-token sha256) and gets
//! back domain outcomes. No `sqlx` type crosses the module boundary; every row is `workspace_id`-scoped; the
//! shared 32-byte-blob / device-row helpers stay in ONE home ([`super::enroll`]).

use sqlx::{Postgres, Transaction};
use topos_core::digest;
use topos_core::sign::{
    GovernanceOpFields, GovernanceOpKind, governance_op_preimage, verify_governance_op,
};

use super::Db;
use super::enroll::{EnrollCorrupt, blob32, read_device};
use crate::enroll::{self, EnrollmentRedeemed, RedeemOutcome};
use crate::error::{AuthorityError, Result};
use crate::governance::{GovernanceInput, GovernanceOp, GovernanceOutcome, Role};
use crate::id::{Principal, WorkspaceId};

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

// ── shared in-txn helpers (governance-only; the cross-domain ones live in [`super::enroll`]) ───────────

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
