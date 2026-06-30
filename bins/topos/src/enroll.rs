//! The on-disk enrollment state the plane transport reads: `instance.json` (which plane + the pinned
//! plane key) and `follows.json` (which skills are followed, in which mode/workspace, with which read
//! credential).
//!
//! **These are client-only transport/enrollment documents — they are deliberately NOT in
//! `topos-types::persisted`.** That crate is the cross-language wire/contract leaf whose shapes are
//! schema-generated into `contracts/`; these two documents are local sidecar state owned by the (future)
//! enrollment subsystem, exactly like `identity/host.json` ([`crate::identity`]). They follow the same
//! idiom — a `schema_version` field read through [`crate::doc::read_doc`], which dispatches the **fail-closed
//! migration** (an unknown/newer `schema_version` is an upgrade error, never silently parsed or deleted) —
//! but they own their own shape rather than freezing it in the public contract on a guess. `follows.json`
//! additionally carries a **secret** (`read_token`), which is another reason it stays out of the public
//! contract.
//!
//! **`read_token` is a `0600` secret.** This increment only READS `follows.json` (production is inert — no
//! enrollment writer exists yet; the tests inject state directly). When the enrollment writer lands it MUST
//! write `follows.json` with `0600` permissions, because `read_token` grants read access to a workspace's
//! skills. Do not add a production writer here; if any future write path is added it must apply `0600`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use topos_types::SCHEMA_VERSION;
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
/// Fail-closed on an unknown/newer `schema_version`.
pub(crate) fn read_follows(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<Follows>, ClientError> {
    doc::read_doc_private(fs, &layout.follows_path())
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
        /// The minimum poll interval, in seconds.
        interval: u64,
        /// The session expiry as epoch-millis — the recovery sweep abandons a WAL past this that never
        /// reached `Redeemed`.
        expires_at_millis: i64,
    },
    /// The grant was redeemed and the read creds minted — recorded BEFORE promotion (the lockout fence: a
    /// single-use grant cannot be re-redeemed, so a crash after redeem completes from here).
    Redeemed {
        context: EnrollContext,
        /// **SECRET** — the minted per-skill read tokens. Redacted in `Debug`.
        read_creds: Vec<RedeemedCredDoc>,
        device_key_id: String,
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

// Redact the WAL's secrets (the device code in `Authorizing`, the read tokens in `Redeemed`) so the whole
// document — held transiently in memory — can never leak a secret through a Debug dump / panic / log.
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
            EnrollPhase::Redeemed {
                context,
                read_creds,
                device_key_id,
                enrolled_at_millis,
            } => {
                s.field("phase", &"redeemed")
                    .field("workspace_id", &context.workspace_id)
                    .field("device_key_id", device_key_id)
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
    let _guard = fs.lock_exclusive(&layout.lock_file("identity"))?;
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
            schema_version: SCHEMA_VERSION,
            follows: merged,
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

/// Read the enrollment WAL (a `0600` secret), or `None` if absent. Fail-closed on a permissive secret.
pub(crate) fn read_wal(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<PendingEnrollment>, ClientError> {
    doc::read_doc_private(fs, &layout.enrollment_path())
}

/// Delete the enrollment WAL (on a completed promotion, or a swept abandon). NotFound-tolerant.
pub(crate) fn delete_wal(fs: &dyn FsOps, layout: &Layout) -> Result<(), ClientError> {
    fs.remove_file(&layout.enrollment_path())?;
    Ok(())
}

/// The recovery sweep for the enrollment WAL: remove an `Authorizing` WAL whose session has expired
/// (`now_millis > expires_at_millis`) and never reached `Redeemed` — a clean abandon. A `Redeemed` WAL is
/// **always preserved** (a re-`--resume` promotes it), and an unexpired `Authorizing` WAL is preserved (a
/// `--resume` can still poll it). Best-effort: an unreadable/corrupt WAL is left in place for the follow
/// op to diagnose, never hard-failing recovery.
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
    if let EnrollPhase::Authorizing {
        expires_at_millis, ..
    } = &wal.state
        && now_millis > *expires_at_millis
    {
        delete_wal(fs, layout)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic::load_versioned;
    use topos_types::SCHEMA_VERSION;

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
            load_versioned::<Instance>(newer, SCHEMA_VERSION),
            Err(ClientError::UnknownSchemaVersion { found: 2, .. })
        ));
        // A v0 doc is below the floor.
        let legacy = br#"{"schema_version":0,"follows":[]}"#;
        assert!(matches!(
            load_versioned::<Follows>(legacy, SCHEMA_VERSION),
            Err(ClientError::UnsupportedLegacy { found: 0 })
        ));
        // A current-version doc parses.
        let ok = br#"{"schema_version":1,"follows":[]}"#;
        assert!(load_versioned::<Follows>(ok, SCHEMA_VERSION).is_ok());
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
}
