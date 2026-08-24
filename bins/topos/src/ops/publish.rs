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
//! An UN-ENROLLED publish is refused typed — enrollment is `topos login <workspace-address>` (the
//! device-authorization flow), and workspaces are born server-side, never from a publish.

use topos_core::digest::{FileMode, to_hex};
use topos_core::identity::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_types::persisted::{ConflictState, Lock, OpKind, OpRecord, PlacementMap, SyncState};
use topos_types::results::{AddedNote, ProposeData, PublishData};
use topos_types::{PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use topos_types::results::{
    ChangeSummary, PublishDescribeData, PublishGate, PublishNoChangesData, PublishResult,
    Republish, ScopeDraft,
};

use super::contribute::{self, PUBLISH_MESSAGE};
use super::sync_engine;
use super::{
    DiscoveryRoots, add, add_with_name, parse_hex32, resolve_add_target, resolve_skill,
    split_target, tracked_skill_at,
};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::plane::{PlaneError, PointerFetch, WriteReceipt};
use crate::source::{self, SourceSpec};
use crate::{doc, op_wal, scan, sidecar};

/// The wire spelling of a recorded import origin (`origin.json` → the publish body's provenance
/// block): `source` is `<host>/<owner>/<repo>` — split into the host and the `owner/repo` pair.
fn origin_to_wire(
    origin: &topos_types::results::SkillOrigin,
) -> topos_types::requests::WireUpstream {
    let (host, repo) = origin
        .source
        .split_once('/')
        .map_or((origin.source.as_str(), ""), |(h, r)| (h, r));
    topos_types::requests::WireUpstream {
        host: host.to_owned(),
        repo: repo.to_owned(),
        path: origin.subdir.clone().filter(|s| !s.is_empty()),
        commit: origin.commit.clone(),
        license: origin.license.clone(),
    }
}

/// The result of `publish`: `current` moved (a direct publish), a proposal opened (`--propose`, or
/// the protection gate's downgrade), or there was nothing to ship.
#[derive(Debug)]
pub(crate) enum PublishOutcome {
    /// A direct publish moved `current` to the draft (boxed: the receipt is the widest payload
    /// the verb yields, and the enum should not be sized by it).
    Published(Box<PublishData>),
    /// `--propose` opened a proposal (NEEDS_REVIEW); `current` did NOT move. Boxed like the
    /// landed receipt, for the same reason.
    Proposed(Box<ProposeData>),
    /// The copy already matches `current` — a SUCCESS with nothing to ship. The converged state is
    /// what the command asked for, so it is reported, never refused.
    NoChanges(PublishNoChangesData),
}

/// What a bare (no `--yes`) publish yields: the preview of what shipping would do, or the same
/// already-published answer the apply gives — a no-op has nothing to preview, and wrapping one in
/// "Nothing has changed yet — apply with:" would offer a command that does nothing.
#[derive(Debug)]
pub(crate) enum PublishPreview {
    /// What shipping this draft WOULD do.
    Describe(Box<PublishDescribeData>),
    /// The copy already matches `current`.
    NoChanges(PublishNoChangesData),
}

/// The genesis base — a skill whose `current` does not exist yet is published as a zero-parent commit at
/// generation `0` (the plane's genesis branch creates `current` at `1`).
const GENESIS: u64 = 0;

/// Whether a publish DESCRIBE may offer an undo. `revert` is the inverse only when it verifiably
/// restores the WHOLE prior state, so both conditions are load-bearing: the verb resolves a
/// FOLLOWED bundle (a locally-authored one is refused there, so naming the command would hand out
/// an undo that cannot run), and it moves the TEAM's `current` — which a review gate never moves.
/// On that gate a `--to <base>` would restore a state that was never left.
fn undo_is_restorative(followed: bool, gate: PublishGate) -> bool {
    followed && gate == PublishGate::Lands
}

/// Whether a LANDED publish's receipt may offer an undo: the same followed rule, plus an earlier
/// version to name — a GENESIS publish CREATED `current` from nothing, so there is no prior state
/// and no `--to` that means anything.
fn landed_undo_is_restorative(followed: bool, expected_generation: u64) -> bool {
    followed && expected_generation != GENESIS
}

/// Ship `target`'s draft (or, with `propose`, open a proposal), ADDING the skill to topos first if it is an
/// untracked LOCAL source. `target` is `<source>[@<digest>]`: the optional `@<digest>` pin re-verifies the
/// scanned bytes, and the SOURCE (the rest) is a tracked skill name, an untracked `<name>` / `<name>@<harness>`
/// / `<dir>` the client adopts before publishing (the auto-add convenience — one command instead of
/// `add` then `publish`), or a remote/unsupported form that is refused typed. An un-enrolled publish is
/// refused BEFORE any local adoption, so it never mutates local state.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled (run `topos login <workspace-address>` first);
/// [`ClientError::InvalidArgument`] if the source is remote/unsupported (add it first);
/// [`ClientError::HarnessMismatch`] if a `@<harness>` names a different harness than the tracked skill;
/// the `add`-family errors ([`ClientError::AmbiguousSource`] / [`ClientError::NoUntrackedSkill`] / …) when
/// resolving an untracked source; [`ClientError::ApprovalMismatch`] if a `@<digest>` pin does not match the
/// scanned bytes; [`ClientError::PublishBlocked`] if an unresolved merge conflict is present;
/// [`ClientError::Conflict`] / [`ClientError::Denied`] on the plane's typed verdict; a transport / store
/// failure otherwise. A draft byte-identical to `current` is NOT an error — it settles as
/// [`PublishOutcome::NoChanges`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish(
    ctx: &Ctx<'_>,
    session: Option<&super::reconcile::SessionConnect<'_>>,
    roots: Option<&DiscoveryRoots>,
    target: &str,
    propose: bool,
    channel: Option<&str>,
    workspace: Option<&str>,
    message: Option<&str>,
    sel: &super::Selection,
    scope: super::StoreScope,
) -> Result<PublishOutcome, ClientError> {
    // Split off an optional `@<digest>` consent pin (64-hex only); everything else is the SOURCE.
    let (source_str, pin) = parse_target(target);

    // A connection first — BEFORE any local adoption, so an unconnected publish never mutates
    // local state. Sharing needs a workspace; joining one is `login`, not a publish.
    let has_sessions = !crate::sessions::read_sessions(ctx.fs, &ctx.layout)?
        .sessions
        .is_empty();
    if !has_sessions {
        return Err(ClientError::SessionRequired {
            address: "<workspace-address>".to_owned(),
            message: "not connected to a workspace — run `topos login <workspace-address>` \
                      first, then re-run this publish"
                .into(),
        });
    }

    // Auto-add: adopt an untracked LOCAL source before publishing, and learn the tracked skill name the
    // rest of the flow resolves. `added` is `Some` iff THIS invocation performed the adoption (disclosure).
    // A name a cwd-chain PROJECT store tracks is already tracked (per-scope stores) — the auto-add
    // must not re-adopt the placed dir as a brand-new skill.
    let (skill_name, added) = match super::project_tracked_name(ctx, &source_str) {
        Some(name) => (name, None),
        None => ensure_tracked(ctx, roots, &source_str)?,
    };

    let outcome = enrolled_publish(
        ctx,
        session.filter(|_| has_sessions),
        &skill_name,
        propose,
        channel,
        pin.as_deref(),
        workspace,
        message,
        sel,
        scope,
    )?;
    Ok(stamp_added(outcome, added))
}

/// Normalize a `--to` channel target: a channel REFERENCE spelling (`@ws/channels/x`, the
/// canonical `host/ws/channels/x`) resolves to its NAME after the workspace is checked against
/// the lane this publish signs in — a reference into a different workspace refuses typed. A bare
/// name (or any non-reference token) passes through untouched.
fn normalize_channel_target(
    channel: Option<&str>,
    lane: Option<&super::WriteLane>,
) -> Result<Option<String>, ClientError> {
    let Some(raw) = channel else { return Ok(None) };
    // The `@ws/…` sugar resolves at the lane's host (the workspace this publish signs in).
    let default_host = lane.map(|l| l.host.as_str());
    if let Ok(crate::manifest::keys::InputRef {
        shape:
            crate::manifest::keys::KeyShape::Channel {
                host,
                workspace,
                channel: name,
            },
        ..
    }) = crate::manifest::keys::parse_input(raw, default_host)
    {
        if let Some(l) = lane {
            let host_ok = host == l.host;
            if !host_ok || workspace != l.workspace_name {
                return Err(ClientError::InvalidArgument(format!(
                    "`--to {raw}` names a channel in another workspace — this publish lands in \
                     {}/{}",
                    l.host, l.workspace_name
                )));
            }
        }
        return Ok(Some(name));
    }
    Ok(Some(raw.to_owned()))
}

/// `--to <channel>` must name an EXISTING channel — publish never silently mints one (creating a
/// channel is a deliberate curation act, done on the web). A value equal to the workspace slug
/// gets the pointed fix (this publish already lands in that workspace); an unreadable index
/// refuses retryable rather than risk a silent server-side create. Checked on the describe AND
/// the apply, before any op is minted.
pub(crate) fn check_channel_exists(
    directory: &dyn crate::plane::DirectorySource,
    lane: &super::WriteLane,
    channel: Option<&str>,
) -> Result<(), ClientError> {
    let Some(ch) = channel else { return Ok(()) };
    if ch == lane.workspace_name {
        return Err(ClientError::InvalidArgument(format!(
            "`--to {ch}` names the workspace, not a channel — this publish already lands in \
             {}/{}; `--to` places the skill into a CHANNEL (e.g. `--to everyone`, or `--to \
             @{}/channels/<name>`)",
            lane.host, lane.workspace_name, lane.workspace_name
        )));
    }
    let index = directory.channels_index(&lane.workspace_id).map_err(|e| {
        ClientError::Plane(format!(
            "could not verify channel '{ch}' in {}: {} — nothing was published; retry",
            lane.workspace_name,
            crate::render::safe_message(&e)
        ))
    })?;
    if !index.channels.iter().any(|c| c.name == ch) {
        return Err(ClientError::NotAvailable(format!(
            "there is no channel '{ch}' in {}, or it is not visible with your current access — \
             nothing was published; pick an existing channel (`--to everyone` reaches the \
             default), or create '{ch}' on the web (the workspace's Channels page) first",
            lane.workspace_name
        )));
    }
    Ok(())
}

/// The bare (no `--yes`) ENROLLED publish describe — what shipping this draft WOULD do: where it lands,
/// the gate outcome, the share line, and the undo path. Mutates NOTHING at all — an
/// untracked source is NOT adopted here (adopting mints a sidecar and arms the session-start hook, a
/// durable change the human has not confirmed); it is refused toward `topos add` / `publish --yes`, which
/// is where the apply performs that adoption. The network is read only AFTER the local scan; the genesis /
/// WAL apply paths are untouched (this runs only for an enrolled `!yes` invocation, dispatched in the
/// composition root).
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::ApprovalMismatch`] on a failed
/// `@<digest>` pin; [`ClientError::PublishBlocked`] on an unresolved merge; name-resolution / scan /
/// transport errors. A draft equal to `current` is not an error — it previews as
/// [`PublishPreview::NoChanges`], the same answer the apply gives.
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_describe(
    ctx: &Ctx<'_>,
    session: Option<&super::reconcile::SessionConnect<'_>>,
    roots: Option<&DiscoveryRoots>,
    target: &str,
    propose: bool,
    channel: Option<&str>,
    workspace: Option<&str>,
    message: Option<&str>,
    sel: &super::Selection,
    scope: super::StoreScope,
) -> Result<PublishPreview, ClientError> {
    let (source_str, pin) = parse_target(target);
    let _ = roots;
    // A describe MUTATES NOTHING (the consent contract). An already-tracked target is scanned in place;
    // an UNTRACKED source is NOT adopted here — adopting mints a sidecar and arms the session-start hook,
    // a durable change the human has not confirmed. The apply (`--yes`) does the adoption and discloses
    // it; the describe points the user at that.
    // The resolver spans the per-scope stores (standing first, then the drafted copy of the other
    // scope) — a project-delivered bundle's draft is described against the checkout's own store.
    let skill_name = match super::resolve_skill_stored(ctx, &source_str, None, scope) {
        Ok(hit) => hit.lock.name,
        // Tracked ambiguously (2+ under this exact name) — the `--workspace`-filtered resolve below picks.
        Err(ClientError::AmbiguousName { .. }) => source_str.clone(),
        Err(ClientError::NoSuchSkill { .. }) if scope == super::StoreScope::Machine => {
            return Err(ClientError::NoSuchSkill { name: source_str });
        }
        Err(ClientError::NoSuchSkill { .. }) => {
            return Err(ClientError::InvalidArgument(format!(
                "'{source_str}' is not tracked yet — publish won't adopt an untracked folder (that would change \
                 this machine before you confirm). Run `topos add {source_str}` first to preview it, \
                 or `topos publish {source_str} --yes` to adopt and ship in one step."
            )));
        }
        Err(e) => return Err(e),
    };

    let super::StoredSkill {
        layout: store,
        id,
        lock,
        other,
        cross_scope,
    } = super::resolve_skill_stored(ctx, &skill_name, workspace, scope)?;
    // THE SCOPE THIS PUBLISH ACTS IN — read off the store the resolution chose, not off the
    // directory the command was typed in. Every refusal below spells its way out for this scope
    // (`topos update -g …` for the machine copy), and a refusal that offers the other scope's
    // command sends the reader to a copy the publish never touched — and back here again.
    let global = !store.is_project_scope();
    // The OTHER scope's copy, when it holds edits this publish will not ship — read before the ctx
    // below is re-pointed at the resolved store, because it is that other store's own state.
    let other_draft = other_scope_draft(ctx, other.as_ref());
    let lane = match session {
        Some(sc) => super::resolve_session_lane(ctx, sc, workspace, Some(id.as_str()))?,
        None => None,
    };
    let channel = normalize_channel_target(channel, lane.as_ref())?;
    let channel = channel.as_deref();
    // The lane a describe reads through — without one there is nothing to describe against.
    let Some(lane) = lane else {
        return Err(ClientError::NotEnrolled);
    };
    let workspace_id = lane.workspace_id.clone();
    // Under a session lane the delivered set IS the follow-state (the cache-backed seam).
    let cache_follow = super::reconcile::CacheFollow::load(ctx.fs, &ctx.layout);
    // The lane's transports + the cache follow seam + the skill's OWNING store.
    let lane_ctx = super::pull::ctx_with_store(ctx, &store, &*lane.transports.plane, &cache_follow);
    let ctx = &lane_ctx;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Publish-block guard (same presence check the apply runs): an unresolved merge blocks a publish.
    if doc::read_doc::<ConflictState>(ctx.fs, &sp.conflict)?.is_some() {
        return Err(ClientError::PublishBlocked {
            skill: skill_name.clone(),
            global,
        });
    }
    // Scan the live draft ONCE → the byte-exact digest the apply would ship; the optional `@<digest>` pin
    // gates it here too (refuse on mismatch), so a describe never previews bytes the apply would refuse.
    let map: PlacementMap = doc::read_map(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
    // WHAT THE KIND CAN SERVE, asked before a work tree is looked for. `publish` ships a bundle's
    // placed files and a connected server has none — its whole delivery is a document the catalog
    // publishes — so it refuses through the ONE file-verb construction. Without this the search
    // for a work tree fails first, and a person is told their own machine is unreadable about a
    // bundle that is behaving exactly as its kind says it should.
    if let Some(refusal) = crate::bundle_kind::refuse_file_verb(
        crate::bundle_kind::FileVerb::Publish,
        &skill_name,
        crate::bundle_kind::classify(ctx, id.as_str(), &map.placements).or_skill(),
    ) {
        return Err(refusal);
    }
    // The WORK TREE: the single edited copy when one exists (the draft being shipped — it may live
    // in the shared dir or any native copy), else the first placement; several DIVERGENT copies are
    // the typed freeze — a BARE publish never picks for you. `--dest`/`-a` is what supplies the
    // missing consent: it names ONE copy, and the copies it does not name are not touched.
    let picked = pick_copy(ctx, sel, &lock.name, &map)?;
    let placement = match &picked {
        Some(p) => p.dir.clone(),
        None => crate::placement::work_tree_dir(ctx, &lock.name, &map)?,
    };
    let scanned = scan::scan(&placement)?;
    let digest_hex = to_hex(&scanned.bundle_digest);
    // The other scope's copy is only NEWS while its bytes differ from these.
    let other_draft = unless_identical(other_draft, &digest_hex);
    if let Some(pin) = &pin
        && digest_hex != *pin
    {
        return Err(ClientError::ApprovalMismatch {
            expected: digest_hex,
            got: pin.clone(),
        });
    }
    // The BUNDLE KIND rides the wire; what a workspace will ACCEPT for it is the workspace's
    // ruling, and it answers the describe and the apply identically. A connected server holds no
    // files at all, so a publish naming one is refused there — one gate, where the bundle lives.
    let bundle_kind = crate::bundle_kind::classify(ctx, id.as_str(), &map.placements).or_skill();

    // THE SERVER'S CURRENT IS THE TRUTH. A bundle HAS a current when it is FOLLOWED or has ever
    // been published from here (`sync.observed != GENESIS` — a landed publish advances `observed`
    // past GENESIS); a never-published local skill has none, and its first publish is the genesis
    // shape. The copy is classified against the LIVE current — read from the server here, not
    // the sidecar's cached notion of it: a revert run from another store (or another machine)
    // moves `current` without touching this cache, and a describe that answered "already
    // published" against the cache was lying about `current`.
    let sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("missing sync state".to_owned()))?;
    let follow_entry = ctx
        .follow
        .followed()
        .into_iter()
        .find(|(fid, _)| fid == id.as_str())
        .map(|(_, fc)| fc);
    let followed = follow_entry.is_some();
    let live = if followed || sync.observed != GENESIS {
        Some(live_or_cached(ctx, &lane, &sp, id.as_str(), &lock, &sync)?)
    } else {
        None
    };
    let standing = standing(live.as_ref(), &lock, &digest_hex);
    match &standing {
        Standing::Matches => {
            return Ok(PublishPreview::NoChanges(no_changes(
                skill_name,
                other_draft,
            )));
        }
        // An EDITED copy behind the live current: the apply would refuse (the plane's lineage
        // fence), so the describe refuses FIRST — a preview of a publish the apply will refuse is
        // two steps to reach one answer. A proposal moves no pointer and is exempt.
        Standing::Behind if !propose => {
            return Err(ClientError::PublishBehind {
                skill: skill_name.clone(),
                global,
            });
        }
        _ => {}
    }

    // The gate the plane will apply: a reviewed bundle (or an explicit `--propose`) becomes a proposal;
    // an open one lands directly. Protection is a SERVER fact that can move after this device's last sync
    // (an owner runs `protect <skill> reviewed` — or loosens it back to `open`), so the sidecar's cached
    // `review_required` — stamped at the last delivery reconcile — can misreport the gate in EITHER
    // direction. The live read above carries the FRESH protection per delivered skill; a bundle the
    // delivery does not name falls back to the cached value. A genesis (unfollowed) skill has no server
    // protection — its first publish keeps the no-gate path.
    let review_required = match &follow_entry {
        Some(fc) => live
            .as_ref()
            .and_then(|l| l.review_required)
            .unwrap_or(fc.review_required),
        None => false,
    };
    let gate = if propose || review_required {
        PublishGate::Proposal
    } else {
        PublishGate::Lands
    };

    // GENESIS = no published `current` exists. It decides the default placement below.
    let genesis = matches!(standing, Standing::Genesis);

    // Network reads AFTER the local scan: the workspace address (the share line).
    let directory: &dyn crate::plane::DirectorySource = &*lane.transports.directory;
    // `--to` takes an EXISTING channel — the describe refuses exactly where the apply would
    // (never a described placement into a channel the apply would have silently minted).
    check_channel_exists(directory, &lane, channel)?;
    let me = directory.me(&workspace_id).ok();

    // Only a genesis apply creates the DEFAULT `everyone` placement server-side; a bare
    // NON-genesis republish (a locally-authored skill's second publish — also `!followed`) moves
    // `current` and alters no placement, so the describe must not claim one.
    // The placement TARGET this apply would touch: an explicit `--to <channel>` places on EVERY
    // publish; without one, only a genesis lands the default `everyone` reference.
    let placement_target = match channel {
        Some(ch) => Some(ch.to_owned()),
        None if genesis => Some("everyone".to_owned()),
        None => None,
    };
    let placements: Vec<String> = placement_target.iter().cloned().collect();
    // The placement's gate: REACH is curation-gated — the default channel AND every named `--to`
    // (`everyone` included; the apply routes them all through the same mode gate and withholds a
    // MEMBER's placement into a curated channel, disclosed on its receipt) — so the describe says
    // so up front whenever the target resolves CURATED against a member caller. The mode rides
    // the channel index the client already reads (`/channels`); a failed read degrades to the
    // plain placement line — same as the share read, the describe keeps working offline.
    // (A `--to` naming a channel absent from the index was already refused above — publish
    // never silently mints a channel.)
    let placement_note = placement_target
        .as_ref()
        .is_some_and(|target| {
            me.as_ref().is_some_and(|m| m.role == "member")
                && directory
                    .channels_index(&workspace_id)
                    .ok()
                    .and_then(|ix| ix.channels.into_iter().find(|c| &c.name == target))
                    .is_some_and(|c| c.mode == "curated")
        })
        .then(|| "curated: lands catalog-only; a curator places it afterwards".to_owned());
    let share_line = me
        .as_ref()
        .map(|m| format!("{}/skills/{}", m.address, skill_name));
    // The teammate handoff — same source data as the share line (the members' deep link above
    // 404s for a non-member, so recruiting a teammate takes this join line instead).
    let invite_line = me.as_ref().and_then(|m| teammate_invite_line(&m.address));
    // The `<host>/<workspace>` handle the describe's header names the destination by — the
    // LANE's own record first (always the full host/workspace pair), the network read's answer
    // only as the fallback: the two receipts must spell one workspace one way.
    let workspace = lane
        .workspace_ref()
        .or_else(|| workspace_ref_of_me(me.as_ref().map(|m| m.address.as_str())));
    // The undo names the version `current` holds NOW — the live one, which is what a landed
    // publish would move away from.
    let undo_base = live
        .as_ref()
        .map_or(lock.base_commit.as_str(), |l| l.hex.as_str());
    let undo = undo_is_restorative(followed, gate)
        .then(|| format!("topos revert {skill_name} --to {undo_base}"));
    // The predicted-conflict preview: a PROPOSAL from an edited copy BEHIND the live current (the
    // direct publish refused above) dry-runs the three-way merge of the draft onto that current
    // PURELY from bytes already on this machine: the draft was scanned above, the base renders
    // from the sidecar store, and the live version's bytes are present iff a prior sweep fetched
    // them. Anything missing ⇒ NO preview (absent = unknown).
    let merge_preview = match (&standing, &live) {
        (Standing::Behind, Some(l)) => (|| {
            let store = Store::open(&sp.store).ok()?;
            let theirs_digest = sync_engine::store_bundle_digest_opt(&store, l.commit).ok()??;
            let theirs = store.render_verified(l.commit, theirs_digest).ok()?;
            let base = store
                .render_verified(
                    parse_hex32(&lock.base_commit).ok()?,
                    parse_hex32(&lock.bundle_digest).ok()?,
                )
                .ok()?;
            Some(super::merge_resolve::preview_merge(
                &base, &scanned, &theirs,
            ))
        })(),
        _ => None,
    };
    let origin_note = match doc::read_doc::<add::OriginDoc>(ctx.fs, &sp.origin)? {
        Some(o) => {
            let mut note = format!(
                "this skill was imported from {} — publishing makes the team copy the source of \
                 truth",
                o.origin.source
            );
            if let Some(asym) = origin_asymmetry_note(ctx, &sp, &skill_name, &lane)? {
                note.push_str("; ");
                note.push_str(&asym);
            }
            Some(note)
        }
        None => None,
    };
    // The PREDICTED governance transfer: the manifest line the apply would rewrite (read-only).
    let (transfer_manifest, transfer_reference, transfer_from) = match super::find_path_line(
        ctx,
        &map.placements
            .iter()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>(),
    )? {
        Some((path, row)) => (
            Some(path.display().to_string()),
            Some(format!(
                "{}/{}/{skill_name}",
                lane.host, lane.workspace_name
            )),
            Some(row.reference),
        ),
        None => (None, None, None),
    };

    let from_machine = cross_scope && global;
    let (from_placement, other_edited) = from_disclosure(
        picked.as_ref(),
        cross_scope.then(|| shipped_from(ctx, &placement)),
    );
    // The read is offered only where there is something to read. A GENESIS publish has no prior
    // version to compare against — the lock names the bytes the adopt just recorded, so
    // `topos diff` there prints nothing at all, and a `review:` line pointing at an empty answer is
    // worse than no line.
    let review = (!genesis).then(|| review_command(&skill_name, from_machine, picked.as_ref()));
    // What this publish CHANGES, counted against the live current (a genesis counts every file
    // as new), and — for a copy equal to an older version — which version `current` is, spelled
    // with the version the apply would mint from this very preimage.
    let (current_files, current_message) = match &live {
        Some(l) => current_bundle_files(ctx, &sp, id.as_str(), l)?,
        None => (Vec::new(), None),
    };
    let changes = Some(change_summary(&current_files, &scanned));
    let republish = match standing {
        Standing::Forward(mut rep) => {
            if let Some(l) = &live {
                let base = PublishBase {
                    parent: l.commit,
                    expected: l.generation,
                };
                rep.new_version_id =
                    predicted_version_id(ctx, &base, scanned.bundle_digest, message)?;
            }
            rep.current_message = current_message;
            Some(rep)
        }
        _ => None,
    };
    let lands_in = lands_in(placement_target.as_deref(), live.as_ref());
    Ok(PublishPreview::Describe(Box::new(PublishDescribeData {
        skill: skill_name,
        skill_id: id.into_string(),
        workspace_id,
        workspace_display_name: me.map(|m| m.display_name),
        workspace,
        bundle_digest: digest_hex,
        placements,
        from_placement,
        from_machine,
        other_scope_draft: other_draft,
        review,
        other_edited,
        gate,
        // The full ancestor-bytes revert detection is the apply path's (the server treats a
        // revert-shaped publish as a forward move); the describe reports the gate + placements
        // without pre-judging it.
        is_revert: false,
        share_line,
        invite_line,
        undo,
        origin_note,
        placement_note,
        merge_preview,
        manifest: transfer_manifest,
        reference: transfer_reference,
        converted_from: transfer_from,
        kind: bundle_kind.tag(),
        republish,
        changes,
        lands_in,
    })))
}

/// The workspace's LIVE `current` for a bundle — read from the server on every publish decision,
/// never from the sidecar's cached notion of it. The cache is what this machine last saw; a
/// `revert` run from another store (or another machine) moves `current` without touching it, and
/// a publish that decided "already published" against the cache was lying about `current`.
struct LiveCurrent {
    commit: [u8; 32],
    hex: String,
    generation: u64,
    digest_hex: String,
    /// The channels currently carrying the bundle, when the delivery named them.
    via_channels: Vec<String>,
    /// The bundle's fresh protection, when the delivery named it.
    review_required: Option<bool>,
}

/// Where the copy stands against the live `current` — the ONE classification both halves of the
/// verb decide from, so the preview and the apply answer alike.
enum Standing {
    /// No server `current` exists: the first publish of a bundle authored here.
    Genesis,
    /// The copy is byte-identical to the live `current` — already published.
    Matches,
    /// The copy is a draft on the live `current` (its base IS that version).
    Draft,
    /// The copy EQUALS an older version than the live `current`: an unmodified copy of a version
    /// a later move (a revert, a teammate's publish) superseded. Its content is carried forward
    /// as a new version parented on the live `current` — re-publishing old content is legitimate
    /// (a commit can restate an earlier tree); the reader is owed which version `current` is.
    Forward(Republish),
    /// The copy carries EDITS and its base is not the live `current`: a publish would land bytes
    /// with the team's newer version nowhere inside them, and the plane's lineage fence refuses
    /// it anyway. `topos update` merges first.
    Behind,
}

/// The version a publish parents on, and the generation the plane's compare-and-swap expects.
/// `None` is the genesis shape (a zero-parent commit at generation 0).
struct PublishBase {
    parent: [u8; 32],
    expected: u64,
}

/// One file of a version, in the shape the preview's change count compares.
struct BundleFile {
    path: String,
    mode: FileMode,
    bytes: Vec<u8>,
}

/// Read the live `current` for `skill_id` from the workspace: the delivery snapshot first (the
/// same read the update sweep drives from — it names the version, its generation, its digest,
/// and the channels carrying it), then the pointer itself for a bundle the delivery does not
/// name (a followed-but-excluded copy). `Ok(None)` = the workspace serves no current for it.
///
/// # Errors
/// [`ClientError::Plane`] on a transport fault — a publish decided against a current this run
/// could not read would be exactly the lie this read exists to prevent; nothing is published.
fn read_live_current(
    ctx: &Ctx<'_>,
    lane: &super::WriteLane,
    sp: &sidecar::SkillPaths,
    skill_id: &str,
    name: &str,
) -> Result<Option<LiveCurrent>, ClientError> {
    let refuse = |m: String| {
        ClientError::Plane(format!(
            "could not read the current version of '{name}' from {}/{}: {m} — nothing was \
             published; retry",
            lane.host, lane.workspace_name
        ))
    };
    // Bind the bundle to the lane's workspace on the read transport FIRST: every per-skill read
    // below and after (the pointer, a version's bytes for the change count or the forward
    // commit's backfill) answers "not served" for an unbound skill — and a brand-new arrival, or
    // a bundle only a checkout delivers, is not in the cache the transport seeds from.
    ctx.plane.bind_skill(&lane.workspace_id, skill_id);
    match lane.transports.plane.fetch_delivery(&lane.workspace_id) {
        Ok(snapshot) => {
            if let Some(s) = snapshot.skills.into_iter().find(|s| s.skill_id == skill_id) {
                return Ok(Some(LiveCurrent {
                    commit: s.version_id,
                    hex: to_hex(&s.version_id),
                    generation: s.generation,
                    digest_hex: to_hex(&s.bundle_digest),
                    via_channels: s.via_channels,
                    review_required: Some(s.review_required),
                }));
            }
        }
        Err(PlaneError::NotFound) => {}
        Err(PlaneError::UpdateRequired { min }) => return Err(ClientError::UpdateRequired { min }),
        Err(PlaneError::Unavailable(m) | PlaneError::Unreachable(m) | PlaneError::Malformed(m)) => {
            return Err(refuse(m));
        }
    }
    // The delivery does not name it — ask for the pointer itself.
    match ctx.plane.get_current(skill_id, None) {
        Ok(PointerFetch::Record(rec)) => {
            let commit = sync_engine::scoped_version_id(&rec, skill_id, &lane.workspace_id)
                .ok_or_else(|| {
                    ClientError::WireInvalid(
                        "the current pointer is scoped to a different workspace/skill".to_owned(),
                    )
                })?;
            // The digest: from the store when this machine holds the version, else from the
            // verified bytes themselves.
            let store = Store::open(&sp.store)?;
            let digest = match sync_engine::store_bundle_digest_opt(&store, commit)? {
                Some(d) => d,
                None => contribute::fetch_verified_bundle(ctx, skill_id, commit)?.0,
            };
            Ok(Some(LiveCurrent {
                commit,
                hex: to_hex(&commit),
                generation: rec.record.generation,
                digest_hex: to_hex(&digest),
                via_channels: Vec::new(),
                review_required: None,
            }))
        }
        Ok(PointerFetch::NotModified) => Err(ClientError::Corrupt(
            "an unconditional current read returned not-modified".to_owned(),
        )),
        Err(PlaneError::NotFound) => Ok(None),
        Err(PlaneError::UpdateRequired { min }) => Err(ClientError::UpdateRequired { min }),
        Err(PlaneError::Unavailable(m) | PlaneError::Unreachable(m) | PlaneError::Malformed(m)) => {
            Err(refuse(m))
        }
    }
}

/// [`read_live_current`] for a bundle that HAS a current (followed, or published from here). The
/// ONE place the sidecar's cached notion still stands: when the server names NO current for the
/// bundle on either read (the workspace serves it to this login no longer, or names it under
/// another record). A decision made there cannot claim "already published" against a newer
/// version — the server just said there is none it can see — and the plane's compare-and-swap
/// still fences the write, so a stale base ends in a typed CONFLICT, never a silent replace.
fn live_or_cached(
    ctx: &Ctx<'_>,
    lane: &super::WriteLane,
    sp: &sidecar::SkillPaths,
    skill_id: &str,
    lock: &Lock,
    sync: &SyncState,
) -> Result<LiveCurrent, ClientError> {
    if let Some(live) = read_live_current(ctx, lane, sp, skill_id, &lock.name)? {
        return Ok(live);
    }
    let commit = parse_hex32(&sync.observed_version_id)?;
    // The observed version's digest: the lock's when it IS the applied base, else the store's
    // when the version is held here — and otherwise unknown, which classifies as nothing (an
    // empty digest matches no copy, so "already published" is never claimed on a guess).
    let digest_hex = if sync.observed_version_id == lock.base_commit {
        lock.bundle_digest.clone()
    } else {
        let store = Store::open(&sp.store)?;
        sync_engine::store_bundle_digest_opt(&store, commit)?
            .map(|d| to_hex(&d))
            .unwrap_or_default()
    };
    Ok(LiveCurrent {
        commit,
        hex: sync.observed_version_id.clone(),
        generation: sync.observed,
        digest_hex,
        via_channels: Vec::new(),
        review_required: None,
    })
}

/// Classify the scanned copy (`digest_hex`) against the live `current` and the version this
/// store applied (`lock`). `live` is `None` only for a genesis bundle (the caller reads it for
/// every bundle that has a current).
fn standing(live: Option<&LiveCurrent>, lock: &Lock, digest_hex: &str) -> Standing {
    let Some(l) = live else {
        return Standing::Genesis;
    };
    if digest_hex == l.digest_hex {
        return Standing::Matches;
    }
    if l.hex == lock.base_commit {
        return Standing::Draft;
    }
    if digest_hex == lock.bundle_digest {
        return Standing::Forward(Republish {
            current_version_id: l.hex.clone(),
            current_message: None,
            copy_version_id: lock.base_commit.clone(),
            new_version_id: String::new(),
        });
    }
    Standing::Behind
}

/// The live current's files + its history line: from this store when the version is held here,
/// else fetched and re-verified (the same bytes that reproduce the id are the bytes compared).
fn current_bundle_files(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    skill_id: &str,
    live: &LiveCurrent,
) -> Result<(Vec<BundleFile>, Option<String>), ClientError> {
    let store = Store::open(&sp.store)?;
    if let Some(digest) = sync_engine::store_bundle_digest_opt(&store, live.commit)? {
        let bundle = store.render_verified(live.commit, digest)?;
        let message = store
            .read_commit_meta(live.commit)
            .ok()
            .map(|node| node.message);
        return Ok((
            bundle
                .files
                .into_iter()
                .map(|f| BundleFile {
                    path: f.path,
                    mode: f.mode,
                    bytes: f.bytes,
                })
                .collect(),
            message,
        ));
    }
    let (_, fetched) = contribute::fetch_verified_bundle(ctx, skill_id, live.commit)?;
    Ok((
        fetched
            .files
            .into_iter()
            .map(|f| BundleFile {
                path: f.path,
                mode: f.mode,
                bytes: f.bytes,
            })
            .collect(),
        Some(fetched.message),
    ))
}

/// Count what `draft` changes against `current`, per file: added, removed, changed in content or
/// mode — and, of those, the files that become executable. A genesis publish counts against
/// nothing, so every file is added.
fn change_summary(current: &[BundleFile], draft: &scan::ScannedBundle) -> ChangeSummary {
    use std::collections::{BTreeMap, BTreeSet};
    let base: BTreeMap<&str, (FileMode, &[u8])> = current
        .iter()
        .map(|f| (f.path.as_str(), (f.mode, f.bytes.as_slice())))
        .collect();
    let mut out = ChangeSummary::default();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for f in &draft.files {
        seen.insert(f.path.as_str());
        let exec = f.mode == FileMode::Executable;
        match base.get(f.path.as_str()) {
            None => {
                out.files += 1;
                out.added += 1;
                if exec {
                    out.executable += 1;
                }
            }
            Some((mode, bytes)) => {
                if *mode != f.mode || *bytes != f.bytes.as_slice() {
                    out.files += 1;
                    if exec && *mode != FileMode::Executable {
                        out.executable += 1;
                    }
                }
            }
        }
    }
    out.removed = base.keys().filter(|p| !seen.contains(*p)).count() as u64;
    out.files += out.removed;
    out
}

/// The channels the published version reaches: the placement this apply makes (`--to`, or the
/// genesis default) first, then the channels already carrying the bundle.
fn lands_in(placement_target: Option<&str>, live: Option<&LiveCurrent>) -> Vec<String> {
    let mut out: Vec<String> = placement_target.map(str::to_owned).into_iter().collect();
    if let Some(l) = live {
        for c in &l.via_channels {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
    }
    out
}

/// The version id a forward publish MINTS — the same preimage the apply commits (parent = the
/// live current, tree = the copy's digest, author = this device, message = `-m` or the default),
/// so the preview names the version the receipt will.
fn predicted_version_id(
    ctx: &Ctx<'_>,
    base: &PublishBase,
    digest: [u8; 32],
    message: Option<&str>,
) -> Result<String, ClientError> {
    let id = identity::commit_id(&Commit {
        parents: &[base.parent],
        tree: digest,
        author: &ctx.device_id,
        message: message.unwrap_or(PUBLISH_MESSAGE),
    })
    .map_err(|_| ClientError::Corrupt("commit id preimage".to_owned()))?;
    Ok(to_hex(&id))
}

/// The already-published answer: nothing to ship, and — when the edits are in the OTHER scope's
/// copy — where they are. Shared by the describe and the apply, which owe the reader the same
/// answer.
fn no_changes(skill: String, other_draft: Option<ScopeDraft>) -> PublishNoChangesData {
    PublishNoChangesData {
        result: PublishResult::NoChanges,
        skill,
        other_scope_draft: other_draft,
    }
}

/// The other scope's DRAFT as a disclosure names it: the folder that copy stands in, spelled at ITS
/// OWN scope (`project/…` inside the checkout, `~/…` on the machine), and which scope that is —
/// which is also what decides whether the command that shares it carries `-g`. The draft's own
/// DIGEST rides along, for the one question the folder cannot answer (see [`unless_identical`]).
///
/// `None` when the other store holds no edits: there is then nothing being left behind, and a line
/// about a copy that matches `current` would be noise.
fn other_scope_draft(
    ctx: &Ctx<'_>,
    other: Option<&super::OtherStore>,
) -> Option<(ScopeDraft, String)> {
    let other = other.filter(|o| o.drafted)?;
    let octx = super::pull::ctx_with_layout(ctx, &other.layout);
    let draft = super::store_draft(&octx, &other.id, &other.lock)?;
    Some((
        ScopeDraft {
            folder: super::dest_select::copy_spellings(&octx, &draft.dir).display,
            machine: !other.layout.is_project_scope(),
        },
        draft.digest,
    ))
}

/// The other scope's draft, WITHHELD when its bytes are the bytes being shipped. The same edit made
/// in both places is one edit: this publish carries it, the other copy is already at it, and the
/// next sweep settles that copy clean without anyone doing anything. Every sentence the disclosure
/// could print there — "keeps its edits", "update it onto this version, then share it" — describes
/// work that does not exist.
fn unless_identical(other: Option<(ScopeDraft, String)>, shipped: &str) -> Option<ScopeDraft> {
    other
        .filter(|(_, digest)| digest != shipped)
        .map(|(draft, _)| draft)
}

/// The folder these bytes were read from, as a person reads it — spelled at the store that owns
/// the copy, so a machine copy read from inside a checkout says `~/…` rather than a `project/`
/// prefix that does not resolve.
fn shipped_from(ctx: &Ctx<'_>, placement: &std::path::Path) -> String {
    super::dest_select::copy_spellings(ctx, placement).display
}

/// The `topos diff …` the describe offers for reading the exact copy it would ship: `-g` when the
/// machine copy answered from inside a checkout (a bare `diff` there reads the project's), and the
/// picked copy's own `--dest` spelling when a selection named one — so the command reaches that
/// folder however the selection was written.
///
/// The caller withholds it on a GENESIS publish: there is no earlier version for a diff to be
/// against.
fn review_command(
    skill: &str,
    machine: bool,
    picked: Option<&super::dest_select::SelectedCopy>,
) -> String {
    let mut cmd = "topos diff".to_owned();
    if machine {
        cmd.push_str(" -g");
    }
    cmd.push(' ');
    cmd.push_str(skill);
    if let Some(p) = picked {
        cmd.push_str(" --dest ");
        cmd.push_str(&p.spelling.dest);
    }
    cmd
}

/// The ONE copy a `-a`/`--dest` selection named, or `None` for a bare publish (which resolves the
/// draft the ordinary way, and refuses when the copies disagree).
///
/// Both halves of the verb — the describe and the apply — go through here, so the preview reads
/// the same folder the apply ships from and a refusal fires on the describe rather than after
/// `--yes`.
fn pick_copy(
    ctx: &Ctx<'_>,
    sel: &super::Selection,
    skill_name: &str,
    map: &PlacementMap,
) -> Result<Option<super::dest_select::SelectedCopy>, ClientError> {
    if sel.is_empty() {
        return Ok(None);
    }
    super::dest_select::select_copy(ctx, sel, skill_name, map).map(Some)
}

/// The two disclosure fields a publish carries — the folder shipped FROM and the other edited
/// copies left alone — populated when the folder was a CHOICE: a `--dest` among several edited
/// copies, or (through `cross_scope`) a copy in a scope other than the one the command stands in.
///
/// With a single edited copy in the standing scope there is nothing to say: that copy IS the draft,
/// a `--dest` naming it asks for exactly what a bare publish would do, and a `from …` line would
/// name a folder the reader never had to choose between.
fn from_disclosure(
    picked: Option<&super::dest_select::SelectedCopy>,
    cross_scope: Option<String>,
) -> (Option<String>, Vec<String>) {
    match picked.filter(|p| !p.others_edited.is_empty()) {
        Some(p) => (Some(p.spelling.display.clone()), p.others_edited.clone()),
        None => (cross_scope, Vec::new()),
    }
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
    // The built-in `topos` skill ships with the CLI and is never workspace state — refuse before
    // any resolution (its reserved name also can't reach a catalog server-side).
    if super::builtin::is_builtin(source_str) {
        return Err(ClientError::InvalidArgument(
            "`topos` is the built-in skill — it ships with the CLI and cannot be published; to \
             share files, put them in a new skill and publish that"
                .into(),
        ));
    }
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
    if super::builtin::is_builtin(bare) {
        return Err(ClientError::InvalidArgument(
            "`topos` is the built-in skill — it ships with the CLI and cannot be published; to \
             share files, put them in a new skill and publish that"
                .into(),
        ));
    }
    match resolve_skill(ctx, bare) {
        // Uniquely tracked → publish it. A `@<harness>` naming an agent that reads NONE of the
        // folders this skill stands in likely means a different copy was intended — refuse rather
        // than publish these bytes. The question is asked of the PATHS, through the registry's one
        // attribution answer, so a folder several agents share matches every one of them.
        Ok((id, lock)) => {
            if let Some(requested) = harness {
                let map: PlacementMap = doc::read_map(ctx.fs, &ctx.layout.published(&id).map)?
                    .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
                let read_by_requested = map.placements.iter().any(|p| {
                    super::add::folder_readers(std::path::Path::new(p))
                        .iter()
                        .any(|slug| slug == requested)
                });
                if !read_by_requested {
                    return Err(ClientError::HarnessMismatch {
                        name: lock.name,
                        requested: requested.to_owned(),
                        folders: placement_folders(&map),
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
            let (path, name) = resolve_add_target(ctx, roots, raw, "publish")?;
            // Discovery resolves a bare NAME against agent skill folders: an auto-add is
            // always a skill.
            let data = add_with_name(
                ctx,
                &path,
                Some(&name),
                true,
                crate::bundle_kind::BundleKind::Skill,
            )?;
            Ok((
                data.name.clone(),
                Some(AddedNote {
                    name: data.name,
                    folder: parent_folder(&path),
                }),
            ))
        }
        Err(e) => Err(e),
    }
}

/// The folder a source directory sits in — what the auto-add disclosure names.
fn parent_folder(source: &std::path::Path) -> Option<String> {
    Some(source.parent()?.to_string_lossy().into_owned())
}

/// The distinct FOLDERS a tracked skill's placements stand in, in placement order — what a refusal
/// names instead of one guessed agent (a folder is the fact; who reads it is a query over the fact).
fn placement_folders(map: &PlacementMap) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in &map.placements {
        let folder = std::path::Path::new(p)
            .parent()
            .unwrap_or(std::path::Path::new(p))
            .to_string_lossy()
            .into_owned();
        if !out.contains(&folder) {
            out.push(folder);
        }
    }
    out
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
            folder: parent_folder(p),
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
            // An adoption cannot end in "already published": a just-adopted bundle has no
            // published `current` to match.
            PublishOutcome::NoChanges(_) => {}
        }
    }
    outcome
}

/// The ENROLLED publish body. `pin` is the optional `@<digest>` consent — when present, the scanned
/// bytes must match it; when absent, the computed digest ships as-is. The session lane's directory
/// transport feeds the receipt's teammate handoff line (a best-effort `me` read on a landed publish).
#[allow(clippy::too_many_arguments)]
fn enrolled_publish(
    ctx: &Ctx<'_>,
    session: Option<&super::reconcile::SessionConnect<'_>>,
    skill_name: &str,
    propose: bool,
    channel: Option<&str>,
    pin: Option<&str>,
    workspace: Option<&str>,
    message: Option<&str>,
    sel: &super::Selection,
    scope: super::StoreScope,
) -> Result<PublishOutcome, ClientError> {
    // The `--workspace` filter disambiguates a name shared across workspaces. A DELIVERED skill signs in
    // its OWN workspace (the pointer scope); a brand-new local skill (a genesis publish, no delivery)
    // is AMBIENT — the single session/membership or the `--workspace`-selected one. The resolver
    // spans the per-scope stores: a project-delivered bundle's state lives in the checkout's own
    // store, and its per-skill work below runs against that store.
    let super::StoredSkill {
        layout: store,
        id,
        lock,
        other,
        cross_scope,
    } = super::resolve_skill_stored(ctx, skill_name, workspace, scope)?;
    // THE SCOPE THIS PUBLISH ACTS IN — the store the resolution chose, never the directory the
    // command was typed in (see the describe's twin of this line): every refusal below spells its
    // way out for THIS copy.
    let global = !store.is_project_scope();
    // The OTHER scope's copy, when it holds edits this publish will not ship — read before the ctx
    // below is re-pointed at the resolved store, because it is that other store's own state.
    let other_draft = other_scope_draft(ctx, other.as_ref());
    // The SESSION lane (the manifest model): the workspace + transports resolve from the
    // logged-in sessions. No lane is a typed `SessionRequired` refusal just below — a publish has
    // nowhere to go without one.
    let lane = match session {
        Some(sc) => super::resolve_session_lane(ctx, sc, workspace, Some(id.as_str()))?,
        None => None,
    };
    let channel = normalize_channel_target(channel, lane.as_ref())?;
    let channel = channel.as_deref();
    let Some(lane) = lane else {
        return Err(ClientError::SessionRequired {
            address: "<workspace-address>".to_owned(),
            message: "not connected — run `topos login <workspace-address>` first".into(),
        });
    };
    let workspace_id = lane.workspace_id.clone();
    // `--to` takes an EXISTING channel — verified before any op is minted (never a silent
    // server-side create).
    check_channel_exists(&*lane.transports.directory, &lane, channel)?;
    // Under a session lane, the delivered set IS the follow-state (the cache-backed seam) — the
    // no-change and gate reads below see the same truth the reconcile writes.
    let outer_ctx = ctx;
    let cache_follow = super::reconcile::CacheFollow::load(ctx.fs, &ctx.layout);
    // The lane's transports + the cache follow seam + the skill's OWNING store (home, or the
    // project store the resolver located) — the per-skill work below runs there.
    let lane_ctx = super::pull::ctx_with_store(ctx, &store, &*lane.transports.plane, &cache_follow);
    let ctx = &lane_ctx;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Publish guard (presence-based, never a marker scan): an unresolved author merge blocks publish.
    if doc::read_doc::<ConflictState>(ctx.fs, &sp.conflict)?.is_some() {
        return Err(ClientError::PublishBlocked {
            skill: skill_name.to_owned(),
            global,
        });
    }
    let transport: &dyn crate::plane::ContributeSource = &*lane.transports.contribute;
    let map: PlacementMap = doc::read_map(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
    // WHAT THE KIND CAN SERVE — the describe's own guard, asked again here because the apply is
    // reachable without it (`--yes`) and a connected server has no work tree to look for.
    if let Some(refusal) = crate::bundle_kind::refuse_file_verb(
        crate::bundle_kind::FileVerb::Publish,
        skill_name,
        crate::bundle_kind::classify(ctx, id.as_str(), &map.placements).or_skill(),
    ) {
        return Err(refusal);
    }

    // Scan the live draft ONCE under the lock → the byte-exact digest the plane re-derives. When a
    // `@<digest>` pin is present, gate here (refuse on mismatch — the disclosure/integrity gate, never a
    // silent mode-flip); without a pin the computed digest ships. This digest is what the WAL replay
    // compares against, so a re-run whose draft has drifted refuses the in-flight op instead of riding it.
    // The WORK TREE: the single edited copy when one exists (the draft being shipped — it may live
    // in the shared dir or any native copy), else the first placement; several DIVERGENT copies are
    // the typed freeze — a BARE publish never picks for you. `--dest`/`-a` is what supplies the
    // missing consent, and the copies it does not name are never touched: this publish advances
    // ONE copy to the new current, and each other edited copy keeps its bytes and becomes an
    // ordinary draft ahead of it (the shape "a teammate published while I had local edits" has
    // always produced).
    let picked = pick_copy(ctx, sel, &lock.name, &map)?;
    let placement = match &picked {
        Some(p) => p.dir.clone(),
        None => crate::placement::work_tree_dir(ctx, &lock.name, &map)?,
    };
    let scanned = scan::scan(&placement)?;
    let digest_hex = to_hex(&scanned.bundle_digest);
    // The other scope's copy is only NEWS while its bytes differ from these.
    let other_draft = unless_identical(other_draft, &digest_hex);
    if let Some(pin) = pin
        && digest_hex != pin
    {
        return Err(ClientError::ApprovalMismatch {
            expected: digest_hex,
            got: pin.to_owned(),
        });
    }

    // The BUNDLE KIND rides the wire, and the workspace rules on it (see the describe's twin
    // above).
    let bundle_kind = crate::bundle_kind::classify(ctx, id.as_str(), &map.placements).or_skill();

    // Whether this machine FOLLOWS the bundle — read BEFORE the write, like the describe's, and
    // carried to the receipt as the undo gate: `topos revert` resolves only a followed skill, so a
    // locally-authored bundle's receipt must not print an undo the verb would refuse.
    let followed = ctx
        .follow
        .followed()
        .into_iter()
        .any(|(fid, _)| fid == id.as_str());
    let sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("missing sync state".to_owned()))?;
    // The receipt's two live facts: the version the undo puts the team back on (the live current
    // this publish moves away from), and — for a copy equal to an older version — which version
    // `current` was. Both settle in the fresh-op arm; a WAL replay keeps the cached base.
    let mut undo_base = lock.base_commit.clone();
    let mut republish: Option<Republish> = None;

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
        None => {
            // THE SERVER'S CURRENT IS THE TRUTH (see the describe's twin): the copy is classified
            // against the live current, read here, never against the cache.
            let live = if followed || sync.observed != GENESIS {
                Some(live_or_cached(ctx, &lane, &sp, id.as_str(), &lock, &sync)?)
            } else {
                None
            };
            let standing = standing(live.as_ref(), &lock, &digest_hex);
            let base = match (&standing, &live) {
                // The copy IS the live current — an earlier publish of these bytes already LANDED
                // (the retry a failed local rewrite asks for resolves here), so the pending
                // governance rewrite is re-attempted (idempotent: with no matching path line it is
                // a no-op) BEFORE the already-published answer is returned.
                (Standing::Matches, _) => {
                    let dirs: Vec<std::path::PathBuf> = map
                        .placements
                        .iter()
                        .map(std::path::PathBuf::from)
                        .collect();
                    let _ = super::rewrite_to_governed(
                        outer_ctx,
                        &lock.name,
                        &lane.host,
                        &lane.workspace_name,
                        &dirs,
                    );
                    return Ok(PublishOutcome::NoChanges(no_changes(
                        lock.name.clone(),
                        other_draft,
                    )));
                }
                // An EDITED copy behind the live current: the plane's lineage fence would refuse
                // it; refused here, typed, with the one command that merges first.
                (Standing::Behind, _) if !propose => {
                    return Err(ClientError::PublishBehind {
                        skill: skill_name.to_owned(),
                        global,
                    });
                }
                // A PROPOSAL from behind moves no pointer and meets no fence: it stays built on
                // the version this copy applied — a reviewer decides.
                (Standing::Behind, Some(_)) => Some(PublishBase {
                    parent: parse_hex32(&lock.base_commit)?,
                    expected: sync.observed,
                }),
                (Standing::Genesis, _) | (_, None) => None,
                // A draft on the live current, or an older version carried forward: both parent
                // on the live current, and the plane's compare-and-swap expects ITS generation.
                (Standing::Draft | Standing::Forward(_), Some(l)) => Some(PublishBase {
                    parent: l.commit,
                    expected: l.generation,
                }),
            };
            if let Some(l) = &live {
                undo_base = l.hex.clone();
            }
            if let (Standing::Forward(mut rep), Some(l), Some(b)) = (standing, &live, &base) {
                // The forward commit parents on a version this store may never have fetched (a
                // revert made elsewhere) — backfill it so the local history links up and the
                // authoring commit's strict parent check holds; its history line rides the receipt.
                sync_engine::prefetch_version(ctx, &id, &l.hex)?;
                rep.current_message = Store::open(&sp.store)?
                    .read_commit_meta(l.commit)
                    .ok()
                    .map(|node| node.message);
                rep.new_version_id = predicted_version_id(ctx, b, scanned.bundle_digest, message)?;
                republish = Some(rep);
            }
            build_publish_op(
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
                bundle_kind.tag(),
                base.as_ref(),
            )?
        }
    };

    let receipt = contribute::run_write(ctx, transport, &sp, &rec, None)?;
    let dir_ref: &dyn crate::plane::DirectorySource = &*lane.transports.directory;
    let disclosure = ScopeDisclosure {
        cross_from: cross_scope.then(|| shipped_from(ctx, &placement)),
        from_machine: cross_scope && global,
        global,
        other_draft,
    };
    let mut outcome = map_outcome(
        ctx,
        &sp,
        &lock,
        &map,
        &rec,
        &receipt,
        skill_name,
        dir_ref,
        followed,
        picked.as_ref(),
        &disclosure,
        &undo_base,
        republish,
    )?;
    // GOVERNANCE TRANSFER, by default: a landed publish — OR an opened proposal (`--propose`,
    // the reviewed-bundle downgrade) — of a bundle some manifest referenced as a LOCAL PATH
    // rewrites that line to the canonical workspace reference: the local copy is now a managed
    // placement of the governed bundle (on the proposal arm, delivery follows approval); the
    // receipt states each part. Without the proposal arm the path line would sit forever — the
    // publish IS the transfer act, whichever gate it went through.
    let dirs: Vec<std::path::PathBuf> = map
        .placements
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    // THE REMOTE HALF HAS LANDED. From here, a LOCAL failure (the manifest rewrite, the
    // cache seed, the origin note) must never fail the command — the receipt would deny a
    // publish the plane holds, and a retry resolves no-change. The rewrite failure is
    // warned, carried truthfully on the receipt (`rewrite_pending`), and converged
    // idempotently by the next `update` or publish re-run.
    match &mut outcome {
        PublishOutcome::Published(data) => {
            match super::rewrite_to_governed(
                outer_ctx,
                &lock.name,
                &lane.host,
                &lane.workspace_name,
                &dirs,
            ) {
                Ok(super::GovernedOutcome::Rewritten(rw)) => {
                    data.manifest = Some(rw.manifest);
                    data.reference = Some(rw.canonical);
                    data.converted_from = Some(rw.from);
                    // Seed the offline cache with the governed fact (list/remove/the write
                    // lane answer correctly BEFORE the next sweep) — ONLY when a manifest
                    // line now actually references the bundle (`via_manifest` must never
                    // claim a line that does not exist); best-effort, never a failed publish.
                    let _ = crate::sync_status::merge_delivered(
                        outer_ctx.fs,
                        &outer_ctx.layout,
                        &lane.workspace_id,
                        &lane.host,
                        &lane.workspace_name,
                        id.as_str(),
                        crate::sync_status::DeliveredSkill {
                            name: lock.name.clone(),
                            review_required: false,
                            served_version: rec.candidate_commit.clone(),
                            withdrawn: false,
                            via_channels: Vec::new(),
                            via_manifest: true,
                            assigned_by: None,
                            // The kind this publish SHIPPED, replayed from the op record.
                            // A seed row is provenance, not authority — the next sweep's
                            // delivery still brings the catalog's answer — but leaving it
                            // blank made the very next `remove` of a just-published mcp
                            // bundle miss its config converge.
                            kind: rec.bundle_kind.clone(),
                            harness_states: Vec::new(),
                            picked: false,
                        },
                    );
                }
                Ok(super::GovernedOutcome::RowRemoved { manifest }) => {
                    data.rewrite_skipped = Some(rewrite_skipped_note(&lock.name, &manifest));
                }
                Ok(super::GovernedOutcome::None) => {}
                Err(e) => {
                    data.rewrite_pending = Some(rewrite_pending_note(outer_ctx, &lock.name, &e));
                }
            }
            data.origin_note =
                origin_asymmetry_note(outer_ctx, &sp, &lock.name, &lane).unwrap_or_default();
        }
        PublishOutcome::Proposed(data) => {
            match super::rewrite_to_governed(
                outer_ctx,
                &lock.name,
                &lane.host,
                &lane.workspace_name,
                &dirs,
            ) {
                Ok(super::GovernedOutcome::Rewritten(rw)) => {
                    data.manifest = Some(rw.manifest);
                    data.reference = Some(rw.canonical);
                    data.converted_from = Some(rw.from);
                }
                Ok(super::GovernedOutcome::RowRemoved { manifest }) => {
                    data.rewrite_skipped = Some(rewrite_skipped_note(&lock.name, &manifest));
                }
                Ok(super::GovernedOutcome::None) => {}
                Err(e) => {
                    data.rewrite_pending = Some(rewrite_pending_note(outer_ctx, &lock.name, &e));
                }
            }
        }
        // Unreachable here — the no-op arm returns above, having already re-attempted the
        // rewrite itself (a landed publish of these bytes is what made it a no-op).
        PublishOutcome::NoChanges(_) => {}
    }
    Ok(outcome)
}

/// The truthful landed-but-rewrite-pending receipt half: the warning is logged + printed to
/// stderr, and the line says exactly what stands (the path line) and what converges it (the next
/// update / publish re-run — both re-attempt the rewrite idempotently).
fn rewrite_pending_note(ctx: &Ctx<'_>, skill_name: &str, e: &ClientError) -> String {
    let _ = crate::logfile::append_error_event(
        ctx.fs,
        &ctx.layout.log_path(),
        "publish",
        e.code(),
        &format!("governance rewrite {skill_name}: {}", e.detail()),
        None,
        ctx.clock.now_unix_millis(),
    );
    let safe = crate::render::safe_message(e);
    crate::out::errln!("topos publish: {skill_name}: the manifest rewrite did not land: {safe}");
    format!(
        "the publish landed, but the manifest's local-path line could not be rewritten to the \
         governed reference ({safe}) — the next `topos update` (or re-running this publish) \
         completes the transfer"
    )
}

/// The concurrent-removal receipt half: the path line this publish would have rewritten was
/// removed (a concurrent `topos remove`) before the locked rewrite ran — nothing was written,
/// because a completed removal is never silently undone.
fn rewrite_skipped_note(skill_name: &str, manifest: &str) -> String {
    format!(
        "the manifest line for '{skill_name}' ({manifest}) was removed while the publish ran — \
         no workspace row was written (a removal is never silently undone); the publish stands \
         in the catalog, and `topos add` records the demand if you want it delivered here"
    )
}

/// The GitHub-import asymmetry, disclosed: publishing an imported skill does NOT rewrite a
/// manifest's origin-pin line (`github.com/…` refs are pinned demand, deliberately outside the
/// path-line transfer) — when one still references this bundle's recorded origin, the receipt
/// says the project keeps tracking that pin until the line is swapped for the governed reference.
fn origin_asymmetry_note(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    skill_name: &str,
    lane: &super::WriteLane,
) -> Result<Option<String>, ClientError> {
    let Some(origin) = doc::read_doc::<add::OriginDoc>(ctx.fs, &sp.origin)? else {
        return Ok(None);
    };
    // The forge key a manifest would carry for this import: the repo, or its discovered skill's
    // LEAF directory name as the fourth segment (never the literal in-repo path).
    let origin_ref = match origin
        .origin
        .subdir
        .as_deref()
        .and_then(|s| s.trim_end_matches('/').rsplit('/').next())
        .filter(|s| !s.is_empty())
    {
        Some(leaf) => format!("{}/{leaf}", origin.origin.source),
        None => origin.origin.source.clone(),
    };
    let tracked = super::manifest_edit::local_rows(ctx)?
        .into_iter()
        .any(|(_, _, rows)| rows.iter().any(|r| r.reference == origin_ref));
    Ok(tracked.then(|| {
        format!(
            "a manifest still tracks the GitHub origin pin ({origin_ref}) — that line is not \
             rewritten; the project keeps following the pin until you swap it for the governed \
             copy (`topos remove {origin_ref}`, then `topos add @{}/{skill_name}`)",
            lane.workspace_name
        )
    }))
}

/// The teammate handoff line — the one paste-ready instruction that brings a teammate's machine
/// into the workspace: their agent fetches the server's live walkthrough (`<origin>/agent`) and
/// follows it toward the workspace ADDRESS. Composed from the same `me.address` the share line
/// reads; the origin is the address's scheme + host (+ port) — a single-tenant address carries
/// no workspace path, so it already IS its origin. The share line (`<address>/skills/<name>`)
/// stays the members' deep link — it answers only for people already in the workspace, so it is
/// never the recruiting artifact.
///
/// The address is SERVER-SUPPLIED and lands verbatim inside the quoted instruction, so it is
/// gated first: only a clean http(s) URL composes a line. A control character, a quote, a space,
/// or a non-URL shape yields `None` — the line is OMITTED, never rendered mangled.
fn teammate_invite_line(address: &str) -> Option<String> {
    if !url_safe(address) {
        return None;
    }
    let origin = server_origin(address)?;
    Some(format!(
        "Ask your agent: \"Set up Topos for us: fetch {origin}/agent and follow it. \
         Our workspace: {address}\""
    ))
}

/// The output-integrity gate over a WHOLE server-supplied address (the origin/authority checks
/// cover only its host): every byte must be URL-safe printable ASCII. This excludes control
/// characters, whitespace, both quote kinds, backslashes, and non-ASCII bytes — none of which the
/// server-built address shape (`<origin>[/<slug>]`) ever carries, and all of which would land
/// verbatim inside a quoted instruction or a copy line.
fn url_safe(address: &str) -> bool {
    address.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'/'
                    | b'?'
                    | b'#'
                    | b'&'
                    | b'='
                    | b'%'
                    | b'+'
                    | b':'
                    | b'@'
            )
    })
}

/// The `<host>/<workspace>` spelling publish copy names a workspace by (`topos.sh/acme`) — the
/// workspace's own address with the scheme cut off, because that is the handle a person recognizes
/// and types, not a display name two workspaces can share.
///
/// Derived by a real parse, not a trim: the same URL-safety gate the invite line runs, an OPTIONAL
/// exact `http(s)://` scheme, a hostname-shaped authority (any port numeric), and the path kept up
/// to a query/fragment with any trailing `/` dropped. `None` for anything else — the caller falls
/// back to the display name rather than printing a broken address.
fn workspace_handle(address: &str) -> Option<String> {
    if !url_safe(address) {
        return None;
    }
    // The scheme is optional here (unlike `server_origin`, which composes a URL): the handle IS
    // the schemeless form, so an address already spelled that way is already a handle.
    let rest = address
        .strip_prefix("https://")
        .or_else(|| address.strip_prefix("http://"))
        .unwrap_or(address);
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let handle = rest[..end].trim_end_matches('/');
    let authority_len = handle.find('/').unwrap_or(handle.len());
    if !valid_authority(&handle[..authority_len]) {
        return None;
    }
    Some(handle.to_owned())
}

/// The uniform `workspace` field from a membership describe's ADDRESS — the fallback for a receipt
/// whose session record could not name the workspace. It goes through [`workspace_handle`], so a
/// malformed address answers `None` here exactly as it composes no line elsewhere; an address with
/// no workspace segment (a single-workspace deployment) has no name to carry and answers `None`
/// too — the session is what knows it there.
fn workspace_ref_of_me(address: Option<&str>) -> Option<topos_types::results::WorkspaceRef> {
    super::workspace_ref_of_handle(&workspace_handle(address?)?)
}

/// Whether `authority` is hostname-shaped: a non-empty host of hostname bytes, with any port
/// digits only. The shared half of the address parses — a malformed authority composes no line.
fn valid_authority(authority: &str) -> bool {
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return false;
    }
    port.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The server ORIGIN of a workspace address — scheme + host (+ port), derived by a real parse:
/// an exact `http(s)://` scheme, then the authority cut at the first `/`, `?`, or `#` (a query
/// or fragment never rides into the origin), with the host constrained to hostname bytes and any
/// port to digits. `None` for anything else — a schemeless or malformed address composes no line.
fn server_origin(address: &str) -> Option<&str> {
    let rest = address
        .strip_prefix("https://")
        .or_else(|| address.strip_prefix("http://"))?;
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if !valid_authority(&rest[..authority_len]) {
        return None;
    }
    Some(&address[..address.len() - rest.len() + authority_len])
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
/// `enrolled_publish`, and `base` is the version it parents on — the LIVE current the caller read,
/// or `None` for the genesis shape). Computes the byte-identical `(commit_id, bundle_digest)`,
/// commits the candidate into the local store (renderable for a replay + local history), and
/// assembles the [`OpRecord`] (the WAL write itself happens in `run_write`). Runs ONLY in the
/// fresh-op arm of `enrolled_publish`'s WAL match — a crashed pending op replays untouched.
///
/// # Errors
/// A store / scan failure; [`ClientError::Corrupt`] on a commit-id preimage fault.
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
    bundle_kind: Option<String>,
    base: Option<&PublishBase>,
) -> Result<OpRecord, ClientError> {
    // The commit message: `-m <message>` when given (folded into `commit_id`, so it changes the version
    // identity), else the default. It also rides the local store commit, so a WAL replay re-renders the
    // byte-identical candidate (`render_candidate` reads the message back from the store).
    let commit_message = message.unwrap_or(PUBLISH_MESSAGE);
    let digest_hex = to_hex(&digest);

    // Genesis (no `current` yet) is a zero-parent commit at generation 0; a normal publish parents on
    // the live current.
    let (parents, expected): (Vec<[u8; 32]>, u64) = match base {
        None => (Vec::new(), GENESIS),
        Some(b) => (vec![b.parent], b.expected),
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
    // The upstream provenance rides the WAL when the skill was imported from an external
    // origin (`origin.json`) — the server records the fork-that-remembers link.
    let upstream = doc::read_doc::<super::add::OriginDoc>(ctx.fs, &sp.origin)
        .ok()
        .flatten()
        .map(|o| origin_to_wire(&o.origin));
    Ok(OpRecord {
        schema_version: PERSISTED_SCHEMA_VERSION,
        upstream,
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
        // The BUNDLE KIND rides the WAL, so a replay re-sends the same `kind` the first attempt
        // did (`contribute::run_write` reads it back onto `PublishRequest`/`ProposeRequest`) —
        // the catalog can never learn a different answer from a retry than from the original.
        bundle_kind,
        last_receipt: None,
    })
}

/// What a publish's SCOPE choice owes the receipt: the folder it shipped from when that folder was
/// not in the scope the command stood in, and the other scope's copy left holding its own edits.
struct ScopeDisclosure {
    /// The shipped folder, present only on a cross-scope ship (a same-scope publish leaves the
    /// `--dest` disclosure to say whether the folder was a choice at all).
    cross_from: Option<String>,
    /// Whether that folder is the MACHINE copy's, read from inside a project checkout.
    from_machine: bool,
    /// The RESOLVED store's scope — machine or a checkout's. It is not a receipt line; it is what a
    /// refusal raised after the write (the plane's lineage fence) spells its way out for.
    global: bool,
    /// The other scope's copy, when it carries edits this publish did not ship.
    other_draft: Option<ScopeDraft>,
}

/// Map the plane's typed write outcome to a [`PublishOutcome`] (or a typed [`ClientError`]).
/// `directory` feeds the landed receipt's workspace address, share line, and teammate handoff — one
/// best-effort `me` read AFTER the write settled (a failed read leaves those lines absent; the
/// outcome is untouched). `followed` gates the undo (see the landed arm).
#[allow(clippy::too_many_arguments)]
fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    map: &PlacementMap,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    skill_name: &str,
    directory: &dyn crate::plane::DirectorySource,
    followed: bool,
    picked: Option<&super::dest_select::SelectedCopy>,
    disclosure: &ScopeDisclosure,
    undo_base: &str,
    republish: Option<Republish>,
) -> Result<PublishOutcome, ClientError> {
    // Both landed shapes name their destination from the workspace's own ADDRESS — ONE
    // best-effort read, AFTER the write; a failure just leaves the lines off, it never fails a
    // write the plane already holds. Each arm then composes exactly the lines it prints from that
    // one address, so no arm builds a line it goes on to drop.
    let address = || directory.me(&rec.workspace_id).ok().map(|m| m.address);
    // The lane session's OWN record of the workspace address — the local fallback that keeps an
    // applied receipt naming its workspace even when the post-write read does not answer.
    let session_ref = || super::workspace_ref(ctx, &rec.workspace_id);
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            // A direct publish moved `current` — advance the local state (read-your-writes).
            let record = receipt.wire_record.as_ref().ok_or_else(|| {
                ClientError::Corrupt("an OK publish carried no current pointer".to_owned())
            })?;
            // The read-your-writes advance re-reads the SAME copy this publish shipped from — the
            // selection is threaded through rather than re-derived, or a `--dest` publish out of a
            // freeze would land remotely and then refuse locally on the very freeze it resolved.
            let new_gen = contribute::apply_publish_ok(
                ctx,
                sp,
                lock,
                map,
                rec,
                record,
                picked.map(|p| p.dir.as_path()),
            )?;
            // The receipt's placement detail: `curated_role_required` means the channel placement
            // (the op's `--to` target, or the default `everyone` on a genesis) was WITHHELD by a
            // curated channel's role gate — the publish landed, the reference did not. Surfaced so
            // the receipt never implies a reach the placement did not gain.
            let placement_outcome = receipt
                .receipt
                .as_ref()
                .and_then(|r| r.details.as_ref())
                .and_then(|d| d.get("placement"))
                .and_then(|p| p.as_str())
                .map(str::to_owned);
            let target_channel = || rec.channel.clone().unwrap_or_else(|| "everyone".to_owned());
            let placement_withheld = (placement_outcome.as_deref()
                == Some("curated_role_required"))
            .then(target_channel);
            // The deletion race's in-transaction refusal: the channel the client verified was
            // deleted before the write — the publish landed catalog-only, never a silent mint.
            let placement_missing =
                (placement_outcome.as_deref() == Some("channel_not_found")).then(target_channel);
            let address = address();
            let workspace = session_ref().or_else(|| workspace_ref_of_me(address.as_deref()));
            let share_line = address
                .as_deref()
                .map(|a| format!("{a}/skills/{skill_name}"));
            // The teammate handoff — the members' deep link above 404s for a non-member, so
            // recruiting a teammate takes this join line instead.
            let invite_line = address.as_deref().and_then(teammate_invite_line);
            // The version the undo restores is the LIVE current this publish moved away from —
            // named SHORT: `revert --to` resolves a unique prefix of 8+ chars, so the receipt
            // hands back the same 12-char spelling every other surface prints.
            let undo = landed_undo_is_restorative(followed, rec.expected_generation).then(|| {
                format!(
                    "topos revert {skill_name} --to {}",
                    crate::render::short(undo_base)
                )
            });
            let (from_placement, other_edited) =
                from_disclosure(picked, disclosure.cross_from.clone());
            Ok(PublishOutcome::Published(Box::new(PublishData {
                project_lock: None,
                republish,
                skill_id: rec.skill_id.clone(),
                name: skill_name.to_owned(),
                version_id: rec.candidate_commit.clone(),
                bundle_digest: rec.bundle_digest.clone(),
                current_generation: new_gen,
                added: None,
                placement_withheld,
                placement_missing,
                manifest: None,
                reference: None,
                converted_from: None,
                invite_line,
                origin_note: None,
                rewrite_pending: None,
                rewrite_skipped: None,
                // The kind the catalog now records for this bundle, replayed from the op record —
                // so a WAL retry's receipt says exactly what the first attempt's said.
                kind: rec.bundle_kind.clone(),
                workspace,
                share_line,
                undo,
                from_placement,
                from_machine: disclosure.from_machine,
                other_scope_draft: disclosure.other_draft.clone(),
                other_edited,
            })))
        }
        TerminalOutcome::NeedsReview => {
            // The `--to` placement applies on the proposal arm too — its outcome rides the
            // receipt details exactly as on a landed publish.
            let placement_outcome = receipt
                .receipt
                .as_ref()
                .and_then(|r| r.details.as_ref())
                .and_then(|d| d.get("placement"))
                .and_then(|p| p.as_str())
                .map(str::to_owned);
            let target_channel = || rec.channel.clone().unwrap_or_else(|| "everyone".to_owned());
            // The proposal receipt names the same destination the landed one does. No undo rides
            // it: `current` never moved, so there is no prior state to restore — the author's
            // escape is `review <handle> --withdraw`, which the renderer names.
            let address = address();
            let workspace = session_ref().or_else(|| workspace_ref_of_me(address.as_deref()));
            let share_line = address
                .as_deref()
                .map(|a| format!("{a}/skills/{skill_name}"));
            // The copy question is the SAME on this arm: a proposal ships bytes, so which folder
            // they came from and what the copies left behind become are the same two facts the
            // describe predicted and the landed receipt states.
            let (from_placement, other_edited) =
                from_disclosure(picked, disclosure.cross_from.clone());
            Ok(PublishOutcome::Proposed(Box::new(ProposeData {
                proposal: format!("{skill_name}@{}", rec.candidate_commit),
                base_version_id: lock.base_commit.clone(),
                title: skill_name.to_owned(),
                body: None,
                added: None,
                placement_withheld: (placement_outcome.as_deref() == Some("curated_role_required"))
                    .then(target_channel),
                placement_missing: (placement_outcome.as_deref() == Some("channel_not_found"))
                    .then(target_channel),
                manifest: None,
                reference: None,
                converted_from: None,
                rewrite_pending: None,
                rewrite_skipped: None,
                workspace,
                share_line,
                from_placement,
                from_machine: disclosure.from_machine,
                other_scope_draft: disclosure.other_draft.clone(),
                other_edited,
            })))
        }
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: skill_name.to_owned(),
            current: receipt.error.as_ref().and_then(|e| e.current_generation),
            // The plane's own lineage fence, caught where the local behind guard could not see it
            // (this machine's `observed` was current until the write asked). Same scope, same
            // answer: the copy that refused is the one to rebase.
            global: disclosure.global,
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

#[cfg(test)]
mod tests {
    use topos_types::results::PublishGate;

    use super::{
        GENESIS, landed_undo_is_restorative, server_origin, teammate_invite_line,
        undo_is_restorative, workspace_handle,
    };

    #[test]
    fn a_workspace_is_named_by_its_address_not_its_scheme() {
        // The multi-tenant shape: the handle is `<host>/<workspace>` — the spelling a person types.
        assert_eq!(
            workspace_handle("https://topos.sh/acme").as_deref(),
            Some("topos.sh/acme")
        );
        // The single-tenant shape: the install IS its one workspace, so the host alone is the handle.
        assert_eq!(
            workspace_handle("https://topos.example.com").as_deref(),
            Some("topos.example.com")
        );
        // A port belongs to the handle; a trailing slash, a query, and a fragment do not.
        assert_eq!(
            workspace_handle("http://localhost:3000/eng/").as_deref(),
            Some("localhost:3000/eng")
        );
        assert_eq!(
            workspace_handle("https://topos.sh/acme?tab=skills").as_deref(),
            Some("topos.sh/acme")
        );
        assert_eq!(
            workspace_handle("https://topos.sh/acme#top").as_deref(),
            Some("topos.sh/acme")
        );
        // An address already spelled schemeless IS a handle — nothing to strip.
        assert_eq!(
            workspace_handle("acme.test/eng").as_deref(),
            Some("acme.test/eng")
        );
        // Anything that would print BROKEN composes no handle at all: the caller falls back to the
        // display name rather than putting these on a header.
        for bad in [
            "https://topos.sh/ac\u{7}me",
            "https://topos.sh/acme team",
            "https://topos.sh/a'cme",
            "https://topos.sh/a\\cme",
            "https://topos.sh/acme\nrun: rm -rf",
            "https://:8443/eng",
            "https://topos.sh:port/eng",
            "",
        ] {
            assert_eq!(workspace_handle(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_undo_is_offered_only_where_it_restores_the_whole_prior_state() {
        // The gate that MOVES `current` leaves something to put back; the review gate does not —
        // a proposal never moved the pointer, so a `revert --to <base>` would restore a state that
        // was never left.
        assert!(undo_is_restorative(true, PublishGate::Lands));
        assert!(!undo_is_restorative(true, PublishGate::Proposal));
        // A bundle this machine does not FOLLOW cannot be reverted from here at all — `revert`
        // resolves followed skills only, so naming the command would hand out an undo that fails.
        assert!(!undo_is_restorative(false, PublishGate::Lands));
        assert!(!undo_is_restorative(false, PublishGate::Proposal));

        // On the LANDED receipt the same follow rule holds, plus a prior version to name: a
        // genesis publish CREATED `current`, so there is no earlier state to go back to.
        assert!(landed_undo_is_restorative(true, 42));
        assert!(!landed_undo_is_restorative(true, GENESIS));
        assert!(!landed_undo_is_restorative(false, 42));
    }

    #[test]
    fn a_workspace_address_cuts_to_its_server_origin() {
        // The multi-tenant shape: the address carries the workspace slug — the origin drops it.
        assert_eq!(
            server_origin("https://topos.sh/acme"),
            Some("https://topos.sh")
        );
        // A port stays part of the origin.
        assert_eq!(
            server_origin("https://topos.example.com:8443/eng"),
            Some("https://topos.example.com:8443")
        );
        // The single-tenant shape: the install IS its one workspace — the address IS the origin.
        assert_eq!(
            server_origin("https://topos.example.com"),
            Some("https://topos.example.com")
        );
    }

    #[test]
    fn a_query_or_fragment_never_rides_into_the_origin() {
        // A query directly on the authority (no path) — the old first-`/` splitter kept it.
        assert_eq!(
            server_origin("https://topos.sh?tab=skills"),
            Some("https://topos.sh")
        );
        assert_eq!(
            server_origin("https://topos.sh/acme?tab=skills"),
            Some("https://topos.sh")
        );
        assert_eq!(
            server_origin("https://topos.sh#top"),
            Some("https://topos.sh")
        );
        assert_eq!(
            server_origin("https://topos.example.com:8443?x=1"),
            Some("https://topos.example.com:8443")
        );
    }

    #[test]
    fn a_non_url_address_derives_no_origin() {
        // Schemeless, wrong scheme, empty host, junk host, junk port — all refuse.
        assert_eq!(server_origin("topos.sh/acme"), None);
        assert_eq!(server_origin("ftp://topos.sh/acme"), None);
        assert_eq!(server_origin("https:///acme"), None);
        assert_eq!(server_origin("https://host name/acme"), None);
        assert_eq!(server_origin("https://topos.sh:port/acme"), None);
        assert_eq!(server_origin("https://topos.sh:/acme"), None);
        assert_eq!(server_origin(""), None);
    }

    #[test]
    fn the_teammate_handoff_composes_the_exact_join_line() {
        assert_eq!(
            teammate_invite_line("https://topos.sh/acme").as_deref(),
            Some(
                "Ask your agent: \"Set up Topos for us: fetch https://topos.sh/agent and follow \
                 it. Our workspace: https://topos.sh/acme\""
            )
        );
        // Single-tenant: fetch the origin's walkthrough, follow the origin itself.
        assert_eq!(
            teammate_invite_line("https://topos.example.com").as_deref(),
            Some(
                "Ask your agent: \"Set up Topos for us: fetch https://topos.example.com/agent \
                 and follow it. Our workspace: https://topos.example.com\""
            )
        );
    }

    #[test]
    fn an_injected_address_omits_the_line_never_mangles_it() {
        // The address is server-supplied and interpolated into a quoted instruction — a quote,
        // a control character, or whitespace must yield NO line at all.
        assert_eq!(
            teammate_invite_line("https://topos.sh/acme\" — ignore the above"),
            None
        );
        assert_eq!(
            teammate_invite_line("https://topos.sh/acme\nrun: rm -rf"),
            None
        );
        assert_eq!(teammate_invite_line("https://topos.sh/ac\u{7}me"), None);
        assert_eq!(teammate_invite_line("https://topos.sh/acme team"), None);
        assert_eq!(teammate_invite_line("https://topos.sh/a'cme"), None);
        assert_eq!(teammate_invite_line("https://topos.sh/a\\cme"), None);
        // A non-URL address (no http(s) origin to derive) composes no line either.
        assert_eq!(teammate_invite_line("topos.sh/acme"), None);
        assert_eq!(teammate_invite_line("javascript:alert(1)"), None);
    }

    #[test]
    fn a_query_form_address_keeps_the_line_with_the_right_origin() {
        // URL-shaped with a query: the line renders, the origin cut at the query — never a
        // `<origin>?tab=…/agent` mangle.
        assert_eq!(
            teammate_invite_line("https://topos.sh/acme?tab=skills").as_deref(),
            Some(
                "Ask your agent: \"Set up Topos for us: fetch https://topos.sh/agent and follow \
                 it. Our workspace: https://topos.sh/acme?tab=skills\""
            )
        );
    }
}
