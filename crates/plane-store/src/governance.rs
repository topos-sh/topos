//! The governance + admin-claim core — the orchestration half (outside the transaction).
//!
//! Split from [`crate::enroll`] (the enrollment issuance orchestration) so the role-gated governance
//! surface — the owner-signed invite mint, the roster/revoke mutations, and the self-host first-boot
//! admin claim — reads on its own. This module models the signed governance ops and the workspace roles
//! and does the work OUTSIDE the one write transaction (parse the op id, derive the deterministic invite
//! link, build the server-trusted inputs); the raw SQL — and the `SERIALIZABLE` (`run_serializable!`)
//! governance/claim transactions — live in [`crate::db`]. The credential derivations (HMAC token mint,
//! sha256 storage form, the server-derived device key id) stay in ONE home, [`crate::enroll`].

use crate::authority::Authority;
use crate::enroll::{
    DeploymentMode, InviteBootstrap, RedeemOutcome, device_key_id_for, parse_op_id,
    random_claim_token, sha256_token,
};
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, SkillId, WorkspaceId};

// ── governance op modeling (an owned mirror of the kernel's borrowed GovernanceOpKind) ─────────────────

/// An owned mirror of [`topos_core::sign::GovernanceOpKind`], so the [`Authority`](crate::Authority) can
/// carry a governance op across the call/transaction boundary and rebuild the borrowed
/// `GovernanceOpFields` for the signing preimage in-transaction. Each variant carries its full signed
/// parameter set; `Invite` additionally carries the per-skill display `name`s (NOT bound into the preimage —
/// only the skill ids are — so a rename never forks the deterministic invite link).
#[derive(Debug, Clone)]
pub enum GovernanceOp {
    /// Invite principals at `role`, expiring `expires_at` (epoch-ms; `None` = never), pre-offering `skills`.
    Invite {
        /// The role the invitees are granted on the workspace roster.
        role: Role,
        /// The invite expiry (epoch-ms; `None` = never expires).
        expires_at: Option<i64>,
        /// The invited principals (the emails), bound into the preimage as a set.
        emails: Vec<Principal>,
        /// The pre-offered skills, each with an optional display name (the name is NOT in the preimage).
        skills: Vec<(SkillId, Option<String>)>,
    },
    /// Set `target`'s workspace role (owner-only).
    RosterSet {
        /// The role to set.
        role: Role,
        /// The principal whose role is set.
        target: Principal,
    },
    /// Remove `target` from the workspace roster (owner-only).
    RosterRemove {
        /// The principal removed.
        target: Principal,
    },
    /// Revoke a registered device key (owner, or the device's own principal).
    DeviceRevoke {
        /// The id of the device key revoked.
        target_device_key_id: String,
    },
}

impl GovernanceOp {
    /// The audit verb string (`workspace_events.gov_op_type`).
    pub(crate) fn audit_verb(&self) -> &'static str {
        match self {
            GovernanceOp::Invite { .. } => "invite",
            GovernanceOp::RosterSet { .. } => "roster_set",
            GovernanceOp::RosterRemove { .. } => "roster_remove",
            GovernanceOp::DeviceRevoke { .. } => "device_revoke",
        }
    }

    /// The op's `target` for the audit row (the affected principal/device; `None` for an invite — its
    /// targets are the multiple invited emails, recorded in `details` instead).
    pub(crate) fn audit_target(&self) -> Option<&str> {
        match self {
            GovernanceOp::Invite { .. } => None,
            GovernanceOp::RosterSet { target, .. } | GovernanceOp::RosterRemove { target } => {
                Some(target.as_str())
            }
            GovernanceOp::DeviceRevoke {
                target_device_key_id,
            } => Some(target_device_key_id.as_str()),
        }
    }
}

/// A governance request signed by an owner's registered device key. The signature is over the kernel
/// governance frame the plane reconstructs from server-trusted values (the request scope + the typed `op`),
/// so a valid signature IS the binding of this device to this exact op — never a client-claimed authority.
#[derive(Debug, Clone)]
pub struct GovernanceSignedOp {
    /// The id of the **signing** owner's device key (the registry selects the public key by this).
    pub device_key_id: String,
    /// The governance op (its kind + parameter tail are rebuilt into the signing preimage).
    pub op: GovernanceOp,
    /// The raw 64-byte Ed25519 governance-op signature.
    pub signature: [u8; 64],
}

/// The result of creating an invite — the shareable link plus the roster + skills it seeded. Re-derives
/// byte-identically on an `op_id` replay (the link is deterministic; the roster/skills come from the op).
#[derive(Debug, Clone)]
pub struct InviteCreated {
    /// The `/i/<token>` link to share (the plaintext token appears ONLY here — only its sha256 is stored).
    pub link: String,
    /// The principals UPSERTed onto the workspace roster as `invited`.
    pub roster_added: Vec<Principal>,
    /// The skills the invite offers, each with its optional display name.
    pub skills: Vec<(SkillId, Option<String>)>,
}

/// The outcome of [`Authority::create_invite`](crate::Authority::create_invite).
#[derive(Debug, Clone)]
pub enum CreateInviteOutcome {
    /// The invite was created (or replayed).
    Created(InviteCreated),
    /// The request was denied (a uniform denial; the static reason is for server logs, never an oracle).
    Denied(&'static str),
}

/// The outcome of a governance mutation (roster set/remove, device revoke).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernanceOutcome {
    /// The mutation applied (or replayed).
    Ok,
    /// The mutation was denied (a uniform denial; the static reason is for server logs).
    Denied(&'static str),
}

/// A workspace-level governance role (the `workspace_member` RBAC roster — DISTINCT from the per-skill read
/// `roster`). `owner` signs invites + roster mutations; `member`/`reviewer` cannot. The `signing_byte` is the
/// `u8` bound into the governance/invite signing frames — one mapping, used on both the sign and verify sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Full governance authority (invite, roster, revoke).
    Owner,
    /// A reviewer (review-gate authority; no governance authority in v0).
    Reviewer,
    /// An ordinary member (no governance authority).
    Member,
}

impl Role {
    /// The stored discriminant (`workspace_member.role`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Reviewer => "reviewer",
            Role::Member => "member",
        }
    }

    /// Parse a stored discriminant. `None` on an unknown value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Role::Owner),
            "reviewer" => Some(Role::Reviewer),
            "member" => Some(Role::Member),
            _ => None,
        }
    }

    /// The `u8` bound into the invite/governance signing frame (owner = 1, reviewer = 2, member = 3) —
    /// delegating to the kernel's `GovernanceRole`, the single mapping the client signer calls too, so
    /// this signature-preimage input can never fork between the halves.
    #[must_use]
    pub fn signing_byte(self) -> u8 {
        match self {
            Role::Owner => topos_core::sign::GovernanceRole::Owner,
            Role::Reviewer => topos_core::sign::GovernanceRole::Reviewer,
            Role::Member => topos_core::sign::GovernanceRole::Member,
        }
        .signing_byte()
    }
}

/// A freshly-minted one-time admin claim. The `token` is the bearer plaintext, returned ONCE (only its
/// sha256 is stored); `Debug` REDACTS it so it can never reach a log or trace through a formatted value.
#[derive(Clone)]
pub struct MintedClaim {
    /// The claim-token plaintext (shown once by the minting surface; never logged, never stored).
    pub token: String,
    /// The claim's expiry (epoch-ms) — enforced on the FIRST consumption only.
    pub expires_at: i64,
}

impl std::fmt::Debug for MintedClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedClaim")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The outcome of [`Authority::mint_admin_claim`](crate::Authority::mint_admin_claim).
#[derive(Debug, Clone)]
pub enum MintClaimOutcome {
    /// The claim was minted (the plaintext token is returned once, redacted in `Debug`).
    Minted(MintedClaim),
    /// The mint was refused (a typed reason for the minting operator/surface — this is a privileged
    /// operator/composition op, so the reason is not an oracle).
    Denied(&'static str),
}

/// The db-half's typed mint refusal (mapped onto [`MintClaimOutcome::Denied`] by the orchestration).
#[derive(Debug, Clone, Copy)]
pub(crate) enum MintClaimDenied {
    /// The named workspace already exists — a claim is a genesis capability, never a door into a live one.
    WorkspaceExists,
}

/// A created (or replayed) workspace — what both create-workspace outcomes carry: the fresh id, the display
/// name actually stored, and the owner's deterministic self-invite token (the `/i/` link tail a replay
/// re-derives byte-identically).
#[derive(Debug, Clone)]
pub struct WorkspaceCreated {
    /// The fresh server-minted workspace id.
    pub workspace_id: WorkspaceId,
    /// The stored display name (the caller's, or the server default from the owner email's local part).
    pub display_name: String,
    /// The owner's self-invite token (compose `<base_url>/i/<token>` to show it; deterministic per request).
    pub invite_token: String,
}

/// The outcome of [`Authority::create_workspace`](crate::Authority::create_workspace).
#[derive(Debug, Clone)]
pub enum CreateWorkspaceOutcome {
    /// A fresh workspace was created and its owner seated.
    Created(WorkspaceCreated),
    /// The SAME request (same id, same owner) already created a workspace — the identical result replays.
    Replayed(WorkspaceCreated),
    /// The request was denied (a static, typed reason — the cap, a reused request id, a bad email).
    Denied(&'static str),
}

/// The outcome of [`Authority::approve_standup`](crate::Authority::approve_standup). An unknown / expired /
/// raced / re-bound `user_code` is the uniform [`AuthorityError::NotFound`], not a variant — indistinguishable
/// misses stay indistinguishable.
#[derive(Debug, Clone)]
pub enum ApproveStandupOutcome {
    /// The session was approved: the workspace exists and the session's next poll yields a grant.
    Approved {
        /// The fresh workspace's id.
        workspace_id: WorkspaceId,
        /// The stored display name.
        display_name: String,
    },
    /// The SAME email already approved this session (an idempotent re-click).
    AlreadyApproved {
        /// The workspace the earlier approval created.
        workspace_id: WorkspaceId,
    },
    /// The approval was denied (the per-owner workspace cap).
    Denied(&'static str),
}

/// An unconsumed, unexpired admin-claim row's disclosure facts (the `/i/` bootstrap read's db half).
pub(crate) struct ClaimBootstrapRow {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) display_name: Option<String>,
}

/// The server-trusted inputs to a governance transaction (create-invite or a roster/revoke mutation).
pub(crate) struct GovernanceInput<'a> {
    /// The target workspace.
    pub ws: &'a WorkspaceId,
    /// The client-minted op id (the idempotency key).
    pub op_id: &'a str,
    /// The op id's raw 16 bytes (bound into the governance signing frame).
    pub op_id_bytes: [u8; 16],
    /// The signed governance op (the signing device key id + the typed op + the signature).
    pub signed: &'a GovernanceSignedOp,
    /// The server-stamped creation timestamp (governance rows are timestamped by `created_at`, not the clock).
    pub created_at: &'a str,
}

// ── the orchestration ops (the public Authority methods delegate to these) ─────────────────────────────

/// Create an owner-signed invite (the orchestration half of [`Authority::create_invite`]). Derives the
/// deterministic invite token, then runs the one governance transaction (authz + store + roster + receipt).
pub(crate) async fn create_invite(
    authority: &Authority,
    ws: &WorkspaceId,
    op_id: &str,
    signed: &GovernanceSignedOp,
    created_at: &str,
) -> Result<CreateInviteOutcome> {
    let GovernanceOp::Invite {
        role,
        expires_at,
        emails,
        skills,
    } = &signed.op
    else {
        return Ok(CreateInviteOutcome::Denied("op is not an invite"));
    };
    let Some(op_id_bytes) = parse_op_id(op_id) else {
        return Ok(CreateInviteOutcome::Denied("op_id is not a canonical UUID"));
    };

    // Derive the deterministic invite token (so a lost-ack retry re-derives the IDENTICAL link). Bind the
    // op id, workspace, role byte, the sorted+deduped skill-id set, and the expiry.
    let mut ids: Vec<&str> = skills.iter().map(|(id, _)| id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let joined = ids.join("\n");
    let role_byte = [role.signing_byte()];
    // An absent expiry binds the shared no-expiry sentinel — the same value the client signs and the
    // governance frame encodes (`expires_to_u64(None)`); 0i64 and 0u64 share the identical be-bytes.
    let expires_be = expires_at.map_or(
        topos_core::sign::INVITE_NO_EXPIRY.to_be_bytes(),
        i64::to_be_bytes,
    );
    let token = authority.derive_token(
        b"invite",
        &[
            op_id_bytes.as_slice(),
            ws.as_str().as_bytes(),
            role_byte.as_slice(),
            joined.as_bytes(),
            expires_be.as_slice(),
        ],
    )?;
    let token_sha256 = sha256_token(&token);
    let link = format!("{}/i/{token}", authority.enrollment()?.config.base_url);

    let input = GovernanceInput {
        ws,
        op_id,
        op_id_bytes,
        signed,
        created_at,
    };
    match authority
        .db()
        .create_invite_txn(&input, &token_sha256)
        .await?
    {
        GovernanceOutcome::Ok => Ok(CreateInviteOutcome::Created(InviteCreated {
            link,
            roster_added: emails.clone(),
            skills: skills.clone(),
        })),
        GovernanceOutcome::Denied(reason) => Ok(CreateInviteOutcome::Denied(reason)),
    }
}

/// Run a governance roster/revoke mutation (the shared orchestration half of
/// [`Authority::roster_set`] / [`Authority::roster_remove`] / [`Authority::revoke_device`]). Parses the op
/// id, then runs the one governance transaction (authz + the op-specific role check + the mutation + receipt).
pub(crate) async fn governance_mutation(
    authority: &Authority,
    ws: &WorkspaceId,
    op_id: &str,
    signed: &GovernanceSignedOp,
    created_at: &str,
) -> Result<GovernanceOutcome> {
    let Some(op_id_bytes) = parse_op_id(op_id) else {
        return Ok(GovernanceOutcome::Denied("op_id is not a canonical UUID"));
    };
    let input = GovernanceInput {
        ws,
        op_id,
        op_id_bytes,
        signed,
        created_at,
    };
    authority.db().governance_mutation_txn(&input).await
}

/// Consume a one-time admin-claim token (the orchestration half of [`Authority::admin_claim`]). The seated
/// workspace's name and owner come from the CLAIM ROW (minted facts, never the request); its deployment
/// mode is THE PLANE'S own (threaded from the enrollment config — a cloud plane's break-glass claim stands
/// up a cloud-mode workspace).
pub(crate) async fn admin_claim(
    authority: &Authority,
    claim_token: &str,
    device_public_key: [u8; 32],
    now: i64,
    created_at: &str,
) -> Result<RedeemOutcome> {
    let claim_sha256 = sha256_token(claim_token);
    let server_device_key_id = device_key_id_for(&device_public_key);
    let plane_mode = authority.enrollment()?.config.deployment_mode;
    authority
        .db()
        .admin_claim_txn(
            &claim_sha256,
            &server_device_key_id,
            &device_public_key,
            plane_mode.as_str(),
            now,
            created_at,
        )
        .await
}

/// Mint a one-time admin-claim link token (the orchestration half of
/// [`Authority::mint_admin_claim`]). Refuses a workspace that already exists (typed) and — on a CLOUD-mode
/// plane — a mint with no owner email (the claim would otherwise seat a device-rooted owner no human
/// identity can govern). The plaintext token is returned ONCE; only its sha256 is stored, and
/// [`MintedClaim`]'s `Debug` redacts it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn mint_admin_claim(
    authority: &Authority,
    ws: &WorkspaceId,
    display_name: Option<&str>,
    owner_email: Option<&str>,
    plane_mode: DeploymentMode,
    ttl_ms: i64,
    now: i64,
    created_at: &str,
) -> Result<MintClaimOutcome> {
    if plane_mode == DeploymentMode::Cloud && owner_email.is_none() {
        return Ok(MintClaimOutcome::Denied(
            "a cloud-mode claim requires an owner email",
        ));
    }
    let owner = match owner_email {
        Some(email) => match Principal::parse(email) {
            Ok(p) => Some(p),
            Err(_) => return Ok(MintClaimOutcome::Denied("invalid owner email")),
        },
        None => None,
    };
    let token = random_claim_token()?;
    let token_sha256 = sha256_token(&token);
    let expires_at = now.saturating_add(ttl_ms.max(0));
    match authority
        .db()
        .mint_admin_claim_txn(
            &token_sha256,
            ws,
            display_name,
            owner.as_ref().map(Principal::as_str),
            expires_at,
            created_at,
        )
        .await?
    {
        Ok(()) => Ok(MintClaimOutcome::Minted(MintedClaim { token, expires_at })),
        Err(MintClaimDenied::WorkspaceExists) => {
            Ok(MintClaimOutcome::Denied("workspace already exists"))
        }
    }
}

/// Create a workspace for an already-verified owner email (the orchestration half of
/// [`Authority::create_workspace`] — door 2, the web "Create workspace" page). Parses the email INSIDE the
/// op, applies the server-side display-name default and the freemail-aware domain claim, and runs the one
/// genesis transaction (idempotency probe → cap → seat → self-invite → request ledger).
pub(crate) async fn create_workspace(
    authority: &Authority,
    request_id: &str,
    display_name: Option<&str>,
    owner_email: &str,
    plane_mode: DeploymentMode,
    created_at: &str,
) -> Result<CreateWorkspaceOutcome> {
    let Ok(owner) = Principal::parse(owner_email) else {
        return Ok(CreateWorkspaceOutcome::Denied("invalid owner email"));
    };
    let request_sha256 = topos_core::digest::sha256(request_id.as_bytes());
    let name = resolved_display_name(display_name, owner_email);
    let (domain, domain_status) = email_domain_claim(owner_email);
    let secret = authority.enrollment()?.secret.as_bytes();
    authority
        .db()
        .create_workspace_txn(
            &request_sha256,
            &name,
            &owner,
            plane_mode.as_str(),
            domain.as_deref(),
            domain_status,
            secret,
            created_at,
        )
        .await
}

/// Approve a STANDUP device-auth session with a web-verified email (the orchestration half of
/// [`Authority::approve_standup`] — door 1's human leg). Parses the email INSIDE the op (a malformed one is
/// the uniform miss), applies the same name default + domain claim as create-workspace, and runs the one
/// approve transaction (session probe → cap → seat → session CAS).
pub(crate) async fn approve_standup(
    authority: &Authority,
    user_code: &str,
    verified_email: &str,
    display_name: Option<&str>,
    plane_mode: DeploymentMode,
    now: i64,
    created_at: &str,
) -> Result<ApproveStandupOutcome> {
    let principal = Principal::parse(verified_email).map_err(|_| AuthorityError::NotFound)?;
    let name = resolved_display_name(display_name, verified_email);
    let (domain, domain_status) = email_domain_claim(verified_email);
    authority
        .db()
        .approve_standup_txn(
            user_code,
            &principal,
            &name,
            plane_mode.as_str(),
            domain.as_deref(),
            domain_status,
            now,
            created_at,
        )
        .await
}

/// Resolve an admin-claim link token to its bootstrap payload (the orchestration half of
/// [`Authority::read_claim_bootstrap`]) — the `/i/` claim branch. Unconsumed ∧ unexpired ⇒ the claim's own
/// disclosure (its display name; NO skills — a claim offers membership, not bytes) with the plane signing
/// root and `enrollment_method = "admin_claim"`; consumed/expired/unknown is the single indistinguishable
/// `NotFound`. Claim resolution never touches the invites table (and invite resolution never touches this).
pub(crate) async fn read_claim_bootstrap(
    authority: &Authority,
    token: &str,
    now: i64,
) -> Result<InviteBootstrap> {
    let token_sha256 = sha256_token(token);
    let Some(row) = authority
        .db()
        .read_claim_bootstrap_row(&token_sha256, now)
        .await?
    else {
        return Err(AuthorityError::NotFound);
    };
    let plane_public_key = authority.plane_public_key()?;
    let plane_key_id = authority.plane_key_id()?;
    let config = &authority.enrollment()?.config;
    let display_name = row
        .display_name
        .unwrap_or_else(|| row.workspace_id.as_str().to_owned());
    Ok(InviteBootstrap {
        workspace_id: row.workspace_id,
        display_name,
        // The workspace does not exist yet; at redeem it is seated with THE PLANE'S mode, so that is the
        // honest posture to disclose.
        deployment_mode: config.deployment_mode,
        verified_domain: None,
        verified_domain_status: "unverified".to_owned(),
        skills: Vec::new(),
        plane_public_key,
        plane_key_id,
        base_url: config.base_url.clone(),
        enrollment_method: "admin_claim".to_owned(),
    })
}

/// The stored display name: the caller's, or the server-side default `"<email local part>'s workspace"`.
fn resolved_display_name(display_name: Option<&str>, owner_email: &str) -> String {
    match display_name {
        Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
        _ => {
            let local = owner_email.split('@').next().unwrap_or(owner_email);
            format!("{local}'s workspace")
        }
    }
}

/// Freemail providers whose domain says nothing about an organization — a workspace created under one gets
/// NO domain claim. Deliberately small and static: a miss only means a claim stays `unverified`.
const FREEMAIL_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "yahoo.com",
    "hotmail.com",
    "outlook.com",
    "live.com",
    "msn.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "pm.me",
    "gmx.com",
    "gmx.net",
    "mail.com",
    "yandex.com",
    "yandex.ru",
    "zoho.com",
    "fastmail.com",
    "hey.com",
    "qq.com",
    "163.com",
    "126.com",
];

/// The workspace's domain claim from its owner's email: a non-freemail domain is recorded with status
/// `verified` (the owner PROVED control of an address on it via the web sign-in — that proof is the
/// verification); a freemail or unparseable address yields no claim (`unverified`).
fn email_domain_claim(owner_email: &str) -> (Option<String>, &'static str) {
    let Some((local, domain)) = owner_email.rsplit_once('@') else {
        return (None, "unverified");
    };
    if local.is_empty() || domain.is_empty() {
        return (None, "unverified");
    }
    let domain = domain.to_ascii_lowercase();
    if FREEMAIL_DOMAINS.contains(&domain.as_str()) {
        (None, "unverified")
    } else {
        (Some(domain), "verified")
    }
}

#[cfg(test)]
mod tests {
    use super::{email_domain_claim, resolved_display_name};

    #[test]
    fn display_name_defaults_to_the_email_local_part() {
        assert_eq!(
            resolved_display_name(None, "robert@acme.com"),
            "robert's workspace"
        );
        assert_eq!(
            resolved_display_name(Some("  "), "robert@acme.com"),
            "robert's workspace"
        );
        assert_eq!(
            resolved_display_name(Some("Acme"), "robert@acme.com"),
            "Acme"
        );
    }

    #[test]
    fn domain_claim_is_verified_only_for_non_freemail() {
        assert_eq!(
            email_domain_claim("robert@acme.com"),
            (Some("acme.com".to_owned()), "verified")
        );
        assert_eq!(email_domain_claim("robert@GMAIL.com"), (None, "unverified"));
        assert_eq!(email_domain_claim("not-an-email"), (None, "unverified"));
        assert_eq!(email_domain_claim("@acme.com"), (None, "unverified"));
    }
}
