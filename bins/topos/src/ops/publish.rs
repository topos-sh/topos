//! `publish [--propose] <skill>[@<digest>]` — ship a draft to the team.
//!
//! `publish` moves `current` to the freshly-scanned draft (a direct publish, or a genesis create for a
//! never-published skill); `--propose` opens a PR without moving `current`. The client computes the
//! byte-identical `commit_id`/`bundle_digest` the plane re-derives (I-COMMIT-PARITY); when the target
//! carries an optional `@<digest>` pin it gates the outward ship on that pin matching the scanned bytes
//! (refusing on mismatch — never a silent mode-flip), and without a pin it just ships the computed digest.
//! It persists an op-WAL before the first send (so an uncertain retry replays the same
//! `op_id`), and maps the plane's typed outcome.
//!
//! An UN-ENROLLED publish is refused typed — enrollment is `topos follow <workspace-address>` (the
//! device-authorization flow), and workspaces are born server-side, never from a publish.

use topos_core::digest::to_hex;
use topos_core::identity::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_types::persisted::{ConflictState, Lock, OpKind, OpRecord, PlacementMap, SyncState};
use topos_types::results::{AddedNote, ProposeData, PublishData};
use topos_types::{PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use topos_types::results::{PublishDescribeData, PublishGate};

use super::contribute::{self, ContributeConnect, PUBLISH_MESSAGE};
use super::follow::{DeliveryConnect, DirectoryConnect};
use super::sync_engine;
use super::{
    DiscoveryRoots, add, add_with_name, parse_hex32, resolve_add_target, resolve_skill,
    resolve_skill_in_workspace, split_target, tracked_skill_at, write_workspace_for_skill,
};
use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::WriteReceipt;
use crate::source::{self, SourceSpec};
use crate::{doc, op_wal, scan, sidecar};

/// The result of `publish`: either `current` moved (a direct publish), or a proposal opened
/// (`--propose`, or the protection gate's downgrade).
#[derive(Debug)]
pub(crate) enum PublishOutcome {
    /// A direct publish moved `current` to the draft.
    Published(PublishData),
    /// `--propose` opened a proposal (NEEDS_REVIEW); `current` did NOT move.
    Proposed(ProposeData),
}

/// The genesis base — a skill whose `current` does not exist yet is published as a zero-parent commit at
/// generation `0` (the plane's genesis branch creates `current` at `1`).
const GENESIS: u64 = 0;

/// Ship `target`'s draft (or, with `propose`, open a proposal), ADDING the skill to topos first if it is an
/// untracked LOCAL source. `target` is `<source>[@<digest>]`: the optional `@<digest>` pin re-verifies the
/// scanned bytes, and the SOURCE (the rest) is a tracked skill name, an untracked `<name>` / `<name>@<harness>`
/// / `<dir>` the client adopts before publishing (the auto-add convenience — one command instead of
/// `add` then `publish`), or a remote/unsupported form that is refused typed. An un-enrolled publish is
/// refused BEFORE any local adoption, so it never mutates local state.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled (run `topos follow <workspace-address>` first);
/// [`ClientError::InvalidArgument`] if the source is remote/unsupported (add it first);
/// [`ClientError::HarnessMismatch`] if a `@<harness>` names a different harness than the tracked skill;
/// the `add`-family errors ([`ClientError::AmbiguousHarness`] / [`ClientError::NoUntrackedSkill`] / …) when
/// resolving an untracked source; [`ClientError::ApprovalMismatch`] if a `@<digest>` pin does not match the
/// scanned bytes; [`ClientError::PublishBlocked`] if an unresolved merge conflict is present;
/// [`ClientError::Conflict`] / [`ClientError::Denied`] on the plane's
/// typed verdict; a transport / store failure otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    roots: Option<&DiscoveryRoots>,
    target: &str,
    propose: bool,
    channel: Option<&str>,
    workspace: Option<&str>,
    message: Option<&str>,
) -> Result<PublishOutcome, ClientError> {
    // Split off an optional `@<digest>` consent pin (64-hex only); everything else is the SOURCE.
    let (source_str, pin) = parse_target(target);

    // Enrollment first — BEFORE any local adoption, so an un-enrolled publish never mutates local
    // state. Sharing needs a workspace, and joining one is the device flow, not a publish.
    if enroll::read_instance(ctx.fs, &ctx.layout)?.is_none() {
        return Err(ClientError::Enrollment(
            "not enrolled — run `topos follow <workspace-address>` first, then re-run this publish"
                .into(),
        ));
    }

    // Auto-add: adopt an untracked LOCAL source before publishing, and learn the tracked skill name the
    // rest of the flow resolves. `added` is `Some` iff THIS invocation performed the adoption (disclosure).
    let (skill_name, added) = ensure_tracked(ctx, roots, &source_str)?;

    let outcome = enrolled_publish(
        ctx,
        connect,
        &skill_name,
        propose,
        channel,
        pin.as_deref(),
        workspace,
        message,
    )?;
    Ok(stamp_added(outcome, added))
}

/// The seams `publish`'s describe needs, both read only AFTER the local scan: the directory connector
/// reads the audience (reach) + the workspace address (the share line); the delivery connector reads the
/// FRESH per-skill protection the gate turns on — the one server fact the sidecar's cached follow-state
/// (stamped at the last delivery reconcile) can misreport in either direction after an owner re-protects.
pub(crate) struct PublishDescribeConnectors<'a> {
    pub directory: &'a DirectoryConnect<'a>,
    pub delivery: &'a DeliveryConnect<'a>,
}

/// The bare (no `--yes`) ENROLLED publish describe — what shipping this draft WOULD do: where it lands,
/// the gate outcome, the audience, the share line, and the undo path. Mutates nothing on the plane (a
/// local auto-add of an untracked source is the one disclosed local step, exactly as the apply performs).
/// The network is read only AFTER the local scan; the genesis / WAL apply paths are untouched
/// (this runs only for an enrolled `!yes` invocation, dispatched in the composition root).
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::NoChanges`] when the draft equals current;
/// [`ClientError::ApprovalMismatch`] on a failed `@<digest>` pin; [`ClientError::PublishBlocked`] on an
/// unresolved merge; name-resolution / scan / transport errors.
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_describe(
    ctx: &Ctx<'_>,
    connectors: &PublishDescribeConnectors<'_>,
    roots: Option<&DiscoveryRoots>,
    target: &str,
    propose: bool,
    channel: Option<&str>,
    workspace: Option<&str>,
) -> Result<PublishDescribeData, ClientError> {
    let (source_str, pin) = parse_target(target);
    let _ = roots;
    // A describe MUTATES NOTHING (the consent contract). An already-tracked target is scanned in place;
    // an UNTRACKED source is NOT adopted here — adopting mints a sidecar and arms the session-start hook,
    // a durable change the human has not confirmed. The apply (`--yes`) does the adoption and discloses
    // it; the describe points the user at that.
    let skill_name = match resolve_skill(ctx, &source_str) {
        Ok((_, lock)) => lock.name,
        // Tracked ambiguously (2+ under this exact name) — the `--workspace`-filtered resolve below picks.
        Err(ClientError::AmbiguousName { .. }) => source_str.clone(),
        Err(ClientError::NoSuchSkill { .. }) => {
            return Err(ClientError::InvalidArgument(format!(
                "'{source_str}' is not tracked yet — a describe will not adopt it (that would change \
                 this machine before you confirm). Run `topos add {source_str}` first to preview it, \
                 or `topos publish {source_str} --yes` to adopt and ship in one step."
            )));
        }
        Err(e) => return Err(e),
    };

    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    let (id, lock) = resolve_skill_in_workspace(ctx, &skill_name, workspace)?;
    let workspace_id = write_workspace_for_skill(ctx, id.as_str(), workspace)?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Publish-block guard (same presence check the apply runs): an unresolved merge blocks a publish.
    if doc::read_doc::<ConflictState>(ctx.fs, &sp.conflict)?.is_some() {
        return Err(ClientError::PublishBlocked {
            skill: skill_name.clone(),
        });
    }

    // Scan the live draft ONCE → the byte-exact digest the apply would ship; the optional `@<digest>` pin
    // gates it here too (refuse on mismatch), so a describe never previews bytes the apply would refuse.
    let map: PlacementMap = doc::read_doc(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
    let placement = sync_engine::first_placement(&map)?;
    let scanned = scan::scan(std::path::Path::new(&placement))?;
    let digest_hex = to_hex(&scanned.bundle_digest);
    if let Some(pin) = &pin
        && digest_hex != *pin
    {
        return Err(ClientError::ApprovalMismatch {
            skill: skill_name.clone(),
            expected: digest_hex,
            got: pin.clone(),
        });
    }

    // A FOLLOWED skill's `current` is the bytes this client holds (`lock.bundle_digest`); an identical
    // draft is a no-op. A genesis skill (no follow entry) has no current — its first publish is never a
    // no-op.
    let follow_entry = ctx
        .follow
        .followed()
        .into_iter()
        .find(|(fid, _)| fid == id.as_str())
        .map(|(_, fc)| fc);
    let followed = follow_entry.is_some();
    if followed && digest_hex == lock.bundle_digest {
        return Err(ClientError::NoChanges { skill: skill_name });
    }

    // The gate the plane will apply: a reviewed bundle (or an explicit `--propose`) becomes a proposal;
    // an open one lands directly. Protection is a SERVER fact that can move after this device's last sync
    // (an owner runs `protect <skill> reviewed` — or loosens it back to `open`), so the sidecar's cached
    // `review_required` — stamped at the last delivery reconcile — can misreport the gate in EITHER
    // direction. Read the FRESH protection the delivery carries per skill (a read, after the local scan —
    // the describe still mutates nothing) and prefer it, so the consent shown matches the act the apply
    // performs; an offline/failed read falls back to the cached value, so the describe keeps working
    // offline. A genesis (unfollowed) skill has no server protection — its first publish keeps the
    // no-gate path.
    let review_required = match &follow_entry {
        Some(fc) => {
            fresh_review_required(connectors, &instance.base_url, &workspace_id, id.as_str())
                .unwrap_or(fc.review_required)
        }
        None => false,
    };
    let gate = if propose || review_required {
        PublishGate::Proposal
    } else {
        PublishGate::Lands
    };

    // Network reads AFTER the local scan: the audience (reach) + the workspace address (the share line).
    let directory = (connectors.directory)(&instance.base_url);
    let reach = directory
        .reach(&workspace_id, id.as_str())
        .ok()
        .map(|r| r.persons);
    let me = directory.me(&workspace_id).ok();

    let placements = match channel {
        Some(ch) => vec![ch.to_owned()],
        // A brand-new skill's reference lands in `everyone`; a re-publish of a followed skill keeps its
        // existing placements (none added here).
        None if !followed => vec!["everyone".to_owned()],
        None => Vec::new(),
    };
    let share_line = me
        .as_ref()
        .map(|m| format!("{}/skills/{}", m.address, skill_name));
    let undo = followed.then(|| format!("topos revert {skill_name} --to {}", lock.base_commit));
    let origin_note = doc::read_doc::<add::OriginDoc>(ctx.fs, &sp.origin)?.map(|o| {
        format!(
            "this skill was imported from {} — publishing makes the team copy the source of truth",
            o.origin.source
        )
    });

    Ok(PublishDescribeData {
        skill: skill_name,
        skill_id: id.into_string(),
        workspace_id,
        workspace_display_name: me.map(|m| m.display_name),
        bundle_digest: digest_hex,
        placements,
        gate,
        // The full ancestor-bytes revert detection is the apply path's (the server treats a revert-shaped
        // publish as a forward move); the describe reports the gate + placements without pre-judging it.
        is_revert: false,
        reach,
        share_line,
        undo,
        origin_note,
    })
}

/// The server's FRESH per-skill protection for the describe's gate — the delivery read carries it (each
/// delivered skill's re-resolved `review_required` posture). It is the authoritative answer the apply will
/// see, unlike the sidecar's cached follow-state, which is stamped at the last delivery reconcile and can
/// lie in EITHER direction after an owner tightens or loosens `protect`. `None` on an offline/failed read
/// or a skill the delivery does not name (a followed-but-excluded copy) — the caller falls back to the
/// cached value, so the describe still works offline.
fn fresh_review_required(
    connectors: &PublishDescribeConnectors<'_>,
    base_url: &str,
    workspace_id: &str,
    skill_id: &str,
) -> Option<bool> {
    let delivery = (connectors.delivery)(base_url);
    let snapshot = delivery.fetch_delivery(workspace_id).ok()?;
    snapshot
        .skills
        .into_iter()
        .find(|s| s.skill_id == skill_id)
        .map(|s| s.review_required)
}

/// Resolve `source_str` (the target minus any `@<digest>` pin) to a TRACKED skill NAME the rest of the
/// publish flow resolves, ADDING it first if it is an untracked local source. Returns the name plus the
/// per-invocation [`AddedNote`] disclosure (`Some` iff THIS call adopted the skill; `None` when already
/// tracked).
///
/// An EXACT tracked-name match wins BEFORE any source-shape classification — so a tracked skill whose name
/// happens to look like a path / remote / `<name>@<harness>` shape (`owner/repo`, `foo@bar`) still publishes
/// by its literal name. Only a name tracked NOWHERE is classified by shape ([`crate::source::classify`],
/// the same classifier `add` uses) and adopted.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a remote/unsupported source (add it first);
/// [`ClientError::HarnessMismatch`] for a `@<harness>` that disagrees with an already-tracked name; the
/// `add`-family resolution errors when adopting an untracked source; a store/io failure otherwise.
pub(crate) fn ensure_tracked(
    ctx: &Ctx<'_>,
    roots: Option<&DiscoveryRoots>,
    source_str: &str,
) -> Result<(String, Option<AddedNote>), ClientError> {
    // Exact literal tracked name wins first (never re-adopt / misclassify a tracked skill).
    match resolve_skill(ctx, source_str) {
        Ok((_, lock)) => return Ok((lock.name, None)),
        // Tracked ambiguously (2+ under this exact name) — hand it to the ordinary `--workspace`-filtered
        // resolve downstream; never auto-add over it.
        Err(ClientError::AmbiguousName { .. }) => return Ok((source_str.to_owned(), None)),
        // Not a literal tracked name — fall through to source classification + auto-add.
        Err(ClientError::NoSuchSkill { .. }) => {}
        Err(e) => return Err(e),
    }
    match source::classify(source_str) {
        SourceSpec::LocalName(raw) => ensure_name(ctx, roots, &raw),
        SourceSpec::LocalPath(p) => ensure_path(ctx, &p),
        // `publish` adopts LOCAL skills only — a remote import is a deliberate, separate `add` step (it
        // reaches the network and lands foreign bytes; the source's trust is the caller's to verify there).
        SourceSpec::Remote(_) => Err(ClientError::InvalidArgument(format!(
            "`topos publish` adds LOCAL skills only — '{source_str}' is a remote source; run \
             `topos add {source_str}` to import it first, then `topos publish <skill>`"
        ))),
        SourceSpec::Unsupported(msg) => Err(ClientError::InvalidArgument(msg)),
    }
}

/// The `<name>` / `<name>@<harness>` arm of [`ensure_tracked`]: publish an already-tracked name (verifying
/// any `@<harness>` matches it), or resolve the name against discovery and adopt it.
fn ensure_name(
    ctx: &Ctx<'_>,
    roots: Option<&DiscoveryRoots>,
    raw: &str,
) -> Result<(String, Option<AddedNote>), ClientError> {
    let (bare, harness) = split_target(raw);
    match resolve_skill(ctx, bare) {
        // Uniquely tracked → publish it. A `@<harness>` that names a DIFFERENT harness than the tracked
        // skill's likely means a different copy was intended — refuse rather than publish these bytes.
        Ok((id, lock)) => {
            if let Some(requested) = harness {
                let map: PlacementMap = doc::read_doc(ctx.fs, &ctx.layout.published(&id).map)?
                    .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
                if map.harness_slug.as_deref() != Some(requested) {
                    return Err(ClientError::HarnessMismatch {
                        name: lock.name,
                        requested: requested.to_owned(),
                        tracked: map.harness_slug.unwrap_or_else(|| "<none>".to_owned()),
                    });
                }
            }
            Ok((lock.name, None))
        }
        // Tracked under this name more than once (across workspaces) — NOT an auto-add case; hand the bare
        // name to the ordinary flow, whose `--workspace`-filtered resolve disambiguates (or re-errors). A
        // `@<harness>` is only a verification for a UNIQUELY-tracked name; across ambiguous copies `--workspace`
        // is the deliberate selector, so the harness qualifier is advisory here (not re-checked per copy).
        Err(ClientError::AmbiguousName { .. }) => Ok((bare.to_owned(), None)),
        // Untracked → resolve the name against discovery + adopt it (the `add <name>` path), then publish
        // under the resolved name.
        Err(ClientError::NoSuchSkill { .. }) => {
            let roots = roots.ok_or_else(|| {
                ClientError::InvalidArgument(
                    "cannot resolve a skill name without $HOME set — publish a directory by path \
                     (`topos publish ./<dir>`)"
                        .into(),
                )
            })?;
            let (path, name) = resolve_add_target(ctx, roots, raw)?;
            let data = add_with_name(ctx, &path, Some(&name))?;
            Ok((
                data.name.clone(),
                Some(AddedNote {
                    name: data.name,
                    harness_slug: data.harness_slug,
                }),
            ))
        }
        Err(e) => Err(e),
    }
}

/// The `<dir>` arm of [`ensure_tracked`]: publish the tracked skill already at this directory, else adopt it
/// in place (the `add --path`-equivalent) and publish the adopted name.
fn ensure_path(
    ctx: &Ctx<'_>,
    p: &std::path::Path,
) -> Result<(String, Option<AddedNote>), ClientError> {
    // Already tracked at this dir → publish it (never re-adopt). Reachable only when the path canonicalizes;
    // a bad/absent path falls through to `add`, which produces the proper scan/io error.
    if let Ok(canonical) = p.canonicalize()
        && let Some(id_str) = tracked_skill_at(ctx, &canonical)?
    {
        let id = crate::id::SkillId::parse(&id_str)?;
        let lock: Lock = doc::read_doc(ctx.fs, &ctx.layout.published(&id).lock)?
            .ok_or_else(|| ClientError::Corrupt("missing lock doc".to_owned()))?;
        return Ok((lock.name, None));
    }
    let data = add(ctx, p)?;
    Ok((
        data.name.clone(),
        Some(AddedNote {
            name: data.name,
            harness_slug: data.harness_slug,
        }),
    ))
}

/// Attach the per-invocation `added` disclosure to the outcome — Published AND Proposed both carry
/// it (a `--propose` of an untracked source adopts it first too), so a success path never hides the local
/// `add` it performed. A no-op when nothing was added this invocation.
fn stamp_added(mut outcome: PublishOutcome, added: Option<AddedNote>) -> PublishOutcome {
    if let Some(note) = added {
        match &mut outcome {
            PublishOutcome::Published(data) => data.added = Some(note),
            PublishOutcome::Proposed(data) => data.added = Some(note),
        }
    }
    outcome
}

/// The ENROLLED publish body. `pin` is the optional `@<digest>` consent — when present, the scanned
/// bytes must match it; when absent, the computed digest ships as-is.
#[allow(clippy::too_many_arguments)]
fn enrolled_publish(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    skill_name: &str,
    propose: bool,
    channel: Option<&str>,
    pin: Option<&str>,
    workspace: Option<&str>,
    message: Option<&str>,
) -> Result<PublishOutcome, ClientError> {
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment(
            "not enrolled — run `topos follow <workspace-address>` first".into(),
        )
    })?;

    // The `--workspace` filter disambiguates a name shared across workspaces. A FOLLOWED skill signs in
    // its OWN workspace (the pointer scope); a brand-new local skill (a genesis publish, no follow entry)
    // is AMBIENT — the single membership or the `--workspace`-selected one.
    let (id, lock) = resolve_skill_in_workspace(ctx, skill_name, workspace)?;
    let workspace_id = write_workspace_for_skill(ctx, id.as_str(), workspace)?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Publish guard (presence-based, never a marker scan): an unresolved author merge blocks publish.
    if doc::read_doc::<ConflictState>(ctx.fs, &sp.conflict)?.is_some() {
        return Err(ClientError::PublishBlocked {
            skill: skill_name.to_owned(),
        });
    }

    let transport = connect(&instance.base_url);
    let map: PlacementMap = doc::read_doc(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;

    // Scan the live draft ONCE under the lock → the byte-exact digest the plane re-derives. When a
    // `@<digest>` pin is present, gate here (refuse on mismatch — the disclosure/integrity gate, never a
    // silent mode-flip); without a pin the computed digest ships. This digest is what the WAL replay
    // compares against, so a re-run whose draft has drifted refuses the in-flight op instead of riding it.
    let placement = sync_engine::first_placement(&map)?;
    let scanned = scan::scan(std::path::Path::new(&placement))?;
    let digest_hex = to_hex(&scanned.bundle_digest);
    if let Some(pin) = pin
        && digest_hex != pin
    {
        return Err(ClientError::ApprovalMismatch {
            skill: lock.name.clone(),
            expected: digest_hex,
            got: pin.to_owned(),
        });
    }

    // Resume a crashed prior publish/propose for this skill (replay the SAME op_id) before minting a new
    // one — the plane returns the byte-identical receipt, so there is no double-advance / duplicate commit.
    let kinds = [OpKind::PublishDirect, OpKind::PublishPropose];
    let rec = match op_wal::find_pending_for_skill(
        ctx.fs,
        &ctx.layout,
        &workspace_id,
        id.as_str(),
        &kinds,
    )? {
        // A crashed prior publish is still in-flight: replay it ONLY if it matches THIS command (same
        // scanned digest + same direct/propose mode) — otherwise refuse, so a new intent never silently
        // rides the old op's mode/bytes.
        Some(pending) => {
            let pending_propose = matches!(pending.op, OpKind::PublishPropose);
            if pending.bundle_digest != digest_hex || pending_propose != propose {
                return Err(ClientError::PendingOp {
                    skill: skill_name.to_owned(),
                    detail: format!(
                        "a {} of {skill_name}@{} is in flight — settle it (re-run that publish), then retry",
                        if pending_propose {
                            "proposal"
                        } else {
                            "direct publish"
                        },
                        pending.bundle_digest
                    ),
                });
            }
            pending
        }
        None => build_publish_op(
            ctx,
            &sp,
            id.as_str(),
            &lock,
            &workspace_id,
            propose,
            channel,
            &scanned,
            scanned.bundle_digest,
            message,
        )?,
    };

    let receipt = contribute::run_write(ctx, &*transport, &sp, &rec, None)?;
    map_outcome(ctx, &sp, &lock, &map, &rec, &receipt, skill_name)
}

/// Split the single positional `target` into `(skill, Option<consent-digest>)`. A trailing `@<digest>` is
/// the optional consent pin only when the suffix is a full 64-char lowercase-hex bundle digest; otherwise
/// the whole token is the skill name (so a name that itself contains `@` still resolves). Infallible — a
/// malformed suffix is simply treated as part of the name (which then fails resolution, not consent).
fn parse_target(target: &str) -> (String, Option<String>) {
    if let Some((name, suffix)) = target.rsplit_once('@')
        && is_full_digest(suffix)
    {
        return (name.to_owned(), Some(suffix.to_owned()));
    }
    (target.to_owned(), None)
}

/// A byte-exact bundle digest: exactly 64 lowercase-hex chars (the schema-pinned `^[0-9a-f]{64}$`).
fn is_full_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Build the fresh op from the already-scanned draft (`scanned` / `digest` were computed + gated in
/// `enrolled_publish`): precondition the state, compute the byte-identical `(commit_id, bundle_digest)`,
/// commit the candidate into the local store (renderable for a replay + local history), and assemble the
/// [`OpRecord`] (the WAL write itself happens in `run_write`).
#[allow(clippy::too_many_arguments)]
fn build_publish_op(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    id: &str,
    lock: &Lock,
    workspace_id: &str,
    propose: bool,
    channel: Option<&str>,
    scanned: &scan::ScannedBundle,
    digest: [u8; 32],
    message: Option<&str>,
) -> Result<OpRecord, ClientError> {
    // The commit message: `-m <message>` when given (folded into `commit_id`, so it changes the version
    // identity), else the default. It also rides the local store commit, so a WAL replay re-renders the
    // byte-identical candidate (`render_candidate` reads the message back from the store).
    let commit_message = message.unwrap_or(PUBLISH_MESSAGE);
    let sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("missing sync state".to_owned()))?;

    // Be current before publishing: a behind state (a newer `current` not yet applied) would publish on a
    // stale base and could clobber the unapplied version — surface it as a locally-detected CONFLICT
    // (pull to rebase), never a confusing server DENIED.
    if sync.applied != sync.observed {
        return Err(ClientError::Conflict {
            skill: lock.name.clone(),
            current: Some(sync.observed),
        });
    }

    let digest_hex = to_hex(&digest);

    // Genesis (no `current` yet) is a zero-parent commit at generation 0; a normal publish parents on
    // `current`.
    let (parents, expected): (Vec<[u8; 32]>, u64) = if sync.observed == GENESIS {
        (Vec::new(), GENESIS)
    } else {
        (vec![parse_hex32(&lock.base_commit)?], sync.observed)
    };

    // The byte-identical id the plane re-derives (I-COMMIT-PARITY): author = the device id, message = the
    // publish message (`-m` or the default) — both folded into `commit_id`.
    let commit_id = identity::commit_id(&Commit {
        parents: &parents,
        tree: digest,
        author: &ctx.device_id,
        message: commit_message,
    })
    .map_err(|_| ClientError::Corrupt("commit id preimage".to_owned()))?;

    // Pin the candidate in the local store (so a replay re-renders the byte-identical snapshot, and the
    // local history/diff can reach it) BEFORE the WAL/send.
    let store = Store::open(&sp.store)?;
    let import: Vec<ImportFile<'_>> = scanned
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    store.commit(commit_id, &parents, &tree, &ctx.device_id, commit_message)?;
    // The candidate's own objects + ref — durable before the WAL names it; never the whole store.
    sync_engine::fsync_batch(ctx, &store.version_durability(&commit_id)?)?;

    let op_id_bytes = ctx.ids.new_op_id();
    let op_id = uuid::Uuid::from_bytes(op_id_bytes)
        .as_hyphenated()
        .to_string();
    Ok(OpRecord {
        schema_version: PERSISTED_SCHEMA_VERSION,
        op_id,
        workspace_id: workspace_id.to_owned(),
        skill_id: id.to_owned(),
        op: if propose {
            OpKind::PublishPropose
        } else {
            OpKind::PublishDirect
        },
        candidate_commit: to_hex(&commit_id),
        bundle_digest: digest_hex,
        expected_generation: expected,
        good: None,
        // The author's folder name — advisory, so the plane can name the followers' folders + dashboard
        // entry after it (a revert/review carries no name and preserves the stored one).
        display_name: Some(lock.name.clone()),
        channel: channel.map(str::to_owned),
        last_receipt: None,
    })
}

/// Map the plane's typed write outcome to a [`PublishOutcome`] (or a typed [`ClientError`]).
#[allow(clippy::too_many_arguments)]
fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    map: &PlacementMap,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    skill_name: &str,
) -> Result<PublishOutcome, ClientError> {
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            // A direct publish moved `current` — advance the local state (read-your-writes).
            let record = receipt.wire_record.as_ref().ok_or_else(|| {
                ClientError::Corrupt("an OK publish carried no current pointer".to_owned())
            })?;
            let new_gen = contribute::apply_publish_ok(ctx, sp, lock, map, rec, record)?;
            Ok(PublishOutcome::Published(PublishData {
                skill_id: rec.skill_id.clone(),
                name: skill_name.to_owned(),
                version_id: rec.candidate_commit.clone(),
                bundle_digest: rec.bundle_digest.clone(),
                current_generation: new_gen,
                added: None,
            }))
        }
        TerminalOutcome::NeedsReview => Ok(PublishOutcome::Proposed(ProposeData {
            proposal: format!("{skill_name}@{}", rec.candidate_commit),
            base_version_id: lock.base_commit.clone(),
            title: skill_name.to_owned(),
            body: None,
            added: None,
        })),
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: skill_name.to_owned(),
            current: receipt.error.as_ref().and_then(|e| e.current_generation),
        }),
        TerminalOutcome::Denied => Err(ClientError::Denied(denied_code(receipt))),
        // Any other terminal class (RetryableFailure / Unavailable / PermanentFailure / …) is surfaced
        // verbatim, not flattened to a generic transport error.
        _ => Err(contribute::plane_terminal(receipt)),
    }
}

/// The wire error code on a DENIED (for the agent to branch on); never a secret.
fn denied_code(receipt: &WriteReceipt) -> String {
    receipt
        .error
        .as_ref()
        .map(|e| e.code.clone())
        .unwrap_or_else(|| "DENIED".to_owned())
}
