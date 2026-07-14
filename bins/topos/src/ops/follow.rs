//! `follow` — the device-flow enrollment + first-receive client.
//!
//! One verb, dispatched by the single positional (the harness drives it non-interactively):
//! - **`follow <link>`** (call 1) — read the `/i/` bootstrap, guard one-plane-per-install, start a device
//!   authorization, write a `0600` WAL, and return `ENROLLMENT_PENDING` + the verification URL.
//! - **re-invoking `follow`** (call 2) — with a pending enrollment WAL on disk, re-invoking `follow` (with
//!   any target, or none) RESUMES it — the "re-invoking IS the resume" idiom the standup publish uses:
//!   poll once; on a granted poll, redeem the grant into the workspace credential, record it in the WAL
//!   (the lockout fence), PROMOTE (write `instance.json` / `credentials.json` / `follows.json` /
//!   `user.json` / the device key + lay the first-receive baselines), delete the WAL, and disclose the
//!   offers.
//! - **`follow <skill>[@<hash>]`** (post-enroll) — a KNOWN followed-skill name drives the existing pull
//!   engine to place the named, already-disclosed first-receive bytes (the I-TOFU "one accept"). On a
//!   retained entry `unfollow` paused (`following == false`) it RESUMES the follow instead: the flag flips
//!   back on, a still-pending first-receive offer is placed, and otherwise the next `pull` lands current.
//!
//! The positional is dispatched by SHAPE (see [`follow`]): a pending WAL wins (re-invoke resumes); `@`
//! forces the skill path; a known skill name is the skill path; otherwise it is an `/i/` link or a bare
//! invite token.
//!
//! **I-NO-USER-TOKEN.** The agent only ever holds the opaque grant + the minted workspace credential —
//! never a user token; enrollment completes by POLLING. **Secrets** (the device code, the grant, the
//! workspace credential) live only in the `0600` WAL / `credentials.json`, are redacted in `Debug`, and
//! never reach a URL / log / error.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use topos_core::digest::{self, ManifestEntry, to_hex};
use topos_gitstore::Store;
use topos_types::bootstrap::{DeploymentMode, VerifiedDomainStatus};
use topos_types::persisted::{Lock, PlacementMap, SwapCapability, SyncState};
use topos_types::requests::{WireChannelIndex, WireMe, WireSkillIndex};
use topos_types::results::{EnrollmentPending, FollowData, FollowOffer, Offer, PullAction};
use topos_types::{Generation, PERSISTED_SCHEMA_VERSION};

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::error::ClientError;
use crate::identity::{self, DeviceKeyRef};
use crate::plane::{
    Card, DeliverySnapshot, DeliverySource, DirectorySource, EnrollSource, FollowContext,
    PlaneError, PlaneSource, PointerFetch, ReconcileTransport, TokenPoll,
};
use crate::plane_http::{FileFollow, SkillCred};
use crate::resolve::{self, ParsedTarget, Resolution, ResourceKind};
use crate::{doc, enroll, sidecar};

use super::pull::ReconcileOpts;
use super::sync_engine::{self, Invocation};

/// The 64-char all-zero hex sentinel a never-received baseline uses for its (absent) base commit / digest.
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// The genesis generation sentinel — `(0,0)` means "nothing authenticated / applied yet".
const GENESIS: Generation = Generation { epoch: 0, seq: 0 };

/// `follow`'s flags, parsed from argv (the positional targets ride separately).
#[derive(Clone)]
pub(crate) struct FollowOpts {
    /// `--manual` ⇒ confirm-each adoption (else auto).
    pub manual: bool,
    /// The global `--workspace <id>` filter — disambiguates a positional skill NAME shared across the
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
}

/// Builds the creds-free enrollment transport for a plane base URL.
pub(crate) type EnrollConnect<'a> = dyn Fn(&str) -> Box<dyn EnrollSource> + 'a;
/// Builds the read transport (the offer-disclosure source) for a base URL + the minted workspace credential.
pub(crate) type PlaneConnect<'a> =
    dyn Fn(&str, HashMap<String, SkillCred>) -> Box<dyn PlaneSource> + 'a;
/// Builds the credentialed DIRECTORY transport (describe reads + subscription rows) for a base URL.
/// Re-reads `credentials.json` per build, so a mid-invocation enrollment's fresh mint is seen.
pub(crate) type DirectoryConnect<'a> = dyn Fn(&str) -> Box<dyn DirectorySource> + 'a;
/// Builds the credentialed RECONCILE transport (delivery + fleet report + the per-skill read lane,
/// on one object — the reconcile binds a new arrival's credential onto the read side). Re-reads the
/// on-disk credentials per build, for the same mid-invocation reason.
pub(crate) type DeliveryConnect<'a> = dyn Fn(&str) -> Box<dyn ReconcileTransport> + 'a;

/// The network seams the op needs, as factories — the base URL is known only after the op parses the
/// target / the card / the WAL, so the transports can't be pre-built in the composition root.
/// Production wires the `ureq` transports; the tests wire fakes (no HTTP).
pub(crate) struct FollowConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub plane: &'a PlaneConnect<'a>,
    pub directory: &'a DirectoryConnect<'a>,
    pub delivery: &'a DeliveryConnect<'a>,
    /// The default WEB origin the token-less doors dial when nothing is pinned yet (`follow <bare
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
/// 1. a pending enrollment WAL exists → RESUME it (poll/promote/retry/continue), regardless of the
///    targets — the "re-invoking IS the resume" path;
/// 2. a single `/i/` link → the admin-CLAIM door (unchanged; `/i/` is claims-only);
/// 3. a single bare `<skill>@<digest>` — the name-part MUST be a known followed skill — or a bare
///    word matching a KNOWN followed skill → the classic skill path (place offer / resume a paused
///    entry). **Known-skill-name wins** over the address grammar;
/// 4. everything else is the ADDRESS/SUBSCRIBE grammar ([`crate::resolve`]): full addresses,
///    qualified paths, bare channel/skill names, `--channel`/`--skill` selectors — resolved
///    all-or-none; a single unresolved workspace-shaped target folds the ENROLL flow in; then the
///    two-phase describe / `--yes` apply.
///
/// # Errors
/// [`ClientError::Enrollment`] for a missing target / denied / expired session;
/// [`ClientError::InvalidArgument`] for an `@`-pinned unknown skill or a malformed address;
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
    // 1) A pending enrollment WAL: re-invoking `follow` (with any target, or none) resumes it. `resume`
    // routes each WAL phase (Authorizing poll / Redeemed promote / ClaimPending retry / a standup owned by
    // `publish` / a login owned by `auth login`) — so a second follow while one is in flight never
    // clobbers the in-flight session's single-use secrets; it advances it.
    if enroll::read_wal(ctx.fs, &ctx.layout)?.is_some() {
        return resume(ctx, connectors, &opts);
    }
    let ws = opts.workspace.as_deref();
    if targets.is_empty() && opts.channels.is_empty() && opts.skills.is_empty() {
        return Err(ClientError::Enrollment(
            "follow needs a target — a workspace address, a channel or skill name, or an /i/ claim \
             link (or a pending enrollment to resume)"
                .into(),
        ));
    }
    if opts.channels.is_empty()
        && opts.skills.is_empty()
        && let [single] = targets.as_slice()
    {
        // 2) The `/i/` claim door. Checked BEFORE `@` so an `/i/` link carrying userinfo
        // (`https://u@host/i/tok`) or a query param (`?x=a@b`) is never misread as `<skill>@<hash>`.
        if single.contains("/i/") {
            return begin(ctx, connectors, single, opts.manual).map(FollowOutcome::plain);
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

// =================================================================================================
// Call 1 — `follow <//i/ link>`: bootstrap → one-plane guard → the claim door (claims-only now).
// =================================================================================================

fn begin(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    link: &str,
    _manual: bool,
) -> Result<FollowData, ClientError> {
    let (link_base, token) = parse_link(link)?;

    // `begin` is only reached with NO pending enrollment WAL on disk: the [`follow`] dispatch resumes an
    // in-flight session (rule 1) BEFORE ever dispatching a target to `begin`, and the start-of-command
    // recovery sweep reaps an expired authorizing/standup WAL first. So a fresh follow never clobbers a
    // live session's single-use secrets — re-invoking `follow` advances it (`resume`), it does not begin.

    let bootstrap = (connectors.enroll)(&link_base).fetch_bootstrap(&token)?;
    // RE-ROOT onto the plane's declared API base. The link is only where the bootstrap lives (a hosted
    // team's share links ride its web origin); the bootstrap declares the plane every later call — the
    // redeem, every pull — must dial. The declared base passes the same gate as the link base and may never
    // downgrade the transport. This adds no attacker capability: whoever mints the link already controls the
    // bootstrap (the link IS the channel the human chose to trust).
    let base_url = resolve_api_base(&link_base, &bootstrap.plane.base_url)?;
    // v0 is one plane per install: refuse a follow that would enroll against a DIFFERENT plane than the one
    // already on disk (keyed on the RE-ROOTED API base — the base every later call dials and `instance.json`
    // records). There is no trust root to pin — the `current` pointer is unsigned, its integrity the
    // content-addressed version id.
    guard_one_plane(ctx, &base_url)?;

    // Branch on the enrollment method the bootstrap disclosed. `/i/` is now the admin-CLAIM door ONLY.
    match bootstrap.plane.enrollment_method.as_str() {
        // The one-shot admin-claim door (self-host bearer): no device-auth session, enrolls in one call.
        "admin_claim" => claim_follow(ctx, connectors, &base_url, &token, &bootstrap),
        // The old invite-link device/passcode enrollment is retired — joining an existing workspace
        // is a workspace-ADDRESS follow (invites are roster rows now; the address carries nothing).
        "device_code" | "passcode" => Err(ClientError::Enrollment(
            "invite links are retired — join by the workspace ADDRESS instead: `topos follow \
             <server>/<workspace>` (ask your inviter for it)"
                .into(),
        )),
        other => Err(ClientError::Enrollment(format!(
            "this plane offers enrollment method '{other}', which this topos build does not \
             support; upgrade topos"
        ))),
    }
}

/// The one-plane-per-install guard, shared by every pre-enrollment door (`/i/` bootstrap, standup authorize).
/// `base_url` is the plane's API base — for a link follow that is the RE-ROOTED base the bootstrap declared
/// (never the share host the link string rode), so it matches what every later call dials. If an
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
// Call 2 — re-invoking `follow` with a pending WAL: poll → (granted) redeem → Redeemed WAL → promote.
// =================================================================================================

fn resume(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    opts: &FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    let wal = enroll::read_wal(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("no enrollment in progress; run `follow <link>` first".into())
    })?;

    match wal.state {
        // A Redeemed-but-unpromoted WAL: PROMOTE without re-redeeming (the single-use grant is spent;
        // recovery completes from the persisted workspace credential). An ADDRESS-rooted redeem then
        // CONTINUES into its recorded follow intent (the describe / `--yes` apply).
        enroll::EnrollPhase::Redeemed {
            context,
            credential,
            device_key_id,
            principal,
            enrolled_at_millis,
        } => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            if let Some(target) = context.follow_target.clone() {
                promote_core(
                    ctx,
                    &context,
                    &credential,
                    &device_key_id,
                    principal.as_deref(),
                    enrolled_at_millis,
                    &signer,
                )?;
                return continue_into_target(ctx, connectors, &context, &target, opts);
            }
            promote(
                ctx,
                connectors,
                &context,
                &credential,
                &device_key_id,
                principal.as_deref(),
                enrolled_at_millis,
                &signer,
            )
            .map(FollowOutcome::plain)
        }
        // A standup session belongs to `publish` — its resume is the ORIGINAL publish command (the
        // optional `@<digest>` pin re-derives from that command each invocation, which `follow` cannot
        // supply).
        enroll::EnrollPhase::AuthorizingStandup { .. } => Err(ClientError::Enrollment(
            "a workspace standup is in progress; re-run the `topos publish …` command that started it"
                .into(),
        )),
        // A login session belongs to `auth login` — same ownership rule as the standup.
        enroll::EnrollPhase::AuthorizingLogin { .. } => Err(ClientError::Enrollment(
            "a sign-in is in progress; re-run `topos auth login` to finish it first".into(),
        )),
        // An unsettled claim redeem: retry the POST directly (never refetch the possibly-consumed /i/).
        state @ enroll::EnrollPhase::ClaimPending { .. } => {
            let wal = enroll::PendingEnrollment {
                schema_version: PERSISTED_SCHEMA_VERSION,
                state,
            };
            retry_claim(ctx, connectors, &wal).map(FollowOutcome::plain)
        }
        // A live ADDRESS enrollment: poll; granted ⇒ redeem into the workspace the grant names,
        // fence, promote, and continue into the recorded follow intent.
        enroll::EnrollPhase::AuthorizingAddress {
            base_url,
            workspace_name,
            follow_target,
            mode,
            device_code,
            user_code,
            verification_uri_complete,
            ..
        } => resume_address(
            ctx,
            connectors,
            opts,
            AddressWal {
                base_url,
                workspace_name,
                follow_target,
                mode,
                device_code,
                user_code,
                verification_uri_complete,
            },
        ),
        enroll::EnrollPhase::Authorizing {
            context,
            device_code,
            user_code,
            verification_uri_complete,
            ..
        } => {
            let enroll_src = (connectors.enroll)(&context.base_url);
            match enroll_src.poll_token(&device_code)? {
                // Still pending — re-surface the persisted SERVER-built URL, verbatim. There is no
                // client-side reconstruction: the plane's verification page lives on its (possibly
                // separate) verify base, which this client cannot derive — a fabricated URL would point
                // the human at a page that does not exist. A WAL an older build wrote without the URL
                // restarts cleanly.
                TokenPoll::Pending | TokenPoll::SlowDown => {
                    let complete = verification_uri_complete.ok_or_else(|| {
                        ClientError::Enrollment(
                            "this enrollment session carries no verification URL; start over with \
                             `follow <link>`"
                                .into(),
                        )
                    })?;
                    // The device key is deterministic (load-or-generate returns the same key), so the
                    // re-surfaced pending discloses the same fingerprint the human sees on the page.
                    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
                    Ok(FollowOutcome::plain(pending_followdata(
                        &context,
                        &user_code,
                        complete,
                        device_fingerprint(&signer),
                    )))
                }
                // A terminal denial / expiry — sweep the WAL, surface a typed error.
                TokenPoll::Denied => {
                    enroll::delete_wal(ctx.fs, &ctx.layout)?;
                    Err(ClientError::Enrollment(
                        "the enrollment was denied at the verification page".into(),
                    ))
                }
                TokenPoll::Expired => {
                    enroll::delete_wal(ctx.fs, &ctx.layout)?;
                    Err(ClientError::Enrollment(
                        "the enrollment session expired; start over with `follow <link>`".into(),
                    ))
                }
                TokenPoll::Granted(granted) => {
                    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
                    // Redeem the grant (the bearer credential) into the workspace credential; nothing is signed.
                    let redeem =
                        redeem_grant(&*enroll_src, &context, granted.grant.as_str(), &signer)?;
                    let enrolled_at = now_millis(ctx);
                    // The lockout fence: record the redeemed workspace credential (a single-use grant cannot
                    // be re-redeemed) BEFORE promotion, so a crash mid-promote completes from this WAL.
                    enroll::write_wal(
                        ctx.fs,
                        &ctx.layout,
                        &enroll::PendingEnrollment {
                            schema_version: PERSISTED_SCHEMA_VERSION,
                            state: enroll::EnrollPhase::Redeemed {
                                context: context.clone(),
                                credential: redeem.credential.clone(),
                                device_key_id: redeem.device_key_id.clone(),
                                principal: redeem.principal.clone(),
                                enrolled_at_millis: enrolled_at,
                            },
                        },
                    )?;
                    promote(
                        ctx,
                        connectors,
                        &context,
                        &redeem.credential,
                        &redeem.device_key_id,
                        redeem.principal.as_deref(),
                        enrolled_at,
                        &signer,
                    )
                    .map(FollowOutcome::plain)
                }
            }
        }
    }
}

// =================================================================================================
// The ADDRESS flow — `follow <workspace>[/channels|skills/<name>]`: card → re-root guard →
// device-authorize (intent enroll, the workspace named by ADDRESS) → WAL → poll/resume → redeem at
// the granted workspace → promote → the two-phase subscribe (describe / `--yes` apply).
// =================================================================================================

/// The `AuthorizingAddress` WAL fields, destructured once (mirrors `publish`'s `StandupWal`).
struct AddressWal {
    base_url: String,
    workspace_name: String,
    follow_target: Option<enroll::FollowTargetDoc>,
    mode: enroll::FollowModeDoc,
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
}

/// The enroll intent an unresolved single target may fold in: the workspace ADDRESS name, the
/// follow intent to continue into, and the explicit host when the target was a full URL.
struct EnrollIntent {
    host: Option<String>,
    workspace_name: String,
    target: enroll::FollowTargetDoc,
}

/// Whether an UNRESOLVED parsed target is shaped like a workspace address this install could enroll
/// toward. Only address shapes qualify — a bare word must be a valid ADDRESS name; anything else
/// stays the uniform not-found.
fn enroll_intent(parsed: &ParsedTarget) -> Option<EnrollIntent> {
    match parsed {
        ParsedTarget::Address {
            host,
            workspace,
            resource,
        } => {
            if !resolve::is_workspace_name(workspace) {
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
/// named workspace, and persist the `AuthorizingAddress` WAL with the follow intent.
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
    let card_url = format!("{origin}/{}", intent.workspace_name);
    let base_url = match (connectors.enroll)(&origin).fetch_card(&card_url)? {
        Card::Protocol(card) => resolve_api_base(&origin, &card.api_base_url)?,
        // A claim bootstrap only ever lives on an `/i/` link, which the dispatch routes to the claim
        // door before this flow — an ADDRESS answering one is a mis-pasted link.
        Card::Claim(bootstrap) => {
            return Err(ClientError::Enrollment(format!(
                "this address answered a claim bootstrap (workspace '{}') — pass the full /i/ \
                 claim link to `topos follow` instead",
                bootstrap.workspace.display_name
            )));
        }
    };
    guard_one_plane(ctx, &base_url)?;

    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let auth = (connectors.enroll)(&base_url).device_authorize(
        &intent.workspace_name,
        signer.public_key(),
        &machine_name(&signer),
    )?;
    let complete = auth
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| complete_uri(&auth.verification_uri, &auth.user_code));
    let expires_at = now_millis(ctx)
        .saturating_add(i64::try_from(auth.expires_in.saturating_mul(1000)).unwrap_or(i64::MAX));
    enroll::write_wal(
        ctx.fs,
        &ctx.layout,
        &enroll::PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: enroll::EnrollPhase::AuthorizingAddress {
                base_url: base_url.clone(),
                workspace_name: intent.workspace_name.clone(),
                follow_target: Some(intent.target),
                mode: if opts.manual {
                    enroll::FollowModeDoc::ConfirmEach
                } else {
                    enroll::FollowModeDoc::Auto
                },
                device_code: auth.device_code.clone(),
                user_code: auth.user_code.clone(),
                verification_uri_complete: complete.clone(),
                expires_at_millis: expires_at,
            },
        },
    )?;
    Ok(FollowOutcome::plain(pending_address_followdata(
        &intent.workspace_name,
        &base_url,
        &auth.user_code,
        complete,
        device_fingerprint(&signer),
    )))
}

/// Resume a live ADDRESS enrollment: poll once; granted ⇒ redeem at the workspace the grant names
/// (the authoritative id — the address name was never trusted to exist), fence, promote, continue.
fn resume_address(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    opts: &FollowOpts,
    wal: AddressWal,
) -> Result<FollowOutcome, ClientError> {
    let enroll_src = (connectors.enroll)(&wal.base_url);
    match enroll_src.poll_token(&wal.device_code)? {
        TokenPoll::Pending | TokenPoll::SlowDown => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            Ok(FollowOutcome::plain(pending_address_followdata(
                &wal.workspace_name,
                &wal.base_url,
                &wal.user_code,
                wal.verification_uri_complete,
                device_fingerprint(&signer),
            )))
        }
        TokenPoll::Denied => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the enrollment was denied at the verification page".into(),
            ))
        }
        TokenPoll::Expired => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the enrollment session expired; start over with `topos follow <address>`".into(),
            ))
        }
        TokenPoll::Granted(granted) => {
            // The granted poll carries the AUTHORITATIVE workspace context (the requested name was
            // only ever a request; unknown/not-yours dies at the redeem's uniform denial).
            let workspace = granted.workspace.ok_or_else(|| {
                ClientError::WireInvalid("a granted enroll poll carried no workspace".into())
            })?;
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            let redeem = enroll_src.redeem(
                &workspace.workspace_id,
                granted.grant.as_str(),
                signer.public_key(),
            )?;
            if redeem.workspace_id != workspace.workspace_id {
                return Err(ClientError::Enrollment(
                    "the redeemed workspace does not match the granted session".into(),
                ));
            }
            // The deployment posture is unknowable from the constant card — keep whatever an earlier
            // enrollment recorded, else the conservative self-host default (disclosure only).
            let deployment_mode = enroll::read_instance(ctx.fs, &ctx.layout)?
                .map(|i| i.deployment_mode)
                .unwrap_or(DeploymentMode::SelfHost);
            let context = enroll::EnrollContext {
                base_url: wal.base_url,
                deployment_mode,
                enrollment_method: "device_code".to_owned(),
                workspace_id: workspace.workspace_id,
                workspace_display_name: workspace.display_name,
                verified_domain: None,
                verified_domain_status: VerifiedDomainStatus::Unverified,
                offered_skills: Vec::new(),
                mode: wal.mode,
                root: enroll::EnrollRoot::Address,
                follow_target: wal.follow_target.clone(),
            };
            let enrolled_at = now_millis(ctx);
            // The lockout fence, exactly as the invite flow: the redeemed facts land BEFORE
            // promotion, so a crash mid-promote completes from the WAL without re-redeeming.
            enroll::write_wal(
                ctx.fs,
                &ctx.layout,
                &enroll::PendingEnrollment {
                    schema_version: PERSISTED_SCHEMA_VERSION,
                    state: enroll::EnrollPhase::Redeemed {
                        context: context.clone(),
                        credential: redeem.credential.clone(),
                        device_key_id: redeem.device_key_id.clone(),
                        principal: redeem.principal.clone(),
                        enrolled_at_millis: enrolled_at,
                    },
                },
            )?;
            promote_core(
                ctx,
                &context,
                &redeem.credential,
                &redeem.device_key_id,
                redeem.principal.as_deref(),
                enrolled_at,
                &signer,
            )?;
            let target = context
                .follow_target
                .clone()
                .unwrap_or(enroll::FollowTargetDoc {
                    kind: enroll::FollowKindDoc::Workspace,
                    name: wal.workspace_name,
                });
            continue_into_target(ctx, connectors, &context, &target, opts)
        }
    }
}

/// The pending `FollowData` an ADDRESS enrollment surfaces (there is no workspace ID yet — the
/// requested ADDRESS name rides the disclosure slot; the id arrives with the grant).
fn pending_address_followdata(
    workspace_name: &str,
    base_url: &str,
    user_code: &str,
    verification_uri_complete: String,
    device_fingerprint: String,
) -> FollowData {
    FollowData {
        workspace_id: workspace_name.to_owned(),
        enrolled: false,
        skills: Vec::new(),
        deployment_mode: None,
        workspace_display_name: None,
        verified_domain: None,
        verified_domain_status: None,
        plane_base_url: Some(base_url.to_owned()),
        pending: Some(EnrollmentPending {
            verification_uri_complete,
            user_code: user_code.to_owned(),
            device_fingerprint,
            expires_at: None,
        }),
        currency: None,
    }
}

// =================================================================================================
// The admin-claim door — `follow <claim-link>` in ONE invocation (self-host bearer). The `/i/` bootstrap
// disclosed `enrollment_method: "admin_claim"`; there is no device-auth session and no re-invoke on the
// happy path. A pre-send WAL makes an uncertain send safely retryable: the retry POSTs `/v1/admin-claim`
// directly (a consumed claim's bootstrap serves 404 by design; the server's same-device replay of a
// consumed claim deterministically re-answers Redeemed).
// =================================================================================================

fn claim_follow(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    // The RE-ROOTED API base (the WAL records it; the redeem POST + the promote ride it).
    base_url: &str,
    token: &str,
    bootstrap: &topos_types::BootstrapData,
) -> Result<FollowData, ClientError> {
    // The pre-send WAL (0600 — the claim token is a bearer secret), BEFORE the first POST.
    let wal = enroll::PendingEnrollment {
        schema_version: PERSISTED_SCHEMA_VERSION,
        state: enroll::EnrollPhase::ClaimPending {
            base_url: base_url.to_owned(),
            deployment_mode: bootstrap.plane.deployment_mode,
            enrollment_method: bootstrap.plane.enrollment_method.clone(),
            workspace_id: bootstrap.workspace.workspace_id.clone(),
            workspace_display_name: bootstrap.workspace.display_name.clone(),
            claim_token: token.to_owned(),
        },
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;
    retry_claim(ctx, connectors, &wal)
}

/// Send (or re-send) the claim redeem recorded in a `ClaimPending` WAL, then convert to the ordinary
/// `Redeemed` fence and promote. Callable any number of times: the server treats a same-device replay of a
/// consumed claim as the SAME redeem (lost-200 recovery), so the retry is idempotent by construction.
fn retry_claim(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    wal: &enroll::PendingEnrollment,
) -> Result<FollowData, ClientError> {
    let enroll::EnrollPhase::ClaimPending {
        base_url,
        deployment_mode,
        enrollment_method,
        workspace_id,
        workspace_display_name,
        claim_token,
    } = &wal.state
    else {
        return Err(ClientError::Corrupt(
            "retry_claim needs a claim_pending WAL".into(),
        ));
    };
    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let enroll_src = (connectors.enroll)(base_url);
    // The display name rides for DISCLOSURE only (the seated name comes from the mint-time claim row).
    let redeem =
        match enroll_src.admin_claim(claim_token, signer.public_key(), workspace_display_name) {
            Ok(redeem) => redeem,
            // A TERMINAL plane denial — per the seam's contract, `admin_claim` returns
            // [`ClientError::Enrollment`] ONLY for the 200+DENIED claim verdict (consumed by another device /
            // expired / the workspace already exists). The claim is definitively dead, so the ClaimPending WAL
            // is cleared BEFORE the error surfaces (mirroring the poll Denied/Expired arms clearing the
            // Authorizing WAL) — otherwise the sweep-exempt WAL wedges every later `follow <other-link>`
            // behind the dispatch while a re-invoke re-denies forever.
            Err(e @ ClientError::Enrollment(_)) => {
                enroll::delete_wal(ctx.fs, &ctx.layout)?;
                return Err(e);
            }
            // Everything else is an UNCERTAIN fault (a transport error, a non-200, a malformed body — the
            // send may or may not have consumed the claim): KEEP the WAL, so the next invocation retries the
            // POST directly and the server's same-device replay re-answers Redeemed.
            Err(e) => return Err(e),
        };
    // The redeem names the claim row's authoritative workspace — it must match what the link disclosed.
    if redeem.workspace_id != *workspace_id {
        return Err(ClientError::Enrollment(
            "the claimed workspace does not match the claim link".into(),
        ));
    }
    let context = enroll::EnrollContext {
        base_url: base_url.clone(),
        deployment_mode: *deployment_mode,
        enrollment_method: enrollment_method.clone(),
        workspace_id: workspace_id.clone(),
        workspace_display_name: workspace_display_name.clone(),
        verified_domain: None,
        verified_domain_status: VerifiedDomainStatus::Unverified,
        offered_skills: Vec::new(),
        mode: enroll::FollowModeDoc::Auto,
        root: enroll::EnrollRoot::Claim,
        follow_target: None,
    };
    let enrolled_at = now_millis(ctx);
    // The same lockout fence as the grant flow: the claim is consumed server-side, so the redeemed facts
    // are recorded BEFORE promotion and a crash mid-promote completes from here without re-sending.
    enroll::write_wal(
        ctx.fs,
        &ctx.layout,
        &enroll::PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: enroll::EnrollPhase::Redeemed {
                context: context.clone(),
                credential: redeem.credential.clone(),
                device_key_id: redeem.device_key_id.clone(),
                principal: redeem.principal.clone(),
                enrolled_at_millis: enrolled_at,
            },
        },
    )?;
    promote(
        ctx,
        connectors,
        &context,
        &redeem.credential,
        &redeem.device_key_id,
        redeem.principal.as_deref(),
        enrolled_at,
        &signer,
    )
}

/// Redeem the grant into a registered device + its workspace credential. The grant is the bearer credential
/// and the body's `device_public_key` registers this device (the server checks it matches the grant's bound
/// pubkey) — nothing is signed.
fn redeem_grant(
    enroll_src: &dyn EnrollSource,
    context: &enroll::EnrollContext,
    grant: &str,
    signer: &DeviceSigner,
) -> Result<crate::plane::Redeem, ClientError> {
    let redeem = enroll_src.redeem(&context.workspace_id, grant, signer.public_key())?;
    // The redeem echoes the grant's authoritative workspace — it must match the one we enrolled against.
    if redeem.workspace_id != context.workspace_id {
        return Err(ClientError::Enrollment(
            "the redeemed workspace does not match the invite".into(),
        ));
    }
    Ok(redeem)
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

/// Continue a just-promoted ADDRESS enrollment into its recorded follow intent: resolve the intent
/// WITHIN the newly-joined workspace, then describe/apply per this invocation's flags. A bare
/// resumed `follow` therefore lands on the DESCRIBE (with the `--yes` argv as its next action) —
/// the enrollment happened, the subscription still waits for consent.
fn continue_into_target(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    context: &enroll::EnrollContext,
    target: &enroll::FollowTargetDoc,
    opts: &FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    let directory = (connectors.directory)(&context.base_url);
    let names = universe_for(&*directory, &context.workspace_id)?;
    let resolution = match target.kind {
        enroll::FollowKindDoc::Workspace => Resolution::Workspace {
            workspace_id: context.workspace_id.clone(),
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
        &context.base_url,
        &context.workspace_id,
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
    let describe = build_describe(
        ctx,
        &me,
        &channels,
        &catalog,
        &snapshot,
        resolutions,
        enrolled_now,
    )?;

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
    // 0) Refresh the stored display name to the workspace's TRUE name — the enroll poll could only
    //    echo the requested address slug (it must not disclose the real name before the redeem gate),
    //    and this member-authenticated `me` read is the first place the real name is known.
    enroll::set_membership_display_name(ctx.fs, &ctx.layout, ws_id, &me.display_name)?;
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
    })
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
        let placement = doc::read_doc::<PlacementMap>(ctx.fs, &ctx.layout.published(&sid).map)?
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
// Promotion — the sidecar writers (crash-safe; idempotent so a re-resume re-promotes cleanly).
// =================================================================================================

#[allow(clippy::too_many_arguments)]
fn promote(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    context: &enroll::EnrollContext,
    credential: &str,
    device_key_id: &str,
    principal: Option<&str>,
    enrolled_at: i64,
    signer: &DeviceSigner,
) -> Result<FollowData, ClientError> {
    let currency = promote_core(
        ctx,
        context,
        credential,
        device_key_id,
        principal,
        enrolled_at,
        signer,
    )?;

    // Disclose the batched offers (a READ-ONLY metadata fetch — places NOTHING, never mutates the
    // sidecar, so first-receive stays an OFFER). Best-effort: a fetch hiccup omits that skill's offer.
    let skills = disclose_offers(connectors, context, credential);

    Ok(FollowData {
        workspace_id: context.workspace_id.clone(),
        enrolled: true,
        skills,
        deployment_mode: Some(context.deployment_mode),
        workspace_display_name: Some(context.workspace_display_name.clone()),
        verified_domain: context.verified_domain.clone(),
        verified_domain_status: Some(context.verified_domain_status),
        plane_base_url: Some(context.base_url.clone()),
        pending: None,
        currency: Some(currency),
    })
}

/// The sidecar half of a promotion — every durable write, WITHOUT the follow-shaped offer disclosure (the
/// standup `publish` promotes through this too, then continues into its own publish body). Returns the
/// currency-trigger report. Idempotent, so a re-resume re-promotes cleanly.
#[allow(clippy::too_many_arguments)]
pub(super) fn promote_core(
    ctx: &Ctx<'_>,
    context: &enroll::EnrollContext,
    credential: &str,
    device_key_id: &str,
    principal: Option<&str>,
    enrolled_at: i64,
    signer: &DeviceSigner,
) -> Result<topos_types::TriggerReport, ClientError> {
    // Crash-coherence of a (possibly second, same-plane) promotion. Every step below is a SINGLE
    // atomic-per-file durable write (or an idempotent read-merge-write). The `Redeemed` enrollment WAL —
    // written BEFORE this fn and deleted only in the last step — is the transaction log: a crash at any step
    // leaves every touched file byte-for-byte pre- or post-write (never torn), and the next
    // re-invoking `follow` REPLAYS this whole sequence from the WAL until it converges. Each step is
    // idempotent under that replay:
    //   1) instance.json      — atomic write of identical bytes (the plane is the same on a re-promote);
    //   2) credentials.json   — read-merge-write, upsert this workspace's credential (the Bearer for every
    //                           request in it), deduped by workspace_id, so an already-joined workspace's
    //                           credential is never dropped;
    //   3) follows.json       — read-merge-write, deduped by skill_id, so this workspace's rows are added-
    //                           or-refreshed and an already-followed workspace's rows are never dropped;
    //   4) user.json          — read-upsert-write, deduped by workspace_id, so this membership is added-or-
    //                           refreshed and any already-joined workspace's membership is never dropped;
    //   5) host.json          — re-records the same device-key reference (identical bytes);
    //   6) baselines          — a skill dir that already exists is left untouched; a partial staging is rebuilt;
    //   7) delete the WAL      — the LAST durable step, so "WAL absent" proves steps 1-6 all completed.
    // The write ORDER is load-bearing: credentials.json (step 2) lands before the membership (step 4), so a
    // committed membership always implies its workspace credential is already on disk. Proven by the
    // crash-gate test `second_enrollment_promote_is_crash_coherent_and_never_drops_the_first`.
    //
    // Residual (documented, not closed): the steps are atomic PER FILE, not as one transaction, so a
    // DIFFERENT command running CONCURRENTLY during this promotion could observe the transient gap where
    // credentials.json already carries this workspace but user.json does not yet. This is benign and
    // self-healing: every file is atomic (no torn reads), the WAL heals the gap on the next resume, and the
    // worst an ambient write can observe is a fail-closed WorkspaceSelection/Enrollment error — user.json
    // is all-or-nothing, so a membership is either fully present or absent, never a wrong-workspace
    // signature and never corruption. Widening the identity lock across the credential / follows / user
    // writes to make them one critical section would need lock-free inner variants (each re-takes that
    // lock, so calling one under a held lock would deadlock) — meaningful new internal surface to close a
    // sub-second window whose worst case is already a clean, self-healing error, so this stays a documented
    // residual rather than a lock-widening refactor.

    // 1) instance.json — PUBLIC (no secret) → ordinary perms. PLANE-scoped only now; the per-workspace
    // disclosure moved to the user.json membership written in step 4.
    enroll::write_instance(
        ctx.fs,
        &ctx.layout,
        &enroll::Instance {
            schema_version: PERSISTED_SCHEMA_VERSION,
            base_url: context.base_url.clone(),
            deployment_mode: context.deployment_mode,
            enrollment_method: context.enrollment_method.clone(),
        },
    )?;

    // 2) credentials.json — 0600 (the workspace credential is THE secret); UPSERT this workspace's
    // credential, so a second-workspace follow never drops the first's credential. Lands BEFORE the
    // membership (step 4), so a committed membership always implies its Bearer credential is on disk.
    enroll::write_credential(ctx.fs, &ctx.layout, &context.workspace_id, credential)?;

    // 3) follows.json — pure subscription state; READ-MERGE-WRITE so a second follow never clobbers the
    // first. One entry per skill the enrollment offered (the bootstrap-declared set the WAL carries).
    let additions: Vec<enroll::FollowEntry> = context
        .offered_skills
        .iter()
        .map(|s| enroll::FollowEntry {
            skill_id: s.skill_id.clone(),
            workspace_id: context.workspace_id.clone(),
            mode: context.mode,
            review_required: false,
            following: true,
            excluded_here: false,
        })
        .collect();
    enroll::write_follows_merged(ctx.fs, &ctx.layout, &additions)?;

    // 4) user.json — metadata only (no secret) → ordinary perms. READ-MERGE-WRITE the memberships so a
    // second follow (into another workspace on the same plane) ADDS a membership rather than dropping the
    // first. The per-INSTALL identity (principal/email) is refreshed only when this promote carries a
    // principal (an email-shaped one also fills `email`; a device-rooted `dev.…` id is NOT an email and
    // never pretends to be one — and never clobbers a previously-seated email).
    let mut user = enroll::read_user(ctx.fs, &ctx.layout)?.unwrap_or_else(|| enroll::UserDoc {
        schema_version: PERSISTED_SCHEMA_VERSION,
        email: None,
        principal: None,
        workspaces: Vec::new(),
    });
    user.schema_version = PERSISTED_SCHEMA_VERSION;
    if let Some(p) = principal {
        user.principal = Some(p.to_owned());
        if p.contains('@') {
            user.email = Some(p.to_owned());
        }
    }
    enroll::upsert_membership(
        &mut user,
        enroll::Membership {
            workspace_id: context.workspace_id.clone(),
            display_name: Some(context.workspace_display_name.clone()),
            roles: Vec::new(),
            verified_domain: context.verified_domain.clone(),
            verified_domain_status: context.verified_domain_status,
            invite_rooted: matches!(context.root, enroll::EnrollRoot::Invite),
            enrolled_at,
        },
    );
    enroll::write_user(ctx.fs, &ctx.layout, &user)?;

    // 5) Record the device key reference in host.json (the PUBLIC key + a pointer to the 0600 seed).
    identity::set_device_key(
        ctx.fs,
        &ctx.layout,
        &DeviceKeyRef {
            alg: "Ed25519".to_owned(),
            device_key_id: device_key_id.to_owned(),
            public_key: to_hex(&signer.public_key()),
            private_key_ref: "device.key".to_owned(),
        },
    )?;

    // 6) Lay the first-receive baseline for each offered skill (so the pull engine treats it as state-②).
    // The bootstrap-declared skill id is parsed at this boundary too (the wire transport already validated
    // it; a WAL-resumed promote revalidates) — only the validated newtype reaches the path joins below.
    for offered in &context.offered_skills {
        let skill_id = crate::id::SkillId::parse(&offered.skill_id)?;
        lay_first_receive_baseline(
            ctx,
            &skill_id,
            display_name(context, &offered.skill_id),
            &context.workspace_display_name,
        )?;
    }

    // 7) Delete the WAL — enrollment is complete.
    enroll::delete_wal(ctx.fs, &ctx.layout)?;

    // 8) Arm session-start currency for this follower — best-effort + idempotent, mirroring `add`. A pure
    // follower never runs `add`, so this is the one place their hook gets installed; it edits the harness
    // CONFIG (never a skill dir), and the sweep no-ops until the first bytes land. Infallible (a
    // TriggerReport, degraded on a config hiccup), so it can never roll back the completed enrollment;
    // the outcome is disclosed on the result.
    Ok(ctx.harness.install_currency_trigger())
}

/// Lay the NEVER-RECEIVED sidecar baseline for `skill_id` (mirrors `ops::add`'s staged-then-renamed,
/// all-or-nothing publish). A fresh `sync` (`observed = applied = (0,0)`, empty `recorded`), an empty
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

    // The adapter keeps a `&str` seam; the id here is the validated newtype, honoring its "callers pass
    // an already-validated id" contract. The display name + workspace slug are UNTRUSTED advisory hints —
    // the adapter sanitizes them and falls back to the id, so they can never redirect the placement.
    let placement = ctx
        .harness
        .placement_for(
            skill_id.as_str(),
            topos_harness::PlacementNaming {
                name: Some(&name),
                workspace_slug: Some(workspace_slug),
            },
            None,
        )
        .dir;
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
    doc::write_doc(
        ctx.fs,
        &sp.map,
        &PlacementMap {
            schema_version: PERSISTED_SCHEMA_VERSION,
            placements: vec![placement.to_string_lossy().into_owned()],
            applied_commit: ZERO_HEX.to_owned(),
            materialized_sha: ZERO_HEX.to_owned(),
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
            harness: Some(ctx.harness.id()),
            harness_layer: None,
            harness_slug: Some(ctx.harness.id().slug().to_owned()),
        },
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

/// Disclose the batched first-receive offers via a READ-ONLY metadata fetch (get the served `current`,
/// scope-check it, fetch the version bytes, recompute the `bundle_digest`). It writes NOTHING to the
/// sidecar, so the skill stays never-received (an OFFER on the next pull). Best-effort.
fn disclose_offers(
    connectors: &FollowConnectors<'_>,
    context: &enroll::EnrollContext,
    credential: &str,
) -> Vec<FollowOffer> {
    if context.offered_skills.is_empty() {
        return Vec::new();
    }
    // Every offered skill reads under the ONE workspace credential (a `SkillCred` per skill sharing it).
    let creds: HashMap<String, SkillCred> = context
        .offered_skills
        .iter()
        .map(|s| {
            (
                s.skill_id.clone(),
                SkillCred::new(context.workspace_id.clone(), credential.to_owned()),
            )
        })
        .collect();
    let plane = (connectors.plane)(&context.base_url, creds);

    let mut offers = Vec::new();
    for s in &context.offered_skills {
        if let Some(offer) = disclose_one(&*plane, &s.skill_id, &context.workspace_id) {
            offers.push(FollowOffer {
                skill_id: s.skill_id.clone(),
                name: display_name(context, &s.skill_id),
                offer,
            });
        }
    }
    offers
}

/// One skill's offer: scope-check its `current` pointer, fetch the version, recompute the digest. `None`
/// on any read/scope failure (the offer is then simply not disclosed — the subsequent `pull` discloses it).
fn disclose_one(plane: &dyn PlaneSource, skill_id: &str, workspace_id: &str) -> Option<Offer> {
    let PointerFetch::Record(rec) = plane.get_current(skill_id, None).ok()? else {
        return None;
    };
    let version_id = sync_engine::scoped_version_id(&rec, skill_id, workspace_id)?;
    let fetched = plane.fetch_version(skill_id, version_id).ok()?;
    let entries: Vec<ManifestEntry> = fetched
        .files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let digest = digest::bundle_digest(&entries).ok()?;
    Some(Offer {
        version_id: to_hex(&version_id),
        bundle_digest: to_hex(&digest),
    })
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
            deployment_mode: None,
            workspace_display_name: None,
            verified_domain: None,
            verified_domain_status: None,
            plane_base_url: None,
            pending: None,
            currency: None,
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
                .and_then(|m| m.display_name)
        })
        .unwrap_or_else(|| workspace_id.to_owned())
}

// =================================================================================================
// Small helpers.
// =================================================================================================

/// Parse `<base_url>/i/<token>` into `(base_url, token)` — the FULL URL splits on `/i/` (the token is
/// the first path segment after it). That is the ONLY spelling the product hands out (`mint-claim`
/// prints the whole link; the web shows the whole link), and the dispatch routes only `/i/`-carrying
/// targets here, so a bare token is a malformed link — never a base to guess.
///
/// The base is validated as a well-formed absolute http(s) URL HERE — before the secret token is ever
/// spliced into a request URL — because a malformed base would otherwise surface downstream as a ureq
/// `BadUri` transport error whose message echoes the FULL URI (token included), and every transport error
/// detail is persisted to the `~/.topos/log.jsonl` diagnostics file.
fn parse_link(link: &str) -> Result<(String, String), ClientError> {
    let link = link.trim();
    if let Some(idx) = link.find("/i/") {
        let base = link[..idx].trim_end_matches('/');
        let rest = &link[idx + 3..];
        let token = rest.split(['/', '?', '#']).next().unwrap_or("");
        if base.is_empty() || token.is_empty() {
            return Err(ClientError::Enrollment("malformed invite link".into()));
        }
        validate_base_url(base)?;
        return Ok((base.to_owned(), token.to_owned()));
    }
    Err(ClientError::Enrollment(
        "pass the full /i/<token> claim link".into(),
    ))
}

/// Resolve the API base a follow re-roots onto: the bootstrap's declared `plane.base_url`, normalized
/// (trimmed of trailing slashes — the pin comparisons are exact string equality) and gated the same way
/// as the link base — plus the one extra rule the re-root introduces: an `https` link must never re-root
/// onto a plain-`http` plane (a transport downgrade the human who pasted the link could not see).
pub(super) fn resolve_api_base(link_base: &str, declared: &str) -> Result<String, ClientError> {
    let declared = declared.trim().trim_end_matches('/');
    if declared.is_empty() {
        return Err(ClientError::Enrollment(
            "the bootstrap declared no plane base URL; upgrade the plane".into(),
        ));
    }
    validate_base_url(declared)?;
    if link_base.starts_with("https://") && !declared.starts_with("https://") {
        return Err(ClientError::Enrollment(
            "refusing to enroll: the link is https but the plane declares a plain-http base URL"
                .into(),
        ));
    }
    Ok(declared.to_owned())
}

/// Refuse a plane base that is not a well-formed absolute `http(s)://…` URL (the transport's own `Uri`
/// grammar, so anything accepted here builds cleanly downstream). The error names the problem — never the
/// link's token, which the caller has not yet joined onto the base.
fn validate_base_url(base: &str) -> Result<(), ClientError> {
    let well_formed = base.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && authority_usable(&uri)
    });
    if well_formed {
        Ok(())
    } else {
        Err(ClientError::Enrollment(
            "malformed invite link: the plane base URL is not a valid http(s) URL".into(),
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

/// Build the pending FollowData (the agent surfaces the URL WITH the verified-domain provenance — the
/// relay-phishing guard — then re-invokes `follow`). `verification_uri_complete` is the SERVER-built
/// link when the plane provided one (used verbatim), else the caller's reconstruction.
fn pending_followdata(
    context: &enroll::EnrollContext,
    user_code: &str,
    verification_uri_complete: String,
    device_fingerprint: String,
) -> FollowData {
    FollowData {
        workspace_id: context.workspace_id.clone(),
        enrolled: false,
        skills: Vec::new(),
        deployment_mode: Some(context.deployment_mode),
        workspace_display_name: Some(context.workspace_display_name.clone()),
        verified_domain: context.verified_domain.clone(),
        verified_domain_status: Some(context.verified_domain_status),
        plane_base_url: Some(context.base_url.clone()),
        pending: Some(EnrollmentPending {
            verification_uri_complete,
            user_code: user_code.to_owned(),
            device_fingerprint,
            // No RFC-3339 formatter client-side; the WAL holds the absolute expiry for the recovery sweep.
            expires_at: None,
        }),
        currency: None,
    }
}

/// The verification URL with the `user_code` embedded (RFC-8628 `verification_uri_complete`) — the
/// CLIENT-side reconstruction, used only as the fallback when the plane did not provide the complete URI.
pub(super) fn complete_uri(verification_uri: &str, user_code: &str) -> String {
    let sep = if verification_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{verification_uri}{sep}user_code={user_code}")
}

/// A followed skill's display name from the bootstrap (else its id).
fn display_name(context: &enroll::EnrollContext, skill_id: &str) -> String {
    context
        .offered_skills
        .iter()
        .find(|s| s.skill_id == skill_id)
        .and_then(|s| s.name.clone())
        .unwrap_or_else(|| skill_id.to_owned())
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

/// A human-readable machine name for the verification page (a confused-deputy aid, not authority) — carries
/// the device key id so a human can cross-check the fingerprint shown on the page.
pub(super) fn machine_name(signer: &DeviceSigner) -> String {
    format!("topos CLI ({})", signer.device_key_id())
}

/// The 16-hex device fingerprint the plane shows on its verification page — the first 16 hex chars of
/// `sha256(device_public_key)`. The device key id is `dk_` + the 32 hex of that same digest, so the
/// fingerprint is exactly the leading half of the id's hex portion. Returned raw (no grouping); the TTY
/// renderer groups it for eyeball comparison. A human cross-checks it against the page before approving.
pub(super) fn device_fingerprint(signer: &DeviceSigner) -> String {
    let id = signer.device_key_id();
    id.strip_prefix("dk_")
        .unwrap_or(id)
        .chars()
        .take(16)
        .collect()
}

/// `now` as epoch-millis (saturating), via the injected clock.
fn now_millis(ctx: &Ctx<'_>) -> i64 {
    i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{device_fingerprint, resolve_api_base, validate_base_url};

    /// The device fingerprint is the leading 16 hex of the device key id's hex portion (`dk_` + 32 hex);
    /// grouped in fours by the TTY renderer, it is what a human cross-checks against the verification page.
    #[test]
    fn device_fingerprint_is_the_first_16_hex_of_the_key_id() {
        use crate::device_signer::DeviceSigner;
        use crate::fs_seam::RealFs;
        use crate::sidecar::Layout;

        let dir = std::env::temp_dir().join(format!(
            "topos-fp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let signer = DeviceSigner::load_or_generate(&RealFs, &Layout::new(&dir)).unwrap();

        let fp = device_fingerprint(&signer);
        // Exactly the first 16 hex of the id (id = `dk_` + 32 hex of sha256(pubkey)).
        let id_hex = signer.device_key_id().strip_prefix("dk_").unwrap();
        assert_eq!(fp, id_hex[..16]);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));

        // The grouped display (raw id → grouped): 4×4 hex separated by single spaces.
        let grouped = crate::render::group_fingerprint(&fp);
        assert_eq!(grouped.split(' ').count(), 4);
        assert!(grouped.split(' ').all(|chunk| chunk.len() == 4));
        assert_eq!(grouped.replace(' ', ""), fp);

        let _ = std::fs::remove_dir_all(&dir);
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
