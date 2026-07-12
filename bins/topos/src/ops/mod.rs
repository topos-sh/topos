//! The verb layer — one file per CLI verb — plus the shared engine machinery those verbs compose.
//!
//! **Verbs** (each maps 1:1 to a `cli::Command` arm): [`add`], [`follow`], [`unfollow`], [`invite`],
//! [`list`], [`diff`], [`publish`], [`review`], [`revert`], [`log`], [`update`](pull), [`self_update`].
//!
//! **Shared machinery** (no verb of its own — the verbs above drive it):
//! - [`sync_engine`] — the per-skill `checkForUpdates → plan → apply` currency machine over the kernel's
//!   four-state transition. `pull` is its scope dispatch; the `follow <skill>` path drives it too.
//! - [`merge_resolve`] — the author-side resolution of a diverged draft (three-way merge / conflict
//!   materialization / the `--onto-current` escape), reachable only through the engine's witness token.
//! - [`contribute`] — the device-signed write plumbing `publish`/`review`/`revert` share: the fresh-current
//!   read, identity re-derivation, the op-WAL replay, and the all-outcome receipt mapping.
//! - [`crate::materialize`] (at the crate root, beside the other placement seams) — the engine's
//!   byte-writing half: the crash-safe staged-then-swapped install into a harness dir.
//!
//! This file itself holds only the cross-verb helpers: name resolution, the hex-id parsers, and the
//! short-version-prefix resolver.

mod add;
mod auth;
mod channel;
mod contribute;
mod diff;
mod follow;
mod invite;
mod list;
mod log;
mod merge_resolve;
mod protect;
mod publish;
mod pull;
mod remove;
mod revert;
mod review;
mod self_update;
mod sync_engine;
mod unfollow;

pub(crate) use add::{
    AddRemoteOpts, add, add_remote, add_with_name, resolve_add_target, split_target,
    tracked_skill_at,
};
pub(crate) use auth::{
    AuthConnectors, AuthLoginData, AuthLoginOutcome, AuthLoginPending, AuthLogoutData,
    AuthLogoutDescribe, AuthLogoutOutcome, AuthStatusData, login, logout, status,
};
pub(crate) use channel::{ChannelConnectors, ChannelOutcome, channel};
pub(crate) use diff::diff;
pub(crate) use follow::{
    FollowApplied, FollowConnectors, FollowDescribe, FollowOpts, FollowOutcome, follow,
};
pub(crate) use invite::{InviteConnectors, InviteOutcome, invite};
pub(crate) use list::{DiscoveryRoots, ListOutcome, RemoteScope, list};
pub(crate) use unfollow::{UnfollowApplied, UnfollowDescribe};
// The TTY-only enrollment row types are constructed in `list` and rendered by field access; the named
// re-export exists for the renderer's tests, which build them by hand.
#[cfg(test)]
pub(crate) use list::{FollowNote, ListEnrollment};
pub(crate) use log::{LogConnectors, log};
pub(crate) use publish::{
    PublishDescribeConnectors, PublishOutcome, StandupConnectors, publish, publish_describe,
};
// The auto-add pre-step is driven internally by `publish`; the re-export exists only for its unit tests.
pub(crate) use protect::{ProtectConnectors, ProtectOutcome, protect};
#[cfg(test)]
pub(crate) use publish::ensure_tracked;
pub(crate) use pull::{
    PullOutcome, PullScope, ReconcileOpts, ResetOutcome, TargetMode, pull, pull_reconcile_with,
    quiet_hook_lines, quiet_soft_failure, reset,
};
pub(crate) use remove::{RemoveConnectors, RemoveOutcome, remove};
pub(crate) use revert::revert;
pub(crate) use review::{ReviewConnectors, ReviewOutcome, ReviewVerdict, review_dispatch};
pub(crate) use self_update::{SelfUpdateAction, SelfUpdateOpts, SelfUpdateOutcome, self_update};
pub(crate) use unfollow::{UnfollowConnectors, UnfollowOutcome, unfollow};

use topos_core::digest::to_hex;
use topos_gitstore::Store;
use topos_types::persisted::Lock;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::sidecar::SkillPaths;
use crate::{doc, enroll};

/// A MARKED SEAM: a verb path that parses today but whose full behavior (resolution grammar, describe /
/// `--yes` two-phase flow, or a new server call) lands in a later leg. Returns a typed `INVALID_ARGUMENT`
/// refusal so the caller gets an honest answer, and the later leg finds every seam by grepping `not_yet(`.
pub(crate) fn not_yet(what: &str) -> ClientError {
    ClientError::InvalidArgument(format!(
        "`{what}` is recognized but not wired yet — coming with the full verb grammar"
    ))
}

/// Resolve a skill name to its `(id, lock)` across the tracked skills, WITHOUT a workspace filter — the
/// common case (the local verbs that do not act in a workspace: `add`, `log`, `diff`, `unfollow`, `pull`,
/// the `follow <skill>` path). See [`resolve_skill_in_workspace`] for the filtered form the write verbs use.
fn resolve_skill(ctx: &Ctx<'_>, name: &str) -> Result<(SkillId, Lock), ClientError> {
    resolve_skill_in_workspace(ctx, name, None)
}

/// Resolve a skill name to its `(id, lock)`, optionally narrowed to one workspace. A name is the
/// user-facing handle; two same-name skills are distinct, so an ambiguous name is a typed error carrying
/// the count. The returned id is the VALIDATED newtype (parsed from the directory name), so every
/// downstream path join is charset-proven; a dir whose name fails the parse was never minted by topos and
/// is skipped.
///
/// When `workspace = Some(ws)`, a candidate whose `follows.json` entry names a DIFFERENT workspace is
/// dropped BEFORE the duplicate-name check — this is how one install following the same skill NAME in two
/// workspaces disambiguates via `--workspace`. A candidate with NO follow entry (a purely local /
/// genesis skill) is unscoped and survives any filter, so a `--workspace`-qualified genesis publish still
/// resolves its local skill. The deterministic-error-on-ambiguity behavior is preserved when the filter
/// still leaves more than one.
fn resolve_skill_in_workspace(
    ctx: &Ctx<'_>,
    name: &str,
    workspace: Option<&str>,
) -> Result<(SkillId, Lock), ClientError> {
    resolve_skill_scoped(ctx, name, workspace, false)
}

/// The STRICT resolver `review` / `revert` use: like [`resolve_skill_in_workspace`], but a candidate with
/// NO follow entry (a purely local / genesis skill that merely SHARES the name) is dropped BEFORE the
/// ambiguity count. Those verbs only ever act on a FOLLOWED skill, so a local skill named the same as a
/// followed one must never make the op spuriously [`ClientError::AmbiguousName`]. (Publish keeps the
/// lenient [`resolve_skill_in_workspace`] — its genesis path deliberately resolves a brand-new local skill
/// under a `--workspace` filter.)
fn resolve_followed_skill_in_workspace(
    ctx: &Ctx<'_>,
    name: &str,
    workspace: Option<&str>,
) -> Result<(SkillId, Lock), ClientError> {
    resolve_skill_scoped(ctx, name, workspace, true)
}

/// The shared name-resolution core. Two filters run BEFORE the ambiguity count (so a same-name collision
/// disambiguates deterministically):
/// - `followed_only` drops a candidate with NO follow entry (a purely local / genesis skill);
/// - `workspace = Some(ws)` drops a FOLLOWED candidate scoped to a different workspace (an unscoped /
///   no-entry candidate survives the workspace filter — the lenient genesis-publish path — unless
///   `followed_only` already dropped it).
fn resolve_skill_scoped(
    ctx: &Ctx<'_>,
    name: &str,
    workspace: Option<&str>,
    followed_only: bool,
) -> Result<(SkillId, Lock), ClientError> {
    // The skill_id → workspace_id join comes from the follow-state; only read it when a filter needs it.
    let followed = if workspace.is_some() || followed_only {
        ctx.follow.followed()
    } else {
        Vec::new()
    };
    let mut matches: Vec<(SkillId, Lock)> = Vec::new();
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        let Ok(id) = SkillId::parse(id) else {
            continue;
        };
        if let Some(lock) = doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&id).lock)?
            && lock.name == name
        {
            let follow_entry = followed.iter().find(|(fid, _)| fid == id.as_str());
            // The strict form drops a candidate with NO follow entry (a purely local / genesis skill that
            // merely shares the name) — `review` / `revert` only ever act on a followed skill.
            if followed_only && follow_entry.is_none() {
                continue;
            }
            // Drop a FOLLOWED candidate scoped to a different workspace; keep an unscoped (no-entry) one.
            if let Some(ws) = workspace
                && let Some((_, fc)) = follow_entry
                && fc.workspace_id != ws
            {
                continue;
            }
            matches.push((id, lock));
        }
    }
    // Deterministic across same-name skills.
    matches.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    match matches.len() {
        0 => Err(ClientError::NoSuchSkill {
            name: name.to_owned(),
        }),
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        count => Err(ClientError::AmbiguousName {
            name: name.to_owned(),
            count,
        }),
    }
}

/// The workspace a FOLLOWED skill lives in (its expected signed-pointer scope), from the follow-state —
/// or `None` when the skill has no follow entry (a purely local / genesis skill). A retained-but-paused
/// (`following == false`) entry still resolves: the workspace outlives an `unfollow`.
pub(crate) fn followed_workspace(ctx: &Ctx<'_>, skill_id: &str) -> Option<String> {
    ctx.follow
        .followed()
        .into_iter()
        .find(|(id, _)| id == skill_id)
        .map(|(_, fc)| fc.workspace_id)
}

/// The workspace a followed skill lives in, or a typed error if it is not followed — the STRICT scope the
/// plane ops on an already-existing skill need (`diff <ref>` reads it; `review` / `revert` sign in it).
/// Unlike [`write_workspace_for_skill`], there is NO ambient fallback: those verbs always act on a skill
/// you follow, so a non-followed target fails cleanly here instead of as an opaque plane rejection.
///
/// # Errors
/// [`ClientError::Plane`] if `skill_id` is not a followed skill (no workspace to scope the op).
pub(crate) fn workspace_of(ctx: &Ctx<'_>, skill_id: &str) -> Result<String, ClientError> {
    followed_workspace(ctx, skill_id).ok_or_else(|| {
        ClientError::Plane(format!(
            "'{skill_id}' is not a followed skill; a plane op needs its workspace"
        ))
    })
}

/// The workspace a `publish` / `review` / `revert` signs its op in for an already-resolved skill:
/// - a FOLLOWED skill signs in its own follow-entry workspace (the pointer scope — it MUST be the skill's
///   own workspace, not an ambient guess);
/// - a skill with NO follow entry (a genesis publish of a locally-`add`ed skill) is AMBIENT: the single
///   membership, or the `--workspace`-selected one.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::WorkspaceSelection`] if the install has
/// joined several workspaces and `explicit` neither names one nor is the sole choice.
pub(crate) fn write_workspace_for_skill(
    ctx: &Ctx<'_>,
    skill_id: &str,
    explicit: Option<&str>,
) -> Result<String, ClientError> {
    if let Some(ws) = followed_workspace(ctx, skill_id) {
        return Ok(ws);
    }
    let user = enroll::read_user(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment(
            "could not determine your workspace; complete enrollment with `topos follow` first"
                .into(),
        )
    })?;
    Ok(user.resolve_write_workspace(explicit)?.workspace_id.clone())
}

/// Parse 64 lowercase-hex chars into a 32-byte id (a sidecar doc field) via the shared `hex` codec.
/// Fails **closed** on uppercase: the persisted + result schemas pin `^[0-9a-f]{64}$`, and `diff` echoes
/// the original string straight into its `--json`, so an uppercase byte (which `hex::decode_to_slice`
/// would accept case-insensitively) must be rejected here, not passed through as schema-invalid output.
pub(crate) fn parse_hex32(hex_str: &str) -> Result<[u8; 32], ClientError> {
    if hex_str.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(ClientError::Corrupt("hex id must be lowercase".into()));
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut out)
        .map_err(|e| ClientError::Corrupt(format!("invalid hex id: {e}")))?;
    Ok(out)
}

/// The ARGV-boundary wrapper over [`parse_hex32`]: a user-typed hash that fails to parse is a usage
/// error (`INVALID_ARGUMENT`, with `usage` shown verbatim on both surfaces), never `CORRUPT_STATE` —
/// that family stays reserved for the sidecar-document call sites, where the same malformed bytes
/// genuinely mean a corrupt persisted doc. `usage` describes the expected shape; it never echoes the
/// raw input (the caller names the argument, not its bytes).
pub(crate) fn parse_hex32_arg(hex_str: &str, usage: &str) -> Result<[u8; 32], ClientError> {
    parse_hex32(hex_str).map_err(|_| ClientError::InvalidArgument(usage.to_owned()))
}

/// The shortest version prefix an argv surface accepts (git-style; outputs render 12 chars, so a pasted
/// short form always clears this floor).
pub(crate) const MIN_VERSION_PREFIX: usize = 8;

/// An argv version reference: the always-valid full 64-hex id, or a short lowercase-hex prefix
/// ([`MIN_VERSION_PREFIX`]..64 chars) resolved against the skill's locally recorded pointer history via
/// [`resolve_version_ref`]. The local-resolution surfaces (`pull <skill>@<ref>`, `revert --to`, the
/// `diff <ref>` endpoints) accept both forms; `review <skill>@<hash>` stays full-hash-only (a proposal's
/// candidate id lives on the plane, never in local history — see [`review`]'s parse site).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VersionRef {
    Full([u8; 32]),
    Prefix(String),
}

impl VersionRef {
    /// Recognize `token` as a version reference, or `None` when it is not shaped like one — lowercase
    /// hex only (the schema's pinned charset), exactly 64 chars for the full id, [`MIN_VERSION_PREFIX`]
    /// up to 63 for a prefix. The `pull <name>@<suffix>` split uses the `None` to keep a non-ref suffix
    /// part of the skill name (so a name like `team@cli` still resolves).
    pub(crate) fn recognize(token: &str) -> Option<Self> {
        if token.is_empty()
            || !token
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        if token.len() == 64 {
            let mut out = [0u8; 32];
            hex::decode_to_slice(token, &mut out).ok()?;
            return Some(VersionRef::Full(out));
        }
        (MIN_VERSION_PREFIX..64)
            .contains(&token.len())
            .then(|| VersionRef::Prefix(token.to_owned()))
    }

    /// Parse an argv token that MUST be a version reference; anything unrecognizable (uppercase,
    /// non-hex, or shorter than [`MIN_VERSION_PREFIX`]) is a usage error carrying `usage` verbatim.
    pub(crate) fn parse_arg(token: &str, usage: &str) -> Result<Self, ClientError> {
        Self::recognize(token).ok_or_else(|| ClientError::InvalidArgument(usage.to_owned()))
    }

    /// The user-facing spelling for an error message: the full hex, or the prefix as typed.
    pub(crate) fn shown(&self) -> String {
        match self {
            VersionRef::Full(h) => hex::encode(h),
            VersionRef::Prefix(p) => p.clone(),
        }
    }
}

/// Resolve a [`VersionRef`] against `known` — the 64-hex ids of the versions this client holds LOCALLY (the
/// skill's gitstore). That set is THE resolution source for every prefix surface: a go-back can only install
/// bytes it already has, and a revert/plane-diff target is a version this client has previously fetched.
///
/// A full id passes through untouched (it needs no local history). A prefix resolves iff it matches
/// exactly one distinct local version; zero matches → `Ok(None)` (each caller maps its own flow's
/// not-found error), two or more → `INVALID_ARGUMENT` naming the candidates' short forms.
pub(crate) fn resolve_version_ref(
    known: &[String],
    vref: &VersionRef,
) -> Result<Option<[u8; 32]>, ClientError> {
    let prefix = match vref {
        VersionRef::Full(h) => return Ok(Some(*h)),
        VersionRef::Prefix(p) => p,
    };
    // Distinct commits only (a version id appearing twice must not read as ambiguity).
    let mut matches: Vec<&str> = known
        .iter()
        .map(String::as_str)
        .filter(|c| c.starts_with(prefix.as_str()))
        .collect();
    matches.sort_unstable();
    matches.dedup();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(parse_hex32(matches[0])?)),
        _ => {
            let shorts: Vec<&str> = matches.iter().map(|c| c.get(..12).unwrap_or(c)).collect();
            Err(ClientError::InvalidArgument(format!(
                "the version prefix '{prefix}' is ambiguous here — it matches {}; use a longer \
                 prefix or the full 64-char id",
                shorts.join(", ")
            )))
        }
    }
}

/// The 64-hex ids of the versions this skill holds in its local gitstore, or empty when no store exists yet
/// — the candidate set [`resolve_version_ref`] resolves a prefix against.
pub(crate) fn local_version_ids(
    _ctx: &Ctx<'_>,
    sp: &SkillPaths,
) -> Result<Vec<String>, ClientError> {
    if !sp.store.exists() {
        return Ok(Vec::new());
    }
    let store = Store::open(&sp.store)?;
    Ok(store.list_versions()?.iter().map(|v| to_hex(v)).collect())
}

#[cfg(test)]
mod tests {
    use super::{VersionRef, parse_hex32, parse_hex32_arg, resolve_version_ref};

    fn recorded(commits: &[&str]) -> Vec<String> {
        commits.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn version_ref_recognizes_full_and_prefix_shapes_only() {
        let full = "ab".repeat(32);
        assert!(matches!(
            VersionRef::recognize(&full),
            Some(VersionRef::Full(_))
        ));
        // A ≥8-char lowercase-hex prefix is a prefix ref.
        assert_eq!(
            VersionRef::recognize("ab12cd34ef56"),
            Some(VersionRef::Prefix("ab12cd34ef56".to_owned()))
        );
        assert_eq!(
            VersionRef::recognize("ab12cd34"),
            Some(VersionRef::Prefix("ab12cd34".to_owned()))
        );
        // Too short, uppercase, non-hex, empty: not a version ref.
        for not_a_ref in ["ab12cd3", "AB12CD34", "docs", "", "xyz12345"] {
            assert_eq!(VersionRef::recognize(not_a_ref), None, "{not_a_ref:?}");
        }
        // The argv parser turns an unrecognizable token into INVALID_ARGUMENT with the usage verbatim.
        let err = VersionRef::parse_arg("ab12cd3", "usage text").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert_eq!(err.to_string(), "usage text");
    }

    #[test]
    fn prefix_resolution_unique_ambiguous_and_no_match() {
        let a = format!("ab12cd34{}", "0".repeat(56));
        let b = format!("ab12cd99{}", "1".repeat(56));
        let recs = recorded(&[&a, &b]);

        // Unique prefix resolves to the full commit.
        let hit = resolve_version_ref(&recs, &VersionRef::Prefix("ab12cd34".into()))
            .unwrap()
            .expect("unique prefix resolves");
        assert_eq!(hex::encode(hit), a);
        // A full id passes through without consulting history.
        let full = VersionRef::recognize(&"cd".repeat(32)).unwrap();
        assert!(resolve_version_ref(&recs, &full).unwrap().is_some());
        // Ambiguous across two distinct commits → INVALID_ARGUMENT naming both short forms.
        let err = resolve_version_ref(&recs, &VersionRef::Prefix("ab12cd".into())).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        let msg = err.to_string();
        assert!(msg.contains(&a[..12]) && msg.contains(&b[..12]), "{msg}");
        // No match → Ok(None): each caller maps its own flow's not-found error.
        assert!(
            resolve_version_ref(&recs, &VersionRef::Prefix("ffffffff".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn prefix_resolution_dedupes_a_repeated_version_id() {
        // A version id appearing twice in the local set is ONE candidate, not an ambiguity.
        let c = format!("ab12cd34{}", "0".repeat(56));
        let recs = recorded(&[&c, &c]);
        let hit = resolve_version_ref(&recs, &VersionRef::Prefix("ab12cd34".into()))
            .unwrap()
            .expect("a duplicated commit still resolves");
        assert_eq!(hex::encode(hit), c);
    }

    #[test]
    fn parse_hex32_is_lowercase_only_and_length_checked() {
        // 64 lowercase hex chars round-trips.
        assert!(parse_hex32(&"abcdef0123456789".repeat(4)).is_ok());
        // Uppercase must fail closed — the schema pins lowercase and `diff` echoes the raw string.
        assert!(parse_hex32(&"ABCDEF0123456789".repeat(4)).is_err());
        // Wrong length and non-hex are rejected by the codec.
        assert!(parse_hex32("abc").is_err());
        assert!(parse_hex32(&"g".repeat(64)).is_err());
    }

    #[test]
    fn argv_and_document_boundaries_classify_the_same_bytes_differently() {
        // The SAME malformed hash: a usage error from argv, corruption from a persisted doc.
        let arg = parse_hex32_arg("abc", "`--to` must be a 64-char lowercase hex version id")
            .unwrap_err();
        assert_eq!(arg.code(), "INVALID_ARGUMENT");
        assert_eq!(
            arg.to_string(),
            "`--to` must be a 64-char lowercase hex version id"
        );
        assert_eq!(parse_hex32("abc").unwrap_err().code(), "CORRUPT_STATE");
        // A good hash parses identically through the wrapper.
        assert!(parse_hex32_arg(&"abcdef0123456789".repeat(4), "unused").is_ok());
    }

    /// The workspace-scoped + followed-only name resolvers over a real fs + a fixture follow-state.
    mod workspace_resolution {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU32, Ordering};

        use topos_harness::ClaudeCode;
        use topos_types::PERSISTED_SCHEMA_VERSION;
        use topos_types::persisted::Lock;

        use super::super::{resolve_followed_skill_in_workspace, resolve_skill_in_workspace};
        use crate::ctx::Ctx;
        use crate::doc;
        use crate::error::ClientError;
        use crate::fs_seam::{FsOps, RealFs};
        use crate::ids::{RealClock, RealIds};
        use crate::plane::{FollowContext, FollowMode, FollowSource, InertPlane};
        use crate::sidecar::Layout;

        const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

        struct FixtureFollow(Vec<(String, FollowContext)>);
        impl FollowSource for FixtureFollow {
            fn followed(&self) -> Vec<(String, FollowContext)> {
                self.0.clone()
            }
        }

        fn scratch(tag: &str) -> PathBuf {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-res-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// Lay a tracked skill dir (`skills/<id>/lock.json` naming `name`) so the resolver's walk finds it.
        fn lay_skill(fs: &RealFs, layout: &Layout, id: &str, name: &str) {
            let sid = crate::id::SkillId::parse(id).unwrap();
            fs.create_dir_all(&layout.skill_dir(&sid)).unwrap();
            doc::write_doc(
                fs,
                &layout.published(&sid).lock,
                &Lock {
                    schema_version: PERSISTED_SCHEMA_VERSION,
                    skill_id: id.to_owned(),
                    name: name.to_owned(),
                    base_commit: ZERO_HEX.to_owned(),
                    bundle_digest: ZERO_HEX.to_owned(),
                    files: Vec::new(),
                },
            )
            .unwrap();
        }

        fn followed(id: &str, ws: &str) -> (String, FollowContext) {
            (
                id.to_owned(),
                FollowContext {
                    workspace_id: ws.to_owned(),
                    mode: FollowMode::Auto,
                    review_required: false,
                    following: true,
                },
            )
        }

        /// A resolver-only [`Ctx`] over a real fs + a fixture follow-state. Name resolution touches only
        /// `fs` / `layout` / `follow`; the other seams are inert stand-ins.
        fn with_ctx<R>(
            home: &Layout,
            follow: &dyn FollowSource,
            f: impl FnOnce(&Ctx<'_>) -> R,
        ) -> R {
            let fs = RealFs;
            let ids = RealIds;
            let clock = RealClock;
            let plane = InertPlane;
            let harness = ClaudeCode::new(scratch("adapter"), &fs);
            let ctx = Ctx {
                fs: &fs,
                ids: &ids,
                clock: &clock,
                device_id: String::new(),
                layout: home.clone(),
                harness: &harness,
                plane: &plane,
                follow,
            };
            f(&ctx)
        }

        #[test]
        fn workspace_filter_disambiguates_a_name_followed_in_two_workspaces() {
            // One install follows the SAME NAME "docs" in two workspaces (distinct plane-minted ids). A
            // bare resolve is ambiguous; a `--workspace` filter picks exactly the matching one.
            let home = Layout::new(&scratch("p2a"));
            let fs = RealFs;
            lay_skill(&fs, &home, "topos_a", "docs");
            lay_skill(&fs, &home, "topos_b", "docs");
            let follow =
                FixtureFollow(vec![followed("topos_a", "w_a"), followed("topos_b", "w_b")]);

            with_ctx(&home, &follow, |ctx| {
                assert!(matches!(
                    resolve_skill_in_workspace(ctx, "docs", None),
                    Err(ClientError::AmbiguousName { count: 2, .. })
                ));
                assert_eq!(
                    resolve_skill_in_workspace(ctx, "docs", Some("w_a"))
                        .unwrap()
                        .0
                        .as_str(),
                    "topos_a"
                );
                assert_eq!(
                    resolve_skill_in_workspace(ctx, "docs", Some("w_b"))
                        .unwrap()
                        .0
                        .as_str(),
                    "topos_b"
                );
            });
        }

        #[test]
        fn followed_only_resolve_drops_a_local_skill_sharing_a_followed_name() {
            // A followed skill "docs" and a purely-local skill "docs" (no follow entry) coexist. The
            // LENIENT resolve is ambiguous (what would make `revert docs` / `review docs` spuriously
            // fail); the STRICT followed-only resolve those verbs use drops the local one and resolves.
            let home = Layout::new(&scratch("p2b"));
            let fs = RealFs;
            lay_skill(&fs, &home, "topos_followed", "docs");
            lay_skill(&fs, &home, "topos_local", "docs");
            let follow = FixtureFollow(vec![followed("topos_followed", "w_a")]);

            with_ctx(&home, &follow, |ctx| {
                // The lenient resolve keeps BOTH → ambiguous (the spurious failure this fix removes).
                assert!(matches!(
                    resolve_skill_in_workspace(ctx, "docs", None),
                    Err(ClientError::AmbiguousName { count: 2, .. })
                ));
                // The followed-only resolve drops the no-follow-entry local skill → resolves to the followed.
                let (id, _lock) = resolve_followed_skill_in_workspace(ctx, "docs", None).unwrap();
                assert_eq!(id.as_str(), "topos_followed");
            });
        }
    }
}
