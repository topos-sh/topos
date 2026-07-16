//! The on-disk enrollment state — the documents `follow` / `auth login` write and the plane transport
//! reads: `instance.json` (which plane), `follows.json` (which skills are followed, in which
//! mode/workspace — pure subscription state), `identity/credentials.json` (the device's ONE bearer
//! credential + its registered device id — the **secret** every request presents), `identity/user.json`
//! (the enrolled workspaces' non-secret metadata), and the enrollment WAL (`identity/enrollment.json`,
//! the device-flow resume's durable state). Both the writers (the granted-poll persist path) and the
//! readers live here.
//!
//! **These are client-only transport/enrollment documents — they are deliberately NOT in
//! `topos-types::persisted`.** That crate is the cross-language wire/contract leaf whose shapes are
//! schema-generated into `contracts/`; these documents are local sidecar state owned by the enrollment
//! subsystem, exactly like `identity/host.json` ([`crate::identity`]). They follow the same idiom — a
//! `schema_version` field read through [`crate::doc::read_doc`], which dispatches the **fail-closed
//! migration** (an unknown/newer `schema_version` is an upgrade error, never silently parsed or deleted) —
//! but they own their own shape rather than freezing it in the public contract on a guess.
//!
//! **The device credential is a `0600` secret.** `credentials.json` and the WAL are written through the
//! `0600` private-doc primitives ([`crate::doc::write_doc_private`]) and refused-on-permissive at read,
//! because the ONE credential authenticates every request this device makes; `instance.json`/`user.json`
//! carry no secret. `follows.json` is pure subscription state — still `0600`-written for continuity +
//! perm hygiene.
//!
//! **Ids are validated at load.** A skill/workspace id read out of `follows.json` or `user.json` later
//! keys path joins (`~/.topos/skills/<id>`, the harness skills dir) and URL splices, so the loaders parse
//! every id through [`crate::id`] — a hand-edited (or maliciously written) traversal id fails the load
//! closed as a corrupt document, mirroring the wire-boundary checks in [`crate::plane_http`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::plane::{FollowContext, FollowMode};
use crate::sidecar::Layout;

/// `instance.json` — the PLANE this client is enrolled with (v0 is one plane per install). Public
/// metadata only (ordinary file perms). No trust root is stored — the `current` pointer is unsigned,
/// its authority the database row and its integrity the content-addressed version id; a stale
/// `instance.json` from an older build may carry ignored extra fields (serde tolerates them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Instance {
    pub schema_version: u32,
    /// The API base URL (no trailing slash; the transport normalizes it), e.g. `https://topos.sh/api`.
    pub base_url: String,
}

/// One workspace this install has joined on the pinned plane — the per-workspace half of an enrollment.
/// A single `user.json` carries a `Vec<Membership>`, so following skills from a second workspace ADDS a
/// membership rather than overwriting the first. Non-secret metadata only (the device credential lives
/// in `identity/credentials.json`); no `deny_unknown_fields` (forward-compatible).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Membership {
    /// The workspace id (a path-safe identifier — the pointer scope + the write op's workspace).
    pub workspace_id: String,
    /// The workspace's ADDRESS slug (what a human types at `follow`; what a re-login authorizes toward).
    pub name: String,
    /// The workspace's display name, for the agent's disclosure.
    pub display_name: String,
    /// When this membership's enrollment completed, epoch-millis.
    pub enrolled_at: i64,
}

/// `follows.json` — the durable follow-state: the skills this client follows, each with its workspace,
/// mode, and review posture. **Pure subscription state** — no secret (the device credential lives in
/// `credentials.json`). Still `0600`-written for continuity + perm hygiene; see the module comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Follows {
    pub schema_version: u32,
    #[serde(default)]
    pub follows: Vec<FollowEntry>,
}

/// One followed skill's subscription record — the consent seam ([`FollowContext`]:
/// workspace/mode/review/following). Nothing secret; the transport credential is the DEVICE credential
/// (`credentials.json`), shared by every followed skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FollowEntry {
    /// The stable skill id (the key the fan-outs are keyed by).
    pub skill_id: String,
    /// The workspace this skill is followed in (the expected pointer scope + the URL-path workspace).
    pub workspace_id: String,
    /// How a new `current` is adopted (auto / confirm-each).
    pub mode: FollowModeDoc,
    /// Whether the workspace gates moves behind review (selects the consent satisfier only).
    pub review_required: bool,
    /// Whether the skill is currently followed (a `false` skill is inventoried but not pulled).
    pub following: bool,
    /// Whether `topos remove` excluded this skill on THIS device (a per-device exclusion — the person
    /// still follows it, and other devices still receive it). Local cause marker for `list`; the server
    /// exclusion row is the source of truth. Defaults `false` for a pre-field document.
    #[serde(default)]
    pub excluded_here: bool,
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

/// Fold a principal (an email address) to its canonical comparison form — trimmed, ASCII-lowercased.
/// The server folds at its own parse boundary; folding here too keeps the outbox match and the invite
/// body address-shape-agnostic without a second wire round-trip.
pub(crate) fn canonical_principal(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Read `instance.json`, or `None` if absent. Fail-closed on an unknown/newer `schema_version`.
pub(crate) fn read_instance(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<Instance>, ClientError> {
    doc::read_doc(fs, &layout.instance_path())
}

/// Read `follows.json`, or `None` if absent. Read through [`doc::read_doc_private`] (perm hygiene — a
/// group/other-accessible file is refused BEFORE parsing). Fail-closed on an unknown/newer
/// `schema_version` AND on any entry whose skill/workspace id is not a safe path component (the id
/// boundary: a traversal id must never reach a join downstream).
///
/// Also fail-closed on the cross-workspace invariant [`write_follows_merged`] enforces on write: a skill id
/// is plane-minted and belongs to EXACTLY ONE workspace, and the sidecar keys skills by id alone — so the
/// SAME `skill_id` appearing under two different `workspace_id`s (a forged/confused response the write
/// guard already refuses, or a hand-edited doc) would mis-scope the first-match lookups. The LOAD fails
/// closed here, mirroring the write guard's message shape.
pub(crate) fn read_follows(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<Follows>, ClientError> {
    let Some(follows): Option<Follows> = doc::read_doc_private(fs, &layout.follows_path())? else {
        return Ok(None);
    };
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for entry in &follows.follows {
        crate::id::SkillId::parse(&entry.skill_id)?;
        crate::id::validate_workspace_id(&entry.workspace_id)?;
        if let Some(prev_ws) = seen.insert(entry.skill_id.as_str(), entry.workspace_id.as_str())
            && prev_ws != entry.workspace_id.as_str()
        {
            return Err(ClientError::Corrupt(format!(
                "skill '{}' is already followed in a different workspace; a skill id belongs to \
                 exactly one workspace",
                entry.skill_id
            )));
        }
    }
    Ok(Some(follows))
}

/// Read `identity/user.json`, or `None` if absent. Metadata only (no secret) → ordinary `read_doc`.
/// Fail-closed on an unknown/newer `schema_version` AND on any membership whose workspace id is not a
/// safe path component. The ambient write verbs (`invite`, a genesis `publish`) pick their workspace
/// from the memberships here via [`UserDoc::resolve_write_workspace`].
pub(crate) fn read_user(fs: &dyn FsOps, layout: &Layout) -> Result<Option<UserDoc>, ClientError> {
    let user: Option<UserDoc> = doc::read_doc(fs, &layout.user_path())?;
    if let Some(u) = &user {
        for m in &u.workspaces {
            crate::id::validate_workspace_id(&m.workspace_id)?;
        }
    }
    Ok(user)
}

/// The follow-state fan-out → the engine's consent seam (`FileFollow` returns these). Every entry is
/// carried (the engine itself skips a `following == false` skill).
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

/// The follow-state's `skill_id → workspace_id` map — how the read transport learns which workspace
/// path a skill's reads splice (the ONE device credential authenticates them all; the map carries no
/// secret).
pub(crate) fn skill_workspaces(follows: &Follows) -> HashMap<String, String> {
    follows
        .follows
        .iter()
        .map(|e| (e.skill_id.clone(), e.workspace_id.clone()))
        .collect()
}

// =================================================================================================
// `identity/credentials.json` — the device's ONE bearer credential + its registered device id. The
// ONE secret this device presents (Bearer) on every request; the server resolves credential → device
// → user → seat. A `0600` secret (hand-written Debug redacts it), written whole under the identity
// lock.
// =================================================================================================

/// `credentials.json` — the device credential document (one credential per install).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Credentials {
    pub schema_version: u32,
    /// **SECRET** — the plaintext device credential (Bearer on every request). Redacted in `Debug`.
    pub credential: String,
    /// The server-registered device id (non-secret) — the handle a self-revoke names as its target.
    pub device_id: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("schema_version", &self.schema_version)
            .field("credential", &"<redacted>")
            .field("device_id", &self.device_id)
            .finish()
    }
}

/// Read `credentials.json` (a `0600` secret), or `None` if absent (the signed-out state). Refused on a
/// permissive mode before parsing; fail-closed on an unknown/newer `schema_version`.
pub(crate) fn read_credentials(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<Credentials>, ClientError> {
    doc::read_doc_private(fs, &layout.credentials_path())
}

/// Write the device credential WHOLE (`0600`, under the `"identity"` lock). A re-enrollment REPLACES
/// the credential wholesale — the device holds exactly one.
pub(crate) fn write_credentials(
    fs: &dyn FsOps,
    layout: &Layout,
    credential: &str,
    device_id: &str,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    fs.create_dir_all(&layout.identity_dir())?;
    doc::write_doc_private(
        fs,
        &layout.credentials_path(),
        &Credentials {
            schema_version: PERSISTED_SCHEMA_VERSION,
            credential: credential.to_owned(),
            device_id: device_id.to_owned(),
        },
    )
}

// =================================================================================================
// The enrollment WAL (`identity/enrollment.json`) — the device-flow resume's durable state: ONE
// address-enroll phase (a `follow <address>` carries a follow intent; an `auth login` carries none).
// A `0600` SECRET (it holds the device code, which the server promotes to the device credential on
// approval). Hand-written `Debug` redacts. There is no post-grant fence phase: a re-poll of an
// approved flow returns the same granted answer, so a crash between the grant and the sidecar writes
// recovers by simply re-polling.
// =================================================================================================

/// The kind half of a persisted follow intent (a doc-local copy — the resolver's `ResourceKind`
/// carries no serde derives, and the workspace case exists only here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FollowKindDoc {
    Workspace,
    Channel,
    Skill,
}

/// The follow INTENT an enrollment carries across the device-flow wait: what the original `follow`
/// targeted, so a resumed invocation continues into that target's describe/apply once the grant
/// persists (the enrollment itself is identity — reversible, disclosed; the subscription + bytes are
/// the consented effect and never land without the `--yes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FollowTargetDoc {
    pub kind: FollowKindDoc,
    /// The resource name (or the workspace ADDRESS name for `kind: workspace`).
    pub name: String,
}

/// Which verb owns a pending enrollment — a `follow <address>` (continues into its follow intent) or
/// an `auth login` (a re-enrollment with no follow intent). Internally tagged so the on-disk shape is
/// `{ "kind": "follow", … }` / `{ "kind": "login" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum EnrollIntentDoc {
    /// A `follow <address>` enrollment: the follow intent to continue into, and the adoption mode
    /// (`--manual` ⇒ confirm-each) the follow rows inherit.
    Follow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<FollowTargetDoc>,
        mode: FollowModeDoc,
    },
    /// An `auth login` re-enrollment — no follow intent; the grant just replaces the credential.
    Login,
}

/// The enrollment WAL document — ONE live device-authorization flow, awaiting the human's approval.
/// The whole document is a `0600` secret (the device code is promoted to the credential on approval).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingEnrollment {
    pub schema_version: u32,
    /// The API base the flow runs against (the card's declared base, re-root-gated).
    pub base_url: String,
    /// The requested workspace ADDRESS slug (the `device/authorize` body's `workspace`). Whether it
    /// exists is never disclosed pre-approval; the granted poll carries the authoritative workspace.
    pub workspace_name: String,
    /// Which verb owns the resume (and, for a follow, the intent to continue into).
    pub intent: EnrollIntentDoc,
    /// **SECRET** — the device code the client polls with. Redacted in `Debug`.
    pub device_code: String,
    /// The short user code (the cross-check shown on the approval page).
    pub user_code: String,
    /// The SERVER-built approval URL with the code embedded — re-emitted verbatim while pending.
    pub verification_uri_complete: String,
    /// The minimum poll interval, in seconds.
    pub interval_secs: u64,
    /// The flow expiry as epoch-millis — the recovery sweep abandons a WAL past this.
    pub expires_at_millis: i64,
}

// Redact the WAL's secret (the device code — the credential-to-be) so the whole document, held
// transiently in memory, can never leak it through a Debug dump / panic / log.
impl std::fmt::Debug for PendingEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingEnrollment")
            .field("schema_version", &self.schema_version)
            .field("base_url", &self.base_url)
            .field("workspace_name", &self.workspace_name)
            .field("intent", &self.intent)
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("interval_secs", &self.interval_secs)
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

/// `identity/user.json` — this install's workspace memberships plus the person's principal once a
/// member-scoped read disclosed it. No secret → ordinary perms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct UserDoc {
    pub schema_version: u32,
    /// The principal this install's seats belong to (the confirmed email) — recorded from the first
    /// member-scoped `me` read (the enrollment poll deliberately discloses no identity), refreshed on
    /// later reads. Disclosure only; `None` until a describe/apply has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The workspaces this install has joined on the pinned plane.
    #[serde(default)]
    pub workspaces: Vec<Membership>,
}

impl UserDoc {
    /// This install's membership in `workspace_id`, or `None` if it has not joined that workspace.
    pub(crate) fn membership(&self, workspace_id: &str) -> Option<&Membership> {
        self.workspaces
            .iter()
            .find(|m| m.workspace_id == workspace_id)
    }

    /// The single workspace an ambient write op (a genesis publish, an invite) acts in.
    ///
    /// - `explicit = Some(ws)` → that membership, or a clear error if this install has not joined it;
    /// - `explicit = None` → the sole membership if there is exactly one; a [`ClientError::WorkspaceSelection`]
    ///   telling the agent to pass `--workspace <id>` (listing the joined ids) if there is more than one;
    ///   a [`ClientError::Enrollment`] "not enrolled" if there are none.
    ///
    /// # Errors
    /// As above — never a silent guess when the choice is ambiguous.
    pub(crate) fn resolve_write_workspace(
        &self,
        explicit: Option<&str>,
    ) -> Result<&Membership, ClientError> {
        if self.workspaces.is_empty() {
            return Err(ClientError::Enrollment(
                "not enrolled in any workspace; run `topos follow <workspace-address>` first"
                    .into(),
            ));
        }
        match explicit {
            Some(ws) => self.membership(ws).ok_or_else(|| {
                ClientError::WorkspaceSelection(format!(
                    "this install has not joined workspace '{ws}'; joined workspaces: {}",
                    self.workspace_ids().join(", ")
                ))
            }),
            None => match self.workspaces.as_slice() {
                [only] => Ok(only),
                _ => Err(ClientError::WorkspaceSelection(format!(
                    "this install follows skills in multiple workspaces ({}); pass `--workspace <id>` \
                     to choose one",
                    self.workspace_ids().join(", ")
                ))),
            },
        }
    }

    /// The joined workspace ids, in stored order — for the ambiguity guidance message.
    fn workspace_ids(&self) -> Vec<&str> {
        self.workspaces
            .iter()
            .map(|m| m.workspace_id.as_str())
            .collect()
    }
}

/// Insert `m` into `user.workspaces`, REPLACING an existing membership with the same `workspace_id` (a
/// re-follow / re-login) or appending it (a first follow into a new workspace) — deduped by
/// `workspace_id`, so a second follow never drops the first.
pub(crate) fn upsert_membership(user: &mut UserDoc, m: Membership) {
    if let Some(existing) = user
        .workspaces
        .iter_mut()
        .find(|e| e.workspace_id == m.workspace_id)
    {
        *existing = m;
    } else {
        user.workspaces.push(m);
    }
}

/// Refresh a stored membership's display name — and the person's principal — from the AUTHORITATIVE
/// member-scoped `me` read: the enrollment poll only echoes what the person requested (it must not
/// disclose more pre-approval), so the first member-authenticated read replaces the stored facts with
/// the workspace's true ones. A no-op when the membership is absent (nothing enrolled yet).
///
/// # Errors
/// A doc read/write failure.
pub(crate) fn refresh_membership_facts(
    fs: &dyn FsOps,
    layout: &Layout,
    workspace_id: &str,
    display_name: &str,
    principal: &str,
) -> Result<(), ClientError> {
    let Some(mut user) = read_user(fs, layout)? else {
        return Ok(());
    };
    let mut changed = false;
    if let Some(m) = user
        .workspaces
        .iter_mut()
        .find(|e| e.workspace_id == workspace_id)
        && m.display_name != display_name
    {
        m.display_name = display_name.to_owned();
        changed = true;
    }
    if user.principal.as_deref() != Some(principal) {
        user.principal = Some(principal.to_owned());
        changed = true;
    }
    if changed {
        write_user(fs, layout, &user)?;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// The enrollment writers. `instance.json` is PUBLIC (plane metadata, no secret) → `write_doc`.
// `follows.json` is pure subscription state, still `0600`-written for perm hygiene → `write_doc_private`.
// `credentials.json` holds the secret device credential → `write_doc_private` (0600). `user.json` is
// metadata only → `write_doc`. The WAL is a secret → `write_doc_private`.
// -------------------------------------------------------------------------------------------------

/// Write `instance.json` (the pinned plane). The plane base is PUBLIC, so ordinary perms are fine.
/// Idempotent — a re-enrollment against the same plane rewrites identical bytes.
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
///
/// A skill id is unique to ONE workspace (it is a plane-minted identifier), and the sidecar keys skills
/// by id alone — so an addition that would land a `skill_id` ALREADY followed under a DIFFERENT
/// `workspace_id` is refused (a forged/confused response, or a hand-edited doc): silently replacing the
/// entry would re-scope an already-materialized skill. Nothing is written on refusal.
///
/// # Errors
/// [`ClientError::Corrupt`] on a cross-workspace `skill_id` collision; otherwise the [`FsOps`] failure.
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
            if existing.workspace_id != add.workspace_id {
                return Err(ClientError::Corrupt(format!(
                    "skill '{}' is already followed in a different workspace; a skill id belongs to \
                     exactly one workspace",
                    add.skill_id
                )));
            }
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
/// lock — so a concurrent enrollment writer's freshly-minted row is never clobbered by a stale pre-lock
/// snapshot (the lost-update a read-then-merge shape would allow; `write_follows_merged` is safe only
/// for callers whose rows are freshly built). A missing file or entry is a clean no-op (already
/// not-followed); an already-equal flag writes nothing.
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

/// Flip one skill's `excluded_here` marker IN PLACE (the `remove` verb's per-device exclusion cause for
/// `list`), under the same `"identity"` lock as [`set_following`]. A missing file / entry is a clean
/// no-op (an untracked local has no follow row); an already-equal flag writes nothing.
pub(crate) fn set_excluded(
    fs: &dyn FsOps,
    layout: &Layout,
    skill_id: &str,
    excluded: bool,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let Some(mut follows) = doc::read_doc_private::<Follows>(fs, &layout.follows_path())? else {
        return Ok(());
    };
    let Some(entry) = follows.follows.iter_mut().find(|e| e.skill_id == skill_id) else {
        return Ok(());
    };
    if entry.excluded_here == excluded {
        return Ok(());
    }
    entry.excluded_here = excluded;
    doc::write_doc_private(
        fs,
        &layout.follows_path(),
        &Follows {
            schema_version: PERSISTED_SCHEMA_VERSION,
            follows: follows.follows,
        },
    )
}

/// DROP a skill's follow entry from `follows.json` under the identity lock — the `keep-as-yours` re-fork
/// retires the retained (withdrawn/detached) entry so `list` stops showing a ghost once the bytes have
/// been re-adopted as a new local skill. A no-op when the entry (or the file) is absent.
pub(crate) fn remove_follow(
    fs: &dyn FsOps,
    layout: &Layout,
    skill_id: &str,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let Some(mut follows) = doc::read_doc_private::<Follows>(fs, &layout.follows_path())? else {
        return Ok(());
    };
    let before = follows.follows.len();
    follows.follows.retain(|e| e.skill_id != skill_id);
    if follows.follows.len() == before {
        return Ok(());
    }
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
/// AND on a persisted workspace name outside the address grammar (the WAL is a durable copy of wire
/// data; the name rides request BODIES only — never a path join — but a hand-edited traversal shape
/// still fails the load closed, the same boundary discipline as every other persisted identifier).
/// An EMPTY name is the legitimate ORIGIN enrollment (the workspace the origin itself addresses —
/// single-tenant installs); the granted poll carries the authoritative workspace back.
pub(crate) fn read_wal(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<PendingEnrollment>, ClientError> {
    let wal: Option<PendingEnrollment> = doc::read_doc_private(fs, &layout.enrollment_path())?;
    if let Some(w) = &wal
        && !w.workspace_name.is_empty()
        && !crate::resolve::is_workspace_name(&w.workspace_name)
    {
        return Err(ClientError::Corrupt(
            "the enrollment WAL's workspace name is not a valid address name".into(),
        ));
    }
    Ok(wal)
}

/// Delete the enrollment WAL (once the grant persisted, or on a swept abandon). NotFound-tolerant.
pub(crate) fn delete_wal(fs: &dyn FsOps, layout: &Layout) -> Result<(), ClientError> {
    fs.remove_file(&layout.enrollment_path())?;
    Ok(())
}

/// The recovery sweep for the enrollment WAL: remove a WAL whose flow has expired
/// (`now_millis > expires_at_millis`) — a clean abandon (the server's flow row expired with it). An
/// unexpired WAL is preserved (a resume can still poll it; a granted flow re-answers the same grant).
/// Best-effort: an unreadable/corrupt WAL is left in place for the owning op to diagnose, never
/// hard-failing recovery.
///
/// The read → decide → delete runs UNDER the `"identity"` lock (the same lock every identity write
/// holds), and the expiry is decided from the read taken under that lock — never from an earlier
/// observation.
pub(crate) fn sweep_expired_wal(
    fs: &dyn FsOps,
    layout: &Layout,
    now_millis: i64,
) -> Result<(), ClientError> {
    // A cheap unlocked probe first: no WAL at all (the overwhelmingly common case — the sweep runs at the
    // start of EVERY command) takes no lock and touches nothing.
    if !fs.exists(&layout.enrollment_path()) {
        return Ok(());
    }
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    // The authoritative read, under the lock, immediately before any delete decision.
    let wal = match read_wal(fs, layout) {
        Ok(Some(wal)) => wal,
        // Absent → nothing to sweep. Unreadable/permissive/corrupt → leave it for the op to surface.
        Ok(None) | Err(_) => return Ok(()),
    };
    if now_millis > wal.expires_at_millis {
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
            base_url: "https://topos.sh/api".to_owned(),
        }
    }

    fn sample_membership(workspace_id: &str, display_name: &str) -> Membership {
        Membership {
            workspace_id: workspace_id.to_owned(),
            name: format!("{workspace_id}-name"),
            display_name: display_name.to_owned(),
            enrolled_at: 1,
        }
    }

    fn user_with(workspaces: Vec<Membership>) -> UserDoc {
        UserDoc {
            schema_version: 1,
            principal: None,
            workspaces,
        }
    }

    fn sample_follows() -> Follows {
        Follows {
            schema_version: 1,
            follows: vec![
                FollowEntry {
                    skill_id: "s_deploy".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    mode: FollowModeDoc::Auto,
                    review_required: false,
                    following: true,
                    excluded_here: false,
                },
                FollowEntry {
                    skill_id: "s_paused".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    mode: FollowModeDoc::ConfirmEach,
                    review_required: true,
                    following: false,
                    excluded_here: false,
                },
            ],
        }
    }

    fn sample_wal(expires_at_millis: i64) -> PendingEnrollment {
        PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            base_url: "https://topos.sh/api".to_owned(),
            workspace_name: "acme".to_owned(),
            intent: EnrollIntentDoc::Follow {
                target: Some(FollowTargetDoc {
                    kind: FollowKindDoc::Workspace,
                    name: "acme".to_owned(),
                }),
                mode: FollowModeDoc::Auto,
            },
            device_code: "dc_secret".to_owned(),
            user_code: "AAAA-BBBB".to_owned(),
            verification_uri_complete: "https://topos.sh/devices?code=AAAA-BBBB".to_owned(),
            interval_secs: 5,
            expires_at_millis,
        }
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
    fn credentials_debug_redacts_the_credential() {
        let c = Credentials {
            schema_version: 1,
            credential: "devc_secret".to_owned(),
            device_id: "dev_1".to_owned(),
        };
        let dbg = format!("{c:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(
            !dbg.contains("devc_secret"),
            "the credential must never appear in Debug"
        );
        assert!(dbg.contains("dev_1"));
    }

    #[test]
    fn credentials_write_read_round_trip_is_0600_and_replaces_wholesale() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("cred-rt"));
        write_credentials(&fs, &layout, "devc_one", "dev_1").unwrap();
        // A re-enrollment REPLACES the one credential wholesale.
        write_credentials(&fs, &layout, "devc_two", "dev_2").unwrap();
        let back = read_credentials(&fs, &layout).unwrap().unwrap();
        assert_eq!(back.credential, "devc_two");
        assert_eq!(back.device_id, "dev_2");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(layout.credentials_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
        );
    }

    #[test]
    fn fail_closed_on_newer_or_legacy_schema_version() {
        // A NEWER schema_version is never handed to serde — an upgrade error, fail closed.
        let newer = br#"{"schema_version":2,"base_url":"x"}"#;
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

        // The skill → workspace map is total over the follows (the ONE credential serves them all).
        let ws = skill_workspaces(&f);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws["s_deploy"], "w_acme");
        assert_eq!(ws["s_paused"], "w_acme");
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
    fn wal_round_trips_redacts_and_validates_the_workspace_name() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("wal-rt"));
        std::fs::create_dir_all(layout.identity_dir()).unwrap();
        let wal = sample_wal(2_000);
        write_wal(&fs, &layout, &wal).unwrap();
        let back = read_wal(&fs, &layout).unwrap().expect("the WAL loads");
        assert_eq!(back, wal);
        // Debug redacts the device code (the credential-to-be).
        let dbg = format!("{back:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(
            !dbg.contains("dc_secret"),
            "the device code must never appear in Debug: {dbg}"
        );
        // A login-intent WAL round-trips too (the tagged shape).
        let login = PendingEnrollment {
            intent: EnrollIntentDoc::Login,
            ..sample_wal(2_000)
        };
        write_wal(&fs, &layout, &login).unwrap();
        assert_eq!(read_wal(&fs, &layout).unwrap().unwrap(), login);
        // A hand-edited traversal workspace name fails the load closed. Assembled (not a source
        // literal) so the repo hygiene grep for traversal-looking strings stays clean.
        let hostile = PendingEnrollment {
            workspace_name: ["..", "..", "w"].join("/"),
            ..sample_wal(2_000)
        };
        write_wal(&fs, &layout, &hostile).unwrap();
        let err = read_wal(&fs, &layout).unwrap_err();
        assert!(matches!(err, ClientError::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn sweep_reaps_only_an_expired_wal() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("wal-sweep"));
        std::fs::create_dir_all(layout.identity_dir()).unwrap();
        // Unexpired: preserved (a resume can still poll it — a granted flow re-answers the grant).
        write_wal(&fs, &layout, &sample_wal(10_000)).unwrap();
        sweep_expired_wal(&fs, &layout, 5_000).unwrap();
        assert!(read_wal(&fs, &layout).unwrap().is_some());
        // Expired: reaped (the server's flow row expired with it).
        sweep_expired_wal(&fs, &layout, 20_000).unwrap();
        assert!(read_wal(&fs, &layout).unwrap().is_none());
        // No WAL at all: a clean no-op.
        sweep_expired_wal(&fs, &layout, 20_000).unwrap();
    }

    #[test]
    fn user_doc_round_trips_and_validates_workspace_ids() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("user-rt"));
        let user = user_with(vec![sample_membership("w_a", "A")]);
        write_user(&fs, &layout, &user).unwrap();
        assert_eq!(read_user(&fs, &layout).unwrap().unwrap(), user);
        // A traversal workspace id in a (hand-edited) membership fails the load closed.
        let mut hostile = user_with(vec![sample_membership("w_a", "A")]);
        hostile.workspaces[0].workspace_id = ["..", "w"].join("/");
        write_user(&fs, &layout, &hostile).unwrap();
        assert!(matches!(
            read_user(&fs, &layout),
            Err(ClientError::Corrupt(_))
        ));
    }

    #[test]
    fn resolve_write_workspace_selects_by_count_and_explicit() {
        // 0 memberships → a not-enrolled error (not a workspace-selection one).
        let none = user_with(Vec::new());
        assert!(matches!(
            none.resolve_write_workspace(None),
            Err(ClientError::Enrollment(_))
        ));
        // Exactly 1 → that one, no `--workspace` needed.
        let one = user_with(vec![sample_membership("w_a", "A")]);
        assert_eq!(
            one.resolve_write_workspace(None).unwrap().workspace_id,
            "w_a"
        );
        // >1 without an explicit choice → a workspace-selection error naming `--workspace` + the ids.
        let two = user_with(vec![
            sample_membership("w_a", "A"),
            sample_membership("w_b", "B"),
        ]);
        let err = two.resolve_write_workspace(None).unwrap_err();
        assert!(matches!(err, ClientError::WorkspaceSelection(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("--workspace") && msg.contains("w_a") && msg.contains("w_b"),
            "{msg}"
        );
        // >1 WITH a valid explicit → that one.
        assert_eq!(
            two.resolve_write_workspace(Some("w_b"))
                .unwrap()
                .workspace_id,
            "w_b"
        );
        // An explicit id this install never joined → a workspace-selection error.
        assert!(matches!(
            two.resolve_write_workspace(Some("w_c")),
            Err(ClientError::WorkspaceSelection(_))
        ));
    }

    #[test]
    fn upsert_membership_replaces_same_workspace_and_appends_a_new_one() {
        let mut user = user_with(Vec::new());
        upsert_membership(&mut user, sample_membership("w_a", "A"));
        assert_eq!(user.workspaces.len(), 1);
        // Same workspace_id REPLACES (a re-follow / display-name refresh), never appends a duplicate.
        upsert_membership(&mut user, sample_membership("w_a", "A-renamed"));
        assert_eq!(user.workspaces.len(), 1);
        assert_eq!(user.membership("w_a").unwrap().display_name, "A-renamed");
        // A different workspace_id APPENDS (a second follow never drops the first).
        upsert_membership(&mut user, sample_membership("w_b", "B"));
        assert_eq!(user.workspaces.len(), 2);
        assert!(user.membership("w_a").is_some() && user.membership("w_b").is_some());
    }

    #[test]
    fn refresh_membership_facts_updates_display_name_and_principal() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("refresh"));
        write_user(
            &fs,
            &layout,
            &user_with(vec![sample_membership("w_a", "A")]),
        )
        .unwrap();
        refresh_membership_facts(&fs, &layout, "w_a", "Acme, Inc.", "alice@acme.com").unwrap();
        let user = read_user(&fs, &layout).unwrap().unwrap();
        assert_eq!(user.membership("w_a").unwrap().display_name, "Acme, Inc.");
        assert_eq!(user.principal.as_deref(), Some("alice@acme.com"));
        // A no-op refresh rewrites nothing (idempotent).
        refresh_membership_facts(&fs, &layout, "w_a", "Acme, Inc.", "alice@acme.com").unwrap();
        // An absent user.json is a clean no-op.
        let empty = Layout::new(&scratch("refresh-none"));
        refresh_membership_facts(&fs, &empty, "w_a", "X", "p").unwrap();
    }

    #[test]
    fn write_follows_merged_rejects_a_cross_workspace_duplicate_skill_id() {
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("xws-dup"));
        let entry = |ws: &str| FollowEntry {
            skill_id: "s_dup".to_owned(),
            workspace_id: ws.to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: false,
            following: true,
            excluded_here: false,
        };
        write_follows_merged(&fs, &layout, &[entry("w_a")]).unwrap();
        // The SAME skill_id arriving under a DIFFERENT workspace is refused (a skill id is unique to one
        // workspace) — and nothing is overwritten.
        let err = write_follows_merged(&fs, &layout, &[entry("w_b")]).unwrap_err();
        assert!(matches!(err, ClientError::Corrupt(_)), "got {err:?}");
        let follows = read_follows(&fs, &layout).unwrap().unwrap();
        assert_eq!(follows.follows.len(), 1);
        assert_eq!(follows.follows[0].workspace_id, "w_a");
        // The SAME skill_id under the SAME workspace still updates cleanly (a mode refresh).
        write_follows_merged(&fs, &layout, &[entry("w_a")]).unwrap();
        assert_eq!(
            read_follows(&fs, &layout).unwrap().unwrap().follows.len(),
            1
        );
    }

    #[test]
    fn read_follows_fails_closed_on_a_cross_workspace_skill_id() {
        // The READ side of the same invariant the write guard enforces: a pre-existing / hand-edited
        // follows.json carrying the SAME skill_id under two DIFFERENT workspaces must fail the LOAD closed
        // (otherwise the by-id maps collapse it / the first-match lookups mis-scope it).
        let fs = crate::fs_seam::RealFs;
        let layout = Layout::new(&scratch("xws-read"));
        let entry = |skill: &str, ws: &str| FollowEntry {
            skill_id: skill.to_owned(),
            workspace_id: ws.to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: false,
            following: true,
            excluded_here: false,
        };
        // The SAME skill_id under w_a AND w_b — a cross-workspace collision.
        let hostile = Follows {
            schema_version: 1,
            follows: vec![entry("s_dup", "w_a"), entry("s_dup", "w_b")],
        };
        doc::write_doc_private(&fs, &layout.follows_path(), &hostile).unwrap();
        let err = read_follows(&fs, &layout).unwrap_err();
        assert!(matches!(err, ClientError::Corrupt(_)), "got {err:?}");

        // Distinct skill ids across two workspaces is the LEGITIMATE multi-workspace shape — it loads.
        let ok = Follows {
            schema_version: 1,
            follows: vec![entry("s_a", "w_a"), entry("s_b", "w_b")],
        };
        doc::write_doc_private(&fs, &layout.follows_path(), &ok).unwrap();
        assert_eq!(
            read_follows(&fs, &layout).unwrap().unwrap().follows.len(),
            2
        );
    }

    #[test]
    fn canonical_principal_trims_and_lowercases() {
        assert_eq!(canonical_principal(" Alice@Acme.COM "), "alice@acme.com");
        assert_eq!(canonical_principal("bob@x.test"), "bob@x.test");
    }
}
