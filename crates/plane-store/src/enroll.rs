//! The enrollment issuance core — the orchestration half (outside the transaction).
//!
//! This is where a device proves possession of the key it registers, and where the plane mints the only
//! credentials it ever issues: **workspace-scoped** invites, enrollment grants, and per-skill read tokens —
//! **never** a user OAuth token. Every issuance / roster / device decision is made INSIDE these ops against
//! a server-trusted row; a client-asserted id is never parsed into authority. Every opaque credential is
//! **deterministically HMAC-derived** from a `0600` enrollment secret (so a lost-ack retry re-derives the
//! IDENTICAL credential) and stored ONLY as its sha256 (so a database read can never recover a live
//! credential and a revoke is an instant row flip). This module does the work OUTSIDE the one write
//! transaction (derive the credentials, build the kernel possession frames); the raw SQL — and the
//! `SERIALIZABLE` (`run_serializable!`) redeem transactions — live in [`crate::db`]. The governance +
//! admin-claim orchestration (which reuses this module's credential derivations) is split into
//! [`crate::governance`].

use std::path::PathBuf;

use base64::Engine as _;
use zeroize::Zeroizing;

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, SkillId, WorkspaceId};

// ── TTLs + caps (epoch-MILLISECOND budgets, the one server-clock unit) ─────────────────────────────────

/// A device-auth session lives ~15 minutes (the human has that long to confirm).
pub(crate) const DEVICE_AUTH_TTL_MS: i64 = 15 * 60 * 1000;
/// The minimum poll interval a device must respect (RFC-8628 `interval`), in seconds.
pub(crate) const DEVICE_AUTH_INTERVAL_SECS: i64 = 5;
/// An issued enrollment grant lives ~12 minutes (the device must redeem it promptly).
pub(crate) const GRANT_TTL_MS: i64 = 12 * 60 * 1000;
/// A passcode lives ~10 minutes.
pub(crate) const PASSCODE_TTL_MS: i64 = 10 * 60 * 1000;
/// A passcode locks after this many wrong attempts.
pub(crate) const PASSCODE_MAX_ATTEMPTS: i64 = 5;

// ── return types (domain values; B4 maps these to wire DTOs) ───────────────────────────────────────────

/// The bootstrap payload an invite link resolves to (everything a device needs to begin enrolling). Carries
/// the plane's signing root to TOFU-pin, the workspace identity, the offered skills, and the verification
/// base — but **no bytes and no role** (the role lives server-side on the pre-seeded member rows).
#[derive(Debug, Clone)]
pub struct InviteBootstrap {
    /// The workspace the invite is for.
    pub workspace_id: WorkspaceId,
    /// The workspace display name.
    pub display_name: String,
    /// The workspace deployment posture.
    pub deployment_mode: DeploymentMode,
    /// The org domain claim (if any).
    pub verified_domain: Option<String>,
    /// The domain-verification state.
    pub verified_domain_status: String,
    /// The skills the invite pre-offers, each with an optional display name.
    pub skills: Vec<(SkillId, Option<String>)>,
    /// The plane's raw 32-byte Ed25519 public key (the trust root the device pins).
    pub plane_public_key: [u8; 32],
    /// The plane signing key id.
    pub plane_key_id: String,
    /// The plane's public base URL.
    pub base_url: String,
    /// The offered enrollment method.
    pub enrollment_method: String,
}

/// The result of starting a device-authorization flow (RFC-8628-shaped).
#[derive(Debug, Clone)]
pub struct DeviceAuthStart {
    /// The SECRET device code the client polls with (the plaintext appears ONLY here; only its sha256 is stored).
    pub device_code: String,
    /// The short code a human types on the verification page.
    pub user_code: String,
    /// The verification URL (built from the plane base URL).
    pub verification_uri: String,
    /// The session expiry (epoch-ms).
    pub expires_at: i64,
    /// The minimum poll interval (seconds).
    pub interval_secs: i64,
}

/// An issued single-use enrollment grant + the binding fields the device needs to build its possession proof.
#[derive(Debug, Clone)]
pub struct GrantIssued {
    /// The SECRET grant token to redeem (the plaintext appears ONLY here; only its sha256 is stored).
    pub grant_token: String,
    /// The workspace the grant is scoped to.
    pub workspace_id: WorkspaceId,
    /// The non-secret device-auth id bound into the enroll frame.
    pub device_auth_id: String,
    /// The server-derived device key id.
    pub device_key_id: String,
    /// The skills the grant offers (the redeem rosters + mints read tokens for these).
    pub offered_skills: Vec<SkillId>,
    /// The grant expiry (epoch-ms).
    pub expires_at: i64,
}

/// The outcome of polling a device-authorization session.
#[derive(Debug, Clone)]
pub enum DeviceAuthPoll {
    /// Not yet confirmed — keep polling at the interval.
    Pending,
    /// Polled too fast — back off.
    SlowDown,
    /// The session was denied at the verification page.
    Denied,
    /// The session expired before confirmation.
    Expired,
    /// Confirmed — here is the single-use grant (idempotent: a re-poll re-derives the SAME grant).
    Granted(GrantIssued),
}

/// The result of starting a passcode challenge — the plaintext code (returned ONCE for the mailer; never logged).
#[derive(Debug, Clone)]
pub struct PasscodeStart {
    /// The 6-digit code to mail (the plaintext appears ONLY here; only its sha256 is stored).
    pub passcode: String,
    /// The principal (email) the code proves control of.
    pub principal: Principal,
}

/// The outcome of completing a passcode challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasscodeComplete {
    /// The code matched — the session's identity is confirmed.
    Confirmed,
    /// The code was wrong — this many attempts remain before lockout.
    WrongCode {
        /// Attempts left before the passcode locks.
        remaining: i64,
    },
    /// The passcode expired.
    Expired,
    /// The attempt cap was hit — the passcode is locked.
    TooManyAttempts,
}

/// The verification-page disclosure for a LIVE device-auth session — what a human sees BEFORE confirming an
/// identity (the RFC-8628 confused-deputy guard: the page shows which device + workspace + skills are being
/// authorized, so a human approves the session they actually started, not one an attacker raced in). Carries
/// **no secret** — no device code, no grant, no token.
#[derive(Debug, Clone)]
pub struct VerificationContext {
    /// The human-readable machine name the device offered at start.
    pub machine_name: String,
    /// A short hex fingerprint of the device's public key — a human cross-checks it against the device. NOT
    /// the `device_key_id` (no `dk_` prefix, shorter); a display aid only, never an authority input.
    pub device_fingerprint: String,
    /// The workspace display name the device would join.
    pub workspace_display_name: String,
    /// The org-domain claim (if any).
    pub verified_domain: Option<String>,
    /// The domain-verification state.
    pub verified_domain_status: String,
    /// The skills the invite pre-offers, each with an optional display name.
    pub offered_skills: Vec<(SkillId, Option<String>)>,
}

/// The outcome of confirming a session's external identity (the OIDC callback's in-Authority half). A single
/// success marker — the identity proof happened in the CALLER (the OIDC module validated the id_token); this
/// op only records the proven principal onto the live session, exactly like [`PasscodeComplete::Confirmed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The session's identity was confirmed (the device's next poll yields a grant).
    Confirmed,
}

/// One minted read token (returned ONCE on redeem; only its sha256 is stored).
#[derive(Debug, Clone)]
pub struct MintedReadToken {
    /// The skill the token reads.
    pub skill_id: SkillId,
    /// The plaintext read token (the `0600` at-rest secret on the follower; only its sha256 is stored here).
    pub token: String,
    /// The token expiry (epoch-ms; `None` = non-expiring — per-device revoke is the kill switch).
    pub expires_at: Option<i64>,
}

/// The successful result of an enrollment redeem (or an admin claim) — the confirmed identity, the registered
/// device, and the minted per-skill read tokens. **NO user token, ever.**
#[derive(Debug, Clone)]
pub struct EnrollmentRedeemed {
    /// The workspace the device enrolled into.
    pub workspace_id: WorkspaceId,
    /// The confirmed principal the device acts as.
    pub principal: Principal,
    /// The server-derived device key id now registered.
    pub device_key_id: String,
    /// The minted per-skill read tokens (returned once).
    pub read_tokens: Vec<MintedReadToken>,
}

/// The outcome of [`Authority::redeem_enrollment`](crate::Authority::redeem_enrollment) /
/// [`Authority::admin_claim`](crate::Authority::admin_claim).
#[derive(Debug, Clone)]
pub enum RedeemOutcome {
    /// The device enrolled.
    Redeemed(EnrollmentRedeemed),
    /// The redeem was denied (a uniform denial; the static reason is for server logs, never an oracle).
    Denied(&'static str),
}

/// The deployment posture of a plane (and of each `workspace` row). It decides the **redeem gate**: a
/// `Cloud` workspace requires a confirmed identity already on the roster (the invite carries no role); a
/// `SelfHost` workspace grants membership straight from the bearer of a valid grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentMode {
    /// The hosted plane: redeem requires a confirmed, already-rostered identity.
    Cloud,
    /// A self-hosted plane: redeem grants membership from a valid grant (no human identity step).
    SelfHost,
}

impl DeploymentMode {
    /// The stored discriminant (`workspace.deployment_mode`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeploymentMode::Cloud => "cloud",
            DeploymentMode::SelfHost => "self_host",
        }
    }

    /// Parse a stored discriminant. `None` on an unknown value (store corruption at a read site).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cloud" => Some(DeploymentMode::Cloud),
            "self_host" => Some(DeploymentMode::SelfHost),
            _ => None,
        }
    }
}

/// The enrollment subsystem's static configuration — held on the [`Authority`](crate::Authority) so
/// `read_invite_bootstrap` / `start_device_auth` can return the plane's signing root, the verification base
/// URL, and the offered enrollment method without re-plumbing them per call. The `secret_path` is consumed
/// once at [`Authority::with_enrollment_config`](crate::Authority::with_enrollment_config) load time.
#[derive(Debug, Clone)]
pub struct EnrollmentConfig {
    /// The `0600` file the 32-byte HMAC enrollment secret is load-or-generated from (load-time only).
    pub secret_path: PathBuf,
    /// The plane's public base URL (the `/i/<token>` link + the device-auth `verification_uri` are built on it).
    pub base_url: String,
    /// This plane's deployment posture (the default for a workspace this plane stands up).
    pub deployment_mode: DeploymentMode,
    /// The enrollment method offered to a bootstrapping device (e.g. `"device_code"`), surfaced in the bootstrap.
    pub enrollment_method: String,
}

/// The 32-byte HMAC enrollment secret — the root every opaque credential is derived from. Wrapped so it
/// **self-zeroizes on drop** and its `Debug` **redacts** (the crate lints `missing_debug_implementations`, so
/// a field needs `Debug`; a derived one would print the secret). Never leaves this crate.
pub(crate) struct EnrollmentSecret(Zeroizing<[u8; 32]>);

impl EnrollmentSecret {
    /// The raw secret bytes (for the HMAC derivation only).
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for EnrollmentSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollmentSecret").finish_non_exhaustive()
    }
}

/// The configured enrollment state on the [`Authority`](crate::Authority): the secret + the static config.
/// Absent until [`Authority::with_enrollment_config`](crate::Authority::with_enrollment_config) is called;
/// every enrollment/governance op requires it (a typed precondition, exactly as the pointer-move requires the
/// plane signing key).
#[derive(Debug)]
pub(crate) struct EnrollmentState {
    pub(crate) secret: EnrollmentSecret,
    pub(crate) config: EnrollmentConfig,
}

impl EnrollmentState {
    /// Load-or-generate the `0600` secret (the plane signing key's exact custody) and capture the config.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if the secret file cannot be read/created/validated.
    pub(crate) fn load(config: EnrollmentConfig) -> Result<Self> {
        let seed = crate::signer::load_or_generate_seed(&config.secret_path)?;
        Ok(Self {
            secret: EnrollmentSecret(seed),
            config,
        })
    }
}

/// Derive a deterministic opaque credential: `base64url-unpadded(HMAC-SHA256(secret, domain ‖ each part
/// length-prefixed))`. **Determinism is the keystone** — a lost-ack create/issue retry re-derives the
/// IDENTICAL credential, and a consumed grant re-derives the SAME read tokens, so redeem is naturally
/// idempotent with no fresh mint per call. The `domain` is a fixed byte tag (`b"invite"` / `b"grant"` /
/// `b"readtoken"` — none a prefix of another) that separates the credential families; each `part` is framed
/// with a `u32be` length prefix so the concatenation is unambiguous. Only `sha256(token)` is ever stored.
pub(crate) fn derive_token(secret: &[u8; 32], domain: &[u8], parts: &[&[u8]]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    // HMAC accepts a key of any length, so `new_from_slice` over a fixed 32-byte secret never errors — an
    // `Err` here would be a build-time invariant violation, not a runtime/user condition.
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC-SHA256 accepts a 32-byte key");
    mac.update(domain);
    for part in parts {
        // A `u32be` length prefix per part (the kernel's length-prefix convention) makes the byte stream
        // self-delimiting, so two different part decompositions can never collide on one HMAC message.
        mac.update(
            &(u32::try_from(part.len()).expect(
                "HMAC credential part length fits in u32 (parts are short validated ids/tokens)",
            ))
            .to_be_bytes(),
        );
        mac.update(part);
    }
    let tag = mac.finalize().into_bytes();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag)
}

/// The server-derived device key id from a raw Ed25519 public key: `dk_<first 32 hex of sha256(pubkey)>`,
/// via the ONE kernel derivation (`topos_core::sign::device_key_id` — the same fn the client signer
/// calls). The plane derives this ITSELF on enroll and re-derives it on redeem — a client-asserted id is
/// never trusted, so a mismatch between a grant's bound key and the presented key is caught structurally.
#[must_use]
pub(crate) fn device_key_id_for(device_public_key: &[u8; 32]) -> String {
    topos_core::sign::device_key_id(device_public_key)
}

/// Map a sha256 over a credential's UTF-8 bytes (the one stored form of every opaque credential).
pub(crate) fn sha256_token(token: &str) -> [u8; 32] {
    topos_core::digest::sha256(token.as_bytes())
}

/// The length, in hex chars, of the human-comparable device fingerprint shown on the verification page.
const DEVICE_FINGERPRINT_HEX_LEN: usize = 16;

/// A short hex fingerprint of a device's public key — `sha256(pubkey)` truncated to
/// [`DEVICE_FINGERPRINT_HEX_LEN`] hex chars — for the verification page, so a human can visually cross-check
/// the device asking to enroll. NOT the `device_key_id` (no `dk_` prefix, shorter); a display aid only, never
/// an authority input.
#[must_use]
pub(crate) fn device_fingerprint(device_public_key: &[u8; 32]) -> String {
    let hex = topos_core::digest::to_hex(&topos_core::digest::sha256(device_public_key));
    hex[..DEVICE_FINGERPRINT_HEX_LEN].to_owned()
}

/// The server-trusted inputs to the one redeem transaction (built in orchestration, consumed in
/// [`crate::db`]). Every identity field is the SERVER's value — the rehashed grant, the re-derived device
/// key id — never a client claim.
pub(crate) struct RedeemInput<'a> {
    /// `sha256(grant_token)` — the grant row's PK and the enroll frame's `grant_hash`.
    pub grant_sha256: [u8; 32],
    /// The device's enrollment possession-proof signature.
    pub enroll_sig: &'a [u8; 64],
    /// The raw device public key presented (must equal the grant's bound key).
    pub device_public_key: [u8; 32],
    /// The SERVER-derived device key id from `device_public_key` (a client-asserted id is never trusted).
    pub server_device_key_id: &'a str,
    /// The server clock (epoch-ms).
    pub now: i64,
    /// The server-stamped creation timestamp.
    pub created_at: &'a str,
}

/// A precondition fault: an enrollment/governance op was attempted with no enrollment config (call
/// `with_enrollment_config`). Wired as an internal error so no config state crosses the public boundary.
#[derive(Debug, thiserror::Error)]
#[error("no enrollment config (call with_enrollment_config)")]
pub(crate) struct NoEnrollmentConfig;

/// An entropy fault gathering OS randomness for a fresh device-code / user-code / passcode.
#[derive(Debug, thiserror::Error)]
#[error("could not gather OS entropy for an enrollment credential")]
pub(crate) struct EnrollEntropy;

/// Fill `N` bytes from the OS CSPRNG.
fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|_| AuthorityError::internal(EnrollEntropy))?;
    Ok(b)
}

/// A fresh, high-entropy device code (the secret poll credential) — 32 random bytes, base64url-unpadded.
pub(crate) fn random_device_code() -> Result<String> {
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

/// A fresh user code: 8 chars from an unambiguous alphabet (no vowels — no accidental words — and no
/// `0/O/1/I`), grouped `XXXX-XXXX`. Short and low-value (the device-code is the real secret).
pub(crate) fn random_user_code() -> Result<String> {
    const ALPHABET: &[u8; 28] = b"BCDFGHJKLMNPQRSTVWXZ23456789";
    let raw = random_bytes::<8>()?;
    let chars: Vec<char> = raw
        .iter()
        .map(|&b| ALPHABET[usize::from(b) % ALPHABET.len()] as char)
        .collect();
    Ok(format!(
        "{}{}{}{}-{}{}{}{}",
        chars[0], chars[1], chars[2], chars[3], chars[4], chars[5], chars[6], chars[7]
    ))
}

/// A fresh 6-digit numeric passcode.
pub(crate) fn random_passcode() -> Result<String> {
    let raw = random_bytes::<4>()?;
    let n = u32::from_be_bytes(raw) % 1_000_000;
    Ok(format!("{n:06}"))
}

/// The server-derived, device-rooted principal a self-host / admin-claim device acts as: `dev.<device_key_id>`
/// (the `.` keeps it inside the principal charset — a `:` would be rejected). NEVER a client-asserted id.
pub(crate) fn device_rooted_principal(device_key_id: &str) -> Result<Principal> {
    Principal::parse(&format!("dev.{device_key_id}")).map_err(AuthorityError::internal)
}

/// Parse a **canonical** lowercase-hyphenated UUID op-id into the 16 bytes the governance frame binds (the
/// same 1:1 string↔bytes guard the pointer-move uses, so a varied spelling can never split an idempotency slot).
pub(crate) fn parse_op_id(op_id: &str) -> Option<[u8; 16]> {
    let uuid = uuid::Uuid::parse_str(op_id).ok()?;
    (uuid.as_hyphenated().to_string() == op_id).then(|| uuid.into_bytes())
}

/// A stored `workspace.deployment_mode` did not parse — store corruption (a CHECK should forbid it).
#[derive(Debug, thiserror::Error)]
#[error("stored workspace has an invalid deployment mode")]
struct BadStoredDeploymentMode;

// ── the orchestration ops (the public Authority methods delegate to these) ─────────────────────────────

/// Resolve an invite link to its bootstrap payload (the orchestration half of
/// [`Authority::read_invite_bootstrap`]). A revoked/expired/absent invite — or a missing workspace — is the
/// single indistinguishable `NotFound`.
pub(crate) async fn read_invite_bootstrap(
    authority: &Authority,
    token: &str,
    now: i64,
) -> Result<InviteBootstrap> {
    let token_sha256 = sha256_token(token);
    let Some(invite) = authority.db().read_invite(&token_sha256).await? else {
        return Err(AuthorityError::NotFound);
    };
    if invite.revoked || invite.expires_at.is_some_and(|e| now > e) {
        return Err(AuthorityError::NotFound);
    }
    let Some(workspace) = authority.db().read_workspace(&invite.workspace_id).await? else {
        return Err(AuthorityError::NotFound);
    };
    let deployment_mode = DeploymentMode::parse(&workspace.deployment_mode)
        .ok_or_else(|| AuthorityError::integrity(BadStoredDeploymentMode))?;
    let skills = authority.db().read_invite_skills(&token_sha256).await?;
    let plane_public_key = authority.plane_public_key()?;
    let plane_key_id = authority.plane_key_id()?;
    let config = &authority.enrollment()?.config;
    Ok(InviteBootstrap {
        workspace_id: invite.workspace_id,
        display_name: workspace.display_name,
        deployment_mode,
        verified_domain: workspace.verified_domain,
        verified_domain_status: workspace.verified_domain_status,
        skills,
        plane_public_key,
        plane_key_id,
        base_url: config.base_url.clone(),
        enrollment_method: config.enrollment_method.clone(),
    })
}

/// Resolve a `user_code` to its verification-page disclosure (the orchestration half of
/// [`Authority::read_verification_context`]). A miss — an unknown code, a non-live (issued/denied/expired)
/// session, an expired one, or a missing workspace — is the single indistinguishable `NotFound`. A pure read
/// (no mutation, no secret), mirroring [`read_invite_bootstrap`].
pub(crate) async fn read_verification_context(
    authority: &Authority,
    user_code: &str,
    now: i64,
) -> Result<VerificationContext> {
    let Some(session) = authority
        .db()
        .read_live_verification_session(user_code, now)
        .await?
    else {
        return Err(AuthorityError::NotFound);
    };
    let Some(workspace) = authority.db().read_workspace(&session.workspace_id).await? else {
        return Err(AuthorityError::NotFound);
    };
    // The offered skills are the session invite's (a self-host device-rooted session has no invite ⇒ none).
    let offered_skills = match &session.invite_sha256 {
        Some(invite_sha256) => authority.db().read_invite_skills(invite_sha256).await?,
        None => Vec::new(),
    };
    Ok(VerificationContext {
        machine_name: session.machine_name,
        device_fingerprint: device_fingerprint(&session.device_pubkey),
        workspace_display_name: workspace.display_name,
        verified_domain: workspace.verified_domain,
        verified_domain_status: workspace.verified_domain_status,
        offered_skills,
    })
}

/// Start a device-authorization flow (the orchestration half of [`Authority::start_device_auth`]). Resolves
/// the invite, SERVER-derives the device key id (a client-asserted id is ignored), generates a fresh secret
/// device code + a unique user code, and inserts the session (cloud `pending`; self-host `confirmed` with a
/// server-derived device-rooted principal, so the first poll yields a grant with no human step).
pub(crate) async fn start_device_auth(
    authority: &Authority,
    invite_token: &str,
    device_public_key: &[u8; 32],
    machine_name: &str,
    now: i64,
    created_at: &str,
) -> Result<DeviceAuthStart> {
    let token_sha256 = sha256_token(invite_token);
    let Some(invite) = authority.db().read_invite(&token_sha256).await? else {
        return Err(AuthorityError::NotFound);
    };
    if invite.revoked || invite.expires_at.is_some_and(|e| now > e) {
        return Err(AuthorityError::NotFound);
    }
    let Some(workspace) = authority.db().read_workspace(&invite.workspace_id).await? else {
        return Err(AuthorityError::NotFound);
    };
    let deployment = DeploymentMode::parse(&workspace.deployment_mode)
        .ok_or_else(|| AuthorityError::integrity(BadStoredDeploymentMode))?;

    let device_key_id = device_key_id_for(device_public_key);
    let confirmed_principal_owned = match deployment {
        DeploymentMode::Cloud => None,
        DeploymentMode::SelfHost => Some(device_rooted_principal(&device_key_id)?),
    };
    let status = match deployment {
        DeploymentMode::Cloud => "pending",
        DeploymentMode::SelfHost => "confirmed",
    };

    let device_code = random_device_code()?;
    let device_code_sha256 = sha256_token(&device_code);
    let user_code = unique_user_code(authority).await?;
    let expires_at = now.saturating_add(DEVICE_AUTH_TTL_MS);

    authority
        .db()
        .insert_device_auth_session(
            &device_code_sha256,
            &user_code,
            &invite.workspace_id,
            &token_sha256,
            device_public_key,
            &device_key_id,
            machine_name,
            status,
            confirmed_principal_owned.as_ref().map(Principal::as_str),
            expires_at,
            DEVICE_AUTH_INTERVAL_SECS,
            created_at,
        )
        .await?;

    Ok(DeviceAuthStart {
        device_code,
        user_code,
        verification_uri: format!("{}/device", authority.enrollment()?.config.base_url),
        expires_at,
        interval_secs: DEVICE_AUTH_INTERVAL_SECS,
    })
}

/// A user code that no LIVE session already holds (the partial-unique index forbids a clash). Astronomically
/// unlikely to need more than one try; bounded retries keep it total.
async fn unique_user_code(authority: &Authority) -> Result<String> {
    for _ in 0..8 {
        let code = random_user_code()?;
        if !authority.db().live_user_code_exists(&code).await? {
            return Ok(code);
        }
    }
    Err(AuthorityError::internal(EnrollEntropy))
}

/// Poll a device-authorization session (the orchestration half of [`Authority::poll_device_auth`]).
pub(crate) async fn poll_device_auth(
    authority: &Authority,
    device_code: &str,
    now: i64,
    created_at: &str,
) -> Result<DeviceAuthPoll> {
    let device_code_sha256 = sha256_token(device_code);
    let secret = authority.enrollment()?.secret.as_bytes();
    authority
        .db()
        .poll_txn(&device_code_sha256, now, created_at, secret)
        .await
}

/// Start a passcode challenge for an email on a live session (the orchestration half of
/// [`Authority::start_passcode`]). The email is parsed INSIDE the op (never a handler-parsed principal).
/// Returns a constant-shaped ack so a non-rostered address is no roster-enumeration oracle (the cloud gate is
/// enforced at redeem); the plaintext code is returned ONCE for the mailer and NEVER logged.
pub(crate) async fn start_passcode(
    authority: &Authority,
    user_code: &str,
    email: &str,
    now: i64,
    created_at: &str,
) -> Result<PasscodeStart> {
    let principal = Principal::parse(email).map_err(|_| AuthorityError::NotFound)?;
    let Some(device_code_sha256) = authority.db().live_session_device_code(user_code).await? else {
        return Err(AuthorityError::NotFound);
    };
    let passcode = random_passcode()?;
    let passcode_sha256 = sha256_token(&passcode);
    let expires_at = now.saturating_add(PASSCODE_TTL_MS);
    authority
        .db()
        .upsert_passcode(
            &device_code_sha256,
            &principal,
            &passcode_sha256,
            expires_at,
            created_at,
        )
        .await?;
    Ok(PasscodeStart {
        passcode,
        principal,
    })
}

/// Complete a passcode challenge (the orchestration half of [`Authority::complete_passcode`]). Parses the
/// email INSIDE the op, then verifies under the TTL + attempt cap, confirming the session on success.
pub(crate) async fn complete_passcode(
    authority: &Authority,
    user_code: &str,
    email: &str,
    code: &str,
    now: i64,
) -> Result<PasscodeComplete> {
    let principal = Principal::parse(email).map_err(|_| AuthorityError::NotFound)?;
    authority
        .db()
        .complete_passcode_txn(user_code, &principal, code, now)
        .await
}

/// Confirm a session's EXTERNAL identity (the orchestration half of
/// [`Authority::confirm_external_identity`]). The CALLER (the OIDC module) has already proven the email via a
/// validated id_token; this op only records it onto the live session (status `confirmed` + the principal),
/// exactly as [`complete_passcode`]'s success half does — minus the code check. The email is parsed INSIDE
/// the op (never a handler-parsed principal); a malformed email is the indistinguishable `NotFound`.
pub(crate) async fn confirm_external_identity(
    authority: &Authority,
    user_code: &str,
    verified_email: &str,
    now: i64,
) -> Result<ConfirmOutcome> {
    let principal = Principal::parse(verified_email).map_err(|_| AuthorityError::NotFound)?;
    authority
        .db()
        .confirm_external_identity_txn(user_code, &principal, now)
        .await
}

/// Redeem an enrollment grant (the orchestration half of [`Authority::redeem_enrollment`]). SERVER-derives
/// the device key id from the presented key, then runs the one possession-proof + gate + register + mint
/// transaction. Returns minted per-skill read tokens — NEVER a user token.
pub(crate) async fn redeem_enrollment(
    authority: &Authority,
    grant_token: &str,
    enroll_sig: &[u8; 64],
    device_public_key: [u8; 32],
    now: i64,
    created_at: &str,
) -> Result<RedeemOutcome> {
    let grant_sha256 = sha256_token(grant_token);
    let server_device_key_id = device_key_id_for(&device_public_key);
    let secret = authority.enrollment()?.secret.as_bytes();
    let input = RedeemInput {
        grant_sha256,
        enroll_sig,
        device_public_key,
        server_device_key_id: &server_device_key_id,
        now,
        created_at,
    };
    authority.db().redeem_txn(&input, secret).await
}
