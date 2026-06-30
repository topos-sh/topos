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

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::plane::{FollowContext, FollowMode};
use crate::plane_http::SkillCred;
use crate::sidecar::Layout;

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

/// Read `follows.json`, or `None` if absent. Fail-closed on an unknown/newer `schema_version`.
pub(crate) fn read_follows(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<Follows>, ClientError> {
    doc::read_doc(fs, &layout.follows_path())
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
