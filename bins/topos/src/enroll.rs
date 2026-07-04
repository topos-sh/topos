//! The on-disk enrollment state — the documents `follow` writes and the plane transport reads:
//! `instance.json` (which plane + the pinned plane key), `follows.json` (which skills are followed, in
//! which mode/workspace, with which read credential), `identity/user.json` (the enrolled principal's
//! non-secret metadata), and the enrollment WAL (`identity/enrollment.json`, the two-call resume's
//! durable state). Both the writers (the `follow` promote path) and the readers live here.
//!
//! **These are client-only transport/enrollment documents — they are deliberately NOT in
//! `topos-types::persisted`.** That crate is the cross-language wire/contract leaf whose shapes are
//! schema-generated into `contracts/`; these documents are local sidecar state owned by the enrollment
//! subsystem, exactly like `identity/host.json` ([`crate::identity`]). They follow the same idiom — a
//! `schema_version` field read through [`crate::doc::read_doc`], which dispatches the **fail-closed
//! migration** (an unknown/newer `schema_version` is an upgrade error, never silently parsed or deleted) —
//! but they own their own shape rather than freezing it in the public contract on a guess. `follows.json`
//! additionally carries a **secret** (`read_token`), which is another reason it stays out of the public
//! contract.
//!
//! **`read_token` is a `0600` secret.** `follows.json` and the WAL are written through the `0600`
//! private-doc primitives ([`crate::doc::write_doc_private`]) and refused-on-permissive at read, because
//! a read token grants read access to a workspace's skills; `instance.json`/`user.json` carry no secret.
//!
//! **Ids are validated at load.** A skill/workspace id read out of `follows.json` or the WAL later keys
//! path joins (`~/.topos/skills/<id>`, the harness skills dir) and URL splices, so the loaders parse
//! every id through [`crate::id`] — a hand-edited (or maliciously written) traversal id fails the load
//! closed as a corrupt document, mirroring the wire-boundary checks in [`crate::plane_http`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::bootstrap::{DeploymentMode, VerifiedDomainStatus};

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::plane::{FollowContext, FollowMode};
use crate::plane_http::SkillCred;
use crate::sidecar::Layout;

/// The plane's deployment posture defaults to self-host (a missing field on a hand-written/older
/// `instance.json` reads as the no-identity-step posture).
fn default_deployment_mode() -> DeploymentMode {
    DeploymentMode::SelfHost
}

/// A missing domain-verification field reads as unverified.
fn default_domain_status() -> VerifiedDomainStatus {
    VerifiedDomainStatus::Unverified
}

/// `instance.json` — the plane this client is enrolled with + the pinned plane public key. Public metadata
/// only (the plane key is a PUBLIC Ed25519 key — ordinary file perms are fine).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Instance {
    pub schema_version: u32,
    /// The plane base URL (no trailing slash required; the transport normalizes it), e.g. `https://topos.sh`.
    pub base_url: String,
    /// The pinned plane **public** Ed25519 key, 32 bytes as 64-char lowercase hex — the signed `current`
    /// pointer is verified against it.
    pub plane_key: String,
    /// The id of that plane key (advisory; the signature carries its own key id).
    pub plane_key_id: String,
    /// The plane's deployment posture (disclosed at enrollment; not a trust input).
    #[serde(default = "default_deployment_mode")]
    pub deployment_mode: DeploymentMode,
    /// The enrollment method the plane advertised (e.g. `"device_code"`); disclosure only.
    #[serde(default)]
    pub enrollment_method: String,
    /// The workspace display name (for the agent's disclosure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_display_name: Option<String>,
    /// The workspace's org-domain claim, if any — the relay-phishing provenance shown next to the URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_domain: Option<String>,
    /// The domain-verification state.
    #[serde(default = "default_domain_status")]
    pub verified_domain_status: VerifiedDomainStatus,
}

/// `follows.json` — the durable follow-state: the skills this client follows, each with its workspace,
/// mode, review posture, and **secret** read token. See the module comment: `0600` on any write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Follows {
    pub schema_version: u32,
    #[serde(default)]
    pub follows: Vec<FollowEntry>,
}

/// One followed skill's enrollment record. Fans out two ways: the consent seam ([`FollowContext`] —
/// workspace/mode/review/following) and the transport credential ([`SkillCred`] — workspace/read_token).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FollowEntry {
    /// The stable skill id (the key both fan-outs are keyed by).
    pub skill_id: String,
    /// The workspace this skill is followed in (the expected signed-pointer scope).
    pub workspace_id: String,
    /// The per-follower read token (Bearer for versions/bundles, path segment for current). **SECRET.**
    pub read_token: String,
    /// How a new `current` is adopted (auto / confirm-each).
    pub mode: FollowModeDoc,
    /// Whether the workspace gates moves behind review (selects the consent satisfier only).
    pub review_required: bool,
    /// Whether the skill is currently followed (a `false` skill is inventoried but not pulled).
    pub following: bool,
}

// `read_token` is a secret — redact it so it never reaches a log / panic message / Debug dump.
impl std::fmt::Debug for FollowEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FollowEntry")
            .field("skill_id", &self.skill_id)
            .field("workspace_id", &self.workspace_id)
            .field("read_token", &"<redacted>")
            .field("mode", &self.mode)
            .field("review_required", &self.review_required)
            .field("following", &self.following)
            .finish()
    }
}

/// The on-disk spelling of [`FollowMode`] (snake_case). A local copy because [`FollowMode`] is a
/// `pub(crate)` engine enum with no serde derives; mapped 1:1 at load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FollowModeDoc {
    Auto,
    ConfirmEach,
}

impl FollowModeDoc {
    fn to_plane(self) -> FollowMode {
        match self {
            FollowModeDoc::Auto => FollowMode::Auto,
            FollowModeDoc::ConfirmEach => FollowMode::ConfirmEach,
        }
    }
}

/// Read `instance.json`, or `None` if absent. Fail-closed on an unknown/newer `schema_version`.
pub(crate) fn read_instance(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<Instance>, ClientError> {
    doc::read_doc(fs, &layout.instance_path())
}

/// Read `follows.json`, or `None` if absent. `follows.json` carries the **secret** read tokens, so it is
/// read through [`doc::read_doc_private`] — a group/other-accessible file is refused BEFORE parsing.
/// Fail-closed on an unknown/newer `schema_version` AND on any entry whose skill/workspace id is not a
/// safe path component (the id boundary: a traversal id must never reach a join downstream).
pub(crate) fn read_follows(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<Follows>, ClientError> {
    let follows: Option<Follows> = doc::read_doc_private(fs, &layout.follows_path())?;
    if let Some(f) = &follows {
        for entry in &f.follows {
            crate::id::SkillId::parse(&entry.skill_id)?;
            crate::id::validate_workspace_id(&entry.workspace_id)?;
        }
    }
    Ok(follows)
}

/// Read `identity/user.json`, or `None` if absent. Metadata only (no secret) → ordinary `read_doc`.
/// Fail-closed on an unknown/newer `schema_version`. The `invite` verb reads the enrolled `workspace_id`
/// (the governance frame's scope) from here.
pub(crate) fn read_user(fs: &dyn FsOps, layout: &Layout) -> Result<Option<UserDoc>, ClientError> {
    doc::read_doc(fs, &layout.user_path())
}

/// The follow-state fan-out → the engine's consent seam (`FileFollow` returns these). Every entry is
/// carried (the engine itself skips a `following == false` skill); creds live in the transport, not here.
pub(crate) fn follow_contexts(follows: &Follows) -> Vec<(String, FollowContext)> {
    follows
        .follows
        .iter()
        .map(|e| {
            (
                e.skill_id.clone(),
                FollowContext {
                    workspace_id: e.workspace_id.clone(),
                    mode: e.mode.to_plane(),
                    review_required: e.review_required,
                    following: e.following,
                },
            )
        })
        .collect()
}

/// The follow-state fan-out → the transport credential map (`UreqPlane` looks a skill's cred up here). All
/// entries are included so any skill the engine queries resolves; a skill absent from the map is a
/// `NotFound`.
pub(crate) fn skill_creds(follows: &Follows) -> HashMap<String, SkillCred> {
    follows
        .follows
        .iter()
        .map(|e| {
            (
                e.skill_id.clone(),
                SkillCred::new(e.workspace_id.clone(), e.read_token.clone()),
            )
        })
        .collect()
}

// =================================================================================================
// The enrollment WAL (`identity/enrollment.json`) — the two-call resume's durable state. A `0600`
// SECRET (it holds the device code and, once redeemed, the read tokens). Hand-written `Debug` redacts.
// =================================================================================================

/// One skill an invite pre-offered (carried in the WAL so a re-`--resume` can write `follows.json` + lay
/// the first-receive baselines without re-reading the bootstrap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OfferedSkill {
    pub skill_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// How an enrollment was rooted — an `/i/` invite (the original door), a workspace-standup sign-in, or a
/// one-time admin claim. Persisted in the WAL context so the promotion writes an honest
/// `user.json.invite_rooted` and the standup publish can disclose its receipt; a missing field on an older
/// WAL reads as `invite` (the only root that existed before this field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EnrollRoot {
    /// Rooted in an `/i/` invite link.
    Invite,
    /// Rooted in the workspace-standup sign-in (an un-enrolled `publish` on a hosted plane).
    Standup,
    /// Rooted in a one-time admin-claim link (the self-host bearer door).
    Claim,
}

/// The serde default for [`EnrollRoot`] — invite (the pre-existing WALs were all invite-rooted).
fn default_enroll_root() -> EnrollRoot {
    EnrollRoot::Invite
}

/// The non-secret workspace/plane context both WAL phases carry — everything a promotion needs to write
/// `instance.json` + `follows.json` + `user.json` without re-contacting the plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EnrollContext {
    pub base_url: String,
    /// The TOFU-decided plane public key (32-byte lowercase hex) — pinned at this `base_url`.
    pub pinned_plane_key: String,
    pub plane_key_id: String,
    pub deployment_mode: DeploymentMode,
    pub enrollment_method: String,
    pub workspace_id: String,
    pub workspace_display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_domain: Option<String>,
    pub verified_domain_status: VerifiedDomainStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offered_skills: Vec<OfferedSkill>,
    /// How a followed skill adopts a new `current` (`--manual` ⇒ confirm-each, else auto).
    pub mode: FollowModeDoc,
    /// Which door rooted this enrollment (invite / standup / claim); absent on an older WAL ⇒ invite.
    #[serde(default = "default_enroll_root")]
    pub root: EnrollRoot,
}

/// One minted read credential persisted into the Redeemed WAL (a `0600` secret — the `read_token` grants
/// read access to a workspace's skills).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RedeemedCredDoc {
    pub skill_id: String,
    /// **SECRET** — redacted in the WAL's `Debug`.
    pub read_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

// Redact the read token so it never reaches a log / panic / Debug dump.
impl std::fmt::Debug for RedeemedCredDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedeemedCredDoc")
            .field("skill_id", &self.skill_id)
            .field("read_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The WAL phase. **Internally tagged** (`"phase"`), so the on-disk shape is
/// `{ "phase": "authorizing", "context": {…}, … }`. Hand-written `Debug` on the wrapper redacts secrets.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(crate) enum EnrollPhase {
    /// A live device-authorization session, awaiting the human's verification + a granted poll.
    Authorizing {
        context: EnrollContext,
        /// **SECRET** — the device code the client polls with. Redacted in `Debug`.
        device_code: String,
        /// The short user code (also the `device_auth_id` the enroll possession frame binds).
        user_code: String,
        /// The SERVER-provided verification URL with the code embedded — persisted so a pending resume
        /// re-emits it verbatim. `None` from an older plane (the resume then reconstructs a fallback).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification_uri_complete: Option<String>,
        /// The minimum poll interval, in seconds.
        interval: u64,
        /// The session expiry as epoch-millis — the recovery sweep abandons a WAL past this that never
        /// reached `Redeemed`.
        expires_at_millis: i64,
    },
    /// A live workspace-STANDUP device-authorization session (an un-enrolled `publish` on a hosted plane),
    /// awaiting the human's sign-in approval. There is NO workspace yet — approving CREATES one — so this
    /// phase carries the plane facts the standup authorize response disclosed instead of an
    /// [`EnrollContext`]; the granted poll supplies `{workspace_id, display_name}`, at which point the
    /// flow converts to the ordinary [`EnrollPhase::Redeemed`].
    AuthorizingStandup {
        /// The plane base URL this standup was started against (env override or the hosted default).
        base_url: String,
        /// The TOFU-decided plane public key (32-byte lowercase hex) — pinned at this `base_url`.
        pinned_plane_key: String,
        plane_key_id: String,
        /// The plane's deployment posture (from the authorize response's plane block; disclosure).
        deployment_mode: DeploymentMode,
        /// The enrollment method the plane advertised (disclosure only).
        enrollment_method: String,
        /// **SECRET** — the device code the client polls with. Redacted in `Debug`.
        device_code: String,
        /// The high-entropy standup code (also the `device_auth_id` the possession frame binds).
        user_code: String,
        /// The SERVER-provided sign-in URL with the code embedded — re-emitted verbatim while pending.
        verification_uri_complete: String,
        /// The session expiry as epoch-millis — the recovery sweep abandons an expired standup WAL
        /// exactly like an expired `Authorizing` one.
        expires_at_millis: i64,
    },
    /// A one-time admin-CLAIM redeem about to be (or uncertainly) sent — written BEFORE the first POST so
    /// an uncertain send retries `/v1/admin-claim` DIRECTLY from here (never refetching the `/i/` link —
    /// a consumed claim bootstraps 404 by design; the server's same-device replay re-answers Redeemed).
    ClaimPending {
        base_url: String,
        /// The TOFU-decided plane public key (32-byte lowercase hex) — pinned at this `base_url`.
        pinned_plane_key: String,
        plane_key_id: String,
        /// The plane's deployment posture (from the claim bootstrap; disclosure).
        deployment_mode: DeploymentMode,
        /// The enrollment method the bootstrap advertised (`"admin_claim"`; disclosure only).
        enrollment_method: String,
        /// The workspace the claim stands up (from the bootstrap row).
        workspace_id: String,
        /// The workspace display name (disclosure).
        workspace_display_name: String,
        /// **SECRET** — the one-time claim token. Redacted in `Debug`.
        claim_token: String,
    },
    /// The grant was redeemed and the read creds minted — recorded BEFORE promotion (the lockout fence: a
    /// single-use grant cannot be re-redeemed, so a crash after redeem completes from here).
    Redeemed {
        context: EnrollContext,
        /// **SECRET** — the minted per-skill read tokens. Redacted in `Debug`.
        read_creds: Vec<RedeemedCredDoc>,
        device_key_id: String,
        /// The principal the redeem seated (the confirmed email, or a device-rooted id) — persisted into
        /// `user.json` on promotion and disclosed on the receipt. `None` from an older plane.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<String>,
        /// When the redeem completed (epoch-millis), recorded into `user.json` on promotion.
        enrolled_at_millis: i64,
    },
}

/// The enrollment WAL document. `state` is the phase; `schema_version` rides at the top so the fail-closed
/// migration dispatch can probe it. The whole document is a `0600` secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingEnrollment {
    pub schema_version: u32,
    pub state: EnrollPhase,
}

// Redact the WAL's secrets (the device code in `Authorizing`/`AuthorizingStandup`, the claim token in
// `ClaimPending`, the read tokens in `Redeemed`) so the whole document — held transiently in memory — can
// never leak a secret through a Debug dump / panic / log.
impl std::fmt::Debug for PendingEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("PendingEnrollment");
        s.field("schema_version", &self.schema_version);
        match &self.state {
            EnrollPhase::Authorizing {
                context,
                user_code,
                interval,
                expires_at_millis,
                ..
            } => {
                s.field("phase", &"authorizing")
                    .field("workspace_id", &context.workspace_id)
                    .field("device_code", &"<redacted>")
                    .field("user_code", user_code)
                    .field("interval", interval)
                    .field("expires_at_millis", expires_at_millis);
            }
            EnrollPhase::AuthorizingStandup {
                base_url,
                user_code,
                expires_at_millis,
                ..
            } => {
                s.field("phase", &"authorizing_standup")
                    .field("base_url", base_url)
                    .field("device_code", &"<redacted>")
                    .field("user_code", user_code)
                    .field("expires_at_millis", expires_at_millis);
            }
            EnrollPhase::ClaimPending {
                base_url,
                workspace_id,
                ..
            } => {
                s.field("phase", &"claim_pending")
                    .field("base_url", base_url)
                    .field("workspace_id", workspace_id)
                    .field("claim_token", &"<redacted>");
            }
            EnrollPhase::Redeemed {
                context,
                read_creds,
                device_key_id,
                principal,
                enrolled_at_millis,
            } => {
                s.field("phase", &"redeemed")
                    .field("workspace_id", &context.workspace_id)
                    .field("device_key_id", device_key_id)
                    .field("principal", principal)
                    .field("read_creds", read_creds) // RedeemedCredDoc Debug redacts the token
                    .field("enrolled_at_millis", enrolled_at_millis);
            }
        }
        s.finish()
    }
}

/// `identity/user.json` — the enrolled principal's NON-secret metadata. No secret → ordinary perms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserDoc {
    pub schema_version: u32,
    pub workspace_id: String,
    pub deployment_mode: DeploymentMode,
    /// The confirmed email, if the wire ever carries one (the redeem response does not in v0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The principal the redeem seated this device as (a confirmed email, or a device-rooted id like
    /// `dev.dk_…`) — the disclosure the standup receipt prints ("workspace X — owner Y").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// Workspace roles, if the wire ever carries them (the redeem response does not in v0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Whether this membership was rooted in an `/i/` invite (always true for the device-flow follow).
    pub invite_rooted: bool,
    /// When enrollment completed, epoch-millis.
    pub enrolled_at: i64,
}

// -------------------------------------------------------------------------------------------------
// The enrollment writers. `instance.json` is PUBLIC (the plane key is a public key) → `write_doc`.
// `follows.json` carries the secret read tokens → `write_doc_private` (0600). `user.json` is metadata
// only → `write_doc`. The WAL is a secret → `write_doc_private`.
// -------------------------------------------------------------------------------------------------

/// Write `instance.json` (the pinned plane + the workspace disclosure). The plane key is PUBLIC, so
/// ordinary perms are fine. Idempotent — a re-promote rewrites identical bytes.
pub(crate) fn write_instance(
    fs: &dyn FsOps,
    layout: &Layout,
    instance: &Instance,
) -> Result<(), ClientError> {
    doc::write_doc(fs, &layout.instance_path(), instance)
}

/// READ-MERGE-WRITE `follows.json` under the `"identity"` lock: ADD/UPDATE each entry in `additions`
/// (dedupe by `skill_id` — a later entry replaces an earlier one), preserving every untouched entry, then
/// write the whole list `0600`. A second `follow` to another skill therefore never clobbers the first.
pub(crate) fn write_follows_merged(
    fs: &dyn FsOps,
    layout: &Layout,
    additions: &[FollowEntry],
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let mut merged = doc::read_doc_private::<Follows>(fs, &layout.follows_path())?
        .map(|f| f.follows)
        .unwrap_or_default();
    for add in additions {
        if let Some(existing) = merged.iter_mut().find(|e| e.skill_id == add.skill_id) {
            *existing = add.clone();
        } else {
            merged.push(add.clone());
        }
    }
    doc::write_doc_private(
        fs,
        &layout.follows_path(),
        &Follows {
            schema_version: PERSISTED_SCHEMA_VERSION,
            follows: merged,
        },
    )
}

/// Flip one skill's `following` flag IN PLACE, with the whole read-modify-write under the `"identity"`
/// lock — so a concurrent enrollment writer's freshly-minted row (token/mode) is never clobbered by a
/// stale pre-lock snapshot (the lost-update a read-then-merge shape would allow; `write_follows_merged`
/// is safe only for callers whose rows are freshly built, like the promote). A missing file or entry is
/// a clean no-op (already not-followed); an already-equal flag writes nothing.
pub(crate) fn set_following(
    fs: &dyn FsOps,
    layout: &Layout,
    skill_id: &str,
    following: bool,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let Some(mut follows) = doc::read_doc_private::<Follows>(fs, &layout.follows_path())? else {
        return Ok(());
    };
    let Some(entry) = follows.follows.iter_mut().find(|e| e.skill_id == skill_id) else {
        return Ok(());
    };
    if entry.following == following {
        return Ok(());
    }
    entry.following = following;
    doc::write_doc_private(
        fs,
        &layout.follows_path(),
        &Follows {
            schema_version: PERSISTED_SCHEMA_VERSION,
            follows: follows.follows,
        },
    )
}

/// Write `identity/user.json` (metadata only; ordinary perms). The identity dir must exist.
pub(crate) fn write_user(
    fs: &dyn FsOps,
    layout: &Layout,
    user: &UserDoc,
) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.identity_dir())?;
    doc::write_doc(fs, &layout.user_path(), user)
}

/// Write the enrollment WAL `0600` (a secret). The identity dir must exist.
pub(crate) fn write_wal(
    fs: &dyn FsOps,
    layout: &Layout,
    wal: &PendingEnrollment,
) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.identity_dir())?;
    doc::write_doc_private(fs, &layout.enrollment_path(), wal)
}

/// Read the enrollment WAL (a `0600` secret), or `None` if absent. Fail-closed on a permissive secret
/// AND on any persisted skill/workspace id outside the validated charset (the WAL is a durable copy of
/// wire data whose ids later key path joins — the same boundary rule as `follows.json`).
pub(crate) fn read_wal(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<PendingEnrollment>, ClientError> {
    let wal: Option<PendingEnrollment> = doc::read_doc_private(fs, &layout.enrollment_path())?;
    if let Some(w) = &wal {
        let context = match &w.state {
            EnrollPhase::Authorizing { context, .. } => context,
            EnrollPhase::Redeemed {
                context,
                read_creds,
                ..
            } => {
                for cred in read_creds {
                    crate::id::SkillId::parse(&cred.skill_id)?;
                }
                context
            }
            // A standup session has NO workspace yet — there is no id to validate (the granted poll's
            // workspace id is validated at the wire boundary when it arrives).
            EnrollPhase::AuthorizingStandup { .. } => return Ok(wal),
            // A claim WAL carries the workspace-to-be's id (it later keys user.json + URL splices).
            EnrollPhase::ClaimPending { workspace_id, .. } => {
                crate::id::validate_workspace_id(workspace_id)?;
                return Ok(wal);
            }
        };
        crate::id::validate_workspace_id(&context.workspace_id)?;
        for s in &context.offered_skills {
            crate::id::SkillId::parse(&s.skill_id)?;
        }
    }
    Ok(wal)
}

/// Delete the enrollment WAL (on a completed promotion, or a swept abandon). NotFound-tolerant.
pub(crate) fn delete_wal(fs: &dyn FsOps, layout: &Layout) -> Result<(), ClientError> {
    fs.remove_file(&layout.enrollment_path())?;
    Ok(())
}

/// The recovery sweep for the enrollment WAL: remove an `Authorizing` / `AuthorizingStandup` WAL whose
/// session has expired (`now_millis > expires_at_millis`) and never reached `Redeemed` — a clean abandon.
/// A `Redeemed` WAL is **always preserved** (a re-resume promotes it), a `ClaimPending` WAL is preserved
/// (its retry is always safe — the server's same-device replay of a consumed claim re-answers Redeemed,
/// and the claim's expiry is enforced server-side), and an unexpired authorizing WAL of either kind is
/// preserved (a resume can still poll it). Best-effort: an unreadable/corrupt WAL is left in place for
/// the owning op to diagnose, never hard-failing recovery.
pub(crate) fn sweep_expired_wal(
    fs: &dyn FsOps,
    layout: &Layout,
    now_millis: i64,
) -> Result<(), ClientError> {
    let wal = match read_wal(fs, layout) {
        Ok(Some(wal)) => wal,
        // Absent → nothing to sweep. Unreadable/permissive/corrupt → leave it for the op to surface.
        Ok(None) | Err(_) => return Ok(()),
    };
    let expired = match &wal.state {
        EnrollPhase::Authorizing {
            expires_at_millis, ..
        }
        | EnrollPhase::AuthorizingStandup {
            expires_at_millis, ..
        } => now_millis > *expires_at_millis,
        EnrollPhase::ClaimPending { .. } | EnrollPhase::Redeemed { .. } => false,
    };
    if expired {
        delete_wal(fs, layout)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic::load_versioned;
    use topos_types::PERSISTED_SCHEMA_VERSION;

    fn sample_instance() -> Instance {
        Instance {
            schema_version: 1,
            base_url: "https://topos.sh".to_owned(),
            plane_key: "a".repeat(64),
            plane_key_id: "pk_demo".to_owned(),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "device_code".to_owned(),
            workspace_display_name: Some("Acme".to_owned()),
            verified_domain: Some("acme.com".to_owned()),
            verified_domain_status: VerifiedDomainStatus::Verified,
        }
    }

    fn sample_follows() -> Follows {
        Follows {
            schema_version: 1,
            follows: vec![
                FollowEntry {
                    skill_id: "s_deploy".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    read_token: "rt_secret".to_owned(),
                    mode: FollowModeDoc::Auto,
                    review_required: false,
                    following: true,
                },
                FollowEntry {
                    skill_id: "s_paused".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    read_token: "rt_other".to_owned(),
                    mode: FollowModeDoc::ConfirmEach,
                    review_required: true,
                    following: false,
                },
            ],
        }
    }

    #[test]
    fn instance_round_trips() {
        let i = sample_instance();
        let mut bytes = serde_json::to_vec(&i).unwrap();
        bytes.push(b'\n');
        let back: Instance = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(i, back);
    }

    #[test]
    fn follows_and_entry_round_trip_snake_case_mode() {
        let f = sample_follows();
        let v = serde_json::to_value(&f).unwrap();
        // The mode renders snake_case on the wire.
        assert_eq!(v["follows"][0]["mode"], "auto");
        assert_eq!(v["follows"][1]["mode"], "confirm_each");
        let back: Follows = serde_json::from_value(v).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn debug_redacts_the_read_token() {
        let e = &sample_follows().follows[0];
        let dbg = format!("{e:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(
            !dbg.contains("rt_secret"),
            "the secret must never appear in Debug"
        );
    }

    #[test]
    fn fail_closed_on_newer_or_legacy_schema_version() {
        // A NEWER schema_version is never handed to serde — an upgrade error, fail closed.
        let newer = br#"{"schema_version":2,"base_url":"x","plane_key":"a","plane_key_id":"k"}"#;
        assert!(matches!(
            load_versioned::<Instance>(newer, PERSISTED_SCHEMA_VERSION),
            Err(ClientError::UnknownSchemaVersion { found: 2, .. })
        ));
        // A v0 doc is below the floor.
        let legacy = br#"{"schema_version":0,"follows":[]}"#;
        assert!(matches!(
            load_versioned::<Follows>(legacy, PERSISTED_SCHEMA_VERSION),
            Err(ClientError::UnsupportedLegacy { found: 0 })
        ));
        // A current-version doc parses.
        let ok = br#"{"schema_version":1,"follows":[]}"#;
        assert!(load_versioned::<Follows>(ok, PERSISTED_SCHEMA_VERSION).is_ok());
    }

    #[test]
    fn fan_outs_carry_all_entries_and_map_mode() {
        let f = sample_follows();
        let ctxs = follow_contexts(&f);
        assert_eq!(ctxs.len(), 2);
        assert_eq!(ctxs[0].0, "s_deploy");
        assert_eq!(ctxs[0].1.mode, FollowMode::Auto);
        assert!(ctxs[0].1.following);
        assert_eq!(ctxs[1].1.mode, FollowMode::ConfirmEach);
        assert!(!ctxs[1].1.following);

        let creds = skill_creds(&f);
        assert_eq!(creds.len(), 2);
        assert_eq!(creds["s_deploy"].workspace_id, "w_acme");
        assert_eq!(creds["s_deploy"].read_token, "rt_secret");
    }

    /// A throwaway home for the load-boundary tests (mirrors the doc-module scratch pattern).
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-enr-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_follows_refuses_a_traversal_id_on_load() {
        // The persisted boundary: a follows.json naming a traversal skill/workspace id (hand-edited, or
        // written by a compromised prior run) must fail the LOAD closed — the id would otherwise reach
        // `~/.topos/skills/<id>` joins and request URL paths.
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("hostile"));
        for (skill_id, workspace_id) in [
            ("../../x", "w_acme"),
            ("a/b", "w_acme"),
            ("A", "w_acme"),
            ("", "w_acme"),
            (".", "w_acme"),
            ("..", "w_acme"),
            ("s_deploy", "../../w"),
        ] {
            let mut f = sample_follows();
            f.follows[0].skill_id = skill_id.to_owned();
            f.follows[0].workspace_id = workspace_id.to_owned();
            doc::write_doc_private(&fs, &layout.follows_path(), &f).unwrap();
            let err = read_follows(&fs, &layout).unwrap_err();
            assert!(
                matches!(err, ClientError::Corrupt(_)),
                "({skill_id:?}, {workspace_id:?}) must fail the load as Corrupt, got {err:?}"
            );
        }
        // A clean document still loads.
        doc::write_doc_private(&fs, &layout.follows_path(), &sample_follows()).unwrap();
        assert!(read_follows(&fs, &layout).unwrap().is_some());
    }

    #[test]
    fn read_wal_refuses_a_traversal_id_on_load() {
        // The WAL is a durable copy of wire data — same boundary rule as follows.json.
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("hostile-wal"));
        std::fs::create_dir_all(layout.identity_dir()).unwrap();
        let wal = PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: EnrollPhase::Redeemed {
                context: EnrollContext {
                    base_url: "https://acme.topos.test".to_owned(),
                    pinned_plane_key: "a".repeat(64),
                    plane_key_id: "pk".to_owned(),
                    deployment_mode: DeploymentMode::SelfHost,
                    enrollment_method: "device_code".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    workspace_display_name: "Acme".to_owned(),
                    verified_domain: None,
                    verified_domain_status: VerifiedDomainStatus::Unverified,
                    offered_skills: vec![OfferedSkill {
                        skill_id: "../../x".to_owned(),
                        name: None,
                    }],
                    mode: FollowModeDoc::Auto,
                    root: EnrollRoot::Invite,
                },
                read_creds: vec![RedeemedCredDoc {
                    skill_id: "../../x".to_owned(),
                    read_token: "rt".to_owned(),
                    expires_at: None,
                }],
                device_key_id: "dk_abc".to_owned(),
                principal: None,
                enrolled_at_millis: 1,
            },
        };
        write_wal(&fs, &layout, &wal).unwrap();
        let err = read_wal(&fs, &layout).unwrap_err();
        assert!(matches!(err, ClientError::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn standup_and_claim_wal_phases_round_trip_and_redact_their_secrets() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("standup-wal"));
        std::fs::create_dir_all(layout.identity_dir()).unwrap();

        // AuthorizingStandup: round-trips through the 0600 WAL, Debug redacts the device code.
        let standup = PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: EnrollPhase::AuthorizingStandup {
                base_url: "https://api.topos.sh".to_owned(),
                pinned_plane_key: "a".repeat(64),
                plane_key_id: "pk_hosted".to_owned(),
                deployment_mode: DeploymentMode::Cloud,
                enrollment_method: "device_code".to_owned(),
                device_code: "dc_standup_secret".to_owned(),
                user_code: "ABCDEFGH23456789".to_owned(),
                verification_uri_complete: "https://topos.sh/verify/ABCDEFGH23456789".to_owned(),
                expires_at_millis: 2_000,
            },
        };
        write_wal(&fs, &layout, &standup).unwrap();
        let back = read_wal(&fs, &layout).unwrap().expect("standup WAL loads");
        assert_eq!(back, standup);
        let dbg = format!("{back:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(
            !dbg.contains("dc_standup_secret"),
            "the device code must never appear in Debug: {dbg}"
        );

        // ClaimPending: round-trips, Debug redacts the claim token, and a traversal workspace id in a
        // (hand-edited) claim WAL fails the load closed.
        let claim = PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: EnrollPhase::ClaimPending {
                base_url: "https://plane.acme.test".to_owned(),
                pinned_plane_key: "b".repeat(64),
                plane_key_id: "pk_selfhost".to_owned(),
                deployment_mode: DeploymentMode::SelfHost,
                enrollment_method: "admin_claim".to_owned(),
                workspace_id: "w_acme".to_owned(),
                workspace_display_name: "Acme".to_owned(),
                claim_token: "claim_bearer_secret".to_owned(),
            },
        };
        write_wal(&fs, &layout, &claim).unwrap();
        let back = read_wal(&fs, &layout).unwrap().expect("claim WAL loads");
        assert_eq!(back, claim);
        let dbg = format!("{back:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(
            !dbg.contains("claim_bearer_secret"),
            "the claim token must never appear in Debug: {dbg}"
        );
        let hostile = PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: EnrollPhase::ClaimPending {
                base_url: "https://plane.acme.test".to_owned(),
                pinned_plane_key: "b".repeat(64),
                plane_key_id: "pk_selfhost".to_owned(),
                deployment_mode: DeploymentMode::SelfHost,
                enrollment_method: "admin_claim".to_owned(),
                workspace_id: "../../w".to_owned(),
                workspace_display_name: "Acme".to_owned(),
                claim_token: "claim_bearer_secret".to_owned(),
            },
        };
        write_wal(&fs, &layout, &hostile).unwrap();
        let err = read_wal(&fs, &layout).unwrap_err();
        assert!(matches!(err, ClientError::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn sweep_reaps_an_expired_standup_wal_and_keeps_a_claim_wal() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("standup-sweep"));
        std::fs::create_dir_all(layout.identity_dir()).unwrap();
        let standup = |expires_at_millis: i64| PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: EnrollPhase::AuthorizingStandup {
                base_url: "https://api.topos.sh".to_owned(),
                pinned_plane_key: "a".repeat(64),
                plane_key_id: "pk".to_owned(),
                deployment_mode: DeploymentMode::Cloud,
                enrollment_method: "device_code".to_owned(),
                device_code: "dc".to_owned(),
                user_code: "CODE".to_owned(),
                verification_uri_complete: "https://topos.sh/verify/CODE".to_owned(),
                expires_at_millis,
            },
        };
        // Unexpired: preserved (a re-invoked publish can still poll it).
        write_wal(&fs, &layout, &standup(10_000)).unwrap();
        sweep_expired_wal(&fs, &layout, 5_000).unwrap();
        assert!(read_wal(&fs, &layout).unwrap().is_some());
        // Expired: reaped, exactly like an expired invite Authorizing WAL.
        sweep_expired_wal(&fs, &layout, 20_000).unwrap();
        assert!(read_wal(&fs, &layout).unwrap().is_none());
        // A ClaimPending WAL has no client-side expiry — the sweep always preserves it (the retry is
        // idempotent server-side, and the claim's own expiry is enforced at redeem).
        let claim = PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: EnrollPhase::ClaimPending {
                base_url: "https://plane.acme.test".to_owned(),
                pinned_plane_key: "b".repeat(64),
                plane_key_id: "pk".to_owned(),
                deployment_mode: DeploymentMode::SelfHost,
                enrollment_method: "admin_claim".to_owned(),
                workspace_id: "w_acme".to_owned(),
                workspace_display_name: "Acme".to_owned(),
                claim_token: "tok".to_owned(),
            },
        };
        write_wal(&fs, &layout, &claim).unwrap();
        sweep_expired_wal(&fs, &layout, i64::MAX).unwrap();
        assert!(read_wal(&fs, &layout).unwrap().is_some());
    }
}
