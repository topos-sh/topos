//! The enrollment issuance core — the orchestration half (outside the transaction).
//!
//! This is where a device registers its key and redeems the bearer grant bound to it, and where the plane mints the only
//! credentials it ever issues: enrollment grants and the ONE workspace credential per enrolled device —
//! **never** a user OAuth token, never a per-skill token. Enrollment is BY ADDRESS (the workspace's
//! URL name): links carry nothing, the roster is the lock, and every not-yours redeem answers the one
//! uniform [`ENROLL_UNAVAILABLE`] denial. Every issuance / roster / device decision is made INSIDE these ops against
//! a server-trusted row; a client-asserted id is never parsed into authority. Every opaque credential is
//! **deterministically HMAC-derived** from a `0600` enrollment secret (so a lost-ack retry re-derives the
//! IDENTICAL credential) and stored ONLY as its sha256 (so a database read can never recover a live
//! credential and a revoke is an instant row flip). This module does the work OUTSIDE the one write
//! transaction (derive the credentials); the raw SQL — and the
//! `SERIALIZABLE` (`run_serializable!`) redeem transactions — live in [`crate::db`]. The governance +
//! admin-claim orchestration (which reuses this module's credential derivations) is split into
//! [`crate::governance`].

use std::path::PathBuf;

use base64::Engine as _;
use zeroize::Zeroizing;

use topos_core::digest;

use crate::authority::Authority;
use crate::custody::read::ReadScope;
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

/// The bootstrap payload an `/i/` claim link resolves to (everything a device needs to begin the
/// one-time standup). Carries the workspace-to-be's identity and the verification base — but **no
/// bytes and no role**. (The tokened INVITE door is gone — joining is `follow <address>` — so the
/// only `/i/` resolution left is the admin claim's.)
#[derive(Debug, Clone)]
pub struct InviteBootstrap {
    /// The workspace the claim is for.
    pub workspace_id: WorkspaceId,
    /// The workspace display name.
    pub display_name: String,
    /// The workspace deployment posture.
    pub deployment_mode: DeploymentMode,
    /// The org domain claim (if any).
    pub verified_domain: Option<String>,
    /// The domain-verification state.
    pub verified_domain_status: String,
    /// The plane's public API base URL (what the bootstrap payload declares; the client re-roots onto it).
    pub base_url: String,
    /// The base the minted `/i/` share links ride (`link_base_url` else `base_url`) — for a serving layer
    /// that re-renders the link (e.g. the agent-readable bootstrap document). Never on the wire payload.
    pub link_base: String,
    /// The offered enrollment method.
    pub enrollment_method: String,
}

/// The plane's enrollment-config disclosure — what a STANDUP `device/authorize` response carries as its
/// plane block (the same facts the `/i/` bootstrap serves an invited device; a standup device has no
/// `/i/` link to fetch them from). One authoritative source: the enrollment config on the Authority.
#[derive(Debug, Clone)]
pub struct EnrollmentDisclosure {
    /// The plane's public API base URL (what the client re-roots onto and pins).
    pub base_url: String,
    /// The base the minted `/i/` share links ride (`link_base_url` else `base_url`).
    pub link_base: String,
    /// The plane's deployment posture.
    pub deployment_mode: DeploymentMode,
    /// The offered enrollment method.
    pub enrollment_method: String,
}

/// The result of starting a device-authorization flow (RFC-8628-shaped).
#[derive(Debug, Clone)]
pub struct DeviceAuthStart {
    /// The SECRET device code the client polls with (the plaintext appears ONLY here; only its sha256 is stored).
    pub device_code: String,
    /// The opaque code identifying the session, embedded in the verification URL (clicked, not typed).
    pub user_code: String,
    /// The verification URL (built from the plane's verification base — `verify_base_url` when configured,
    /// else `base_url`).
    pub verification_uri: String,
    /// The verification URL with the user code embedded (`{verification_uri}/{user_code}`) — the one link a
    /// human opens; the client uses it VERBATIM (RFC-8628 `verification_uri_complete`).
    pub verification_uri_complete: String,
    /// The session expiry (epoch-ms).
    pub expires_at: i64,
    /// The minimum poll interval (seconds).
    pub interval_secs: i64,
}

/// An issued single-use enrollment grant + the binding fields the redeem checks the presented device against.
#[derive(Clone)]
pub struct GrantIssued {
    /// The SECRET grant token to redeem (the plaintext appears ONLY here; only its sha256 is stored).
    pub grant_token: String,
    /// The workspace the grant is scoped to. `None` for a LOGIN grant (workspace-less by design) and
    /// for an enroll grant whose requested address never resolved (the flow runs on to the redeem's
    /// one uniform denial — resolution is never disclosed here).
    pub workspace_id: Option<WorkspaceId>,
    /// The workspace display name — the context a standup client lacks (it never read an `/i/` bootstrap),
    /// surfaced with the grant so the wire can put it beside `workspace_id` ("" when there is no row).
    pub workspace_display_name: String,
    /// The workspace's full ADDRESS (`<link_base>/<name>`) — the share line's root; `None` when the
    /// grant is workspace-less.
    pub workspace_address: Option<String>,
    /// The non-secret device-auth id bound into the enroll frame.
    pub device_auth_id: String,
    /// The server-derived device key id.
    pub device_key_id: String,
    /// The grant expiry (epoch-ms).
    pub expires_at: i64,
}

// `grant_token` is a LIVE single-use bearer secret — redact it so a formatted value (a debug trace, a
// panic message) can never mint a second custody surface for it (the same redaction discipline the
// workspace-credential carriers follow).
impl std::fmt::Debug for GrantIssued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantIssued")
            .field("grant_token", &"<redacted>")
            .field("workspace_id", &self.workspace_id)
            .field("workspace_display_name", &self.workspace_display_name)
            .field("workspace_address", &self.workspace_address)
            .field("device_auth_id", &self.device_auth_id)
            .field("device_key_id", &self.device_key_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
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
/// identity (the RFC-8628 confused-deputy guard: the page shows which device + workspace is being
/// authorized, so a human approves the session they actually started, not one an attacker raced in). Carries
/// **no secret** — no device code, no grant, no token.
#[derive(Debug, Clone)]
pub struct VerificationContext {
    /// The session's intent — `enroll` (join a workspace named by its address), `standup` (create one
    /// on approval), or `login` (re-mint this device's credentials). The page branches its copy on this.
    pub intent: SessionIntent,
    /// The human-readable machine name the device offered at start.
    pub machine_name: String,
    /// A short hex fingerprint of the device's public key — a human cross-checks it against the device. NOT
    /// the `device_key_id` (no `dk_` prefix, shorter); a display aid only, never an authority input.
    pub device_fingerprint: String,
    /// The workspace display name the device would join. A REQUIRED field kept wire-stable: an enroll
    /// session whose address never resolved echoes the REQUESTED name verbatim (charset-validated at
    /// authorize, so safe to render — and existence is never disclosed here); a standup or login session
    /// has no workspace, so it carries `""` (the page renders that copy from `intent`).
    pub workspace_display_name: String,
    /// The org-domain claim (if any).
    pub verified_domain: Option<String>,
    /// The domain-verification state.
    pub verified_domain_status: String,
}

/// The outcome of confirming a session's external identity (the OIDC callback's in-Authority half). A single
/// success marker — the identity proof happened in the CALLER (the OIDC module validated the id_token); this
/// op only records the proven principal onto the live session, exactly like [`PasscodeComplete::Confirmed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The session's identity was confirmed (the device's next poll yields a grant).
    Confirmed,
}

/// The successful result of an enrollment redeem (or an admin claim) — the confirmed identity, the
/// registered device, and the ONE minted workspace credential. **NO user token, ever.**
#[derive(Clone)]
pub struct EnrollmentRedeemed {
    /// The workspace the device enrolled into.
    pub workspace_id: WorkspaceId,
    /// The confirmed principal the device acts as.
    pub principal: Principal,
    /// The server-derived device key id now registered.
    pub device_key_id: String,
    /// The plaintext workspace credential (the `0600` at-rest secret on the device; only its sha256
    /// is stored server-side, on the device's registry row). Returned once; deterministic per grant,
    /// so a lost-ack replay re-returns the identical value. No expiry — revoke + re-enroll is the
    /// rotation (a directory row-write is the kill switch).
    pub credential: String,
}

// `credential` is a LIVE bearer secret — redact it so a formatted value (a debug trace, a panic
// message) can never mint a second custody surface for it.
impl std::fmt::Debug for EnrollmentRedeemed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollmentRedeemed")
            .field("workspace_id", &self.workspace_id)
            .field("principal", &self.principal)
            .field("device_key_id", &self.device_key_id)
            .field("credential", &"<redacted>")
            .finish()
    }
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

/// The ONE membership-denial detail every not-yours redeem answers with — an address that never
/// resolved, a grant redeemed against the wrong workspace, a vanished workspace row, and an identity
/// with no seat are byte-for-byte indistinguishable, on EVERY deployment posture. Links are just
/// addresses and the ROSTER is the lock, so a redeem must never become a workspace-existence or
/// roster-enumeration oracle.
pub const ENROLL_UNAVAILABLE: &str = "not available for this account — check the address; if you were invited, confirm with your inviter";

/// One workspace a login touched: the seat's facts plus either the freshly re-minted credential or the
/// static reason no credential was minted (this device is revoked there, or its key id is squatted).
#[derive(Clone)]
pub struct LoginSeat {
    /// The workspace of the confirmed seat.
    pub workspace_id: WorkspaceId,
    /// The workspace's ADDRESS name.
    pub name: String,
    /// The workspace's display name.
    pub display_name: String,
    /// The person's role on the seat (`owner` / `reviewer` / `member`).
    pub role: String,
    /// The device's server-derived key id (the same id in every workspace — it is a key derivation).
    pub device_key_id: String,
    /// The freshly minted plaintext workspace credential — deterministic per `(grant, workspace)`, so
    /// a lost-ack replay re-returns the identical value. `None` when `blocked` says why.
    pub credential: Option<String>,
    /// Why no credential was minted; `None` on success.
    pub blocked: Option<&'static str>,
}

// `credential` is a LIVE bearer secret — redact it (the crate's redaction discipline).
impl std::fmt::Debug for LoginSeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginSeat")
            .field("workspace_id", &self.workspace_id)
            .field("name", &self.name)
            .field("display_name", &self.display_name)
            .field("role", &self.role)
            .field("device_key_id", &self.device_key_id)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .field("blocked", &self.blocked)
            .finish()
    }
}

/// The successful result of a login redeem — the proven identity plus one entry per confirmed seat.
/// ZERO seats is a valid success (the identity is established; there is nothing to mint).
#[derive(Debug, Clone)]
pub struct LoginRedeemed {
    /// The proven principal (canonical form).
    pub principal: Principal,
    /// One entry per workspace where the principal holds a confirmed seat, ordered by workspace id.
    pub memberships: Vec<LoginSeat>,
}

/// The outcome of [`Authority::redeem_login`](crate::Authority::redeem_login).
#[derive(Debug, Clone)]
pub enum LoginOutcome {
    /// The identity was proven; per-workspace credentials (or blocked markers) follow.
    Redeemed(LoginRedeemed),
    /// The redeem was denied (a uniform denial; the static reason is for server logs, never an oracle).
    Denied(&'static str),
}

/// The deployment posture of a plane (and of each `workspace` row). Enrollment gates identically on
/// BOTH postures now — a redeem requires a rostered identity, and identity proof is always a passcode
/// or a web-session approval (the self-host bearer shortcut died with the invite token; its trust
/// anchor was that token). The posture still decides which SURFACES exist: standup + every session
/// leg are cloud-only, and the one-time admin claim is how self-host stands up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentMode {
    /// The hosted plane.
    Cloud,
    /// A self-hosted plane (the whole device-lane loop; no web-session legs).
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

/// A device-auth session's intent: `enroll` joins a workspace named by its ADDRESS; `standup` starts
/// with NO workspace — a signed-in human's approval creates one and seats the session's identity as its
/// first owner; `login` proves the person's identity and re-mints this device's credential in every
/// workspace where that identity holds a confirmed seat. A standup session is only ever advanced by its
/// approval (the passcode/OIDC confirm paths refuse it) and refuses to start on a self-host plane;
/// enroll and login run on BOTH postures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIntent {
    /// Join an existing workspace, named by its address.
    Enroll,
    /// Create a workspace on approval (the cloud self-serve first-boot flow).
    Standup,
    /// Prove the person's identity and re-mint this device's workspace credentials.
    Login,
}

impl SessionIntent {
    /// The stored discriminant (`device_auth_sessions.intent`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SessionIntent::Enroll => "enroll",
            SessionIntent::Standup => "standup",
            SessionIntent::Login => "login",
        }
    }

    /// Parse a stored discriminant. `None` on an unknown value (store corruption at a read site).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enroll" => Some(SessionIntent::Enroll),
            "standup" => Some(SessionIntent::Standup),
            "login" => Some(SessionIntent::Login),
            _ => None,
        }
    }
}

/// The enrollment subsystem's static configuration — held on the [`Authority`](crate::Authority) so
/// `start_device_auth` / `read_claim_bootstrap` can return the verification base
/// URL, and the offered enrollment method without re-plumbing them per call. The `secret_path` is consumed
/// once at [`Authority::with_enrollment_config`](crate::Authority::with_enrollment_config) load time.
#[derive(Debug, Clone)]
pub struct EnrollmentConfig {
    /// The `0600` file the 32-byte HMAC enrollment secret is load-or-generated from (load-time only).
    pub secret_path: PathBuf,
    /// The plane's public **API** base URL — the root a client dials for enrollment + sync. The bootstrap
    /// payload's `plane.base_url` is always this (never a web front); the device-auth `verification_uri`
    /// and the minted `/i/` links fall back to it when their dedicated bases are unset.
    pub base_url: String,
    /// The HUMAN-facing verification base URL, when it differs from `base_url` (a hosted plane whose web
    /// pages live on another host). `None` ⇒ `base_url`. Only the device-auth `verification_uri`(+`_complete`)
    /// are built on it.
    pub verify_base_url: Option<String>,
    /// The PUBLIC share-link base the minted `/i/<token>` links ride, when it differs from `base_url` (a
    /// hosted plane whose user-visible links live on the web origin, which serves/proxies the bootstrap
    /// read). `None` ⇒ `base_url`. Only the minted link STRING moves — the bootstrap payload keeps
    /// declaring the API `base_url`, and the client re-roots onto it after the one bootstrap fetch.
    pub link_base_url: Option<String>,
    /// This plane's deployment posture (the default for a workspace this plane stands up).
    pub deployment_mode: DeploymentMode,
    /// The enrollment method offered to a bootstrapping device (e.g. `"device_code"`), surfaced in the bootstrap.
    pub enrollment_method: String,
}

impl EnrollmentConfig {
    /// The base the human-facing verification links are built on (`verify_base_url` else `base_url`).
    pub(crate) fn verify_base(&self) -> &str {
        self.verify_base_url.as_deref().unwrap_or(&self.base_url)
    }

    /// The base the minted `/i/<token>` share links ride (`link_base_url` else `base_url`).
    pub(crate) fn link_base(&self) -> &str {
        self.link_base_url.as_deref().unwrap_or(&self.base_url)
    }
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
        let seed = crate::secret::load_or_generate_seed(&config.secret_path)?;
        Ok(Self {
            secret: EnrollmentSecret(seed),
            config,
        })
    }
}

/// Derive a deterministic opaque credential: `base64url-unpadded(HMAC-SHA256(secret, domain ‖ each part
/// length-prefixed))`. **Determinism is the keystone** — a lost-ack create/issue retry re-derives the
/// IDENTICAL credential, and a consumed grant re-derives the SAME workspace credential, so redeem is
/// naturally idempotent with no fresh mint per call. The `domain` is a fixed byte tag (`b"grant"` /
/// `b"wscred"` — none a prefix of another) that separates the credential
/// families; each `part` is framed
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

/// The server-derived device key id from a raw device public key: `dk_<first 32 hex of sha256(pubkey)>`,
/// via the ONE kernel derivation (`topos_core::identity::device_key_id` — the same fn the client
/// calls). The plane derives this ITSELF on enroll and re-derives it on redeem — a client-asserted id is
/// never trusted, so a mismatch between a grant's bound key and the presented key is caught structurally.
#[must_use]
pub(crate) fn device_key_id_for(device_public_key: &[u8; 32]) -> String {
    topos_core::identity::device_key_id(device_public_key)
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
    /// The workspace the CALLER claims to be joining (the request path's). A grant scoped to a
    /// different workspace — or to none — answers the one uniform membership denial.
    pub ws: &'a WorkspaceId,
    /// `sha256(grant_token)` — the grant row's PK (the bearer credential's stored form).
    pub grant_sha256: [u8; 32],
    /// The raw device public key presented (must equal the grant's bound key).
    pub device_public_key: [u8; 32],
    /// The SERVER-derived device key id from `device_public_key` (a client-asserted id is never trusted).
    pub server_device_key_id: &'a str,
    /// The server clock (epoch-ms).
    pub now: i64,
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

/// A fresh, high-entropy one-time admin-claim token — the same 32-random-byte base64url shape as the device
/// code (a bearer capability; only its sha256 is ever stored).
pub(crate) fn random_claim_token() -> Result<String> {
    random_device_code()
}

/// A fresh `user_code` — the correlation handle a browser approval is matched against. It is **not** a
/// secret (that is the `device_code`); it exists only to identify *which* pending device-authorization
/// session an approval belongs to. Because it rides exclusively inside `verification_uri_complete` (the
/// human clicks the URL — no plane surface accepts a typed code), it is a high-entropy OPAQUE URL token,
/// not a short human-typeable code: 32 random bytes, base64url-unpadded (~256 bits, URL-path-safe). The
/// entropy makes a live code unguessable within its TTL — the property that matters for standup, where an
/// approval mints ownership with no roster gate behind it. ENROLL and STANDUP share this one shape: the
/// old short, human-typeable enroll code existed only to be typed, which never happens.
pub(crate) fn random_user_code() -> Result<String> {
    random_device_code()
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

// ── the orchestration ops (the public Authority methods delegate to these) ─────────────────────────────

/// Resolve a `user_code` to its verification-page disclosure (the orchestration half of
/// [`Authority::read_verification_context`]). A miss — an unknown code, a non-live (issued/denied/expired)
/// session, or an expired one — is the single indistinguishable `NotFound`. A pure read (no mutation,
/// no secret).
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
    let intent = SessionIntent::parse(&session.intent)
        .ok_or_else(|| AuthorityError::integrity(EnrollCorruptIntent))?;
    // An enroll session against an UNRESOLVED address echoes the requested name verbatim (it was
    // charset-validated at authorize, so it is safe to render — and whether it resolved is never
    // disclosed here). A standup/login session has no workspace at all: the required display fields
    // stay wire-stable ("" / empty) and the page renders that copy from `intent`.
    let (workspace_display_name, verified_domain, verified_domain_status) =
        match &session.workspace_id {
            Some(ws) => {
                let Some(workspace) = authority.db().read_workspace(ws).await? else {
                    return Err(AuthorityError::NotFound);
                };
                (
                    workspace.display_name,
                    workspace.verified_domain,
                    workspace.verified_domain_status,
                )
            }
            None => (
                session.requested_workspace.clone().unwrap_or_default(),
                None,
                "unverified".to_owned(),
            ),
        };
    Ok(VerificationContext {
        intent,
        machine_name: session.machine_name,
        device_fingerprint: device_fingerprint(&session.device_pubkey),
        workspace_display_name,
        verified_domain,
        verified_domain_status,
    })
}

/// A stored `device_auth_sessions.intent` did not parse — store corruption (a CHECK should forbid it).
#[derive(Debug, thiserror::Error)]
#[error("stored device-auth session has an invalid intent")]
struct EnrollCorruptIntent;

/// Start a device-authorization flow toward a workspace ADDRESS (the orchestration half of
/// [`Authority::start_device_auth`]). SERVER-derives the device key id (a client-asserted id is
/// ignored), resolves the requested name — WITHOUT disclosing whether it resolved: the session opens
/// `pending` either way (an unknown address runs the same flow to the redeem's one uniform denial) —
/// and inserts the session. Identity proof is ALWAYS a passcode or a web-session approval now, on
/// every deployment posture (the self-host born-confirmed shortcut died with the invite bearer token,
/// which was its trust anchor).
pub(crate) async fn start_device_auth(
    authority: &Authority,
    workspace_name: &str,
    device_public_key: &[u8; 32],
    machine_name: &str,
    now: i64,
    created_at: &str,
) -> Result<DeviceAuthStart> {
    // The SYNTAX belt only (charset + length — a malformed name is a typed parse-boundary refusal the
    // route can 400): the reserved list is a CREATION concern, and an unknown-but-well-formed name must
    // run the flow to the uniform denial, never answer differently here.
    if !crate::governance::workspace_name_syntax_ok(workspace_name) {
        return Err(AuthorityError::InvalidId(
            crate::id::IdError::DisallowedChar,
        ));
    }
    let resolved = authority.db().workspace_id_by_name(workspace_name).await?;
    let device_key_id = device_key_id_for(device_public_key);
    let device_code = random_device_code()?;
    let device_code_sha256 = sha256_token(&device_code);
    let user_code = unique_user_code(authority, random_user_code).await?;
    let expires_at = now.saturating_add(DEVICE_AUTH_TTL_MS);

    authority
        .db()
        .insert_device_auth_session(
            &device_code_sha256,
            &user_code,
            resolved.as_ref(),
            Some(workspace_name),
            device_public_key,
            &device_key_id,
            machine_name,
            SessionIntent::Enroll.as_str(),
            "pending",
            None,
            expires_at,
            DEVICE_AUTH_INTERVAL_SECS,
            created_at,
        )
        .await?;

    device_auth_start(authority, device_code, user_code, expires_at)
}

/// Start a LOGIN device-authorization flow (the orchestration half of
/// [`Authority::start_login_device_auth`]): no workspace, no requested name — the session proves the
/// person's identity, and its grant redeems at the login door into one credential per confirmed seat.
/// Allowed on BOTH deployment postures (unlike standup): sign-in is how a device recovers its
/// credentials wherever the plane runs.
pub(crate) async fn start_login_device_auth(
    authority: &Authority,
    device_public_key: &[u8; 32],
    machine_name: &str,
    now: i64,
    created_at: &str,
) -> Result<DeviceAuthStart> {
    let device_key_id = device_key_id_for(device_public_key);
    let device_code = random_device_code()?;
    let device_code_sha256 = sha256_token(&device_code);
    let user_code = unique_user_code(authority, random_user_code).await?;
    let expires_at = now.saturating_add(DEVICE_AUTH_TTL_MS);

    authority
        .db()
        .insert_device_auth_session(
            &device_code_sha256,
            &user_code,
            None,
            None,
            device_public_key,
            &device_key_id,
            machine_name,
            SessionIntent::Login.as_str(),
            "pending",
            None,
            expires_at,
            DEVICE_AUTH_INTERVAL_SECS,
            created_at,
        )
        .await?;

    device_auth_start(authority, device_code, user_code, expires_at)
}

/// Start a STANDUP device-authorization flow (the orchestration half of
/// [`Authority::start_standup_device_auth`]): no invite, no workspace — the session is born `pending` with
/// `intent = 'standup'`, and a signed-in human's approval later creates the workspace. CLOUD planes only: on
/// a self-host plane this is the single indistinguishable `NotFound` (self-host stands up via the operator's
/// one-time claim link instead — there is no web identity to approve with).
pub(crate) async fn start_standup_device_auth(
    authority: &Authority,
    device_public_key: &[u8; 32],
    machine_name: &str,
    now: i64,
    created_at: &str,
) -> Result<DeviceAuthStart> {
    if authority.enrollment()?.config.deployment_mode != DeploymentMode::Cloud {
        return Err(AuthorityError::NotFound);
    }
    let device_key_id = device_key_id_for(device_public_key);
    let device_code = random_device_code()?;
    let device_code_sha256 = sha256_token(&device_code);
    // The same opaque high-entropy `user_code` shape enroll uses (see `random_user_code`): it rides only
    // inside `verification_uri_complete`, and its entropy is what keeps a live standup code unguessable.
    let user_code = unique_user_code(authority, random_user_code).await?;
    let expires_at = now.saturating_add(DEVICE_AUTH_TTL_MS);

    authority
        .db()
        .insert_device_auth_session(
            &device_code_sha256,
            &user_code,
            None,
            None,
            device_public_key,
            &device_key_id,
            machine_name,
            SessionIntent::Standup.as_str(),
            "pending",
            None,
            expires_at,
            DEVICE_AUTH_INTERVAL_SECS,
            created_at,
        )
        .await?;

    device_auth_start(authority, device_code, user_code, expires_at)
}

/// Assemble the [`DeviceAuthStart`] a start op returns: the verification URIs are built on the plane's
/// HUMAN-facing verification base (`verify_base_url` else `base_url`) as `{base}/verify` and
/// `{base}/verify/{user_code}` — the client uses the complete form verbatim.
fn device_auth_start(
    authority: &Authority,
    device_code: String,
    user_code: String,
    expires_at: i64,
) -> Result<DeviceAuthStart> {
    let verify_base = authority.enrollment()?.config.verify_base();
    let verification_uri = format!("{verify_base}/verify");
    let verification_uri_complete = format!("{verification_uri}/{user_code}");
    Ok(DeviceAuthStart {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        expires_at,
        interval_secs: DEVICE_AUTH_INTERVAL_SECS,
    })
}

/// A user code that no LIVE session already holds (the partial-unique index forbids a clash). Astronomically
/// unlikely to need more than one try; bounded retries keep it total. `mint` is the shape-specific generator
/// (the short enroll code, or the high-entropy standup code).
async fn unique_user_code(authority: &Authority, mint: fn() -> Result<String>) -> Result<String> {
    for _ in 0..8 {
        let code = mint()?;
        if !authority.db().live_user_code_exists(&code).await? {
            return Ok(code);
        }
    }
    Err(AuthorityError::internal(EnrollEntropy))
}

/// Poll a device-authorization session (the orchestration half of [`Authority::poll_device_auth`]).
/// A granted poll's workspace context carries the full ADDRESS (`<link_base>/<name>`), composed here
/// from the enrollment config — the one place the link base lives.
pub(crate) async fn poll_device_auth(
    authority: &Authority,
    device_code: &str,
    now: i64,
    created_at: &str,
) -> Result<DeviceAuthPoll> {
    let device_code_sha256 = sha256_token(device_code);
    let enrollment = authority.enrollment()?;
    let secret = enrollment.secret.as_bytes();
    let link_base = enrollment.config.link_base().to_owned();
    authority
        .db()
        .poll_txn(&device_code_sha256, now, created_at, secret, &link_base)
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

/// Redeem an enrollment grant into `ws` (the orchestration half of
/// [`Authority::redeem_enrollment`]). The GRANT is the bearer credential (a deterministic HMAC secret,
/// stored only as its sha256); the presented device key must equal the grant's bound key (the binding
/// consistency check runs in-transaction). SERVER-derives the device key id from the presented key,
/// then runs the one gate + register + mint transaction — whose membership gate is the ROSTER on every
/// deployment posture ([`ENROLL_UNAVAILABLE`], one uniform denial). Returns the ONE minted workspace
/// credential — NEVER a user token, never a per-skill token.
pub(crate) async fn redeem_enrollment(
    authority: &Authority,
    ws: &WorkspaceId,
    grant_token: &str,
    device_public_key: [u8; 32],
    now: i64,
) -> Result<RedeemOutcome> {
    let grant_sha256 = sha256_token(grant_token);
    let server_device_key_id = device_key_id_for(&device_public_key);
    let secret = authority.enrollment()?.secret.as_bytes();
    let input = RedeemInput {
        ws,
        grant_sha256,
        device_public_key,
        server_device_key_id: &server_device_key_id,
        now,
    };
    authority.db().redeem_txn(&input, secret).await
}

/// Redeem a LOGIN grant (the orchestration half of [`Authority::redeem_login`]): prove the grant's
/// bound device key, then — in ONE transaction — register this device and re-mint its workspace
/// credential in EVERY workspace where the proven identity holds a confirmed seat. Each credential is
/// deterministic per `(grant, workspace)` (`derive_token(b"wscred", [grant_sha256, ws])`), so a
/// lost-ack replay re-returns identical plaintexts; a seat where this device is revoked or its key id
/// squatted comes back `blocked`, with no credential and no side effect there.
pub(crate) async fn redeem_login(
    authority: &Authority,
    grant_token: &str,
    device_public_key: [u8; 32],
    now: i64,
) -> Result<LoginOutcome> {
    let grant_sha256 = sha256_token(grant_token);
    let server_device_key_id = device_key_id_for(&device_public_key);
    let secret = authority.enrollment()?.secret.as_bytes();
    authority
        .db()
        .redeem_login_txn(
            &grant_sha256,
            &device_public_key,
            &server_device_key_id,
            now,
            secret,
        )
        .await
}

/// Resolve a presented workspace credential to an opaque [`ReadScope`] on one of the workspace's
/// skills — the device READ lane's authentication + gate.
///
/// Hashes the credential (the registry stores ONLY the sha256 — the plaintext is a `0600` secret at
/// rest on the device, never recoverable from a database read), probes the registry row bound to the
/// CALLER'S CLAIMED workspace (a cross-workspace credential is the same miss as an unknown one), then
/// gates on a CONFIRMED `workspace_member` row for the row's bound principal — the membership join IS
/// the read authorization; deleting the membership row kills reads immediately. The skill comes from
/// the caller's path and is checked by the lane-blind reachability half, never here. Every miss — an
/// unknown/rotated/revoked credential, a removed member, a malformed id — is the single
/// indistinguishable [`AuthorityError::NotFound`], so a caller can never probe what exists.
///
/// # Errors
/// [`AuthorityError::NotFound`] on any miss; [`AuthorityError::Internal`] on a database fault;
/// [`AuthorityError::Integrity`] if a stored row is corrupt.
pub(crate) async fn resolve_read_scope(
    authority: &Authority,
    ws: WorkspaceId,
    skill: SkillId,
    credential: &str,
) -> Result<ReadScope> {
    let credential_sha256 = digest::sha256(credential.as_bytes());
    let Some(identity) = authority
        .db()
        .resolve_read_credential(&ws, &credential_sha256)
        .await?
    else {
        return Err(AuthorityError::NotFound);
    };
    if !authority
        .db()
        .confirmed_member(&ws, &identity.principal)
        .await?
    {
        return Err(AuthorityError::NotFound);
    }
    Ok(ReadScope::for_member(ws, skill, identity.principal))
}
