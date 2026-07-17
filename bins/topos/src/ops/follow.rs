//! `follow` — the device-flow enrollment + first-receive client.
//!
//! One verb, dispatched by the single positional (the harness drives it non-interactively):
//! - **`follow <workspace-address>`** (call 1) — card-fetch the address, re-root onto the declared API
//!   base, guard one-plane-per-install, start a device authorization
//!   (`POST /v1/device/authorize {requested_name, workspace}`), write a `0600` WAL, and return the
//!   pending disclosure ("open `verification_uri_complete`, enter the code").
//! - **re-invoking `follow`** (call 2) — with a pending enrollment WAL on disk, re-invoking `follow`
//!   (with any target, or none) RESUMES it — "re-invoking IS the resume": poll `/v1/device/token`
//!   once; on a granted poll the answer carries the device's ONE bearer credential (the promoted
//!   device code), the registered device id, and the joined workspace — persist them
//!   (`instance.json` / `credentials.json` / `user.json`), delete the WAL, and CONTINUE into the
//!   recorded follow intent's describe / `--yes` apply in the same invocation.
//! - **`follow <skill>[@<hash>]`** (post-enroll) — a KNOWN followed-skill name drives the existing pull
//!   engine to place the named, already-disclosed first-receive bytes (the I-TOFU "one accept"). On a
//!   retained entry `unfollow` paused (`following == false`) it RESUMES the follow instead: the flag flips
//!   back on, a still-pending first-receive offer is placed, and otherwise the next `pull` lands current.
//!
//! The positional is dispatched by SHAPE (see [`follow`]): a pending WAL wins (re-invoke resumes); a
//! known skill name is the skill path; a retired `/i/` invite link refuses typed (join by the
//! workspace ADDRESS); otherwise it is the address / subscribe grammar.
//!
//! **One secret, one field.** The agent only ever holds the device code (promoted server-side to the
//! device credential on approval) and then the credential itself; enrollment completes by POLLING —
//! no keypair, no link token, no per-workspace mint. **Secrets** live only in the `0600` WAL /
//! `credentials.json`, are redacted in `Debug`, and never reach a URL / log / error.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use topos_core::digest::to_hex;
use topos_gitstore::Store;
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::persisted::{Lock, PlacementMap, SwapCapability, SyncState};
use topos_types::requests::{WireChannelIndex, WireMe, WireSkillIndex};
use topos_types::results::{EnrollmentPending, FollowData, FollowOffer, Offer, PullAction};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::plane::{
    DeliverySnapshot, DeliverySource, DeviceAuthPoll, DirectorySource, EnrollSource, EnrolledGrant,
    FollowContext, PlaneError, PlaneSource, ReconcileTransport,
};
use crate::plane_http::FileFollow;
use crate::resolve::{self, ParsedTarget, Resolution, ResourceKind};
use crate::{doc, enroll, sidecar};

use super::pull::ReconcileOpts;
use super::sync_engine::{self, Invocation};

/// The 64-char all-zero hex sentinel a never-received baseline uses for its (absent) base commit / digest.
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// The genesis generation sentinel — `0` means "nothing authenticated / applied yet".
const GENESIS: u64 = 0;

/// `follow`'s flags, parsed from argv (the positional targets ride separately).
#[derive(Clone)]
pub(crate) struct FollowOpts {
    /// `--manual` ⇒ confirm-each adoption (else auto).
    pub manual: bool,
    /// The global `--workspace` filter, pre-canonicalized to an id — disambiguates a positional skill NAME shared across the
    /// workspaces this install follows on the same plane. Ignored by the enrollment motions.
    pub workspace: Option<String>,
    /// `--yes` — apply the described subscription (the one-shot consent). Bare = describe only.
    pub yes: bool,
    /// `--prefix-dirname` — install a dirname-colliding skill under `<workspace>.<name>` instead of
    /// declining it (the collision choice the describe offers).
    pub prefix_dirname: bool,
    /// `--channel` selectors — kind-forced channel targets (join).
    pub channels: Vec<String>,
    /// `--skill` selectors — kind-forced skill targets (direct follow).
    pub skills: Vec<String>,
    /// `--agent` — the DEVICE-LOCAL placement include-list for the followed skill(s): registry slugs
    /// (repeatable; `'*'` clears back to unscoped). Placement policy only — never told to the plane.
    pub agents: Vec<String>,
}

/// Builds the creds-free enrollment transport for a plane base URL.
pub(crate) type EnrollConnect<'a> = dyn Fn(&str) -> Box<dyn EnrollSource> + 'a;
/// Builds the credentialed DIRECTORY transport (describe reads + subscription rows) for a base URL.
/// Re-reads `credentials.json` per build, so a mid-invocation enrollment's fresh mint is seen.
pub(crate) type DirectoryConnect<'a> = dyn Fn(&str) -> Box<dyn DirectorySource> + 'a;
/// Builds the credentialed RECONCILE transport (delivery + fleet report + the per-skill read lane,
/// on one object — the reconcile binds a new arrival's workspace onto the read side). Re-reads the
/// on-disk credential per build, for the same mid-invocation reason.
pub(crate) type DeliveryConnect<'a> = dyn Fn(&str) -> Box<dyn ReconcileTransport> + 'a;

/// The network seams the op needs, as factories — the base URL is known only after the op parses the
/// target / the card / the WAL, so the transports can't be pre-built in the composition root.
/// Production wires the `ureq` transports; the tests wire fakes (no HTTP).
pub(crate) struct FollowConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub directory: &'a DirectoryConnect<'a>,
    pub delivery: &'a DeliveryConnect<'a>,
    /// The default WEB origin the enrollment door dials when nothing is pinned yet (`follow <bare
    /// workspace>` on a fresh install) — the composition root resolves `TOPOS_PLANE_URL`, else the
    /// hosted default; the card fetch re-roots it onto the declared API base.
    pub web_origin: String,
}

/// The verb's outcome — one of the three surfaces `follow` can answer with.
// One value exists per invocation (the size gap between the inline wire payload and the boxed
// describe/apply is irrelevant here, and boxing `FollowData` would noise every classic-path match).
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum FollowOutcome {
    /// The classic wire payload: a pending device-flow, a claim/invite enrollment, or the
    /// skill-path accept. `resumed` is TTY-only disclosure (display names of skills whose retained
    /// `following == false` entry this skill-path follow flipped on).
    Data {
        data: FollowData,
        resumed: Vec<String>,
    },
    /// The two-phase DESCRIBE (a bare subscribe) — nothing mutated beyond the enrollment itself;
    /// `next_argvs` carry the ready-to-exec apply commands (`--yes`, and the `--prefix-dirname`
    /// variant when collisions exist).
    Described {
        describe: Box<FollowDescribe>,
        next_argvs: Vec<Vec<String>>,
    },
    /// The `--yes` apply report.
    Applied(Box<FollowApplied>),
    /// A per-device exclusion LIFT ("re-attach this device") DESCRIBE — bare `follow <skill>` on a
    /// skill this person follows but THIS device excluded. Nothing mutated; `yes_argv` carries the
    /// ready-to-exec apply command.
    ReattachDescribed {
        reattach: Box<Reattach>,
        yes_argv: Vec<String>,
    },
    /// The `--yes` re-attach report (exclusion lifted, marker cleared, current bytes reinstalled).
    ReattachApplied(Box<Reattach>),
    /// The `--agent` scope UPDATE on already-followed skills (two-phase, offline — the shared
    /// placement-policy surface).
    Scope(super::agent_scope::AgentScopeOutcome),
}

impl FollowOutcome {
    /// Wrap a classic wire payload with no resumed disclosure.
    fn plain(data: FollowData) -> Self {
        FollowOutcome::Data {
            data,
            resumed: Vec::new(),
        }
    }
}

/// The workspace block a describe/apply carries.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DescribedWorkspace {
    pub workspace_id: String,
    /// The ADDRESS name.
    pub name: String,
    pub display_name: String,
    /// The full address (the share link — server-built).
    pub address: String,
}

/// One subscribe target, echoed on the describe/apply (`kind` is `workspace`/`channel`/`skill`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DescribedTarget {
    pub kind: String,
    pub name: String,
}

/// One install `--yes` would land: the catalog identity, the consent digest, and WHY it arrives.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DescribedInstall {
    pub skill_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    /// The channels delivering it (`everyone` included when it delivers).
    pub via_channels: Vec<String>,
    /// Whether it arrives as a direct follow (this invocation's, or a standing one).
    pub via_direct: bool,
}

/// One dirname collision: an incoming install whose name a DIFFERENT local skill already holds. The
/// default `--yes` DECLINES it; `--prefix-dirname` installs it under the prefixed dirname instead.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DescribedCollision {
    pub skill_id: String,
    pub name: String,
    /// Where the existing same-named copy lives (its placement dir, or its tracked identity).
    pub existing: String,
    /// The `--prefix-dirname` alternative (`<workspace>.<name>`).
    pub prefixed_dirname: String,
}

/// The two-phase DESCRIBE a bare subscribe answers — everything `--yes` would change, and nothing
/// changed yet (except the enrollment itself, when this invocation enrolled: identity, reversible,
/// disclosed via `enrolled_now`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FollowDescribe {
    pub workspace: DescribedWorkspace,
    /// The caller's role on the roster.
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invited_by: Option<String>,
    /// Whether THIS invocation enrolled the device (the identity step already happened; the
    /// subscription + bytes are what `--yes` consents to).
    pub enrolled_now: bool,
    /// What this follow subscribes (workspace / channels / skills).
    pub targets: Vec<DescribedTarget>,
    /// The installs `--yes` would land on this device (pending first-receives included).
    pub installs: Vec<DescribedInstall>,
    /// Channels the person is already placed into (an inviter's pre-placement; `everyone` excluded).
    pub preplaced_channels: Vec<String>,
    /// Dirname collisions — declined by default; `--prefix-dirname` opts into the prefixed paths.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub collisions: Vec<DescribedCollision>,
    /// A colliding name in THIS workspace whose skill id changed — a freed name reassigned to a NEW
    /// skill (the old copy stays retained locally).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub freed_name_notes: Vec<String>,
    /// Present when a targeted skill already arrives via a followed channel — a direct follow keeps
    /// it even if the channel drops it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_follow_note: Option<String>,
    /// Following is person-scoped: every enrolled device of this person receives the same set.
    pub all_devices_note: String,
    /// This device reports its applied versions to the workspace's fleet view after each update.
    pub reporting_note: String,
    /// The `--agent` placement plan for the described installs (which dirs land where), when the
    /// invocation carried an include-list. Empty otherwise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agent_notes: Vec<String>,
}

/// The `--yes` apply report: the rows written, the installs landed, the collisions declined.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FollowApplied {
    pub workspace_id: String,
    /// The workspace's ADDRESS name.
    pub workspace_name: String,
    /// Whether THIS invocation enrolled the device first.
    pub enrolled_now: bool,
    /// The subscription rows this apply wrote (channel joins / direct follows).
    pub subscribed: Vec<DescribedTarget>,
    /// The installs the reconcile landed (batch-accepted first receives + refreshed knowns).
    pub installed: Vec<DescribedInstall>,
    /// The dirname collisions the apply DECLINED (absent under `--prefix-dirname`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub declined: Vec<DescribedCollision>,
    /// The reconcile's isolated warnings (ride the envelope's `warnings` too).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// A per-device exclusion LIFT — "re-attach this device". The target is a skill this person still
/// FOLLOWS but THIS device excluded via `remove` (a `follows.json` `excluded_here` marker + the
/// server exclusion row). `follow <skill>` here lifts it: the server exclusion clears (via
/// [`DirectorySource::follow_skill`], the same row op the web "re-attach" uses — it re-affirms the
/// direct follow AND deletes the CALLING device's exclusion row), the local marker clears, and the
/// reconcile reinstalls the current bytes into the agent dirs. This is a DISTINCT surface from the
/// offer/subscribe paths — a re-attach never re-enrolls and never lands a "first-receive" offer,
/// so the enroll/offer arm can never re-materialize bytes while an exclusion stands.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Reattach {
    pub workspace_id: String,
    /// The workspace's human label (from `user.json`, offline), for the describe.
    pub workspace_name: String,
    pub skill_id: String,
    /// The skill's catalog/local name (its dirname).
    pub name: String,
    /// The current bytes this device re-installs — the last-known current from the local lock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    /// APPLY only: whether the reconcile actually placed the bytes back on this device.
    pub installed: bool,
    /// APPLY: the reconcile's isolated warnings (ride the envelope's `warnings` too).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Dispatch the `follow` verb over its positional targets + selectors, in this precedence order:
///
/// 1. a pending enrollment WAL exists → RESUME it (poll / persist / continue), regardless of the
///    targets — the "re-invoking IS the resume" path;
/// 2. a single `/i/` link → the typed retirement refusal (invite links are retired; join by the
///    workspace ADDRESS);
/// 3. a single bare `<skill>@<digest>` — the name-part MUST be a known followed skill — or a bare
///    word matching a KNOWN followed skill → the classic skill path (place offer / resume a paused
///    entry). **Known-skill-name wins** over the address grammar;
/// 4. everything else is the ADDRESS/SUBSCRIBE grammar ([`crate::resolve`]): full addresses,
///    qualified paths, bare channel/skill names, `--channel`/`--skill` selectors — resolved
///    all-or-none; a single unresolved workspace-shaped target folds the ENROLL flow in; then the
///    two-phase describe / `--yes` apply.
///
/// # Errors
/// [`ClientError::Enrollment`] for a missing target / a retired `/i/` link / a denied or expired
/// flow; [`ClientError::InvalidArgument`] for an `@`-pinned unknown skill or a malformed address;
/// [`ClientError::TargetNotFound`] (the uniform not-found) for an unresolvable target;
/// [`ClientError::AmbiguousTarget`] for a name with several meanings;
/// [`ClientError::PlacementUnsupported`] for a follow against a different plane than the one
/// enrolled; otherwise a transport / io / store failure.
pub(crate) fn follow(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    targets: Vec<String>,
    opts: FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    // 1) A pending enrollment WAL: re-invoking `follow` (with any target, or none) resumes it — so a
    // second follow while one is in flight never clobbers the in-flight flow's single-use secret; it
    // advances it. (A login-owned WAL refuses toward `auth login` inside `resume`.)
    if enroll::read_wal(ctx.fs, &ctx.layout)?.is_some() {
        return resume(ctx, connectors, &opts);
    }
    let ws = opts.workspace.as_deref();
    if targets.is_empty() && opts.channels.is_empty() && opts.skills.is_empty() {
        return Err(ClientError::Enrollment(
            "follow needs a target — a workspace address, or a channel or skill name (or a pending \
             enrollment to resume)"
                .into(),
        ));
    }
    // The BUILT-IN `topos` skill — dispatched before the grammar (the name is reserved end-to-end,
    // so it can never shadow a workspace resource): bare `follow topos` re-places it after a
    // `remove` (or repairs it in place); `--agent` scopes it — on a present built-in the ordinary
    // scope update, on an absent/opted-out one the restore records the include-list in the same
    // act (one invocation, never a refusal toward a second command).
    if let [single] = targets.as_slice()
        && super::builtin::is_builtin(single)
        && opts.channels.is_empty()
        && opts.skills.is_empty()
    {
        return Ok(FollowOutcome::Scope(super::builtin::follow_builtin(
            ctx,
            &opts.agents,
            opts.yes,
        )?));
    }
    if !opts.agents.is_empty() {
        if !opts.channels.is_empty() {
            return Err(ClientError::InvalidArgument(
                "`--agent` scopes where a SKILL's bytes land — it cannot combine with `--channel`"
                    .into(),
            ));
        }
        // Refuse unknown slugs up front (both dispatch arms share the same validation).
        let _ = crate::placement::validate_agent_slugs(ctx, &opts.agents)?;
        // Every target already followed (bare/`@digest` names) ⇒ the offline SCOPE-UPDATE path —
        // "a follow of an already-followed skill with --agent just updates the scope, two-phase".
        // Anything else falls through to the ordinary subscribe, which records the include-list at
        // apply (after the reconcile installs the new follow).
        let mut tokens: Vec<String> = targets.clone();
        tokens.extend(opts.skills.iter().cloned());
        let all_followed = !tokens.is_empty()
            && tokens.iter().all(|t| {
                !t.contains("://")
                    && !t.contains('/')
                    && matches!(known_followed_entry(ctx, strip_digest(t), ws), Ok(Some(_)))
            });
        if all_followed {
            return Ok(FollowOutcome::Scope(super::agent_scope::set_scope(
                ctx,
                &tokens,
                &opts.agents,
                ws,
                opts.yes,
            )?));
        }
    }
    if opts.channels.is_empty()
        && opts.skills.is_empty()
        && let [single] = targets.as_slice()
    {
        // 2) A retired `/i/` invite link. Checked BEFORE `@` so a link carrying userinfo
        // (`https://u@host/i/tok`) or a query param (`?x=a@b`) is never misread as `<skill>@<hash>`.
        if single.contains("/i/") {
            return Err(ClientError::Enrollment(
                "invite links are retired — join by the workspace ADDRESS instead: `topos follow \
                 <server>/<workspace>` (ask your inviter for it)"
                    .into(),
            ));
        }
        if !single.contains("://") && !single.contains('/') {
            // 3a/3b) A bare word (or `<skill>@<digest>`) matching a KNOWN followed skill wins over
            // the address grammar. If THIS device EXCLUDED it (`remove`), `follow` RE-ATTACHES the
            // device (lift the exclusion + reinstall the current bytes) rather than replaying a
            // first-receive offer that would leave the exclusion standing and re-materialize an
            // inconsistent split; otherwise it is the classic accept / paused-resume.
            let name = strip_digest(single);
            if let Some((sid, ws_id, excluded)) = known_followed_entry(ctx, name, ws)? {
                if excluded {
                    // The bare positional path carries no address name to qualify with (the re-attach
                    // describe is offline) — the apply argv preserves the caller's `--workspace` filter
                    // instead (see `reattach`).
                    return reattach(ctx, connectors, &ws_id, sid.as_str(), name, None, &opts);
                }
                return approve(ctx, std::slice::from_ref(single), ws);
            }
            // Not a known followed skill: a `@<digest>` suffix has nothing to pin (typed error); a
            // bare word falls through to the address / subscribe grammar.
            if name != single.as_str() {
                return Err(ClientError::InvalidArgument(format!(
                    "'{name}' is not a followed skill; pass a followed skill name, \
                     `<skill>@<hash>`, or a workspace address"
                )));
            }
        }
    }
    // 4) The address / subscribe grammar.
    subscribe_dispatch(ctx, connectors, &targets, &opts)
}

/// The `(skill_id, workspace_id, excluded_here)` of `name` when it resolves to a tracked skill with a
/// follow entry (following OR `unfollow`-paused OR `remove`-excluded), else `None` — the "known followed
/// skill" test the positional dispatch uses, extended to carry the per-device exclusion marker so the
/// dispatch can route an excluded skill to the re-attach arm. Reads `follows.json` directly (mirroring
/// [`approve`]), so it is correct even when the caller's `ctx.follow` seam is inert. A name that resolves
/// to no tracked skill is not known (→ treat the positional as a link/token); an AMBIGUOUS name propagates
/// its typed error (a genuine collision the user resolves with `--workspace`), never a silent token.
fn known_followed_entry(
    ctx: &Ctx<'_>,
    name: &str,
    workspace: Option<&str>,
) -> Result<Option<(crate::id::SkillId, String, bool)>, ClientError> {
    let Some(follows) = enroll::read_follows(ctx.fs, &ctx.layout)? else {
        return Ok(None);
    };
    match super::resolve_skill_in_workspace(ctx, name, workspace) {
        Ok((id, _)) => Ok(follows
            .follows
            .iter()
            .find(|e| e.skill_id == id.as_str())
            .map(|e| (id.clone(), e.workspace_id.clone(), e.excluded_here))),
        Err(ClientError::NoSuchSkill { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `skill_id` carries a `follows.json` `excluded_here` marker — this device's per-device
/// exclusion, written by `remove`. The routing signal for the qualified `<ws>/skills/<name>` path
/// (the local marker; the server exclusion row the lift clears is the authority).
fn is_excluded_here(ctx: &Ctx<'_>, skill_id: &str) -> Result<bool, ClientError> {
    let Some(follows) = enroll::read_follows(ctx.fs, &ctx.layout)? else {
        return Ok(false);
    };
    Ok(follows
        .follows
        .iter()
        .any(|e| e.skill_id == skill_id && e.excluded_here))
}

/// The one-plane-per-install guard, shared by every enrollment door (`follow <address>`,
/// `auth login`). `base_url` is the plane's API base — the RE-ROOTED base the card declared (never
/// the human origin the address string rode), so it matches what every later call dials. If an
/// `instance.json` already names a DIFFERENT plane, refuse (v0 is one plane per install); otherwise OK.
/// There is no trust root to pin — the `current` pointer is unsigned, its integrity the content-addressed
/// version id re-verified by digest on apply.
pub(super) fn guard_one_plane(ctx: &Ctx<'_>, base_url: &str) -> Result<(), ClientError> {
    if let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)?
        && instance.base_url != base_url
    {
        return Err(wrong_server_refusal(&instance.base_url));
    }
    Ok(())
}

/// The wrong-server refusal every pre-enrollment door shares (`follow <address>`, `auth login`): one
/// plane per install, and the escape hatch is a SECOND install home — named explicitly, because
/// "use another machine" is not an answer an agent can act on.
pub(crate) fn wrong_server_refusal(enrolled_base: &str) -> ClientError {
    ClientError::PlacementUnsupported {
        reason: format!(
            "this install is enrolled with {enrolled_base} (one plane per install) — to use a \
             different server, run under a second install home: TOPOS_HOME=<dir> topos …"
        ),
    }
}

// =================================================================================================
// Call 2 — re-invoking `follow` with a pending WAL: poll once → pending re-emit / typed terminal /
// granted persist + continue into the recorded follow intent.
// =================================================================================================

fn resume(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    opts: &FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    let wal = enroll::read_wal(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment(
            "no enrollment in progress; run `follow <workspace-address>` first".into(),
        )
    })?;
    // A login-owned flow belongs to `auth login` (the same ownership rule holds in reverse there) —
    // so a `follow` while a sign-in is in flight never clobbers its single-use secret.
    let (recorded_target, recorded_mode) = match &wal.intent {
        enroll::EnrollIntentDoc::Login => {
            return Err(ClientError::Enrollment(
                "a sign-in is in progress; re-run `topos auth login` to finish it first".into(),
            ));
        }
        enroll::EnrollIntentDoc::Follow { target, mode } => (target.clone(), *mode),
    };
    let enroll_src = (connectors.enroll)(&wal.base_url);
    match enroll_src.device_auth_poll(&wal.device_code)? {
        // Still pending — re-surface the persisted SERVER-built URL, verbatim (the approval page
        // lives wherever the server put it; the client never reconstructs it).
        DeviceAuthPoll::Pending => Ok(FollowOutcome::plain(pending_followdata(&wal))),
        // A terminal denial / expiry — sweep the WAL, surface a typed error.
        DeviceAuthPoll::Denied => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::EnrollDenied)
        }
        DeviceAuthPoll::Expired => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the enrollment flow expired; start over with `topos follow <workspace-address>`"
                    .into(),
            ))
        }
        // Granted: the poll carries the device's ONE credential + the AUTHORITATIVE workspace (the
        // requested name was only ever a request; unknown/not-yours died at the uniform denial).
        // Persist, then CONTINUE into the recorded follow intent in this same invocation.
        DeviceAuthPoll::Granted(grant) => {
            persist_enrollment(ctx, &wal.base_url, &grant)?;
            let target = recorded_target.unwrap_or(enroll::FollowTargetDoc {
                kind: enroll::FollowKindDoc::Workspace,
                name: wal.workspace_name.clone(),
            });
            // The original invocation's `--manual` rode the WAL; honor it on a resume that omits the
            // flag (a fresh flag still wins — the resumed invocation's consent is the current one).
            let mut opts = opts.clone();
            opts.manual = opts.manual || recorded_mode == enroll::FollowModeDoc::ConfirmEach;
            continue_into_target(
                ctx,
                connectors,
                &wal.base_url,
                &grant.workspace.workspace_id,
                &target,
                &opts,
            )
        }
    }
}

/// Persist a granted enrollment — every durable write, in an order that keeps a crash recoverable by
/// RE-POLLING (an approved flow re-answers the same granted poll until it expires, so no separate
/// post-grant fence phase exists):
///
///   1) `instance.json` — pin the plane (idempotent bytes on a same-plane re-enrollment);
///   2) `credentials.json` — the ONE device credential + registered device id (`0600`, replaced
///      wholesale: a device holds exactly one credential);
///   3) `user.json` — upsert the joined membership (a second workspace ADDS, never drops);
///   4) delete the WAL — the last durable step, so "WAL absent" proves 1-3 completed;
///   5) arm the session-start auto-update trigger — best-effort + idempotent (a pure follower never
///      runs `add`, so enrollment is where their hook gets armed); infallible by construction, so it
///      can never roll back a completed enrollment.
///
/// Idempotent — a re-granted resume re-persists identical facts.
pub(super) fn persist_enrollment(
    ctx: &Ctx<'_>,
    base_url: &str,
    grant: &EnrolledGrant,
) -> Result<topos_types::TriggerReport, ClientError> {
    enroll::write_instance(
        ctx.fs,
        &ctx.layout,
        &enroll::Instance {
            schema_version: PERSISTED_SCHEMA_VERSION,
            base_url: base_url.to_owned(),
        },
    )?;
    enroll::write_credentials(ctx.fs, &ctx.layout, &grant.credential, &grant.device_id)?;
    let mut user = enroll::read_user(ctx.fs, &ctx.layout)?.unwrap_or_default();
    user.schema_version = PERSISTED_SCHEMA_VERSION;
    enroll::upsert_membership(
        &mut user,
        enroll::Membership {
            workspace_id: grant.workspace.workspace_id.clone(),
            name: grant.workspace.name.clone(),
            display_name: grant.workspace.display_name.clone(),
            enrolled_at: now_millis(ctx),
        },
    );
    enroll::write_user(ctx.fs, &ctx.layout, &user)?;
    enroll::delete_wal(ctx.fs, &ctx.layout)?;
    Ok(ctx.harness.install_currency_trigger())
}

// =================================================================================================
// The ADDRESS flow — `follow <workspace>[/channels|skills/<name>]`: card → re-root guard →
// device-authorize (the workspace named by ADDRESS) → WAL → poll/resume → the granted credential
// persists → the two-phase subscribe (describe / `--yes` apply).
// =================================================================================================

/// The enroll intent an unresolved single target may fold in: the workspace ADDRESS name, the
/// follow intent to continue into, and the explicit host when the target was a full URL.
struct EnrollIntent {
    host: Option<String>,
    workspace_name: String,
    target: enroll::FollowTargetDoc,
}

/// Whether an UNRESOLVED parsed target is shaped like a workspace address this install could enroll
/// toward. Only address shapes qualify — a bare word must be a valid ADDRESS name; anything else
/// stays the uniform not-found. An ORIGIN address (empty `workspace`) enrolls toward "the workspace
/// this origin itself addresses" (single-tenant installs) — the empty slug rides the wire body, and
/// the granted poll carries the authoritative workspace back.
fn enroll_intent(parsed: &ParsedTarget) -> Option<EnrollIntent> {
    match parsed {
        ParsedTarget::Address {
            host,
            workspace,
            resource,
        } => {
            // Empty = the origin's own workspace (only valid with an explicit host); a NAMED
            // workspace must be a valid address slug.
            if workspace.is_empty() {
                if host.is_none() {
                    return None;
                }
            } else if !resolve::is_workspace_name(workspace) {
                return None;
            }
            let target = match resource {
                Some((ResourceKind::Channel, name)) => enroll::FollowTargetDoc {
                    kind: enroll::FollowKindDoc::Channel,
                    name: name.clone(),
                },
                Some((ResourceKind::Skill, name)) => enroll::FollowTargetDoc {
                    kind: enroll::FollowKindDoc::Skill,
                    name: name.clone(),
                },
                None => enroll::FollowTargetDoc {
                    kind: enroll::FollowKindDoc::Workspace,
                    name: workspace.clone(),
                },
            };
            Some(EnrollIntent {
                host: host.clone(),
                workspace_name: workspace.clone(),
                target,
            })
        }
        ParsedTarget::Bare(name) if resolve::is_workspace_name(name) => Some(EnrollIntent {
            host: None,
            workspace_name: name.clone(),
            target: enroll::FollowTargetDoc {
                kind: enroll::FollowKindDoc::Workspace,
                name: name.clone(),
            },
        }),
        _ => None,
    }
}

/// Start the ADDRESS enrollment: card fetch at the workspace's own address (the card is constant at
/// every path — no existence signal), re-root onto the declared API base, guard one-plane (the
/// wrong-server refusal names the `TOPOS_HOME` second-install hatch), device-authorize toward the
/// named workspace, and persist the WAL with the follow intent.
fn begin_address(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    intent: EnrollIntent,
    opts: &FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    // The card origin: the address's own host, else the already-pinned plane, else the default web
    // origin the composition root resolved (`TOPOS_PLANE_URL`, else the hosted default).
    let origin = match &intent.host {
        Some(h) => h.trim_end_matches('/').to_owned(),
        None => match enroll::read_instance(ctx.fs, &ctx.layout)? {
            Some(i) => i.base_url,
            None => connectors.web_origin.trim_end_matches('/').to_owned(),
        },
    };
    // The card is constant on every path, so an ORIGIN address (empty workspace) fetches it at the
    // bare origin; a named workspace fetches it at its own address (still no existence signal).
    let card_url = if intent.workspace_name.is_empty() {
        origin.clone()
    } else {
        format!("{origin}/{}", intent.workspace_name)
    };
    let card = (connectors.enroll)(&origin).fetch_card(&card_url)?;
    let base_url = resolve_api_base(&origin, &card.api_base_url)?;
    guard_one_plane(ctx, &base_url)?;

    let start = (connectors.enroll)(&base_url)
        .device_auth_start(&intent.workspace_name, &machine_name())?;
    let expires_at = now_millis(ctx).saturating_add(
        i64::try_from(start.expires_in_secs.saturating_mul(1000)).unwrap_or(i64::MAX),
    );
    let wal = enroll::PendingEnrollment {
        schema_version: PERSISTED_SCHEMA_VERSION,
        base_url,
        workspace_name: intent.workspace_name,
        intent: enroll::EnrollIntentDoc::Follow {
            target: Some(intent.target),
            mode: if opts.manual {
                enroll::FollowModeDoc::ConfirmEach
            } else {
                enroll::FollowModeDoc::Auto
            },
        },
        device_code: start.device_code,
        user_code: start.user_code,
        verification_uri_complete: start.verification_uri_complete,
        interval_secs: start.interval_secs,
        expires_at_millis: expires_at,
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;
    Ok(FollowOutcome::plain(pending_followdata(&wal)))
}

/// The pending `FollowData` an enrollment surfaces (there is no workspace ID yet — the requested
/// ADDRESS name rides the disclosure slot; the id arrives with the grant). The human opens
/// `verification_uri_complete` and cross-checks the code; the agent re-invokes `follow` to poll.
/// An ORIGIN enrollment has no requested name — the plane base stands in for the disclosure, so the
/// slot never reads as an empty string.
fn pending_followdata(wal: &enroll::PendingEnrollment) -> FollowData {
    let requested = if wal.workspace_name.is_empty() {
        wal.base_url.clone()
    } else {
        wal.workspace_name.clone()
    };
    FollowData {
        workspace_id: requested,
        enrolled: false,
        skills: Vec::new(),
        workspace_display_name: None,
        plane_base_url: Some(wal.base_url.clone()),
        pending: Some(EnrollmentPending {
            verification_uri_complete: wal.verification_uri_complete.clone(),
            user_code: wal.user_code.clone(),
            expires_at: Some(fmt_rfc3339_millis(wal.expires_at_millis)),
            interval_secs: Some(wal.interval_secs),
        }),
        currency: None,
        triggers: Vec::new(),
    }
}

/// Epoch-millis → an RFC-3339 `YYYY-MM-DDTHH:MM:SSZ` string (UTC, second precision) — enough for the
/// pending disclosure's expiry. Negative inputs clamp to the epoch.
fn fmt_rfc3339_millis(millis: i64) -> String {
    let secs = millis.max(0) as u64 / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = crate::render::civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// =================================================================================================
// The two-phase SUBSCRIBE — resolve (all-or-none) → describe (bare) → apply (`--yes`): the row ops,
// then the delivery-driven reconcile landing the set THIS invocation (batch-accepted first
// receives), then the fleet report. Nothing mutates before `--yes` except the enrollment itself.
// =================================================================================================

/// The address/subscribe dispatch: build the target specs (positionals + selectors), resolve them
/// all-or-none against the enrolled universe, fold the ENROLL flow in for a single unresolved
/// workspace-shaped target, and run the one-workspace describe/apply.
fn subscribe_dispatch(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    targets: &[String],
    opts: &FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    let mut specs: Vec<resolve::TargetSpec> = targets
        .iter()
        .map(|t| resolve::TargetSpec::free(t))
        .collect();
    specs.extend(
        opts.channels
            .iter()
            .map(|c| resolve::TargetSpec::kinded(c, ResourceKind::Channel)),
    );
    specs.extend(
        opts.skills
            .iter()
            .map(|s| resolve::TargetSpec::kinded(s, ResourceKind::Skill)),
    );

    let (base_url, universe) = build_universe_via(ctx, connectors.directory)?;

    let mut resolutions = Vec::with_capacity(specs.len());
    for spec in &specs {
        let parsed = resolve::parse_target(&spec.token)?;
        let scope = match spec.forced {
            Some(ResourceKind::Channel) => resolve::KindScope::CHANNELS,
            Some(ResourceKind::Skill) => resolve::KindScope::SKILLS,
            None => resolve::KindScope::ALL,
        };
        match resolve::resolve_one(&universe, &parsed, scope)? {
            Some(r) => resolutions.push(r),
            None => {
                // Unresolved. A SINGLE free-kind, workspace-shaped target folds the enroll flow in;
                // a workspace this install is ALREADY enrolled in never re-enrolls (its unknown
                // resource is the uniform not-found), and a batch resolves all-or-none.
                if specs.len() == 1
                    && spec.forced.is_none()
                    && let Some(intent) = enroll_intent(&parsed)
                {
                    if universe.iter().any(|w| w.name == intent.workspace_name) {
                        return Err(resolve::not_found(&spec.token));
                    }
                    return begin_address(ctx, connectors, intent, opts);
                }
                return Err(resolve::not_found(&spec.token));
            }
        }
    }

    // A SINGLE followed-but-EXCLUDED skill target RE-ATTACHES this device (lift the exclusion +
    // reinstall the current bytes) instead of replaying a person-scope subscribe that would leave the
    // device exclusion standing. This is how the qualified `<ws>/skills/<name>` path reaches the same
    // arm the bare positional does — but ONLY for a single target: a MULTI-target subscribe (even one
    // whose targets include an excluded skill) falls through to the classic apply below, which clears
    // each re-affirmed skill's stale marker itself. A fresh (never-followed) or non-excluded skill also
    // stays on the ordinary subscribe describe/apply below. The resolved `workspace_name` (the address
    // slug) qualifies the apply argv so a re-run resolves to this workspace even for a shared name.
    if let [
        Resolution::Resource {
            kind: ResourceKind::Skill,
            skill_id: Some(sid),
            name,
            workspace_id,
            workspace_name,
            ..
        },
    ] = resolutions.as_slice()
        && is_excluded_here(ctx, sid)?
    {
        return reattach(
            ctx,
            connectors,
            workspace_id,
            sid,
            name,
            Some(workspace_name),
            opts,
        );
    }

    // One workspace per invocation: the describe is one workspace's story, and the apply's
    // reconcile+report scope one workspace's delivery.
    let ws_id = resolutions[0].workspace_id().to_owned();
    if resolutions.iter().any(|r| r.workspace_id() != ws_id) {
        return Err(ClientError::InvalidArgument(
            "these targets span more than one workspace — follow one workspace per invocation"
                .into(),
        ));
    }
    let base_url = base_url.ok_or_else(|| {
        // Unreachable in practice (a resolution implies an enrolled universe) — fail closed anyway.
        ClientError::Enrollment("not enrolled; follow a workspace address first".into())
    })?;
    subscribe(
        ctx,
        connectors,
        &base_url,
        &ws_id,
        &resolutions,
        opts,
        false,
    )
}

/// Assemble the resolver universe over the enrolled workspaces: the pinned plane base + one
/// [`resolve::WorkspaceNames`] per membership (address name from `/me`, channel names, catalog
/// skills). A workspace whose reads answer the uniform not-found (revoked / removed) is skipped —
/// its names must not resolve; a transport fault propagates (resolution must not silently narrow).
/// Shared with `unfollow` (and any later dual-kind verb), hence the connector-level parameter.
pub(super) fn build_universe_via(
    ctx: &Ctx<'_>,
    directory_connect: &DirectoryConnect<'_>,
) -> Result<(Option<String>, Vec<resolve::WorkspaceNames>), ClientError> {
    let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)? else {
        return Ok((None, Vec::new()));
    };
    let memberships: Vec<String> = enroll::read_user(ctx.fs, &ctx.layout)?
        .map(|u| u.workspaces.into_iter().map(|m| m.workspace_id).collect())
        .unwrap_or_default();
    // No memberships ⇒ nothing to read (and no transport to build — an enrolled-but-memberless
    // install must stay on the offline-graceful paths).
    if memberships.is_empty() {
        return Ok((Some(instance.base_url), Vec::new()));
    }
    let directory = (directory_connect)(&instance.base_url);
    let mut universe = Vec::with_capacity(memberships.len());
    for ws in memberships {
        match universe_for(&*directory, &ws) {
            Ok(names) => universe.push(names),
            Err(ClientError::TargetNotFound { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok((Some(instance.base_url), universe))
}

/// One workspace's resolver names, from the directory reads.
fn universe_for(
    directory: &dyn DirectorySource,
    workspace_id: &str,
) -> Result<resolve::WorkspaceNames, ClientError> {
    let me = directory.me(workspace_id)?;
    let channels = directory.channels_index(workspace_id)?;
    let skills = directory.skills_index(workspace_id)?;
    Ok(resolve::WorkspaceNames::from_wire(
        workspace_id,
        &me.name,
        &channels,
        &skills,
    ))
}

/// Continue a just-enrolled `follow` into its recorded follow intent: resolve the intent WITHIN the
/// newly-joined workspace, then describe/apply per this invocation's flags. A bare resumed `follow`
/// therefore lands on the DESCRIBE (with the `--yes` argv as its next action) — the enrollment
/// happened, the subscription still waits for consent.
fn continue_into_target(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    base_url: &str,
    workspace_id: &str,
    target: &enroll::FollowTargetDoc,
    opts: &FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    let directory = (connectors.directory)(base_url);
    let names = universe_for(&*directory, workspace_id)?;
    let resolution = match target.kind {
        enroll::FollowKindDoc::Workspace => Resolution::Workspace {
            workspace_id: workspace_id.to_owned(),
            workspace_name: names.name.clone(),
        },
        enroll::FollowKindDoc::Channel => {
            let universe = std::slice::from_ref(&names);
            resolve::resolve_one(
                universe,
                &ParsedTarget::Bare(target.name.clone()),
                resolve::KindScope::CHANNELS,
            )?
            .ok_or_else(|| resolve::not_found(&target.name))?
        }
        enroll::FollowKindDoc::Skill => {
            let universe = std::slice::from_ref(&names);
            resolve::resolve_one(
                universe,
                &ParsedTarget::Bare(target.name.clone()),
                resolve::KindScope::SKILLS,
            )?
            .ok_or_else(|| resolve::not_found(&target.name))?
        }
    };
    subscribe(
        ctx,
        connectors,
        base_url,
        workspace_id,
        std::slice::from_ref(&resolution),
        opts,
        true,
    )
}

/// The two-phase subscribe over ONE workspace's resolved targets: assemble the describe from the
/// member-scoped reads; bare = return it (nothing mutated); `--yes` = the row ops, the reconcile
/// (batch-accepted first receives, collisions declined or prefixed), and the report.
fn subscribe(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    base_url: &str,
    ws_id: &str,
    resolutions: &[Resolution],
    opts: &FollowOpts,
    enrolled_now: bool,
) -> Result<FollowOutcome, ClientError> {
    let directory = (connectors.directory)(base_url);
    let me = directory.me(ws_id)?;
    let channels = directory.channels_index(ws_id)?;
    let catalog = directory.skills_index(ws_id)?;
    let delivery = (connectors.delivery)(base_url);
    let snapshot = delivery.fetch_delivery(ws_id).map_err(|e| match e {
        PlaneError::NotFound => resolve::not_found(&me.name),
        PlaneError::Unreachable(m) | PlaneError::Unavailable(m) => ClientError::Plane(m),
        PlaneError::Malformed(m) => ClientError::WireInvalid(m),
    })?;
    let mut describe = build_describe(
        ctx,
        &me,
        &channels,
        &catalog,
        &snapshot,
        resolutions,
        enrolled_now,
    )?;
    if !opts.agents.is_empty() {
        describe.agent_notes = agent_plan_notes(ctx, opts, &describe.installs, &me.name);
    }

    if !opts.yes {
        // The paste-ready apply argvs: the canonical qualified paths + `--yes` (and the
        // `--prefix-dirname` variant when collisions exist).
        let mut base_argv = vec!["topos".to_owned(), "follow".to_owned()];
        for r in resolutions {
            base_argv.push(match r {
                Resolution::Workspace { workspace_name, .. } => workspace_name.clone(),
                Resolution::Resource {
                    workspace_name,
                    kind,
                    name,
                    ..
                } => format!("{workspace_name}/{}/{name}", kind.segment()),
            });
        }
        // `--manual` must ride the apply argv: without it the suggested next action installs in the
        // default AUTO mode, so later updates auto-land despite the confirm-each consent the user chose.
        if opts.manual {
            base_argv.push("--manual".to_owned());
        }
        let mut yes = base_argv.clone();
        yes.push("--yes".to_owned());
        let mut next_argvs = vec![yes];
        if !describe.collisions.is_empty() {
            let mut prefixed = base_argv;
            prefixed.push("--prefix-dirname".to_owned());
            prefixed.push("--yes".to_owned());
            next_argvs.push(prefixed);
        }
        return Ok(FollowOutcome::Described {
            describe: Box::new(describe),
            next_argvs,
        });
    }

    // ---- APPLY (`--yes`) ----
    // 0) Refresh the stored membership facts from this member-authenticated `me` read — the first
    //    place the workspace's true display name and the person's principal are known client-side
    //    (the enrollment poll deliberately disclosed the minimum).
    enroll::refresh_membership_facts(ctx.fs, &ctx.layout, ws_id, &me.display_name, &me.principal)?;
    // 1) The subscription rows — the consented change (a workspace target needs none: membership
    //    itself entitles `everyone`).
    let mut subscribed = Vec::new();
    for r in resolutions {
        if let Resolution::Resource {
            kind,
            name,
            skill_id,
            ..
        } = r
        {
            match kind {
                ResourceKind::Channel => directory.channel_join(ws_id, name)?,
                ResourceKind::Skill => {
                    let id = skill_id.as_deref().ok_or_else(|| {
                        ClientError::WireInvalid("a resolved skill carried no id".into())
                    })?;
                    directory.follow_skill(ws_id, id)?;
                    // The `follow_skill` PUT lifted any SERVER exclusion of this skill; clear the local
                    // per-device marker to match (the single-excluded-target case re-attaches instead, so
                    // this only fires for a MULTI-target subscribe that swept an excluded skill in). A
                    // no-op when nothing was excluded; the reconcile below reinstalls the bytes.
                    enroll::set_excluded(ctx.fs, &ctx.layout, id, false)?;
                }
            }
            subscribed.push(DescribedTarget {
                kind: kind.noun().to_owned(),
                name: name.clone(),
            });
        }
    }

    // 2) The reconcile lands the set THIS invocation — batch-accepting first receives (the describe
    //    disclosed them), declining or prefixing the collisions, one workspace only. The notices
    //    stay unacked (they belong to `update`'s narration).
    let mut rec_opts = ReconcileOpts {
        accept_first_receive: true,
        only_workspace: Some(ws_id.to_owned()),
        ack_notices: false,
        // `--manual` threads through to the adopted entries: every later update is an offer.
        confirm_each: opts.manual,
        ..ReconcileOpts::default()
    };
    for c in &describe.collisions {
        if opts.prefix_dirname {
            rec_opts
                .rename
                .insert(c.skill_id.clone(), c.prefixed_dirname.clone());
        } else {
            rec_opts.decline.insert(c.skill_id.clone());
        }
    }
    // The reconcile's byte fetches ride the SAME transport as the delivery (the engine ctx's plane
    // is swapped onto it) — a mid-invocation enrollment's ctx still carries the inert startup
    // plane, and `bind_skill` must land on the object the fetches use.
    let plane_ref: &dyn PlaneSource = &*delivery;
    let sweep_ctx = super::pull::ctx_with_plane(ctx, plane_ref);
    let delivery_ref: &dyn DeliverySource = &*delivery;
    let out = super::pull::pull_reconcile_with(&sweep_ctx, delivery_ref, &rec_opts)?;

    // 3) The apply report: which of the described installs actually landed (an isolated per-skill
    //    failure stays a warning — the reconcile's isolation semantics hold here too). The rows key
    //    by the skill's local NAME (its dirname), so a `--prefix-dirname` install matches under the
    //    prefixed spelling.
    let landed: HashMap<&str, PullAction> = out
        .data
        .skills
        .iter()
        .map(|row| (row.skill.as_str(), row.action))
        .collect();
    let installed = describe
        .installs
        .iter()
        .filter(|i| {
            let prefixed = format!("{}.{}", me.name, i.name);
            let action = landed
                .get(i.name.as_str())
                .or_else(|| landed.get(prefixed.as_str()));
            matches!(
                action,
                Some(PullAction::FastForwarded | PullAction::UpToDate | PullAction::Merged)
            )
        })
        .cloned()
        .collect();
    let declined = if opts.prefix_dirname {
        Vec::new()
    } else {
        describe.collisions.clone()
    };
    // The `--agent` include-list rides the apply: record it on each directly-followed skill and
    // reconcile its placements (clean what the scope removes, land the native dirs from the local
    // store). Runs AFTER the reconcile so the first receive has already laid bytes to re-scope.
    if !opts.agents.is_empty() {
        let scope: Vec<String> = if opts.agents.iter().any(|a| a == "*") {
            Vec::new()
        } else {
            opts.agents.clone()
        };
        for r in resolutions {
            let Resolution::Resource {
                kind: ResourceKind::Skill,
                skill_id: Some(id),
                ..
            } = r
            else {
                continue;
            };
            enroll::set_agent_scope(ctx.fs, &ctx.layout, id, &scope)?;
            let Ok(sid) = crate::id::SkillId::parse(id) else {
                continue;
            };
            // A declined collision (or a still-pending offer) laid no lock — nothing to re-scope yet;
            // the recorded include-list engages when the bytes land.
            if let Some(lock) = doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&sid).lock)? {
                super::agent_scope::apply_scope_change(
                    ctx,
                    &sid,
                    &lock,
                    crate::placement::AgentScope {
                        agents: &scope,
                        excluded: &[],
                    },
                )?;
            }
        }
    }
    Ok(FollowOutcome::Applied(Box::new(FollowApplied {
        workspace_id: ws_id.to_owned(),
        workspace_name: me.name,
        enrolled_now,
        subscribed,
        installed,
        declined,
        warnings: out.warnings,
    })))
}

/// Assemble the DESCRIBE: everything a `--yes` would land (the pending first-receives already
/// delivered, plus the targets' additions), who you are here, the pre-placements, the dirname
/// collisions with the prefixed choice, and the standing disclosures.
fn build_describe(
    ctx: &Ctx<'_>,
    me: &WireMe,
    channels: &WireChannelIndex,
    catalog: &WireSkillIndex,
    snapshot: &DeliverySnapshot,
    resolutions: &[Resolution],
    enrolled_now: bool,
) -> Result<FollowDescribe, ClientError> {
    let mut installs: Vec<DescribedInstall> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut direct_follow_note = None;

    // The delivered-but-not-yet-received set: `--yes` batch-accepts these pending first receives,
    // so the describe must list them (they land with everything else).
    for ds in &snapshot.skills {
        if locally_received(ctx, &ds.skill_id)? {
            continue;
        }
        seen.insert(ds.skill_id.clone());
        installs.push(DescribedInstall {
            skill_id: ds.skill_id.clone(),
            name: ds.name.clone(),
            version_id: Some(to_hex(&ds.version_id)),
            bundle_digest: Some(to_hex(&ds.bundle_digest)),
            via_channels: ds.via_channels.clone(),
            via_direct: ds.via_direct,
        });
    }

    // The targets' additions (what the subscription would NEWLY entitle).
    for r in resolutions {
        let Resolution::Resource {
            kind,
            name,
            skill_id,
            ..
        } = r
        else {
            continue; // A workspace target adds nothing beyond the delivered set above.
        };
        match kind {
            ResourceKind::Channel => {
                let Some(entry) = channels.channels.iter().find(|c| &c.name == name) else {
                    continue; // Resolved a moment ago; a raced deletion surfaces at apply.
                };
                for skill in &entry.skills {
                    if locally_received(ctx, &skill.skill_id)? {
                        continue;
                    }
                    if seen.contains(&skill.skill_id) {
                        // Already delivered (e.g. via `everyone`) — attribute this channel too.
                        if let Some(i) = installs.iter_mut().find(|i| i.skill_id == skill.skill_id)
                            && !i.via_channels.contains(name)
                        {
                            i.via_channels.push(name.clone());
                        }
                        continue;
                    }
                    seen.insert(skill.skill_id.clone());
                    let cat = catalog.skills.iter().find(|s| s.skill_id == skill.skill_id);
                    installs.push(DescribedInstall {
                        skill_id: skill.skill_id.clone(),
                        name: skill.name.clone(),
                        version_id: cat.map(|c| c.version_id.clone()),
                        bundle_digest: cat.map(|c| c.bundle_digest.clone()),
                        via_channels: vec![name.clone()],
                        via_direct: false,
                    });
                }
            }
            ResourceKind::Skill => {
                // A skill already delivered via channels: the direct follow still adds a row — the
                // explanation is WHY it is not redundant.
                if let Some(ds) = snapshot.skills.iter().find(|s| &s.name == name)
                    && !ds.via_channels.is_empty()
                {
                    direct_follow_note = Some(format!(
                        "'{name}' already arrives via #{} — a direct follow keeps it even if the \
                         channel drops it",
                        ds.via_channels.join(", #")
                    ));
                }
                if seen.contains(skill_id.as_deref().unwrap_or_default()) {
                    if let Some(i) = installs
                        .iter_mut()
                        .find(|i| Some(i.skill_id.as_str()) == skill_id.as_deref())
                    {
                        i.via_direct = true;
                    }
                    continue;
                }
                let Some(cat) = catalog
                    .skills
                    .iter()
                    .find(|s| Some(s.skill_id.as_str()) == skill_id.as_deref())
                else {
                    continue;
                };
                if locally_received(ctx, &cat.skill_id)? {
                    continue;
                }
                seen.insert(cat.skill_id.clone());
                installs.push(DescribedInstall {
                    skill_id: cat.skill_id.clone(),
                    name: cat.name.clone(),
                    version_id: Some(cat.version_id.clone()),
                    bundle_digest: Some(cat.bundle_digest.clone()),
                    via_channels: Vec::new(),
                    via_direct: true,
                });
            }
        }
    }

    // Dirname collisions + the freed-name notes, over the would-land set.
    let tracked = tracked_names(ctx)?;
    let mut collisions = Vec::new();
    let mut freed_name_notes = Vec::new();
    for inst in &installs {
        let Some(existing) = tracked
            .iter()
            .find(|t| t.name == inst.name && t.skill_id != inst.skill_id)
        else {
            continue;
        };
        collisions.push(DescribedCollision {
            skill_id: inst.skill_id.clone(),
            name: inst.name.clone(),
            existing: existing
                .placement
                .clone()
                .unwrap_or_else(|| format!("tracked skill {}", existing.skill_id)),
            prefixed_dirname: format!("{}.{}", me.name, inst.name),
        });
        if existing.workspace_id.as_deref() == Some(me.workspace_id.as_str()) {
            freed_name_notes.push(format!(
                "'{}' is a NEW skill under a previously-used name in this workspace — your \
                 existing copy ({}) stays retained and is NOT this skill's history",
                inst.name, existing.skill_id
            ));
        }
    }

    let targets = resolutions
        .iter()
        .map(|r| match r {
            Resolution::Workspace { workspace_name, .. } => DescribedTarget {
                kind: "workspace".to_owned(),
                name: workspace_name.clone(),
            },
            Resolution::Resource { kind, name, .. } => DescribedTarget {
                kind: kind.noun().to_owned(),
                name: name.clone(),
            },
        })
        .collect();
    let preplaced_channels = channels
        .channels
        .iter()
        .filter(|c| c.member && !c.builtin)
        .map(|c| c.name.clone())
        .collect();

    Ok(FollowDescribe {
        workspace: DescribedWorkspace {
            workspace_id: me.workspace_id.clone(),
            name: me.name.clone(),
            display_name: me.display_name.clone(),
            address: me.address.clone(),
        },
        role: me.role.clone(),
        invited_by: me.invited_by.clone(),
        enrolled_now,
        targets,
        installs,
        preplaced_channels,
        collisions,
        freed_name_notes,
        direct_follow_note,
        all_devices_note: format!(
            "following is person-scoped: every device enrolled as {} receives this set",
            me.principal
        ),
        reporting_note: "this device reports its applied versions to the workspace's fleet view \
                         after each update"
            .to_owned(),
        agent_notes: Vec::new(),
    })
}

/// The `--agent` placement-plan lines for a describe: per install, which dirs the scoped placement
/// lands in (native only — a scope never uses the shared dir), plus the honest per-agent notes
/// (an undetected slug engages later; a docs-level coverage claim is named as such when the scope
/// clears back to unscoped).
fn agent_plan_notes(
    ctx: &Ctx<'_>,
    opts: &FollowOpts,
    installs: &[DescribedInstall],
    workspace_slug: &str,
) -> Vec<String> {
    let scope: Vec<String> = if opts.agents.iter().any(|a| a == "*") {
        Vec::new()
    } else {
        opts.agents.clone()
    };
    let undetected = crate::placement::validate_agent_slugs(ctx, &scope).unwrap_or_default();
    let mut notes = Vec::new();
    for inst in installs {
        let plan = crate::placement::plan_targets(
            ctx,
            &inst.skill_id,
            topos_harness::PlacementNaming {
                name: Some(&inst.name),
                workspace_slug: Some(workspace_slug),
            },
            crate::placement::AgentScope {
                agents: &scope,
                excluded: &[],
            },
            None,
        );
        for target in &plan.targets {
            let where_ = match (&target.kind, &target.agent) {
                (topos_types::persisted::PlacementKind::Shared, _) => {
                    "the shared agents dir".to_owned()
                }
                (_, Some(a)) => format!("{a} (native)"),
                _ => "native".to_owned(),
            };
            notes.push(format!(
                "{} → {} — {where_}",
                inst.name,
                target.dir.display()
            ));
        }
        for c in &plan.shared_covers {
            if c.docs_level {
                notes.push(format!(
                    "the shared dir covers {} (per vendor docs — not yet verified against a live \
                     build)",
                    c.slug
                ));
            }
        }
    }
    for slug in &undetected {
        notes.push(format!(
            "'{slug}' is not detected on this machine — placement engages when the agent is detected"
        ));
    }
    notes
}

/// Whether this device has RECEIVED a skill (a sidecar dir exists past the never-received
/// baseline). A never-received baseline, or no sidecar at all, is an install candidate.
fn locally_received(ctx: &Ctx<'_>, skill_id: &str) -> Result<bool, ClientError> {
    let Ok(sid) = crate::id::SkillId::parse(skill_id) else {
        return Ok(false);
    };
    if !ctx.fs.exists(&ctx.layout.skill_dir(&sid)) {
        return Ok(false);
    }
    let sync: Option<SyncState> = doc::read_doc(ctx.fs, &ctx.layout.published(&sid).sync)?;
    Ok(sync
        .as_ref()
        .is_some_and(|s| !sync_engine::is_never_received(s)))
}

/// One locally-tracked skill's naming facts — the collision detector's input.
struct TrackedName {
    name: String,
    skill_id: String,
    /// Its first placement dir, when the map records one.
    placement: Option<String>,
    /// The workspace its follow entry names, when followed.
    workspace_id: Option<String>,
}

/// Every tracked skill's `(name, id, placement, workspace)` — read from the sidecar walk + the
/// follow-state (directly from `follows.json`, so it is correct even under an inert `ctx.follow`).
fn tracked_names(ctx: &Ctx<'_>) -> Result<Vec<TrackedName>, ClientError> {
    let followed: HashMap<String, String> = enroll::read_follows(ctx.fs, &ctx.layout)?
        .map(|f| {
            f.follows
                .into_iter()
                .map(|e| (e.skill_id, e.workspace_id))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    if !ctx.fs.exists(&ctx.layout.skills_dir()) {
        return Ok(out);
    }
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        let Ok(sid) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let Some(lock) = doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&sid).lock)? else {
            continue;
        };
        let placement = doc::read_map(ctx.fs, &ctx.layout.published(&sid).map)?
            .and_then(|m| m.placements.first().cloned());
        out.push(TrackedName {
            name: lock.name,
            workspace_id: followed.get(sid.as_str()).cloned(),
            skill_id: sid.into_string(),
            placement,
        });
    }
    Ok(out)
}

// =================================================================================================
// The never-received baseline — the sidecar scaffold a brand-new arrival's first receive lands into.
// =================================================================================================

/// Lay the NEVER-RECEIVED sidecar baseline for `skill_id` (mirrors `ops::add`'s staged-then-renamed,
/// all-or-nothing publish). A fresh `sync` (`observed = applied = 0`, empty `recorded`), an empty
/// `lock` (the name + zero base/digest, no files), and a `map` carrying the harness placement target (so
/// the existing apply path can first-install there) but no applied content. Idempotent: a skill dir that
/// already exists (already baselined, or received) is left untouched — `follow` never clobbers bytes.
pub(crate) fn lay_first_receive_baseline(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    name: String,
    workspace_slug: &str,
) -> Result<(), ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    if ctx.fs.exists(&ctx.layout.skill_dir(skill_id)) {
        return Ok(());
    }

    let (staging_base, sp) = ctx.layout.staging(skill_id);
    if ctx.fs.exists(&staging_base) {
        ctx.fs.remove_dir_all(&staging_base)?;
    }
    ctx.fs.create_dir_all(&sp.store)?;
    // An empty embedded-git store the first received version is later written into. The full-tree
    // durability set is exactly right HERE (and only here + `add`'s staging import): the store is a
    // fresh `init_bare`, so the whole tree IS this op's writes (the repo scaffolding — HEAD / config /
    // objects/ / refs/) and never carries history.
    let store = Store::init(&sp.store)?;
    super::sync_engine::fsync_batch(ctx, &store.durability_set()?)?;

    // The placement TARGETS come from the engine (shared-dir-first over the detected agents; the
    // classic active-adapter dir when nothing is detected). The id is the validated newtype, honoring
    // the adapter/registry "callers pass an already-validated id" contract; the display name +
    // workspace slug are UNTRUSTED advisory hints — the naming discipline sanitizes them and falls
    // back to the id, so they can never redirect a placement. A brand-new arrival is UNSCOPED (no
    // follow entry exists yet; a later `--agent` scope narrows through the scope verbs).
    let plan = crate::placement::plan_targets(
        ctx,
        skill_id.as_str(),
        topos_harness::PlacementNaming {
            name: Some(&name),
            workspace_slug: Some(workspace_slug),
        },
        crate::placement::AgentScope::default(),
        None,
    );
    doc::write_doc(
        ctx.fs,
        &sp.sync,
        &SyncState {
            schema_version: PERSISTED_SCHEMA_VERSION,
            observed: GENESIS,
            observed_version_id: ZERO_HEX.to_owned(),
            applied: GENESIS,
            base_commit: ZERO_HEX.to_owned(),
            work_hash: ZERO_HEX.to_owned(),
            held: false,
        },
    )?;
    let baseline = PlacementMap {
        schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
        placements: Vec::new(),
        applied_commit: ZERO_HEX.to_owned(),
        materialized_sha: ZERO_HEX.to_owned(),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        placement_state: Vec::new(),
        harness: Some(ctx.harness.id()),
        harness_layer: None,
        harness_slug: Some(ctx.harness.id().slug().to_owned()),
    };
    doc::write_map(
        ctx.fs,
        &sp.map,
        &crate::placement::reconcile_map(&baseline, &plan),
    )?;
    // lock LAST — the commit marker (recovery keeps a dir only when lock.json is present).
    doc::write_doc(
        ctx.fs,
        &sp.lock,
        &Lock {
            schema_version: PERSISTED_SCHEMA_VERSION,
            skill_id: skill_id.to_string(),
            name,
            base_commit: ZERO_HEX.to_owned(),
            bundle_digest: ZERO_HEX.to_owned(),
            files: Vec::new(),
        },
    )?;

    match ctx
        .fs
        .rename_dir_noreplace(&staging_base, &ctx.layout.skill_dir(skill_id))
    {
        Ok(()) => {}
        // Raced a concurrent baseline/receive — keep theirs, clean our staging.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            ctx.fs.remove_dir_all(&staging_base)?;
            return Ok(());
        }
        Err(e) => return Err(ClientError::Io(format!("publish baseline {skill_id}: {e}"))),
    }
    ctx.fs.fsync_dir(&ctx.layout.skills_dir())?;
    Ok(())
}

// =================================================================================================
// The skill path — drive the existing pull engine to place the named first-receive bytes.
// =================================================================================================

fn approve(
    ctx: &Ctx<'_>,
    targets: &[String],
    workspace: Option<&str>,
) -> Result<FollowOutcome, ClientError> {
    let follows = enroll::read_follows(ctx.fs, &ctx.layout)?
        .ok_or_else(|| ClientError::Enrollment("not enrolled; nothing to accept".into()))?;
    let contexts = enroll::follow_contexts(&follows);
    // The disclosed workspace is the FIRST approved skill's OWN follow-entry workspace (per-skill) — never
    // a global first-follow, which would name the wrong workspace once the install follows skills across
    // several workspaces on the same plane.
    let mut workspace_id = String::new();

    let mut skills = Vec::new();
    let mut resumed = Vec::new();
    for target in targets {
        // Strip an optional `@<digest>` (the disclosed-offer reference) and resolve by skill name. The
        // `--workspace` filter disambiguates a name followed in two workspaces on the same plane (an
        // unscoped local skill of the same name still survives the filter — the lenient resolve).
        let name = strip_digest(target);
        let (skill_id, lock) = super::resolve_skill_in_workspace(ctx, name, workspace)?;
        let mut was_resumed = false;
        if let Some((_, follow_ctx)) = contexts.iter().find(|(id, _)| id == skill_id.as_str()) {
            if workspace_id.is_empty() {
                workspace_id = follow_ctx.workspace_id.clone();
            }
            if follow_ctx.following {
                // The explicit accept IS the I-TOFU first-receive yes (places the bytes).
                sync_engine::sync_one(ctx, &skill_id, follow_ctx, Invocation::Accept)?;
            } else {
                // A retained-but-paused entry (what `unfollow` keeps): the skill path RESUMES it — the
                // command every paused surface points at. Flip the durable flag first; then, if a
                // first-receive offer is still pending, place it as a normal approve. Otherwise nothing
                // is pulled here — the resume is disclosed and the next `pull` lands the team's current.
                enroll::set_following(ctx.fs, &ctx.layout, skill_id.as_str(), true)?;
                was_resumed = true;
                let sync: Option<SyncState> =
                    doc::read_doc(ctx.fs, &ctx.layout.published(&skill_id).sync)?;
                if sync.as_ref().is_some_and(sync_engine::is_never_received) {
                    let resumed_ctx = FollowContext {
                        following: true,
                        ..follow_ctx.clone()
                    };
                    sync_engine::sync_one(ctx, &skill_id, &resumed_ctx, Invocation::Accept)?;
                }
            }
        }
        // Re-read the lock to disclose what is now current locally.
        let updated =
            doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&skill_id).lock)?.unwrap_or(lock);
        if was_resumed {
            resumed.push(updated.name.clone());
        }
        skills.push(FollowOffer {
            skill_id: skill_id.into_string(),
            name: updated.name.clone(),
            offer: Offer {
                version_id: updated.base_commit.clone(),
                bundle_digest: updated.bundle_digest.clone(),
            },
        });
    }

    Ok(FollowOutcome::Data {
        data: FollowData {
            workspace_id,
            enrolled: true,
            skills,
            workspace_display_name: None,
            plane_base_url: None,
            pending: None,
            currency: None,
            triggers: Vec::new(),
        },
        resumed,
    })
}

// =================================================================================================
// The per-device exclusion LIFT — "re-attach this device".
// =================================================================================================

/// Re-attach a skill this person follows but THIS device excluded via `remove`. Reached from BOTH the
/// bare positional path (`address_name = None` — the describe is offline, so it has no address slug to
/// qualify with) and the qualified `<ws>/skills/<name>` subscribe path (`address_name = Some(slug)`).
/// Two-phase:
/// - bare = DESCRIBE the re-attach (nothing mutated) — the truthful "lift the exclusion, reinstall the
///   current bytes" surface, NOT a "first-receive offer" (which would tell the user to re-run the very
///   command they just ran, and re-materialize bytes while the exclusion still stood);
/// - `--yes` = APPLY: (a) lift the SERVER exclusion via [`DirectorySource::follow_skill`] — the row op
///   re-affirms the direct follow AND, because it carries THIS device's credential, deletes this
///   device's exclusion row (the same op the web "re-attach" speaks; no new wire shape); (b) clear the
///   local `excluded_here` marker AND re-affirm the local follow (a prior `unfollow` may have paused it,
///   and the PUT above re-affirmed the person's direct follow — so the local entry must converge too, or
///   the reconcile's `!following` guard skips it and "lands on next update" is a lie); (c) reconcile ONLY
///   this skill's current bytes back into the agent dirs (the never-received baseline `remove` laid makes
///   the accepted first receive PLACE) — every OTHER pending first-receive stays an undisclosed offer;
///   (d) report honestly.
fn reattach(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    workspace_id: &str,
    skill_id: &str,
    name: &str,
    address_name: Option<&str>,
    opts: &FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    // The current bytes to reinstall — from the local lock, kept through the exclusion (`remove`
    // cleans the agent dirs, never the sidecar). Offline; the describe needs no network.
    let sid = crate::id::SkillId::parse(skill_id)?;
    let lock: Option<Lock> = doc::read_doc(ctx.fs, &ctx.layout.published(&sid).lock)?;
    let version_id = lock.as_ref().map(|l| l.base_commit.clone());
    let bundle_digest = lock.as_ref().map(|l| l.bundle_digest.clone());
    let workspace_name = workspace_label(ctx, workspace_id);

    if !opts.yes {
        let reattach = Reattach {
            workspace_id: workspace_id.to_owned(),
            workspace_name,
            skill_id: skill_id.to_owned(),
            name: name.to_owned(),
            version_id,
            bundle_digest,
            installed: false,
            warnings: Vec::new(),
        };
        let yes_argv = reattach_yes_argv(name, address_name, opts.workspace.as_deref());
        return Ok(FollowOutcome::ReattachDescribed {
            reattach: Box::new(reattach),
            yes_argv,
        });
    }

    // ---- APPLY (`--yes`) ----
    let base_url = enroll::read_instance(ctx.fs, &ctx.layout)?
        .map(|i| i.base_url)
        .ok_or_else(|| ClientError::Enrollment("not enrolled; nothing to re-attach".into()))?;
    let directory = (connectors.directory)(&base_url);
    // (a) Lift the SERVER exclusion — `follow_skill` re-affirms the person's direct follow AND deletes
    //     THIS device's exclusion row (the device is resolved from the presented credential server-side).
    directory.follow_skill(workspace_id, skill_id)?;
    // (b) Clear the local per-device marker (the offline cause `list` reads) AND re-affirm the local
    //     follow — a `remove` then `unfollow` leaves the entry PAUSED, and the PUT above just re-affirmed
    //     the direct follow, so the local flag must converge or the reconcile skips a paused skill.
    enroll::set_excluded(ctx.fs, &ctx.layout, skill_id, false)?;
    enroll::set_following(ctx.fs, &ctx.layout, skill_id, true)?;
    // (c) Reconcile the current bytes back — RESTRICTED to the re-attach subject via `install_only`: any
    //     OTHER pending first-receive in the workspace (a teammate's brand-new skill in #everyone) stays
    //     undisclosed, never silently installed under a describe that named just this skill. The reconcile
    //     reads its byte-fetch transport + its follow-state through the swapped ctx: the DELIVERY object
    //     (so `bind_skill` + the fetches share it) and a follow seam re-read from disk (reflecting the
    //     `set_following` above, which the startup seam predates).
    let delivery = (connectors.delivery)(&base_url);
    let rec_opts = ReconcileOpts {
        accept_first_receive: true,
        only_workspace: Some(workspace_id.to_owned()),
        install_only: Some(HashSet::from([skill_id.to_owned()])),
        ack_notices: false,
        ..ReconcileOpts::default()
    };
    let follows = enroll::read_follows(ctx.fs, &ctx.layout)?.unwrap_or_else(|| enroll::Follows {
        schema_version: PERSISTED_SCHEMA_VERSION,
        follows: Vec::new(),
    });
    let follow_seam = FileFollow::new(enroll::follow_contexts(&follows));
    let plane_ref: &dyn PlaneSource = &*delivery;
    let sweep_ctx = super::pull::ctx_with_plane_and_follow(ctx, plane_ref, &follow_seam);
    let delivery_ref: &dyn DeliverySource = &*delivery;
    let out = super::pull::pull_reconcile_with(&sweep_ctx, delivery_ref, &rec_opts)?;
    // (d) Did the current bytes actually land back on this device? (An isolated per-skill failure stays
    //     a warning — the reconcile's isolation semantics.) The row keys by the local dirname.
    let installed = out.data.skills.iter().any(|row| {
        row.skill == name
            && matches!(
                row.action,
                PullAction::FastForwarded | PullAction::UpToDate | PullAction::Merged
            )
    });
    // Re-read the lock so the report discloses what is now current locally.
    let updated: Option<Lock> = doc::read_doc(ctx.fs, &ctx.layout.published(&sid).lock)?;
    Ok(FollowOutcome::ReattachApplied(Box::new(Reattach {
        workspace_id: workspace_id.to_owned(),
        workspace_name,
        skill_id: skill_id.to_owned(),
        name: name.to_owned(),
        version_id: updated
            .as_ref()
            .map(|l| l.base_commit.clone())
            .or(version_id),
        bundle_digest: updated
            .as_ref()
            .map(|l| l.bundle_digest.clone())
            .or(bundle_digest),
        installed,
        warnings: out.warnings,
    })))
}

/// The paste-ready `--yes` apply argv for a re-attach DESCRIBE. A name followed in two workspaces would
/// make a bare `follow <name> --yes` ambiguous, so the argv carries the disambiguator this invocation
/// used: the qualified `<address-slug>/skills/<name>` spelling the classic describe uses (when the
/// qualified path supplied the address slug), else the caller's `--workspace <filter>` (the bare path
/// carries no address slug offline), else nothing (an already-unique name).
fn reattach_yes_argv(
    name: &str,
    address_name: Option<&str>,
    workspace: Option<&str>,
) -> Vec<String> {
    let target = match address_name {
        Some(slug) => format!("{slug}/{}/{name}", ResourceKind::Skill.segment()),
        None => name.to_owned(),
    };
    let mut argv = vec!["topos".to_owned(), "follow".to_owned(), target];
    // Only the bare path (no qualified target) needs the `--workspace` filter preserved — the qualified
    // spelling already pins the workspace.
    if address_name.is_none()
        && let Some(w) = workspace
    {
        argv.push("--workspace".to_owned());
        argv.push(w.to_owned());
    }
    argv.push("--yes".to_owned());
    argv
}

/// The workspace's human label (its display name from `user.json`, offline), falling back to the id —
/// the re-attach describe/apply's disclosure, needing no network on the bare describe.
fn workspace_label(ctx: &Ctx<'_>, workspace_id: &str) -> String {
    enroll::read_user(ctx.fs, &ctx.layout)
        .ok()
        .flatten()
        .and_then(|u| {
            u.workspaces
                .into_iter()
                .find(|m| m.workspace_id == workspace_id)
                .map(|m| m.display_name)
        })
        .unwrap_or_else(|| workspace_id.to_owned())
}

// =================================================================================================
// Small helpers.
// =================================================================================================

/// Resolve the API base a follow re-roots onto: the card's declared `api_base_url`, normalized
/// (trimmed of trailing slashes — the pin comparisons are exact string equality) and gated the same way
/// as the link base — plus the one extra rule the re-root introduces: an `https` link must never re-root
/// onto a plain-`http` plane (a transport downgrade the human who pasted the link could not see).
pub(super) fn resolve_api_base(link_base: &str, declared: &str) -> Result<String, ClientError> {
    let declared = declared.trim().trim_end_matches('/');
    if declared.is_empty() {
        return Err(ClientError::Enrollment(
            "the protocol card declared no API base URL; upgrade the server".into(),
        ));
    }
    validate_base_url(declared)?;
    if link_base.starts_with("https://") && !declared.starts_with("https://") {
        return Err(ClientError::Enrollment(
            "refusing to enroll: the address is https but the card declares a plain-http API base"
                .into(),
        ));
    }
    Ok(declared.to_owned())
}

/// Refuse an API base that is not a well-formed absolute `http(s)://…` URL (the transport's own `Uri`
/// grammar, so anything accepted here builds cleanly downstream). A malformed base would otherwise
/// surface as a transport error whose message echoes the full URI into the diagnostics log.
fn validate_base_url(base: &str) -> Result<(), ClientError> {
    let well_formed = base.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && authority_usable(&uri)
    });
    if well_formed {
        Ok(())
    } else {
        Err(ClientError::Enrollment(
            "the declared API base URL is not a valid http(s) URL".into(),
        ))
    }
}

/// The authority half of the base gate: a non-empty host, and a bracketed literal must be a REAL IPv6
/// address. `http::Uri` itself accepts RFC-3986 IPvFuture-shaped brackets (e.g. `[bad]`), which the
/// transport only rejects LATER — with a URI-echoing error, too late for a URL that carries the token.
fn authority_usable(uri: &ureq::http::Uri) -> bool {
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host_port = authority.as_str().rsplit('@').next().unwrap_or("");
    match host_port.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .is_some_and(|(v6, _port)| v6.parse::<std::net::Ipv6Addr>().is_ok()),
        None => !host_port.is_empty(),
    }
}

/// Drop an `@<hash>` suffix from a `follow <skill>[@<hash>]` target when the part after the last `@` is a valid
/// version id, leaving the skill name (so a name containing `@` is still accepted).
fn strip_digest(target: &str) -> &str {
    if let Some((name, suffix)) = target.rsplit_once('@')
        && super::parse_hex32(suffix).is_ok()
    {
        return name;
    }
    target
}

/// A human-readable device name for the approval page (a confused-deputy aid, not authority) — the
/// host's node name, so a person approving in the browser recognizes which machine is asking. Kept as
/// the device's display name once approved.
pub(super) fn machine_name() -> String {
    let uname = rustix::system::uname();
    let node = uname.nodename().to_string_lossy();
    let node = node.trim();
    if node.is_empty() {
        "topos CLI".to_owned()
    } else {
        format!("topos CLI ({node})")
    }
}

/// `now` as epoch-millis (saturating), via the injected clock.
fn now_millis(ctx: &Ctx<'_>) -> i64 {
    i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{machine_name, resolve_api_base, validate_base_url};

    /// The device name shown on the approval page derives from the HOST (no key material exists to
    /// fingerprint) and always carries the recognizable `topos CLI` prefix.
    #[test]
    fn machine_name_is_host_derived_and_recognizable() {
        let name = machine_name();
        assert!(name.starts_with("topos CLI"), "{name}");
    }

    /// The re-root resolver: normalizes trailing slashes (the pin compares are exact strings), applies
    /// the same URL gate as the link base, and refuses the one thing a re-root could newly smuggle in —
    /// an https→http transport downgrade the human who pasted the link could not see.
    #[test]
    fn api_base_resolver_normalizes_gates_and_refuses_downgrade() {
        assert_eq!(
            resolve_api_base("https://links.example", "https://api.plane.test/").unwrap(),
            "https://api.plane.test"
        );
        assert_eq!(
            resolve_api_base("http://localhost:1", "http://127.0.0.1:2").unwrap(),
            "http://127.0.0.1:2"
        );
        // An http link may upgrade to an https plane…
        assert_eq!(
            resolve_api_base("http://links.example", "https://api.plane.test").unwrap(),
            "https://api.plane.test"
        );
        // …but an https link never downgrades to plain http.
        assert!(resolve_api_base("https://links.example", "http://api.plane.test").is_err());
        // An empty / malformed declared base is refused typed (same gate as the link base).
        assert!(resolve_api_base("https://links.example", "").is_err());
        assert!(resolve_api_base("https://links.example", "not-a-url").is_err());
    }

    /// A PATH-CARRYING api base is a first-class re-root target — the door-cutover card declares
    /// `<origin>/api` (the web app's mount) and every `/v1/…` route joins onto it. This pin keeps a
    /// future "just parse the origin" refactor from silently amputating the mount path.
    #[test]
    fn api_base_resolver_accepts_a_path_carrying_base() {
        assert_eq!(
            resolve_api_base("https://topos.example", "https://topos.example/api").unwrap(),
            "https://topos.example/api"
        );
        // Trailing slashes normalize without touching the mount segment.
        assert_eq!(
            resolve_api_base("http://localhost:3000", "http://localhost:3000/api/").unwrap(),
            "http://localhost:3000/api"
        );
    }

    #[test]
    fn base_url_gate_accepts_the_legit_shapes_and_refuses_the_uri_hazards() {
        for ok in [
            "https://topos.sh",
            "https://api.topos.sh",
            "http://localhost:8787",
            "http://127.0.0.1:8080",
            "http://[::1]:8787",
            "http://[2001:db8::1]",
        ] {
            assert!(validate_base_url(ok).is_ok(), "must accept {ok}");
        }
        for bad in [
            "http://[bad]",     // IPvFuture-shaped garbage http::Uri itself accepts
            "http://[::1",      // unterminated bracket
            "ftp://plane.test", // not http(s)
            "http:",            // no authority
            "plane.test",       // no scheme
            "",
        ] {
            assert!(validate_base_url(bad).is_err(), "must refuse {bad:?}");
        }
    }
}
