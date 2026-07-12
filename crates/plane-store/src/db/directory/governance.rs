//! The governance + admin-claim SQL — the raw-`sqlx` half (owner-driven create-invite, the roster/revoke
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

use super::enroll::{EnrollCorrupt, read_device};
use super::witness::device_by_credential;
use crate::db::{Db, blob32};
use crate::enroll::{self, DeploymentMode, EnrollmentRedeemed, RedeemOutcome};
use crate::error::{AuthorityError, Result};
use crate::governance::{
    ApproveStandupOutcome, ClaimBootstrapRow, CreateWorkspaceOutcome, GovernanceInput,
    GovernanceOp, GovernanceOutcome, MintClaimDenied, Role, WorkspaceCreated,
};
use crate::id::{Principal, SkillId, WorkspaceId};

// ── governance: create-invite + roster/revoke mutations (device-credential, in-txn authorized) ─────────

/// The acting device resolved — the shared governance preamble's success result.
struct GovernActor {
    /// The resolved row's device key id (the audit/receipt actor — never a caller claim).
    device_key_id: String,
    principal: Principal,
    role: Role,
    request_sha256: [u8; 32],
}

/// The governance preamble outcome (replay / authorized / authz-failure).
enum Preamble {
    /// A workspace_events hit — replay the stored outcome (or a key-reuse `Denied`).
    Replay(GovernanceOutcome),
    /// Authorized: the actor's confirmed principal + role + the request identity to record.
    Proceed(GovernActor),
    /// A credential/role-resolution failure — NEVER recorded (see the callers' recording rule).
    Fail(&'static str),
}

/// The versioned domain tag the governance request identity binds — distinct from every session-lane
/// tag, so no stored identity from another domain can byte-match a governance request.
const GOVERNANCE_TAG: &[u8] = b"TOPOS_DEVICE_GOVERNANCE_V1";

/// The governance request identity: sha256 over [`GOVERNANCE_TAG`] + u64-be length-prefixed parts of the
/// FULL request — workspace, op id, the acting device's RESOLVED key id (the device's stable name,
/// never the presented credential: a credential rotates on re-enrollment, and a lost-ack retry across
/// that rotation must still byte-match its own slot), the op's kind byte, and every parameter
/// (list parameters ride as their count then per-element parts). The `emails`/`skills` lists are SETS —
/// canonicalized here (sorted + deduped) so the identity is order-independent, matching the seating
/// effect (order-independent UPSERTs): a lost-ack retry that reorders the list replays rather than
/// tripping the key-reuse guard. The `workspace_events` idempotency slot binds it: a same-op_id retry
/// must byte-match to replay; any divergent payload is a denied key-reuse. Deterministic, built from
/// server-trusted values.
fn governance_request_sha256(input: &GovernanceInput<'_>, acting_device_key_id: &str) -> [u8; 32] {
    fn put(buf: &mut Vec<u8>, part: &[u8]) {
        buf.extend_from_slice(&(part.len() as u64).to_be_bytes());
        buf.extend_from_slice(part);
    }
    /// Canonicalize a set-valued parameter: sorted + deduped, so its bytes are order-independent.
    fn put_set(buf: &mut Vec<u8>, mut parts: Vec<&str>) {
        parts.sort_unstable();
        parts.dedup();
        put(buf, &(parts.len() as u64).to_be_bytes());
        for part in parts {
            put(buf, part.as_bytes());
        }
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(GOVERNANCE_TAG);
    put(&mut buf, input.ws.as_str().as_bytes());
    put(&mut buf, input.op_id.as_bytes());
    put(&mut buf, acting_device_key_id.as_bytes());
    match &input.request.op {
        GovernanceOp::Invite {
            role,
            expires_at,
            emails,
            skills,
        } => {
            put(&mut buf, &[1, role.derivation_byte()]);
            put(&mut buf, &expires_to_u64(*expires_at).to_be_bytes());
            // Display names are advisory (never identity): only the skill ids bind.
            put_set(&mut buf, emails.iter().map(Principal::as_str).collect());
            put_set(&mut buf, skills.iter().map(|(id, _)| id.as_str()).collect());
        }
        GovernanceOp::RosterSet { role, target } => {
            put(&mut buf, &[2, role.derivation_byte()]);
            put(&mut buf, target.as_str().as_bytes());
        }
        GovernanceOp::RosterRemove { target } => {
            put(&mut buf, &[3]);
            put(&mut buf, target.as_str().as_bytes());
        }
        GovernanceOp::DeviceRevoke {
            target_device_key_id,
        } => {
            put(&mut buf, &[4]);
            put(&mut buf, target_device_key_id.as_bytes());
        }
    }
    digest::sha256(&buf)
}

/// The shared in-transaction governance authorization: resolve the ACTING workspace credential to its
/// registry row (the lookup IS the authentication — an unknown credential proceeds no further and can
/// bind no request identity), build the request identity from the RESOLVED device key id, replay-check
/// the op id, THEN enforce non-revoked + the confirmed workspace role. The resolve→replay→revoked order
/// is load-bearing: a since-revoked owner's credential still resolves (the row keeps its hash), so its
/// lost-ack retry still replays the committed OK, while its fresh work is denied. The op-specific ROLE
/// check (owner-only, or owner-or-self) is the caller's — this returns the actor + role.
async fn govern_preamble(
    tx: &mut Transaction<'_, Postgres>,
    input: &GovernanceInput<'_>,
) -> Result<Preamble> {
    // Resolve the ACTING credential FIRST — everything below (the request identity, the replay probe's
    // meaning, the audit actor) is keyed on the resolved device, and an unauthenticated caller gets one
    // uniform denial with no durable side effect.
    let Some(device) = device_by_credential(&mut **tx, input.ws, &input.credential_sha256).await?
    else {
        return Ok(Preamble::Fail("acting device unknown or revoked"));
    };
    let request_sha256 = governance_request_sha256(input, &device.device_key_id);

    // Replay BEFORE the revoked/role checks (mirrors the pointer-move): a since-revoked owner still
    // replays its committed OK.
    if let Some(stored) = read_event(tx, input.ws, input.op_id).await? {
        let replay = if stored.request_sha256 == request_sha256 {
            match stored.outcome.as_str() {
                "OK" => GovernanceOutcome::Ok,
                _ => GovernanceOutcome::Denied("replayed denial"),
            }
        } else {
            GovernanceOutcome::Denied("op id reused with a different request")
        };
        return Ok(Preamble::Replay(replay));
    }

    if device.revoked {
        return Ok(Preamble::Fail("acting device unknown or revoked"));
    }
    // The actor must be a CONFIRMED member with a governance role.
    let Some((role_s, status)) = read_member_role(tx, input.ws, &device.principal).await? else {
        return Ok(Preamble::Fail("actor is not a workspace member"));
    };
    if status != "confirmed" {
        return Ok(Preamble::Fail("actor is not a confirmed member"));
    }
    let role = Role::parse(&role_s)
        .ok_or_else(|| AuthorityError::integrity(EnrollCorrupt("member role")))?;
    Ok(Preamble::Proceed(GovernActor {
        device_key_id: device.device_key_id,
        principal: device.principal,
        role,
        request_sha256,
    }))
}

impl Db {
    /// `create_invite`: the owner-driven invite mint. One `SERIALIZABLE` (`run_serializable!`) txn: governance authz (owner-only) →
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
    } = &input.request.op
    else {
        return Ok(GovernanceOutcome::Denied("op is not an invite"));
    };
    let actor = match govern_preamble(tx, input).await? {
        Preamble::Replay(out) => return Ok(out),
        // A PRE-AUTHENTICATION failure (an unknown or revoked acting device) is NOT
        // attributable to any verified actor, so it must NOT write a durable workspace_events row: recording it
        // would let an UNAUTHENTICATED network client forge audit entries (attacker-chosen actor/target for an
        // arbitrary workspace) and grow storage without bound. The authenticated-but-unauthorized denials below
        // (the role / last-owner guards, reached via Proceed) ARE recorded — they name a verified device.
        Preamble::Fail(reason) => return Ok(GovernanceOutcome::Denied(reason)),
        Preamble::Proceed(s) => s,
    };
    // Owner-only.
    if actor.role != Role::Owner {
        record_event(tx, input, &actor, "DENIED", None).await?;
        return Ok(GovernanceOutcome::Denied("invite requires the owner role"));
    }

    mint_invite_row(
        tx,
        input.ws,
        invite_token_sha256,
        *expires_at,
        actor.principal.as_str(),
        *role,
        emails,
        skills,
        input.created_at,
    )
    .await?;
    let details = serde_json::json!({ "emails": emails.len(), "skills": skills.len() }).to_string();
    record_event(tx, input, &actor, "OK", Some(&details)).await?;
    Ok(GovernanceOutcome::Ok)
}

/// The credential-free invite ROW-WRITER: store the (hash-only) invite + its offered skills and UPSERT the
/// invited members. Shared by the owner-driven [`create_invite_run`] (which authorizes FIRST — its
/// in-transaction device-credential + owner-role check runs before this writer), the create-workspace
/// genesis (which mints the owner's self-invite inside the same transaction that seats that owner — no
/// acting device is presented; the caller's own gate is the authorization), and the web-session roster ops in
/// [`super::session_roster`] (same rule: the session acting gate is the authorization). Idempotent per row.
#[allow(clippy::too_many_arguments)]
pub(super) async fn mint_invite_row(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    invite_token_sha256: &[u8; 32],
    expires_at: Option<i64>,
    created_by: &str,
    role: Role,
    emails: &[Principal],
    skills: &[(SkillId, Option<String>)],
    created_at: &str,
) -> Result<()> {
    let ws_s = ws.as_str();
    let tok = invite_token_sha256.as_slice();
    sqlx::query!(
        "INSERT INTO invites (token_sha256, workspace_id, expires_at, created_by, revoked, created_at) \
         VALUES ($1, $2, $3, $4, 0, $5) ON CONFLICT (token_sha256) DO NOTHING",
        tok,
        ws_s,
        expires_at,
        created_by,
        created_at,
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
        // invite's role. (The genesis self-invite leans on exactly this: the owner is seated `confirmed`
        // first, so its own invite row never demotes it.)
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
            created_by,
            created_at,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }
    Ok(())
}

async fn governance_mutation_run(
    tx: &mut Transaction<'_, Postgres>,
    input: &GovernanceInput<'_>,
) -> Result<GovernanceOutcome> {
    let actor = match govern_preamble(tx, input).await? {
        Preamble::Replay(out) => return Ok(out),
        // Pre-authentication failure (an unknown or revoked acting device): NOT attributable to a
        // verified actor, so record NOTHING (see create_invite_run) — an unauthenticated request can't forge
        // an audit row. Post-auth denials below (role / last-owner) are recorded against the verified device.
        Preamble::Fail(reason) => return Ok(GovernanceOutcome::Denied(reason)),
        Preamble::Proceed(s) => s,
    };
    let ws_s = input.ws.as_str();

    let outcome = match &input.request.op {
        GovernanceOp::RosterSet { role, target } => {
            if actor.role != Role::Owner {
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
            if actor.role != Role::Owner {
                GovernanceOutcome::Denied("roster mutation requires the owner role")
            } else if would_orphan_owner(tx, input.ws, target.as_str(), None).await? {
                GovernanceOutcome::Denied("would remove the last owner")
            } else {
                let tgt = target.as_str();
                // Remove the workspace membership — the membership row IS the access: every read and
                // write gate joins against a CONFIRMED `workspace_member` row, so deleting it kills the
                // principal's access the moment this commits (their devices' credentials still
                // authenticate, and then every gate denies — fail closed, nothing cached). The per-skill
                // roster rows go too: they gate nothing anymore, but they are this principal's
                // follow-state in this workspace and a removed member has none.
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
            let is_self = target_principal.as_deref() == Some(actor.principal.as_str());
            if actor.role != Role::Owner && !is_self {
                GovernanceOutcome::Denied(
                    "revoke requires the owner role or the device's own principal",
                )
            } else {
                // Instant per-device revoke: flip `revoked` in one txn. The row (and its credential
                // hash) stays — a revoked device's credential still RESOLVES, so its lost-ack retry can
                // replay a stored receipt, while every fresh read/write is denied by the revoked check;
                // and a revoked device can never re-enroll (the redeem's anti-squat arm), so the
                // credential can never be re-armed.
                sqlx::query!(
                    "UPDATE device_registry SET revoked = 1 WHERE workspace_id = $1 AND device_key_id = $2",
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
    record_event(tx, input, &actor, outcome_s, None).await?;
    Ok(outcome)
}

/// Would setting `target` to `new_role` (or removing it, `new_role = None`) drop the confirmed-owner count to
/// zero? True only if `target` is CURRENTLY a confirmed owner, the change stops it being an owner, and it is
/// the LAST confirmed owner — the last-owner-lockout guard (shared with the session-leg remove).
pub(super) async fn would_orphan_owner(
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

// ── the ONE genesis seat (shared by admin-claim redeem, create-workspace, and the standup approve) ─────

/// The outcome of [`seat_workspace_and_owner`]: `Created` seated a FRESH workspace + its first owner;
/// `Exists` means the workspace id was already taken — the caller denies (or, for the random-id mint loop,
/// re-mints). The owner-member INSERT runs ONLY on `Created`, so no genesis path can ever seat an owner
/// into a live workspace.
enum GenesisSeat {
    Created,
    Exists,
}

/// Insert the workspace row and — ONLY if this call created it — its first `owner`/`confirmed` member, in
/// the caller's transaction. The `ON CONFLICT DO NOTHING … RETURNING` probe is the atomic created-or-exists
/// witness (0 rows ⇒ Exists); a true concurrent race pair serializes under `SERIALIZABLE` (the loser
/// retries and reads the winner's committed row ⇒ Exists). The deployment mode is a PARAMETER threaded from
/// the plane's own config by every caller — never from a request.
#[allow(clippy::too_many_arguments)]
async fn seat_workspace_and_owner(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    display_name: &str,
    deployment_mode: &str,
    verified_domain: Option<&str>,
    verified_domain_status: &str,
    owner: &Principal,
    created_at: &str,
) -> Result<GenesisSeat> {
    let (ws_s, prin) = (ws.as_str(), owner.as_str());
    let created = sqlx::query!(
        r#"INSERT INTO workspace (workspace_id, display_name, verified_domain, verified_domain_status, deployment_mode, created_at)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (workspace_id) DO NOTHING
           RETURNING workspace_id AS "workspace_id!""#,
        ws_s,
        display_name,
        verified_domain,
        verified_domain_status,
        deployment_mode,
        created_at,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if created.is_none() {
        return Ok(GenesisSeat::Exists);
    }
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
    Ok(GenesisSeat::Created)
}

/// How many workspaces `owner` already owns (confirmed `owner` memberships, plane-wide) — the per-identity
/// creation cap's count.
async fn owned_workspace_count(
    tx: &mut Transaction<'_, Postgres>,
    owner: &Principal,
) -> Result<i64> {
    let prin = owner.as_str();
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM workspace_member
           WHERE principal = $1 AND role = 'owner' AND status = 'confirmed'"#,
        prin,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.n)
}

/// The per-identity workspace-creation cap: a principal may own at most this many workspaces (the durable
/// floor under any rate limiting a composition adds in front).
const MAX_OWNED_WORKSPACES: i64 = 3;

/// A fresh `w_<32 hex>` workspace id from the OS CSPRNG (collision ⇒ the caller's seat returns `Exists`
/// and it re-mints; 128 bits make that ~impossible).
fn fresh_workspace_id() -> Result<WorkspaceId> {
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw)
        .map_err(|_| AuthorityError::internal(GenesisFault("entropy for a workspace id")))?;
    let mut hex = String::with_capacity(34);
    hex.push_str("w_");
    for b in raw {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    WorkspaceId::parse(&hex).map_err(AuthorityError::internal)
}

/// A genesis-standup invariant failed (entropy, or the ~impossible id-mint exhaustion).
#[derive(Debug, thiserror::Error)]
#[error("workspace genesis fault: {0}")]
struct GenesisFault(&'static str);

/// The shared genesis body both self-serve doors run AFTER their own idempotency/identity checks: the
/// per-owner cap, then mint-a-fresh-id + seat (bounded re-mint on the ~impossible collision). Returns the
/// typed denial (`Err` inner) without writing anything.
async fn genesis_create(
    tx: &mut Transaction<'_, Postgres>,
    owner: &Principal,
    display_name: &str,
    deployment_mode: &str,
    verified_domain: Option<&str>,
    verified_domain_status: &str,
    created_at: &str,
) -> Result<std::result::Result<WorkspaceId, &'static str>> {
    if owned_workspace_count(tx, owner).await? >= MAX_OWNED_WORKSPACES {
        return Ok(Err("workspace creation limit reached"));
    }
    for _ in 0..8 {
        let ws = fresh_workspace_id()?;
        match seat_workspace_and_owner(
            tx,
            &ws,
            display_name,
            deployment_mode,
            verified_domain,
            verified_domain_status,
            owner,
            created_at,
        )
        .await?
        {
            GenesisSeat::Created => return Ok(Ok(ws)),
            GenesisSeat::Exists => continue,
        }
    }
    Err(AuthorityError::internal(GenesisFault(
        "could not mint a fresh workspace id",
    )))
}

// ── admin claim (one-time bearer standup: self-host first boot + the cloud break-glass) ────────────────

/// An `admin_claim` row (the mint-time facts the redeem trusts; the request's display name is disclosure-only).
struct ClaimRow {
    workspace_id: String,
    consumed_at: Option<i64>,
    display_name: Option<String>,
    expires_at: Option<i64>,
    owner_email: Option<String>,
}

async fn read_claim_row(
    tx: &mut Transaction<'_, Postgres>,
    claim_sha256: &[u8],
) -> Result<Option<ClaimRow>> {
    let row = sqlx::query!(
        r#"SELECT workspace_id AS "workspace_id!", consumed_at AS "consumed_at?: i64",
                  display_name AS "display_name?", expires_at AS "expires_at?: i64",
                  owner_email AS "owner_email?"
           FROM admin_claim WHERE token_sha256 = $1"#,
        claim_sha256,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| ClaimRow {
        workspace_id: r.workspace_id,
        consumed_at: r.consumed_at,
        display_name: r.display_name,
        expires_at: r.expires_at,
        owner_email: r.owner_email,
    }))
}

/// The claim's seated owner principal: the mint-bound email when present, else the claiming device's
/// server-derived device-rooted principal. A stored email was validated at mint, so a re-parse failure is
/// store corruption.
fn claim_owner_principal(row: &ClaimRow, server_device_key_id: &str) -> Result<Principal> {
    match &row.owner_email {
        Some(email) => Principal::parse(email).map_err(AuthorityError::integrity),
        None => enroll::device_rooted_principal(server_device_key_id),
    }
}

impl Db {
    /// Mint a one-time admin-claim row (store only the token's sha256; the plaintext never reaches here).
    /// Refuses a claim for a workspace that already exists — the claim is a GENESIS capability, never a way
    /// into a live workspace. Re-minting for the same (still absent) workspace is allowed: multiple claim
    /// rows are harmless because the first redeem's genesis seat wins and every later one denies.
    pub(crate) async fn mint_admin_claim_txn(
        &self,
        token_sha256: &[u8; 32],
        ws: &WorkspaceId,
        display_name: Option<&str>,
        owner_email: Option<&str>,
        expires_at: i64,
        created_at: &str,
    ) -> Result<std::result::Result<(), MintClaimDenied>> {
        run_serializable!(self, tx, {
            mint_admin_claim_run(
                &mut tx,
                token_sha256,
                ws,
                display_name,
                owner_email,
                expires_at,
                created_at,
            )
            .await
        })
    }

    /// Consume a one-time admin-claim token: stand up the workspace (the PLANE's deployment mode — never a
    /// request's), seat its first owner, and register the claiming device. One `SERIALIZABLE`
    /// (`run_serializable!`) txn. All checks run before any write; an absent/consumed/expired token is the
    /// uniform denial, EXCEPT the same-device replay of an already-consumed claim, which deterministically
    /// re-returns `Redeemed` (lost-200 recovery).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn admin_claim_txn(
        &self,
        claim_sha256: &[u8; 32],
        server_device_key_id: &str,
        device_public_key: &[u8; 32],
        plane_mode: &str,
        now: i64,
        created_at: &str,
        secret: &[u8; 32],
    ) -> Result<RedeemOutcome> {
        run_serializable!(self, tx, {
            admin_claim_run(
                &mut tx,
                claim_sha256,
                server_device_key_id,
                device_public_key,
                plane_mode,
                now,
                created_at,
                secret,
            )
            .await
        })
    }

    /// Read an admin-claim row for the `/i/` bootstrap: unconsumed ∧ unexpired ⇒ its disclosure facts;
    /// consumed/expired/unknown ⇒ `None` (the caller's uniform NotFound). A pure pool read.
    pub(crate) async fn read_claim_bootstrap_row(
        &self,
        token_sha256: &[u8; 32],
        now: i64,
    ) -> Result<Option<ClaimBootstrapRow>> {
        let key = token_sha256.as_slice();
        let row = sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!", display_name AS "display_name?"
               FROM admin_claim
               WHERE token_sha256 = $1 AND consumed_at IS NULL
                 AND (expires_at IS NULL OR expires_at >= $2)"#,
            key,
            now,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(ClaimBootstrapRow {
                workspace_id: WorkspaceId::parse(&r.workspace_id)
                    .map_err(AuthorityError::integrity)?,
                display_name: r.display_name,
            })),
        }
    }

    /// Create a workspace for an already-verified owner email (door 2): the genesis-requests idempotency
    /// probe, the per-owner cap, the fresh-id seat, the deterministic self-invite, and the request ledger —
    /// ONE `SERIALIZABLE` (`run_serializable!`) txn.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_workspace_txn(
        &self,
        request_sha256: &[u8; 32],
        display_name: &str,
        owner: &Principal,
        plane_mode: &str,
        verified_domain: Option<&str>,
        verified_domain_status: &str,
        secret: &[u8; 32],
        created_at: &str,
    ) -> Result<CreateWorkspaceOutcome> {
        run_serializable!(self, tx, {
            create_workspace_run(
                &mut tx,
                request_sha256,
                display_name,
                owner,
                plane_mode,
                verified_domain,
                verified_domain_status,
                secret,
                created_at,
            )
            .await
        })
    }

    /// Approve a STANDUP device-auth session (the web leg's write half): resolve the live standup session,
    /// run the shared genesis body for the signed-in email, and CAS the session pending→confirmed with the
    /// fresh workspace — ONE `SERIALIZABLE` (`run_serializable!`) txn. The session CAS is the idempotency
    /// (an `AlreadyApproved` re-click replays; a different email is the uniform miss).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn approve_standup_txn(
        &self,
        user_code: &str,
        email: &Principal,
        display_name: &str,
        plane_mode: &str,
        verified_domain: Option<&str>,
        verified_domain_status: &str,
        now: i64,
        created_at: &str,
    ) -> Result<ApproveStandupOutcome> {
        run_serializable!(self, tx, {
            approve_standup_run(
                &mut tx,
                user_code,
                email,
                display_name,
                plane_mode,
                verified_domain,
                verified_domain_status,
                now,
                created_at,
            )
            .await
        })
    }
}

async fn mint_admin_claim_run(
    tx: &mut Transaction<'_, Postgres>,
    token_sha256: &[u8; 32],
    ws: &WorkspaceId,
    display_name: Option<&str>,
    owner_email: Option<&str>,
    expires_at: i64,
    created_at: &str,
) -> Result<std::result::Result<(), MintClaimDenied>> {
    let ws_s = ws.as_str();
    let exists = sqlx::query!(
        r#"SELECT 1::int8 AS "ok!: i64" FROM workspace WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if exists.is_some() {
        return Ok(Err(MintClaimDenied::WorkspaceExists));
    }
    let key = token_sha256.as_slice();
    sqlx::query!(
        "INSERT INTO admin_claim (token_sha256, workspace_id, consumed_at, created_at, display_name, expires_at, owner_email) \
         VALUES ($1, $2, NULL, $3, $4, $5, $6)",
        key,
        ws_s,
        created_at,
        display_name,
        expires_at,
        owner_email,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(Ok(()))
}

#[allow(clippy::too_many_arguments)]
async fn admin_claim_run(
    tx: &mut Transaction<'_, Postgres>,
    claim_sha256: &[u8; 32],
    server_device_key_id: &str,
    device_public_key: &[u8; 32],
    plane_mode: &str,
    now: i64,
    created_at: &str,
    secret: &[u8; 32],
) -> Result<RedeemOutcome> {
    let cs = claim_sha256.as_slice();
    // The claiming device's ONE workspace credential — deterministic in the claim (the same
    // `b"wscred"` derivation the grant redeem uses, over the claim's sha256), so the consumed-replay
    // probe below re-returns the IDENTICAL value on a lost-200 retry.
    let credential = enroll::derive_token(secret, b"wscred", &[cs]);
    let credential_sha = enroll::sha256_token(&credential);
    // (1) Resolve the claim. Absent ⇒ the uniform denial.
    let Some(claim) = read_claim_row(tx, cs).await? else {
        return Ok(RedeemOutcome::Denied("no such claim token"));
    };
    let ws = WorkspaceId::parse(&claim.workspace_id).map_err(AuthorityError::integrity)?;
    let principal = claim_owner_principal(&claim, server_device_key_id)?;

    // (2) CONSUMED-REPLAY PROBE — before the expiry check, so a lost-200 retry recovers even after the TTL
    // (expiry applies only to the FIRST consumption). If the consumed claim's workspace already holds THIS
    // exact device — same key id, same public key, same seated principal, not revoked — the original redeem
    // committed and this is the same caller retrying: deterministically re-return Redeemed (the credential
    // re-derives to the same value the original mint stored). Anything else
    // about a consumed claim is the one static denial.
    if claim.consumed_at.is_some() {
        if let Some((existing_pk, existing_principal, revoked)) =
            read_device(tx, &ws, server_device_key_id).await?
            && &existing_pk == device_public_key
            && existing_principal == principal.as_str()
            && !revoked
        {
            return Ok(RedeemOutcome::Redeemed(EnrollmentRedeemed {
                workspace_id: ws,
                principal,
                device_key_id: server_device_key_id.to_owned(),
                credential,
            }));
        }
        return Ok(RedeemOutcome::Denied("claim token already consumed"));
    }
    // (3) Expiry — first consumption only (nullable: legacy/test rows never expire).
    if claim.expires_at.is_some_and(|e| now > e) {
        return Ok(RedeemOutcome::Denied("claim token expired"));
    }
    // (4) Anti-squat + revocation (all checks BEFORE any write).
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
    // (4b) The per-identity creation cap — CLOUD claims only, the same durable floor `genesis_create`
    // enforces for the two self-serve doors (the break-glass claim must not seat a 4th cloud workspace
    // for one identity). Self-host claims stay uncapped: device-rooted, the operator-run posture with no
    // self-serve exposure. Sits AFTER the consumed-replay probe (a lost-200 replay by a now-at-cap owner
    // still recovers via the early Redeemed) and before the seat, so a denial writes nothing.
    if plane_mode == DeploymentMode::Cloud.as_str()
        && owned_workspace_count(tx, &principal).await? >= MAX_OWNED_WORKSPACES
    {
        return Ok(RedeemOutcome::Denied("workspace creation limit reached"));
    }

    // (5) Seat the workspace + first owner. The display name comes from the CLAIM ROW (the request's is
    // disclosure-only); the deployment mode is THE PLANE'S (a cloud plane's break-glass claim stands up a
    // cloud-mode workspace — never self_host on a cloud plane). Exists ⇒ denied, the claim NOT consumed.
    let display_name = claim.display_name.as_deref().unwrap_or(ws.as_str());
    match seat_workspace_and_owner(
        tx,
        &ws,
        display_name,
        plane_mode,
        None,
        "unverified",
        &principal,
        created_at,
    )
    .await?
    {
        GenesisSeat::Created => {}
        GenesisSeat::Exists => {
            return Ok(RedeemOutcome::Denied("workspace already exists"));
        }
    }

    // (6) Register the claiming device WITH its workspace credential; (7) consume the claim (CAS on the
    // unconsumed row).
    let (ws_s, prin) = (ws.as_str(), principal.as_str());
    let pk = device_public_key.as_slice();
    let crs = credential_sha.as_slice();
    sqlx::query!(
        "INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, revoked, credential_sha256) \
         VALUES ($1, $2, $3, $4, 0, $5) \
         ON CONFLICT (workspace_id, device_key_id) DO UPDATE SET credential_sha256 = excluded.credential_sha256",
        ws_s,
        server_device_key_id,
        pk,
        prin,
        crs,
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
        credential,
    }))
}

// ── create-workspace (door 2) + the standup approve (door 1's web leg) ─────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn create_workspace_run(
    tx: &mut Transaction<'_, Postgres>,
    request_sha256: &[u8; 32],
    display_name: &str,
    owner: &Principal,
    plane_mode: &str,
    verified_domain: Option<&str>,
    verified_domain_status: &str,
    secret: &[u8; 32],
    created_at: &str,
) -> Result<CreateWorkspaceOutcome> {
    let req = request_sha256.as_slice();
    // (1) The idempotency probe: a replay of the SAME request by the SAME owner returns the workspace it
    // already created (re-deriving the identical self-invite token); the same request id under a DIFFERENT
    // owner is denied — the slot belongs to the original.
    let prior = sqlx::query!(
        r#"SELECT owner_principal AS "owner_principal!", workspace_id AS "workspace_id!"
           FROM genesis_requests WHERE request_sha256 = $1"#,
        req,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if let Some(prior) = prior {
        if prior.owner_principal != owner.as_str() {
            return Ok(CreateWorkspaceOutcome::Denied("request id already used"));
        }
        let ws = WorkspaceId::parse(&prior.workspace_id).map_err(AuthorityError::integrity)?;
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT display_name AS "display_name!" FROM workspace WHERE workspace_id = $1"#,
            ws_s,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        let Some(row) = row else {
            return Err(AuthorityError::integrity(GenesisFault(
                "genesis_requests names a missing workspace",
            )));
        };
        let invite_token = self_invite_token(secret, request_sha256, &ws);
        return Ok(CreateWorkspaceOutcome::Replayed(WorkspaceCreated {
            workspace_id: ws,
            display_name: row.display_name,
            invite_token,
        }));
    }

    // (2) The shared genesis body (cap → fresh-id seat).
    let ws = match genesis_create(
        tx,
        owner,
        display_name,
        plane_mode,
        verified_domain,
        verified_domain_status,
        created_at,
    )
    .await?
    {
        Ok(ws) => ws,
        Err(reason) => return Ok(CreateWorkspaceOutcome::Denied(reason)),
    };

    // (3) The owner's SELF-INVITE — the paste-to-agent link the web shows. Deterministic in the request, so
    // a replay re-derives the SAME link; member role (the owner row is already seated `confirmed`, and the
    // row-writer never demotes it); no skills; no expiry.
    let invite_token = self_invite_token(secret, request_sha256, &ws);
    let invite_sha256 = enroll::sha256_token(&invite_token);
    mint_invite_row(
        tx,
        &ws,
        &invite_sha256,
        None,
        owner.as_str(),
        Role::Member,
        std::slice::from_ref(owner),
        &[],
        created_at,
    )
    .await?;

    // (4) The request ledger — a plain INSERT: `genesis_requests_pkey` is in the runner's CONVERGENT 23505
    // set, so a concurrent same-request racer aborts, retries, and replays the winner's workspace.
    let ws_s = ws.as_str();
    let prin = owner.as_str();
    sqlx::query!(
        "INSERT INTO genesis_requests (request_sha256, owner_principal, workspace_id, created_at) \
         VALUES ($1, $2, $3, $4)",
        req,
        prin,
        ws_s,
        created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    let row = sqlx::query!(
        r#"SELECT display_name AS "display_name!" FROM workspace WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(CreateWorkspaceOutcome::Created(WorkspaceCreated {
        workspace_id: ws,
        display_name: row.display_name,
        invite_token,
    }))
}

/// The deterministic self-invite token for a created workspace: the create-invite derivation shape with the
/// REQUEST identity (its sha256) in the op-id slot — so a lost-ack replay re-derives the identical link.
/// Binds the member role byte, an empty skill set, and the no-expiry sentinel, exactly as an owner-driven
/// invite with those parameters would. Shared with [`super::session_roster`]'s door resolution (a
/// create-page-born workspace's standing door IS this token until the first rotation revokes it).
pub(super) fn self_invite_token(
    secret: &[u8; 32],
    request_sha256: &[u8; 32],
    ws: &WorkspaceId,
) -> String {
    let role_byte = [Role::Member.derivation_byte()];
    let expires_be = crate::governance::INVITE_NO_EXPIRY.to_be_bytes();
    enroll::derive_token(
        secret,
        b"invite",
        &[
            request_sha256.as_slice(),
            ws.as_str().as_bytes(),
            role_byte.as_slice(),
            b"",
            expires_be.as_slice(),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
async fn approve_standup_run(
    tx: &mut Transaction<'_, Postgres>,
    user_code: &str,
    email: &Principal,
    display_name: &str,
    plane_mode: &str,
    verified_domain: Option<&str>,
    verified_domain_status: &str,
    now: i64,
    created_at: &str,
) -> Result<ApproveStandupOutcome> {
    // (1) The live STANDUP session this code names. Unknown / non-standup / resolved ⇒ the uniform miss.
    let row = sqlx::query!(
        r#"SELECT status AS "status!", confirmed_principal AS "confirmed_principal?",
                  workspace_id AS "workspace_id?", expires_at AS "expires_at!: i64"
           FROM device_auth_sessions
           WHERE user_code = $1 AND intent = 'standup' AND status IN ('pending', 'confirmed')
           LIMIT 1"#,
        user_code,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let Some(row) = row else {
        return Err(AuthorityError::NotFound);
    };
    // (2) FIRST-WRITER-WINS on the already-confirmed session: the SAME email's re-click replays
    // (AlreadyApproved); a DIFFERENT email is the uniform miss — an approval is never re-bound.
    if row.status == "confirmed" {
        if row.confirmed_principal.as_deref() == Some(email.as_str()) {
            let ws_str = row.workspace_id.as_deref().ok_or_else(|| {
                AuthorityError::integrity(EnrollCorrupt(
                    "confirmed standup session without workspace",
                ))
            })?;
            let ws = WorkspaceId::parse(ws_str).map_err(AuthorityError::integrity)?;
            return Ok(ApproveStandupOutcome::AlreadyApproved { workspace_id: ws });
        }
        return Err(AuthorityError::NotFound);
    }
    // (3) A pending-but-expired session is the same uniform miss (poll flips it to `expired` on its own).
    if now > row.expires_at {
        return Err(AuthorityError::NotFound);
    }

    // (4) The shared genesis body (cap → fresh-id seat) for the signed-in owner. A cap denial propagates
    // typed to the approving web page.
    let ws = match genesis_create(
        tx,
        email,
        display_name,
        plane_mode,
        verified_domain,
        verified_domain_status,
        created_at,
    )
    .await?
    {
        Ok(ws) => ws,
        Err(reason) => return Ok(ApproveStandupOutcome::Denied(reason)),
    };

    // (5) The session CAS — pending→confirmed with the fresh workspace + the approving email. The
    // `status = 'pending'` arm is the idempotency/race guard (a raced second approve loses to the first and
    // resolves via the confirmed branch on its retry — under SERIALIZABLE the racers serialize anyway).
    let ws_s = ws.as_str();
    let prin = email.as_str();
    let updated = sqlx::query!(
        "UPDATE device_auth_sessions \
         SET workspace_id = $2, confirmed_principal = $3, status = 'confirmed' \
         WHERE user_code = $1 AND intent = 'standup' AND status = 'pending'",
        user_code,
        ws_s,
        prin,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if updated.rows_affected() == 0 {
        return Err(AuthorityError::NotFound);
    }
    let row = sqlx::query!(
        r#"SELECT display_name AS "display_name!" FROM workspace WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(ApproveStandupOutcome::Approved {
        workspace_id: ws,
        display_name: row.display_name,
    })
}

// ── shared in-txn helpers (governance-only; the cross-domain ones live in [`super::enroll`]) ───────────

// `pub(in crate::db)`: shared across the seam — the custody pointer-move transaction
// (`db::custody::set_current`) reads the acting principal's workspace role from it.
pub(in crate::db) async fn read_member_role(
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

/// A stored workspace_events row's replay-relevant facts (both legs' op-id replay check reads this;
/// `details` carries what a session-leg replay needs to re-derive its byte-identical response).
pub(super) struct StoredEvent {
    pub(super) request_sha256: [u8; 32],
    pub(super) outcome: String,
    pub(super) details: Option<String>,
}

/// Read a workspace_events row for the op-id replay check.
pub(super) async fn read_event(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &str,
) -> Result<Option<StoredEvent>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT request_sha256 AS "request_sha256!: Vec<u8>", outcome AS "outcome!",
                  details AS "details?"
           FROM workspace_events WHERE workspace_id = $1 AND op_id = $2"#,
        ws_s,
        op_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(StoredEvent {
            request_sha256: blob32(&r.request_sha256)?,
            outcome: r.outcome,
            details: r.details,
        })),
    }
}

/// The plain-field workspace_events row both legs write through — the device lane via
/// [`record_event`] (actor = the acting device key id, method `device`), the session lane in
/// [`super::session_roster`] (actor = the acting principal's verified email, method `web_session`).
pub(super) struct EventRecord<'a> {
    pub(super) ws: &'a WorkspaceId,
    pub(super) op_id: &'a str,
    pub(super) actor: &'a str,
    pub(super) gov_op_type: &'a str,
    pub(super) request_sha256: &'a [u8; 32],
    pub(super) target: Option<&'a str>,
    pub(super) outcome: &'a str,
    pub(super) details: Option<&'a str>,
    pub(super) method: &'a str,
    pub(super) created_at: &'a str,
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
pub(super) async fn record_event_raw(
    tx: &mut Transaction<'_, Postgres>,
    rec: &EventRecord<'_>,
) -> Result<()> {
    let ws_s = rec.ws.as_str();
    let req = rec.request_sha256.as_slice();
    sqlx::query!(
        "INSERT INTO workspace_events \
           (workspace_id, op_id, actor, gov_op_type, request_sha256, target, outcome, details, method, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        ws_s,
        rec.op_id,
        rec.actor,
        rec.gov_op_type,
        req,
        rec.target,
        rec.outcome,
        rec.details,
        rec.method,
        rec.created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// The DEVICE lane's receipt: actor = the acting device's RESOLVED key id (the audit "who" is the
/// acting device the credential resolved to — the request identity is bound to it), method `device`.
async fn record_event(
    tx: &mut Transaction<'_, Postgres>,
    input: &GovernanceInput<'_>,
    actor: &GovernActor,
    outcome: &str,
    details: Option<&str>,
) -> Result<()> {
    record_event_raw(
        tx,
        &EventRecord {
            ws: input.ws,
            op_id: input.op_id,
            actor: &actor.device_key_id,
            gov_op_type: input.request.op.audit_verb(),
            request_sha256: &actor.request_sha256,
            target: input.request.op.audit_target(),
            outcome,
            details,
            method: "device",
            created_at: input.created_at,
        },
    )
    .await
}

/// `expires_at` (epoch-ms; `None` = never) → the `u64` the governance frame binds (`None`/negative → 0).
fn expires_to_u64(expires_at: Option<i64>) -> u64 {
    u64::try_from(expires_at.unwrap_or(0)).unwrap_or(0)
}
