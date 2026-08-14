//! `topos add <reference>` — the demand side of delivery, recorded as ONE manifest row and
//! delivered in the same invocation.
//!
//! The reference's SHAPE decides everything (the joined-key grammar): `@acme/code-review` (or the
//! canonical `topos.sh/acme/code-review`) is a workspace bundle, `@acme/channels/backend` a
//! channel, `@acme` the workspace FEED — personal by nature, so it lives in the global file only —
//! and `owner/repo[/skill]` a git source. Nothing resolves by search: a spelled host/workspace
//! resolves through exactly that session, and a workspace this machine is not logged into refuses
//! toward `topos login`.
//!
//! The two SCOPES are unblended: without `-g` the row lands in the NEAREST project `topos.toml`
//! covering the working directory — and with none in reach the add REFUSES
//! ([`ClientError::NoManifest`]: `topos init` creates one here, `-g` acts on the machine), never
//! creating a file somewhere nobody named. With `-g` it lands in this machine's global file, the
//! complete machine recipe — so a bare `add -g X` of something a standing feed row already
//! delivers writes NOTHING and says so.
//!
//! A git source is always DESCRIBED FIRST here: the repo is fetched read-only, what it holds and
//! what would be written where is spelled out, and it lands only under `--yes`. That is a property
//! of this verb, not of the origin — an interactive `add` is where a person is present to read the
//! answer, so every one of them gets the same two-phase shape however many times the source has
//! been added before. The `-s`/`-a` SELECTOR arm behaves identically: a selector narrows which
//! members land and where, never whose bytes they are. Every other arm is a self-scoped file edit
//! that applies immediately with an undo-led receipt.

use topos_types::results::{AddData, AddDescribeData};

use crate::bundle_kind::BundleKind;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::git_source::GitTarballSource;
use crate::manifest::document::{EntryFields, EntryValue, ManifestScope};
use crate::manifest::keys::{self, InputRef, KeyShape};
use crate::sessions::Session;
use crate::source::{GitHost, RemoteSpec};

use super::manifest_edit::{self as medit, EditTarget};
use super::reconcile::{SessionConnect, SessionTransports};

/// What `add <reference>` did — or, for a git source, what it WOULD do.
#[derive(Debug)]
pub(crate) enum AddRefOutcome {
    /// The row is written (and, where it delivers bytes, they landed). Boxed: `AddData` dwarfs
    /// the describe variant.
    Applied {
        data: Box<AddData>,
        /// The DIAGNOSTICS this invocation's own converge produced — the sweep's warning lines,
        /// which ride the envelope's `warnings` exactly as they do on an `update`. Empty for every
        /// add that converged nothing of its own.
        messages: Vec<topos_types::Message>,
    },
    /// A git source this machine has never used: what it holds and what would be written. Boxed
    /// for the same reason `Applied` is — neither variant should set the enum's size.
    Described {
        data: Box<AddDescribeData>,
        yes_argv: Vec<String>,
    },
}

impl AddRefOutcome {
    /// The applied receipt of an add that converged nothing of its own — every arm but the
    /// set-delivered one.
    fn applied(data: AddData) -> Self {
        AddRefOutcome::Applied {
            data: Box::new(data),
            messages: Vec::new(),
        }
    }
}

/// `topos add <reference> [-g] [-a <agent>]… [--dest <folder>]… [--yes]`.
///
/// # Errors
/// [`ClientError::SessionRequired`] for a workspace this installation is not logged into;
/// [`ClientError::InvalidArgument`] for a reference the grammar (or the target file) refuses;
/// [`ClientError::UnknownAgent`] / [`ClientError::SelectionRefused`] from the `-a`/`--dest`
/// selection; [`ClientError::NotAvailable`] / [`ClientError::Plane`] from the catalog read; the
/// remote-import family from a git source; a filesystem failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_reference(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn GitTarballSource>,
    raw: &str,
    global: bool,
    yes: bool,
    selection: &super::dest_select::Selection,
    declared: Option<BundleKind>,
) -> Result<AddRefOutcome, ClientError> {
    let host = medit::default_host(ctx);
    let parsed = match keys::parse_input(raw, host.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            // `@ws` sugar with several connected hosts: the refusal names them rather than
            // guessing which workspace was meant.
            if raw.starts_with('@')
                && let Some(err) = medit::several_hosts(ctx, raw)
            {
                return Err(err);
            }
            return Err(ClientError::InvalidArgument(e.message));
        }
    };
    match parsed.shape.clone() {
        KeyShape::Feed { host, workspace } => {
            // A feed row reaches every agent by nature (its value is exactly `"*"`) — a
            // selection over it refuses whole, teaching the row that CAN carry one.
            if !selection.is_empty() {
                return Err(ClientError::SelectionRefused(format!(
                    "`{raw}` is a whole feed and reaches every agent — narrow a single skill or \
                     channel instead (`topos add -g {raw}/<skill> -a <agent>`)"
                )));
            }
            add_feed(ctx, &host, &workspace, global)
        }
        KeyShape::WorkspaceBundle { .. } | KeyShape::Channel { .. } => {
            add_workspace(ctx, connect, &parsed, global, selection, declared)
        }
        KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. } => {
            add_forge(ctx, git, &parsed, raw, global, yes, selection)
        }
        KeyShape::LocalPath { .. } => Err(ClientError::InvalidArgument(format!(
            "`{raw}` is a folder — `topos add {raw}` adopts it in place; there is no reference to \
             resolve"
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// The FEED arm — `add -g @acme`
// ---------------------------------------------------------------------------------------------

/// `add -g @acme` — adopt everything that workspace gives you, on this machine. In a project the
/// same token teaches the repo-shaped alternative instead (a feed is personal by nature).
fn add_feed(
    ctx: &Ctx<'_>,
    host: &str,
    workspace: &str,
    global: bool,
) -> Result<AddRefOutcome, ClientError> {
    let reference = format!("{host}/{workspace}");
    if !global {
        // The SAME teaching the manifest grammar gives a feed row written into a project file.
        return Err(ClientError::InvalidArgument(format!(
            "`{reference}` is a feed row — personal by nature, so it lives in the global manifest \
             (`~/.topos/topos.toml`) only; a project manifest is a repo fact, identical for every \
             contributor, and a channel (`{reference}/channels/<name>`) is the repo-shaped set to \
             name here"
        )));
    }
    let connected = medit::connected_workspaces(ctx);
    if !connected.iter().any(|(h, w)| h == host && w == workspace) {
        return Err(ClientError::SessionRequired {
            message: format!(
                "this machine is not logged into {reference} — run `topos login {reference}` \
                 first; adopting a workspace's feed needs the session that serves it"
            ),
            address: reference,
        });
    }
    let target = medit::global_target(ctx);
    let mut data = set_data(workspace);
    // Idempotent: a feed row already standing is stated, never rewritten.
    if let Some(text) = medit::read_text(ctx, &target.path)?
        && let Ok(editor) = crate::manifest::document::ManifestEditor::open(&text, target.scope)
        && editor.row(&reference).is_some()
    {
        data.manifest = Some(target.path.display().to_string());
        data.scope = Some(medit::receipt_scope(&target));
        data.reference = Some(reference.clone());
        data.source = Some(reference.clone());
        medit::push_note(
            &mut data,
            format!("already adopting {workspace}'s feed here — nothing changed"),
        );
        return Ok(AddRefOutcome::applied(data));
    }
    // NO note here: the receipt's own closing sentence for a feed row already says this machine
    // now takes whatever the workspace gives it (`render::add_tty`, off the reference SHAPE). A
    // note repeating it printed the same fact twice, one line apart. The no-op arm above keeps
    // its note — "already adopting …" is a fact that sentence does not carry.
    medit::write_row(ctx, &mut data, &target, &reference, &EntryValue::Star)?;
    Ok(AddRefOutcome::applied(data))
}

// ---------------------------------------------------------------------------------------------
// The WORKSPACE arms — a bundle or a channel
// ---------------------------------------------------------------------------------------------

/// A workspace reference resolved through its session.
struct Resolved {
    session: Session,
    canonical: String,
    /// The catalog entry, for a bundle reference (a channel has no bytes of its own).
    entry: Option<Box<topos_types::requests::WireSkillIndexEntry>>,
    name: String,
}

/// The kind a catalog entry names, or the refusal `add` answers for a kind this build cannot
/// deliver — the same teaching the sweep gives, at the other door: what the bundle is, and the
/// one command that makes this machine able to take it.
///
/// # Errors
/// [`ClientError::InvalidArgument`] naming the bundle, its kind, and `topos self-update`.
fn add_kind(word: &str, name: &str) -> Result<BundleKind, ClientError> {
    BundleKind::parse(word).ok_or_else(|| {
        ClientError::InvalidArgument(format!(
            "'{name}' is a \"{word}\" bundle — this topos does not know how to deliver that \
             kind; run `topos self-update`"
        ))
    })
}

fn add_workspace(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    parsed: &InputRef,
    global: bool,
    selection: &super::dest_select::Selection,
    declared: Option<BundleKind>,
) -> Result<AddRefOutcome, ClientError> {
    let resolved = resolve_workspace(ctx, connect, &parsed.shape)?;
    // The `-a`/`--dest` selection, resolved at the scope the row lands in — config FILES for an
    // `mcp` bundle, skills folders for everything else — BEFORE any write.
    let scope = if global {
        ManifestScope::Global
    } else {
        ManifestScope::Project
    };
    // The catalog's KIND, parsed against what this build can deliver. A bundle a newer server
    // published under a kind this binary predates is refused HERE — before a row is written for
    // something no sweep could ever place.
    let kind = match resolved.entry.as_deref() {
        Some(e) => Some(add_kind(&e.kind, &resolved.name)?),
        None => None,
    };
    // A `--kind` word that CONTRADICTS the catalog is refused, never quietly overruled. The
    // catalog is the authority on what a workspace bundle is, so the flag can only ever agree with
    // it or be wrong — and being wrong mattered: `--kind skill` on a server bundle used to deliver
    // a tool endpoint into the person's agents without a word, while the local-folder door refuses
    // the same mistake by name. Silence still means "whatever the catalog says", which is why the
    // flag is unnecessary here at all.
    if let (Some(said), Some(actual)) = (declared, kind)
        && said != actual
    {
        return Err(ClientError::InvalidArgument(format!(
            "`{}` is {} in the catalog, not {} — a workspace already records what each bundle \
             is, so `topos add {}` needs no `--kind` at all; drop it",
            resolved.name,
            actual.noun_phrase(),
            said.noun_phrase(),
            parsed.shape.canonical(),
        )));
    }
    let mcp_kind = kind.is_some_and(BundleKind::is_mcp);
    let mut dest_entries = if selection.is_empty() {
        Vec::new()
    } else if mcp_kind {
        selection.mcp_entries(scope)?
    } else {
        selection.skill_entries(scope)?
    };
    let mut data = match &resolved.entry {
        Some(e) => AddData {
            skill_id: Some(e.skill_id.clone()),
            name: e.name.clone(),
            version_id: Some(e.version_id.clone()),
            bundle_digest: Some(e.bundle_digest.clone()),
            ..set_data(&resolved.name)
        },
        None => set_data(&resolved.name),
    };
    let pin = parsed.pin.clone();
    let value = match (&pin, dest_entries.is_empty()) {
        (Some(p), true) => EntryValue::Pin(p.clone()),
        (None, true) => EntryValue::Star,
        // A selection rides the row as its `dest` — a standing fact the reconcile plans against.
        (pin, false) => EntryValue::Fields(EntryFields {
            version: pin.clone(),
            dest: Some(dest_entries.clone()),
            ..EntryFields::default()
        }),
    };
    // Prove the row BEFORE anything lands (a channel cannot carry `version`; a dest entry must
    // fit the scope's dialect).
    crate::manifest::document::check_row(&resolved.canonical, scope, &value)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;
    // A PINNED reference does not land `current` — the catalog entry above describes the version
    // this row deliberately is NOT taking, so the receipt must not name it. The pin IS the version
    // that lands; its digest is not knowable from the index read, and an empty digest (the shape
    // every non-bundle arm already carries) says "not stated here" rather than stating the wrong
    // one.
    if let Some(p) = &pin {
        data.version_id = Some(p.clone());
        data.bundle_digest = None;
    }
    // THE SET ALREADY DELIVERS IT HERE. Naming destinations for a bundle a channel or feed row
    // carries is not a demand — the demand stands — and the row this would write could only
    // NARROW what the set reaches. So nothing is recorded and the placements converge instead,
    // which is what actually puts the asked surface's missing copy there. A pinned reference is a
    // real conversion to machine-local control and writes its row as ever.
    //
    // UNLESS THE ASK IS SOMEWHERE THE SET CANNOT GO. A folder that is no agent's own is reached by
    // nothing but a row: recording none would leave the person's `--dest` demanded by nobody, and
    // the next sweep would report the machine up to date over a folder that never gets a copy. Such
    // an ask BIRTHS the row, carrying the token so the set's whole reach rides with it.
    let mut set_line: Option<String> = None;
    if !dest_entries.is_empty() && pin.is_none() && resolved.entry.is_some() {
        let target = if global {
            medit::global_target(ctx)
        } else {
            medit::project_target(ctx)?.ok_or(ClientError::NoManifest)?
        };
        if let Some(set) = medit::set_delivering(
            ctx,
            &target,
            &resolved.session.host,
            &resolved.session.workspace_name,
            &resolved.name,
        )? {
            let outside = medit::outside_default_reach(
                ctx,
                &target,
                &resolved.canonical,
                &resolved.name,
                kind.unwrap_or(BundleKind::Skill),
                &dest_entries,
            )?;
            if outside.is_empty() {
                return set_delivered_add(
                    ctx, connect, data, &resolved, &target, global, selection, set, mcp_kind,
                );
            }
            dest_entries = outside;
            set_line = Some(set);
        }
    }
    // The row a set-delivered ask outside the reach writes: the token first (the set's own reach,
    // recomputed every run), then the destinations only this line can demand.
    let value = match &set_line {
        None => value,
        Some(_) => EntryValue::Fields(EntryFields {
            dest: Some(
                std::iter::once(crate::manifest::dest::DEFAULT_REACH.to_owned())
                    .chain(dest_entries.iter().cloned())
                    .collect(),
            ),
            ..EntryFields::default()
        }),
    };

    if global {
        let target = medit::global_target(ctx);
        // A standing `"off"` switch is the row's own inverse: adding it back DELETES the switch
        // (never a second, redundant positive row beside it). The delete is a read-modify-write,
        // so it holds the manifest writer lock exactly as `write_row` does. A dest-carrying add
        // skips this arm — its row REPLACES the switch through the ordinary write below, which
        // names the replaced prior honestly.
        let off_guard = medit::lock_manifest(ctx, &target.path)?;
        if dest_entries.is_empty()
            && let Some(text) = medit::read_text(ctx, &target.path)?
            && let Ok(mut editor) =
                crate::manifest::document::ManifestEditor::open(&text, ManifestScope::Global)
            && editor
                .row(&resolved.canonical)
                .is_some_and(|r| r.value == EntryValue::Off)
        {
            editor.remove_row(&resolved.canonical);
            editor.write(ctx.fs, &target.path)?;
            data.manifest = Some(target.path.display().to_string());
            data.scope = Some(medit::receipt_scope(&target));
            data.reference = Some(resolved.canonical.clone());
            data.source = Some(resolved.canonical.clone());
            data.undo = medit::undo_add(&resolved.canonical, true);
            medit::push_note(
                &mut data,
                format!(
                    "the `off` switch is gone — '{}' flows from {}'s feed again",
                    resolved.name, resolved.session.workspace_name
                ),
            );
            drop(off_guard);
            return Ok(finish_workspace(
                ctx, connect, data, &resolved, &target, true,
            ));
        }
        // Released before `write_row`, which takes it again for its own read→edit→write (the
        // `flock` is per open file description, so re-taking it in the same process is fine, but
        // holding it across the delivery below would block a concurrent verb for a network call).
        drop(off_guard);
        let feeds = medit::feed_items(ctx, connect);
        let feed_carries = feeds.iter().any(|i| {
            !i.declined
                && i.host == resolved.session.host
                && i.workspace == resolved.session.workspace_name
                && i.name == resolved.name
        });
        let plan = medit::plan_for(ctx, &target)?;
        let flowing = plan.has_feed(&resolved.session.host, &resolved.session.workspace_name);
        // REDUNDANCY: a bare row under a flowing feed adds nothing — say so and write nothing.
        // (A pin, or any field, is a real conversion to machine-local control and does write.)
        if feed_carries && flowing && matches!(value, EntryValue::Star) {
            data.manifest = None;
            data.reference = Some(resolved.canonical.clone());
            data.undo = Vec::new();
            medit::push_note(
                &mut data,
                format!(
                    "{}'s feed already delivers '{}' here — a bare row would add nothing, so \
                     nothing was written",
                    resolved.session.workspace_name, resolved.name
                ),
            );
            return Ok(AddRefOutcome::applied(data));
        }
        let declined = feeds.iter().any(|i| {
            i.declined
                && i.host == resolved.session.host
                && i.workspace == resolved.session.workspace_name
                && i.name == resolved.name
        });
        medit::write_row(ctx, &mut data, &target, &resolved.canonical, &value)?;
        if declined {
            medit::push_note(
                &mut data,
                format!(
                    "'{}' is declined on the web — that stance still stands on your other \
                     machines; this row delivers it here",
                    resolved.name
                ),
            );
        }
        if feed_carries && flowing {
            medit::push_note(
                &mut data,
                "the feed already delivers it — this row pins what this machine takes".to_owned(),
            );
        }
        shape_dest_receipt(
            ctx,
            &mut data,
            &resolved,
            &target,
            &dest_entries,
            set_line.as_deref(),
        );
        return Ok(finish_workspace(
            ctx, connect, data, &resolved, &target, true,
        ));
    }

    let Some(target) = medit::project_target(ctx)? else {
        return Err(ClientError::NoManifest);
    };
    medit::write_row(ctx, &mut data, &target, &resolved.canonical, &value)?;
    shape_dest_receipt(
        ctx,
        &mut data,
        &resolved,
        &target,
        &dest_entries,
        set_line.as_deref(),
    );
    Ok(finish_workspace(
        ctx, connect, data, &resolved, &target, false,
    ))
}

// ---------------------------------------------------------------------------------------------
// The SET-DELIVERED arm — `-a`/`--dest` on a bundle a channel or feed row already delivers here
// ---------------------------------------------------------------------------------------------

/// Converge a bundle the invoked scope's SET already delivers, and answer with what the asked
/// surface got — no row, no undo, nothing recorded.
///
/// The converge is the ORDINARY reconcile narrowed to this bundle and this scope: the same code
/// the sweep runs, at the reach the set's own row resolves (nothing here widens or narrows it).
/// The add's whole act is to make it happen now instead of at the next sweep.
///
/// ITS OUTCOME IS THE ANSWER'S, not a detail it may drop: a converge that could not run is this
/// add's own error, and one that ran and FAILED the bundle says so on the asked agent's line and
/// carries the sweep's warnings out with it. Reporting `not placed — it is not set up here` over
/// `nothing changed` named the wrong cause and closed on a claim the run had not established.
///
/// # Errors
/// Whatever the converge failed with.
#[allow(clippy::too_many_arguments)]
fn set_delivered_add(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    mut data: AddData,
    resolved: &Resolved,
    target: &EditTarget,
    global: bool,
    selection: &super::dest_select::Selection,
    set: String,
    mcp: bool,
) -> Result<AddRefOutcome, ClientError> {
    let kind = if mcp {
        BundleKind::Mcp
    } else {
        BundleKind::Skill
    };
    let sid = resolved.entry.as_deref().map(|e| e.skill_id.clone());
    // The folders standing BEFORE the converge — the one bit the dir plan does not report, and
    // what tells a copy this invocation created from one that was already there. The config
    // converge answers for itself.
    let before = if mcp {
        Vec::new()
    } else {
        placed_dirs(ctx, target, sid.as_deref())
    };
    let outcome = super::reconcile::manifest_update(
        ctx,
        connect,
        None,
        &super::reconcile::ManifestUpdateOpts {
            targets: vec![resolved.name.clone()],
            ack_notices: false,
            scope: if global {
                super::reconcile::UpdateScope::Machine
            } else {
                super::reconcile::UpdateScope::Here
            },
            ..Default::default()
        },
    )?;
    let failure = converge_failure(&outcome, sid.as_deref(), &resolved.name);
    let asked = asked_surfaces(
        selection,
        target.scope,
        kind,
        shared_root(ctx, target, kind),
    );
    let mut surfaces = if mcp {
        converged_entries(ctx, target, Some(&outcome), resolved)
    } else {
        converged_dirs(ctx, target, sid.as_deref(), &before)
    };
    for a in &asked {
        if surfaces.iter().any(|s| s.agent == a.key) {
            continue;
        }
        // A SHARED skills folder names no single agent, so a slug-keyed match found nothing for an
        // agent whose copy was sitting right there and reported it `not placed`. The asked agent's
        // own folder is what the converge answered about: a surface inside it IS that agent's copy,
        // and the line names the agent and the folder it reads.
        //
        // Placement is shared-dir-FIRST, so for an agent the shared folder COVERS that folder is
        // where its copy is and its own root holds nothing — the agent's own dest can only be
        // asked first and missed. Both folders are the same question, asked in the order placement
        // answers it.
        if let Some((folder, state)) = a.dest.iter().chain(a.shared.iter()).find_map(|dest| {
            surfaces
                .iter()
                .find(|s| reads_folder(s.target.as_deref(), dest))
                .map(|s| (dest.clone(), s.state))
        }) {
            surfaces
                .retain(|s| !(s.agent.is_empty() && reads_folder(s.target.as_deref(), &folder)));
            surfaces.push(topos_types::results::Surface {
                agent: a.key.clone(),
                target: Some(folder),
                state,
                note: None,
            });
            continue;
        }
        // Nothing of the bundle stands for this agent. WHY is the converge's answer where it has
        // one — a failed converge is the cause, and "it is not set up here" would name another.
        surfaces.push(topos_types::results::Surface {
            agent: a.key.clone(),
            target: None,
            state: topos_types::results::TargetOutcome::Withheld,
            note: Some(
                failure
                    .clone()
                    .unwrap_or_else(|| "it is not set up here".to_owned()),
            ),
        });
    }
    data.manifest = None;
    data.reference = Some(resolved.canonical.clone());
    data.source = Some(resolved.canonical.clone());
    data.dest = Vec::new();
    data.undo = Vec::new();
    // WHAT THE CONVERGE REGISTERED rides the answer that ran it: a sweep closes by registering a
    // newly detected agent's trigger, and this add IS that sweep — an agent's config file was
    // edited, and the receipt is where a person learns of it.
    data.triggers = outcome.data.triggers.clone();
    data.set_delivery = Some(topos_types::results::SetDelivery {
        set,
        scope: medit::receipt_scope(target),
        surfaces,
        asked: asked.into_iter().map(|a| a.key).collect(),
        failure,
    });
    Ok(AddRefOutcome::Applied {
        data: Box::new(data),
        messages: outcome.warnings,
    })
}

/// WHAT THE CONVERGE FAILED THIS BUNDLE WITH, in the sweep's own words — `None` on a converge that
/// carried it forward. The tally is keyed by identity, so the answer is the skill id's; the line is
/// the warning the sweep already wrote, minus the bundle name it leads with (the receipt has just
/// said it).
fn converge_failure(outcome: &super::PullOutcome, sid: Option<&str>, name: &str) -> Option<String> {
    let failed = outcome
        .failed_bundles
        .iter()
        .any(|(_, identity)| Some(identity.as_str()) == sid);
    if !failed {
        return None;
    }
    // THE LINE ABOUT THIS BUNDLE. The sweep leads every per-item failure with the bundle's name,
    // so that prefix is what picks it out of a run that faulted on more than one thing; anything
    // else fails back to the first failure the run wrote, which is still the run's own words.
    let lead = format!("{name}: ");
    let failures = || {
        outcome
            .warnings
            .iter()
            .filter(|m| m.kind == topos_types::MessageKind::Failure)
    };
    let text = failures()
        .find(|m| m.text.starts_with(&lead))
        .or_else(|| failures().next())
        .map(|m| m.text.clone())?;
    Some(text.strip_prefix(&lead).unwrap_or(&text).to_owned())
}

/// The CROSS-AGENT skills folder at this scope, in the spelling this receipt gives its folders —
/// the one copy every covered harness reads (`placement`'s shared-dir-first policy). `None` for a
/// config-placed bundle, which owns entries in files and has no shared folder at all, and for a
/// machine whose home is unknown.
fn shared_root(ctx: &Ctx<'_>, target: &EditTarget, kind: BundleKind) -> Option<String> {
    if kind.is_mcp() {
        return None;
    }
    let dir = match target.scope {
        ManifestScope::Global => {
            topos_harness::coverage::shared_skills_dir(&ctx.roots.as_ref()?.home)
        }
        ManifestScope::Project => target.dir.join(".agents/skills"),
    };
    Some(spell_in(
        spelled_dir(ctx, target).as_deref(),
        &super::inventory::pretty(ctx, &dir),
    ))
}

/// Whether a converged surface's target is the folder `dest` names, or a copy sitting inside it.
fn reads_folder(target: Option<&str>, dest: &str) -> bool {
    target.is_some_and(|t| {
        t == dest
            || std::path::Path::new(t)
                .parent()
                .is_some_and(|p| p.display().to_string() == dest)
    })
}

/// ONE surface the invocation asked for: the key it is REPORTED under (the harness slug where one
/// names the destination, the destination's own spelling where none does) and the folder or config
/// file it resolves to here — the same `dest_select` resolution the add validated the slug with.
struct AskedSurface {
    key: String,
    /// `None` for a slug with no destination at this scope (the selection would have refused it
    /// already) — such an ask can only be matched by key.
    dest: Option<String>,
    /// The SHARED skills folder, for an asked agent that folder covers — where placement puts its
    /// one copy instead of the agent's own root. `None` for every uncovered agent and for every
    /// config-placed bundle.
    shared: Option<String>,
}

/// The surfaces the invocation ASKED for, keyed the way [`converged_entries`] /
/// [`converged_dirs`] key theirs, each carrying the destination it resolves to — and, for an
/// agent the cross-agent folder covers, that folder too (`shared`).
fn asked_surfaces(
    selection: &super::dest_select::Selection,
    scope: ManifestScope,
    kind: BundleKind,
    shared: Option<String>,
) -> Vec<AskedSurface> {
    let slug_of = |entry: &str| match kind {
        BundleKind::Mcp => super::dest_select::slug_for_mcp_entry(entry, scope),
        BundleKind::Skill => super::dest_select::slug_for_skill_entry(entry, scope),
    };
    // The coverage question is the PLACEMENT engine's own (`topos_harness::coverage`), asked of
    // the slug the ask resolves to, so an asked agent and the folder its copy really landed in
    // cannot disagree.
    let shared_of = |slug: &str| {
        topos_harness::coverage::shared_dir_support(slug)
            .covered()
            .then(|| shared.clone())
            .flatten()
    };
    let asked = selection
        .agents
        .iter()
        .map(|slug| AskedSurface {
            key: slug.clone(),
            dest: match kind {
                BundleKind::Mcp => crate::manifest::dest::mcp_dest_spelling_here(slug, scope),
                BundleKind::Skill => crate::manifest::dest::skills_dest_spelling(slug, scope),
            },
            shared: shared_of(slug),
        })
        .chain(selection.dests.iter().map(|dest| {
            let slug = slug_of(dest);
            AskedSurface {
                shared: slug.as_deref().and_then(shared_of),
                key: slug.unwrap_or_else(|| dest.clone()),
                dest: Some(dest.clone()),
            }
        }));
    let mut out: Vec<AskedSurface> = Vec::new();
    for a in asked {
        if !out.iter().any(|prev| prev.key == a.key) {
            out.push(a);
        }
    }
    out
}

/// What the converge left in each agent's CONFIG, read off the reconcile's own row for this
/// bundle. A reload/sign-in note belongs to a surface this run wrote and is already implied by the
/// word beside it, so only the outcomes that need a REASON carry one.
fn converged_entries(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    outcome: Option<&super::PullOutcome>,
    resolved: &Resolved,
) -> Vec<topos_types::results::Surface> {
    let dir = spelled_dir(ctx, target);
    outcome
        .into_iter()
        .flat_map(|o| o.data.skills.iter())
        .filter(|r| {
            r.skill == resolved.name
                && r.workspace_id.as_deref() == Some(resolved.session.workspace_id.as_str())
        })
        .flat_map(|r| r.harnesses.iter())
        .map(|h| topos_types::results::Surface {
            agent: h.agent.clone(),
            target: h.file.as_deref().map(|f| spell_in(dir.as_deref(), f)),
            state: h.state,
            note: (!h.state.wrote()).then(|| h.note.clone()).flatten(),
        })
        .collect()
}

/// What the converge left in each agent's FOLDER: the bundle's placements now, split against the
/// ones that stood before this invocation ran. A folder several agents share names none of them,
/// and the folder is then the whole line.
fn converged_dirs(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    sid: Option<&str>,
    before: &[String],
) -> Vec<topos_types::results::Surface> {
    let dir = spelled_dir(ctx, target);
    placed_dirs(ctx, target, sid)
        .into_iter()
        .map(|placed| {
            let state = if before.contains(&placed) {
                topos_types::results::TargetOutcome::Current
            } else {
                topos_types::results::TargetOutcome::Created
            };
            let shown = spell_in(dir.as_deref(), &placed);
            let agent = std::path::Path::new(&shown)
                .parent()
                .map(|p| p.display().to_string())
                .and_then(|root| super::dest_select::slug_for_skill_entry(&root, target.scope))
                .unwrap_or_default();
            topos_types::results::Surface {
                agent,
                target: Some(shown),
                state,
                note: None,
            }
        })
        .collect()
}

/// One bundle's recorded placement folders in the scope's own store, `~`-abbreviated. Empty
/// wherever the scope has no store, no record, or an unreadable map — all three of which honestly
/// mean "nothing of it stands here".
fn placed_dirs(ctx: &Ctx<'_>, target: &EditTarget, sid: Option<&str>) -> Vec<String> {
    let Some(layout) = medit::mcp_scope_store(ctx, target) else {
        return Vec::new();
    };
    let Some(id) = sid.and_then(|s| crate::id::SkillId::parse(s).ok()) else {
        return Vec::new();
    };
    crate::doc::read_map(ctx.fs, &layout.published(&id).map)
        .ok()
        .flatten()
        .map(|map| {
            map.placements
                .iter()
                .map(|p| super::inventory::pretty(ctx, std::path::Path::new(p)))
                .collect()
        })
        .unwrap_or_default()
}

/// The folder a PROJECT receipt writes its paths against, in the SAME spelling those paths carry —
/// a checkout under the home is `~`-abbreviated on both sides or neither, or the prefix never
/// matches. `None` for the machine scope, whose paths are whole-machine facts and belong that way.
fn spelled_dir(ctx: &Ctx<'_>, target: &EditTarget) -> Option<String> {
    match target.scope {
        ManifestScope::Global => None,
        ManifestScope::Project => Some(super::inventory::pretty(ctx, &target.dir)),
    }
}

/// A path as this receipt spells it: relative to the project folder where it sits inside one — the
/// same spelling `--dest` takes back — and untouched wherever it does not (an env override that
/// moved a config file out of the checkout keeps its full path, because that is where it is).
fn spell_in(dir: Option<&str>, path: &str) -> String {
    dir.and_then(|d| path.strip_prefix(&format!("{d}/")))
        .unwrap_or(path)
        .to_owned()
}

/// The destination-receipt shaping a `-a`/`--dest` add carries: the row's dest entries (what the
/// receipt's `installed (…)` column speaks in), the workspace-QUALIFIED display name, and the
/// bare-name undo — `topos remove [-g] <name>` drops the whole row, which is a fresh row's exact
/// inverse. A replaced row keeps `write_row`'s own undo story (a wrong undo is worse than none).
///
/// `set_line` names the channel or feed line that ALREADY delivers this bundle here, on the one
/// add that writes a row beside one: the destinations are what that line cannot reach, and the row
/// carries the token so it costs the set nothing. Its receipt is the standing-row shape (what the
/// line gained, and that the default reach rides with it) with NO undo — deleting a row is not a
/// command, and no undo beats a wrong one — closing on the hand edit that puts the file back.
fn shape_dest_receipt(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    resolved: &Resolved,
    target: &medit::EditTarget,
    dest_entries: &[String],
    set_line: Option<&str>,
) {
    if dest_entries.is_empty() {
        return;
    }
    data.dest = dest_entries.to_vec();
    data.display = Some(medit::qualify_display(
        medit::default_host(ctx).as_deref(),
        &resolved.session.host,
        &resolved.session.workspace_name,
        &resolved.name,
    ));
    if let Some(set) = set_line {
        data.dest_change = Some(topos_types::results::DestChange {
            added: dest_entries.to_vec(),
            default_reach: true,
        });
        data.undo = Vec::new();
        medit::push_note(
            data,
            format!(
                "this line is new — delete it from the manifest to hand the bundle back to {set} \
                 alone."
            ),
        );
        return;
    }
    // A REMOVE undo names its target as the bare name — the receipt's promised inverse, byte for
    // byte, whether the inverse drops the whole row (a fresh add) or subtracts just the
    // destinations this add added (an extend). Only a remove: an `add`-shaped restore undo means
    // the row's PRIOR value, and a bare name would resolve to the record now standing instead.
    // And only where the bare name resolves to ONE removal: a channel line carrying the bundle
    // makes the bare spelling a chooser (the row plus the member rewrite both answer), so there
    // the undo keeps the full reference — longer, but runnable as printed.
    if data.undo.get(1).map(String::as_str) == Some("remove")
        && let Some(slot) = data.undo.iter().position(|t| *t == resolved.canonical)
        && !medit::channel_carries(
            ctx,
            target,
            &resolved.session.host,
            &resolved.session.workspace_name,
            &resolved.name,
        )
    {
        data.undo[slot] = resolved.name.clone();
    }
}

/// Deliver what the new row asks for, in the same invocation — the SAME reconcile the sweep runs,
/// narrowed to this name AND to the scope the row was just written in (`-g` delivers the machine
/// set even from inside a project; a project row delivers the project). Best-effort by contract:
/// the demand is durably recorded, so a transport hiccup just moves the byte landing to the next
/// sweep — but the DESTINATION receipt is not: `installed (…)` prints only when this reconcile's
/// own rows prove the bytes present, and anything less keeps the row-recorded receipt with the
/// next-sweep path disclosed. An `mcp` bundle's receipt then folds the typed server block from
/// what that delivery landed (best-effort too — see [`super::add_mcp::fold_workspace_mcp`]).
fn finish_workspace(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    mut data: AddData,
    resolved: &Resolved,
    target: &EditTarget,
    global: bool,
) -> AddRefOutcome {
    let targets = match resolved.entry {
        // A channel expands server-side into member names a targeted filter could not match.
        None => Vec::new(),
        Some(_) => vec![resolved.name.clone()],
    };
    let outcome = super::reconcile::manifest_update(
        ctx,
        connect,
        None,
        &super::reconcile::ManifestUpdateOpts {
            targets,
            ack_notices: false,
            scope: if global {
                super::reconcile::UpdateScope::Machine
            } else {
                super::reconcile::UpdateScope::Here
            },
            ..Default::default()
        },
    );
    // The destination receipt's honesty gate (see [`shape_dest_receipt`]): the `installed (…)`
    // claim stands only when the reconcile REPORTED this bundle's bytes present — a row for the
    // bundle (for a channel: any of its workspace's rows, and only when the channel's own
    // expansion succeeded). A transport drop, a plane failure, or a failed materialize clears
    // the destination shape instead, so the ordinary row-recorded receipt prints with the honest
    // note — the row is the durable demand either way, and the next sweep lands the copies.
    use topos_types::results::{PullAction, PullSkill};
    let out = outcome.as_ref().ok();
    // The registrations this invocation's own converge made — see [`set_delivered_add`]. The
    // breadth sweep the composition root runs writes this field too, on the arms that arm; a
    // reference add arms nothing itself, so nothing here is overwritten.
    if let Some(o) = out {
        data.triggers = o.data.triggers.clone();
    }
    // A row proves THIS reference's bytes only when the reconcile stamped it with the
    // session's workspace id — a same-named local or forge row reconciled in the same scope
    // says nothing about the workspace delivery.
    let owned = |r: &PullSkill| {
        r.workspace_id.as_deref() == Some(resolved.session.workspace_id.as_str())
            && match &resolved.entry {
                Some(_) => r.skill == resolved.name,
                // A channel's members carry their own names.
                None => true,
            }
    };
    // A REDUNDANT ROW is only the whole story when the delivery moved nothing either. This very
    // invocation can leave the file untouched and still land bytes — a merge onto a new current,
    // a first placement at a destination the row already named — and a receipt leading with
    // "nothing changed" would be answering about the file while the folders changed under it.
    if data.unchanged
        && out.is_some_and(|o| {
            o.data
                .skills
                .iter()
                .any(|r| owned(r) && super::reconcile::moved_bytes(r.action))
        })
    {
        medit::clear_unchanged(&mut data);
    }
    if !data.dest.is_empty() {
        // Bytes provably PRESENT: installed / current / fast-forwarded / refreshed — and the draft
        // outcomes (merged; a settled draft synced across folders), where the person's bytes stand
        // placed.
        let bytes_present = |a: PullAction| {
            matches!(
                a,
                PullAction::Installed
                    | PullAction::UpToDate
                    | PullAction::FastForwarded
                    | PullAction::Refreshed
                    | PullAction::Merged
                    | PullAction::DraftSynced
            )
        };
        // A channel whose expansion FAILED proved nothing, whatever else this sweep's scope
        // happened to reconcile for the same workspace (its feed's bundles ride along).
        let channel_failed = resolved.entry.is_none()
            && out.is_some_and(|o| {
                o.failed_channels
                    .contains(&(resolved.session.workspace_id.clone(), resolved.name.clone()))
            });
        let present = !channel_failed
            && out.is_some_and(|o| {
                o.data
                    .skills
                    .iter()
                    .any(|r| owned(r) && bytes_present(r.action))
            });
        if !present {
            data.dest = Vec::new();
            data.display = None;
            // The bundle's own row may report a STANDING state a bare re-update will not change
            // (conflicted / held) — then the note says what the row reports; the next-update
            // promise belongs to the run that produced no row at all.
            let standing = match &resolved.entry {
                Some(_) => out
                    .and_then(|o| o.data.skills.iter().find(|r| owned(r)))
                    .and_then(|r| match r.action {
                        PullAction::Conflicted => Some("conflicted"),
                        PullAction::Held => Some("held"),
                        _ => None,
                    }),
                None => None,
            };
            match standing {
                Some(state) => medit::push_note(
                    &mut data,
                    format!(
                        "the row is recorded — '{name}' reports {state} here; `topos list \
                         {name}` has the detail",
                        name = resolved.name
                    ),
                ),
                None => medit::push_note(
                    &mut data,
                    "the row is recorded, but its copies could not be placed this run — they \
                     land on the next `topos update`",
                ),
            }
        } else if !mcp_bundle(resolved) {
            // EVERY FOLDER THIS RUN WROTE. A row carrying the default-reach token places wherever
            // that reach goes, so a column naming only the folder the ask spelled reported one of
            // four. The row's own spelling stays in the FILE; the receipt says where the bytes are.
            // (A config-placed bundle owns ENTRIES inside files, not folders — its own receipt
            // half states them, and a file's parent directory is not a destination.)
            let dir = spelled_dir(ctx, target);
            let mut roots: Vec<String> = Vec::new();
            for placed in out
                .iter()
                .flat_map(|o| o.data.skills.iter())
                .filter(|r| owned(r) && bytes_present(r.action))
                .flat_map(|r| r.destinations.iter())
            {
                let Some(parent) = std::path::Path::new(placed).parent() else {
                    continue;
                };
                let spelled = spell_in(dir.as_deref(), &parent.display().to_string());
                if !roots.contains(&spelled) {
                    roots.push(spelled);
                }
            }
            if !roots.is_empty() {
                data.dest = roots;
            }
        }
    }
    // The kind was parsed (and an unknown one refused) before any row was written; a bundle
    // that got this far names a kind this build delivers.
    if mcp_bundle(resolved) {
        super::add_mcp::fold_workspace_mcp(ctx, target, global, &mut data);
    }
    // THE SECOND CHECKOUT, disclosed at the moment it is created (see [`machine_copy_beside_project`]).
    if global {
        data.machine_copy = machine_copy_beside_project(ctx, &data);
    }
    AddRefOutcome::applied(data)
}

/// Whether the catalog says this reference is a config-placed bundle. The kind was parsed (and an
/// unknown one refused) before any row was written, so a bundle that got this far names a kind
/// this build delivers.
fn mcp_bundle(resolved: &Resolved) -> bool {
    resolved
        .entry
        .as_deref()
        .and_then(|e| BundleKind::parse(&e.kind))
        .is_some_and(BundleKind::is_mcp)
}

/// The MACHINE folder a `-g` add just landed its own copy in, when a checkout at or above this
/// folder ALREADY delivers the same bundle — and `None` in every other case.
///
/// One bundle, two checkouts, is the state this line exists for: the project's copy keeps its own
/// version and its own draft, and so does the machine's, and nothing about the receipt's other
/// lines says a second copy was just created. Matched on the RECORD IDENTITY, never the name —
/// two scopes can track different bundles under one name, and a sentence about somebody else's
/// bundle is worse than silence.
///
/// The folder is the FIRST placement the delivery recorded: several are the same bundle in several
/// of this machine's agent dirs, and the first is the one the plan lands natively. A copy that did
/// not land names no folder, so there is no line — the row still stands and the next update places
/// it, which is what that receipt already says.
pub(super) fn machine_copy_beside_project(ctx: &Ctx<'_>, data: &AddData) -> Option<String> {
    let id = crate::id::SkillId::parse(data.skill_id.as_deref()?).ok()?;
    super::project_stores(ctx).into_iter().find(|layout| {
        super::store_lock(&super::pull::ctx_with_layout(ctx, layout), &id).is_some()
    })?;
    let map = crate::doc::read_map(ctx.fs, &ctx.layout.published(&id).map).ok()??;
    map.placements.first().cloned()
}

/// Resolve a workspace-shaped reference through the ONE session that serves it: the catalog
/// confirms a bundle exists (an honest transport failure is never folded into "no such skill"),
/// the channel index a channel.
fn resolve_workspace(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    shape: &KeyShape,
) -> Result<Resolved, ClientError> {
    let (host, workspace) = match shape {
        KeyShape::WorkspaceBundle {
            host, workspace, ..
        }
        | KeyShape::Channel {
            host, workspace, ..
        } => (host.clone(), workspace.clone()),
        _ => {
            return Err(ClientError::Corrupt(
                "resolve_workspace takes workspace references only".into(),
            ));
        }
    };
    let session = medit::live_sessions(ctx)?
        .into_iter()
        .find(|s| s.host == host && s.workspace_name == workspace)
        .ok_or_else(|| ClientError::SessionRequired {
            message: format!(
                "not logged into {host}/{workspace} — run `topos login {host}/{workspace}` first"
            ),
            address: format!("{host}/{workspace}"),
        })?;
    let transports = connect(&session);
    match shape {
        KeyShape::WorkspaceBundle { bundle, .. } => {
            let catalog = transports
                .directory
                .skills_index(&session.workspace_id)
                .map_err(|e| unread_catalog(&session, bundle, &e))?;
            let entry = catalog
                .skills
                .iter()
                .find(|e| &e.name == bundle)
                .cloned()
                .ok_or_else(|| {
                    ClientError::NotAvailable(format!(
                        "'{bundle}' is not in {}'s catalog, or is not visible with your current \
                         access — check the name; a teammate can confirm it",
                        session.workspace_name
                    ))
                })?;
            Ok(Resolved {
                canonical: shape.canonical(),
                name: entry.name.clone(),
                entry: Some(Box::new(entry)),
                session,
            })
        }
        KeyShape::Channel { channel, .. } => {
            let index = transports
                .directory
                .channels_index(&session.workspace_id)
                .map_err(|e| unread_catalog(&session, channel, &e))?;
            if !index.channels.iter().any(|c| &c.name == channel) {
                return Err(ClientError::NotAvailable(format!(
                    "there is no channel '{channel}' in {}, or it is not visible with your \
                     current access — check the name; a teammate can confirm it",
                    session.workspace_name
                )));
            }
            Ok(Resolved {
                canonical: shape.canonical(),
                name: channel.clone(),
                entry: None,
                session,
            })
        }
        _ => unreachable!("guarded above"),
    }
}

/// The answer a BARE add gives over a bundle this scope's set line already delivers and its
/// manifest does not record: the same lead the destination-carrying arm prints, closing on the
/// ordinary `nothing changed`.
///
/// Nothing converges here — no destination was asked about, so there is no surface whose missing
/// copy this add was called to place; the sweep owns that. Saying `<name> is already added
/// machine-wide (~/.topos/topos.toml)` named a file that holds no such row: what stands is the
/// set's line, and that is what the answer names.
pub(crate) fn set_delivered_answer(
    name: &str,
    reference: &str,
    set: String,
    global: bool,
) -> AddData {
    AddData {
        reference: Some(reference.to_owned()),
        source: Some(reference.to_owned()),
        set_delivery: Some(topos_types::results::SetDelivery {
            set,
            scope: if global {
                topos_types::results::ReceiptScope::Machine
            } else {
                topos_types::results::ReceiptScope::Project
            },
            surfaces: Vec::new(),
            asked: Vec::new(),
            failure: None,
        }),
        ..set_data(name)
    }
}

/// The honest transport-failure line (never an existence claim).
fn unread_catalog(session: &Session, name: &str, e: &ClientError) -> ClientError {
    ClientError::Plane(format!(
        "could not read {}'s catalog for '{name}': {}",
        session.workspace_name,
        crate::render::safe_message(e)
    ))
}

/// An `AddData` for something with no bytes of its own (a feed, a channel) — the receipt reads the
/// row, not a version.
fn set_data(name: &str) -> AddData {
    AddData {
        skill_id: None,
        name: name.to_owned(),
        version_id: None,
        bundle_digest: None,
        tracked: true,
        currency: None,
        triggers: Vec::new(),
        origin: None,
        source: None,
        manifest: None,
        scope: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
        published_match: None,
        note: None,
        mcp: None,
        dest: Vec::new(),
        dest_resolved: Vec::new(),
        dest_change: None,
        claim: None,
        unchanged: false,
        machine_copy: None,
        set_delivery: None,
        display: None,
    }
}

// ---------------------------------------------------------------------------------------------
// The GIT-FORGE arm — `add owner/repo[/skill]`
// ---------------------------------------------------------------------------------------------

fn add_forge(
    ctx: &Ctx<'_>,
    git: Option<&dyn GitTarballSource>,
    parsed: &InputRef,
    raw: &str,
    global: bool,
    yes: bool,
    selection: &super::dest_select::Selection,
) -> Result<AddRefOutcome, ClientError> {
    let (host, owner, repo, want_skill) = match &parsed.shape {
        KeyShape::RepoSet { host, owner, repo } => {
            (host.clone(), owner.clone(), repo.clone(), None)
        }
        KeyShape::RepoSkill {
            host,
            owner,
            repo,
            skill,
        } => (
            host.clone(),
            owner.clone(),
            repo.clone(),
            Some(skill.clone()),
        ),
        _ => unreachable!("guarded by the caller"),
    };
    if host != "github.com" {
        return Err(ClientError::InvalidArgument(format!(
            "`{host}` sources are not fetchable yet — github.com is"
        )));
    }
    // The `-g` target always resolves; a project one refuses when no `topos.toml` covers this
    // folder — BEFORE the fetch, so an import never lands bytes no file will ask for.
    let Some(target) = medit::edit_target(ctx, global)? else {
        return Err(ClientError::NoManifest);
    };
    let Some(git) = git else {
        return Err(ClientError::InvalidArgument(
            "a git source is needed to add from a repository — this run has none".into(),
        ));
    };
    let Some(roots) = discovery_roots(ctx, &target) else {
        return Err(ClientError::InvalidArgument(
            "cannot import from a repository without $HOME set (needed to resolve the agent \
             skills dir)"
                .into(),
        ));
    };

    // The ref the FETCH uses: an explicit `@<pin>` or a pasted `/tree/<ref>/` segment; else the
    // repo's default branch.
    let git_ref = parsed.pin.clone().or_else(|| parsed.ref_hint.clone());
    let spec = RemoteSpec {
        host: GitHost::GitHub,
        owner: owner.clone(),
        repo: repo.clone(),
        git_ref: git_ref.clone(),
        subdir: parsed.subdir_hint.clone(),
    };
    let source_label = format!("{host}/{owner}/{repo}");
    // The SCOPE's own store: a project row's import lands in (and is trusted from) the
    // project's `.topos/` store, a global row in the home store — the same routing the
    // reconcile's forge arms read.
    let store_layout = match target.scope {
        ManifestScope::Project => crate::sidecar::project_store_layout(&target.dir),
        ManifestScope::Global => ctx.layout.clone(),
    };
    let sctx = super::pull::ctx_with_layout(ctx, &store_layout);

    // ONE fetch serves both the describe and the apply (read-only either way).
    let targz = git.fetch(&spec)?;
    let extracted = crate::git_source::extract_tree(&targz)?;
    let discovered = extracted.skill_names(parsed.subdir_hint.as_deref(), &repo);
    if discovered.is_empty() {
        return Err(ClientError::NoSkillInSource { src: source_label });
    }

    // The row's KEY: the repo (every skill it holds) or ONE discovered skill by its LEAF
    // directory name — a frontmatter name is accepted as input when it uniquely names one.
    //
    // A pasted SUBTREE URL (`/tree/<ref>/<path>`) is the third case: the grammar canonicalizes it
    // to the repo and hands the literal path back as a hint, but the path names ONE skill, not the
    // repo — and a repo-SET row cannot legally carry `subdir` or `version`, so recording it as one
    // would refuse the write after the origin had already been trusted. It becomes the 4-segment
    // skill row (the leaf name selects) whose fields carry the literal path and the pin, both
    // legal there. A path holding SEVERAL skills names none of them: it refuses, with the names.
    let (reference, members) = match (&want_skill, parsed.subdir_hint.as_deref()) {
        (Some(want), _) => {
            let leaf = canonical_leaf(
                &extracted,
                &discovered,
                want,
                parsed.subdir_hint.as_deref(),
                &repo,
                &source_label,
            )?;
            (format!("{source_label}/{leaf}"), vec![leaf])
        }
        (None, Some(sub)) => match discovered.as_slice() {
            [one] => (format!("{source_label}/{one}"), vec![one.clone()]),
            _ => {
                return Err(ClientError::AmbiguousSkillInRepo {
                    src: format!("{source_label}/{sub}"),
                    skills: discovered.clone(),
                });
            }
        },
        (None, None) => (source_label.clone(), discovered.clone()),
    };

    // The row's VALUE: `"*"` tracks the default branch; a hex ref pins straight through; a NAMED
    // ref is a deliberate freeze, so the RESOLVED commit is what the row records. A `-a`/`--dest`
    // selection rides the row as its `dest` field — except on a PINNED whole-repo row, whose
    // shape cannot legally carry both (the pin wins; the withheld selection is disclosed below).
    let dest_entries = if selection.is_empty() {
        Vec::new()
    } else {
        selection.skill_entries(target.scope)?
    };
    let pin = match (&parsed.pin, &parsed.ref_hint) {
        (Some(p), _) => Some(p.clone()),
        (None, Some(r)) if medit::is_commit(r) => Some(r.clone()),
        (None, Some(_)) => extracted.commit.clone().filter(|c| medit::is_commit(c)),
        (None, None) => None,
    };
    let shape = crate::manifest::keys::classify_key(&reference)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;
    let version_legal = crate::manifest::document::legal_fields(&shape).contains(&"version");
    let dest_withheld = !dest_entries.is_empty() && pin.is_some() && !version_legal;
    let row_dest = (!dest_entries.is_empty() && !dest_withheld).then(|| dest_entries.clone());
    let value = match (&pin, &parsed.subdir_hint, &row_dest) {
        (_, Some(sub), dest) => EntryValue::Fields(EntryFields {
            version: pin.clone(),
            subdir: Some(sub.clone()),
            dest: dest.clone(),
            ..EntryFields::default()
        }),
        (pin, None, Some(dest)) => EntryValue::Fields(EntryFields {
            version: pin.clone().filter(|_| version_legal),
            dest: Some(dest.clone()),
            ..EntryFields::default()
        }),
        (Some(p), None, None) => EntryValue::Pin(p.clone()),
        (None, None, None) => EntryValue::Star,
    };
    // PROVE THE ROW BEFORE ANYTHING LANDS: a value the file would refuse is caught here, while
    // nothing has happened yet.
    crate::manifest::document::check_row(&reference, target.scope, &value)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;

    // DESCRIBE FIRST, unconditionally: an interactive add of a git source says what the source
    // holds and where it would land, and installs only under `--yes`. Re-adding a source already
    // tracked here describes again rather than skipping ahead — the answer is what the person
    // came for, and this verb is the one place a person is present to read it.
    if !yes {
        let mut yes_argv = vec!["topos".to_owned(), "add".to_owned(), raw.to_owned()];
        yes_argv.extend(selection.argv_tail());
        if global {
            yes_argv.push("-g".to_owned());
        }
        yes_argv.push("--yes".to_owned());
        // The row may ALREADY be recorded (a cloned manifest; a prior add whose installs all
        // failed) — said out loud, with what the consent covers: the row is standing demand,
        // and applying is what fetches and installs the bytes it names.
        let row_exists = medit::read_text(ctx, &target.path)
            .ok()
            .flatten()
            .and_then(|text| {
                crate::manifest::document::ManifestEditor::open(&text, target.scope).ok()
            })
            .is_some_and(|editor| editor.row(&reference).is_some());
        let note = row_exists.then(|| {
            format!(
                "{} already records this row — it is demand, not consent; applying is what \
                 fetches and installs the skills it names",
                target.path.display()
            )
        });
        return Ok(AddRefOutcome::Described {
            data: Box::new(AddDescribeData {
                source: source_label,
                members,
                manifest: target.path.display().to_string(),
                reference,
                value: value_spelling(&value),
                note,
                // A git import has no server endpoint to disclose.
            }),
            yes_argv,
        });
    }

    // ---- APPLY ----
    // The fetch above SUCCEEDED, which is newer evidence than any refusal on file: a source the
    // automatic update had written off as gone has just handed over its bytes. Forget the verdict,
    // or the row this add is about to write would never be checked again.
    crate::forge_check::forget(ctx.fs, &ctx.layout, &source_label);
    // The DEMAND lands next: with the row durably recorded, a member-install failure part-way
    // leaves a CONVERGENT state — the manifest asks for exactly what was accepted, and the next
    // update (or a re-run of this add) finishes the landing — instead of installed members no
    // manifest row asks for.
    let mut row_receipt = set_data(members.first().map_or(&repo, |m| m));
    medit::write_row(ctx, &mut row_receipt, &target, &reference, &value)?;
    // A project install writes through the project's own self-ignoring `.topos/` store; mint its
    // shell before the first member lands.
    if target.scope == ManifestScope::Project {
        crate::sidecar::ensure_project_store(ctx.fs, &target.dir)?;
    }
    let mut landed: Vec<AddData> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    // One landing per (member × destination slot): a `-a` slug lands through the registry's own
    // root resolution, a literal `--dest` through its resolved root; no selection = the one
    // default slot (exactly the old behavior).
    let slots = selection_slots(selection, &target, &roots);
    for member in &members {
        for (slot_agent, slot_root) in &slots {
            let opts = super::AddRemoteOpts {
                skill: Some(member.clone()),
                harness: slot_agent.clone(),
                dest_root: slot_root.clone(),
                global,
            };
            match super::add::add_remote_fetched(&sctx, &targz, &spec, &roots, &opts) {
                Ok(d) => landed.push(d),
                // Already here: the row still records the demand; the receipt names what was
                // skipped.
                Err(
                    ClientError::AlreadyTracked { .. } | ClientError::AlreadyTrackedName { .. },
                ) => {
                    if !skipped.contains(member) {
                        skipped.push(member.clone());
                    }
                }
                // A per-member failure never unwinds the batch: the receipt names the partial
                // landing, and the recorded row is what converges it.
                Err(e) => failed.push((member.clone(), crate::render::safe_message(&e))),
            }
        }
    }
    let mut data = match landed.first() {
        Some(d) => d.clone(),
        None => set_data(members.first().map_or(&repo, |m| m)),
    };
    data.manifest = row_receipt.manifest.clone();
    data.reference = row_receipt.reference.clone();
    data.undo = row_receipt.undo.clone();
    if let Some(note) = row_receipt.note {
        medit::push_note(&mut data, note);
    }
    if members.len() > 1 {
        medit::push_note(
            &mut data,
            format!(
                "one line, {} skills: {} — new skills in the repo arrive with `topos update`",
                members.len(),
                members.join(", ")
            ),
        );
    }
    if !skipped.is_empty() {
        medit::push_note(
            &mut data,
            format!("already on this machine: {}", skipped.join(", ")),
        );
    }
    if !failed.is_empty() {
        let names: Vec<String> = failed.iter().map(|(n, e)| format!("{n} ({e})")).collect();
        // The origin's trust was granted above, so the ordinary explicit update converges the
        // rest — even when NO member landed this run.
        medit::push_note(
            &mut data,
            format!(
                "did not land: {} — the row records the demand; `topos update` completes the \
                 landing",
                names.join("; ")
            ),
        );
    }
    if pin.is_some() {
        medit::push_note(
            &mut data,
            "pinned to that commit — it stays there until you change the line".to_owned(),
        );
    }
    if dest_withheld {
        medit::push_note(
            &mut data,
            format!(
                "the `-a`/`--dest` selection is not recorded on `{reference}` — a whole-repo row \
                 cannot carry both a commit pin and a dest list; name the skill \
                 (`{reference}/<skill>`) to keep it"
            ),
        );
    } else if !dest_entries.is_empty() {
        data.dest = dest_entries.clone();
    }
    Ok(AddRefOutcome::applied(data))
}

/// The landing slots a `-a`/`--dest` selection fans each member over: one per `-a` slug (the
/// registry's own root resolution inside [`super::add::add_remote_fetched`]) and one per literal
/// `--dest` root (resolved here — `~/`/absolute in the machine scope, project-relative against
/// the checkout, where the import's containment rail re-proves it). Empty selection = the one
/// default slot.
fn selection_slots(
    selection: &super::dest_select::Selection,
    target: &EditTarget,
    roots: &super::DiscoveryRoots,
) -> Vec<(Option<String>, Option<std::path::PathBuf>)> {
    if selection.is_empty() {
        return vec![(None, None)];
    }
    let mut out: Vec<(Option<String>, Option<std::path::PathBuf>)> = Vec::new();
    for slug in &selection.agents {
        out.push((Some(slug.clone()), None));
    }
    for entry in &selection.dests {
        let root = match target.scope {
            ManifestScope::Project => target.dir.join(entry.trim_start_matches("./")),
            ManifestScope::Global => match entry.strip_prefix("~/") {
                Some(rest) => roots.home.join(rest),
                None => std::path::PathBuf::from(entry),
            },
        };
        out.push((None, Some(root)));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The SELECTOR import — `add owner/repo -s <skill>… -a <agent>…` (and their `*` fan-outs)
// ---------------------------------------------------------------------------------------------

/// What a selector-driven import did — or, for a git source, what it WOULD do. Same
/// two-phase shape as [`AddRefOutcome`]; the applied arm is a LIST because the selectors fan out
/// over (skill × harness).
pub(crate) enum AddManyOutcome {
    Applied(Vec<AddData>),
    Described {
        data: Box<AddDescribeData>,
        yes_argv: Vec<String>,
    },
}

/// `topos add <owner/repo> [-s <skill>…] [-a <agent>…] [-g] [--yes]` — the SELECTOR arm.
///
/// The selectors change WHICH members land and WHERE; they change nothing about what a person gets
/// to read first. So this arm runs the SAME describe as the bare reference arm — source, members,
/// what lands where — and applies only under `--yes`. And it runs under
/// the SCOPE's own store — a project-destined import lands in the checkout's `.topos/`, which is
/// the store the project reconcile converges, so a selector import is no longer a set of bytes no
/// scope's `update` can ever see again.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a non-remote source, no resolvable directory, or `-a '*'`
/// matching no detected harness; [`ClientError::NoSkillInSource`]; the remote-import family; a
/// filesystem failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_forge_selected(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: &dyn GitTarballSource,
    source: &str,
    skills: &[String],
    agents: &[String],
    dests: &[String],
    global: bool,
    yes: bool,
) -> Result<AddManyOutcome, ClientError> {
    let crate::source::SourceSpec::Remote(spec) = crate::source::classify(source) else {
        return Err(ClientError::InvalidArgument(
            "`-s`/`-a` selectors (and `*`) apply to a REMOTE import (`owner/repo` or a \
             github.com URL) — a local path or name adopts a single skill"
                .into(),
        ));
    };
    // Same scope rule as every other arm: `-g` always resolves, a project one refuses when no
    // `topos.toml` covers this folder — before the fetch, before any member lands.
    let Some(target) = medit::edit_target(ctx, global)? else {
        return Err(ClientError::NoManifest);
    };
    let Some(roots) = discovery_roots(ctx, &target) else {
        return Err(ClientError::InvalidArgument(
            "cannot import from a repository without $HOME set (needed to resolve the agent \
             skills dir)"
                .into(),
        ));
    };
    // The SCOPE's own store — the same routing the bare reference arm and the reconcile's forge
    // arms use. Without it a project-destined import writes its engine state into the HOME store,
    // and the project reconcile (which reads the checkout's store) can never converge it.
    let store_layout = match target.scope {
        ManifestScope::Project => crate::sidecar::project_store_layout(&target.dir),
        ManifestScope::Global => ctx.layout.clone(),
    };
    let sctx = super::pull::ctx_with_layout(ctx, &store_layout);
    let origin_label = spec.origin();

    // ONE fetch serves the describe and the apply alike (read-only either way) — and serves every
    // (skill × harness) combination, where the old per-combination `add_remote` re-fetched.
    let targz = git.fetch(&spec)?;
    let extracted = crate::git_source::extract_tree(&targz)?;
    let discovered = extracted.skill_names(spec.subdir.as_deref(), &spec.repo);
    if discovered.is_empty() {
        return Err(ClientError::NoSkillInSource { src: spec.label() });
    }
    // `-s '*'` → every skill the repo holds; explicit names loop as-is; no `-s` at all keeps the
    // single-select behavior (a lone skill self-selects; several is a typed refusal downstream).
    let skill_opts: Vec<Option<String>> = if skills.iter().any(|s| s == "*") {
        discovered.iter().cloned().map(Some).collect()
    } else if skills.is_empty() {
        vec![None]
    } else {
        skills.iter().cloned().map(Some).collect()
    };
    // `-a '*'` → every harness DETECTED here with a skills dir at the chosen scope. Named slugs
    // (and the literal `--dest` folders) validate UP FRONT — an unknown agent refuses before a
    // byte moves, with the registry's own list.
    let named: Vec<String> = agents.iter().filter(|a| *a != "*").cloned().collect();
    super::dest_select::Selection::new(&named, dests).skill_entries(target.scope)?;
    let literal_entries =
        super::dest_select::Selection::new(&[], dests).skill_entries(target.scope)?;
    let agent_opts: Vec<Option<String>> = if agents.iter().any(|a| a == "*") {
        let detected = detected_harness_slugs(&roots, global);
        if detected.is_empty() {
            return Err(ClientError::InvalidArgument(
                "`-a '*'` found no harness on this machine to place into at the chosen scope — \
                 name one with `-a <slug>` (or drop `--global` for project scope)"
                    .into(),
            ));
        }
        detected.into_iter().map(Some).collect()
    } else if agents.is_empty() {
        vec![None]
    } else {
        agents.iter().cloned().map(Some).collect()
    };
    // The DESTINATION slots each selected member lands through: the agent slots (the default one
    // when nothing narrows the agents and no literal folder does either), plus one per literal
    // `--dest` root.
    let mut slot_opts: Vec<(Option<String>, Option<std::path::PathBuf>)> = Vec::new();
    if agents.is_empty() && !dests.is_empty() {
        // Only literal folders were named — no default-agent slot rides along.
    } else {
        slot_opts.extend(agent_opts.iter().cloned().map(|a| (a, None)));
    }
    for entry in dests {
        let root = match target.scope {
            ManifestScope::Project => target.dir.join(entry.trim_start_matches("./")),
            ManifestScope::Global => match entry.strip_prefix("~/") {
                Some(rest) => roots.home.join(rest),
                None => std::path::PathBuf::from(entry),
            },
        };
        slot_opts.push((None, Some(root)));
    }

    // DESCRIBE FIRST — identical to the bare reference arm's, selectors and all: what the source
    // holds, and the exact file the rows land in.
    if !yes {
        let members: Vec<String> = skill_opts
            .iter()
            .map(|s| s.clone().unwrap_or_else(|| discovered.join(", ")))
            .collect();
        let mut yes_argv = vec!["topos".to_owned(), "add".to_owned(), source.to_owned()];
        for s in skills {
            yes_argv.push("-s".to_owned());
            yes_argv.push(s.clone());
        }
        for a in agents {
            yes_argv.push("-a".to_owned());
            yes_argv.push(a.clone());
        }
        for d in dests {
            yes_argv.push("--dest".to_owned());
            yes_argv.push(d.clone());
        }
        if global {
            yes_argv.push("-g".to_owned());
        }
        yes_argv.push("--yes".to_owned());
        let mut placed: Vec<String> = Vec::new();
        if !agents.is_empty() || dests.is_empty() {
            placed.extend(agent_opts.iter().map(|a| {
                a.clone()
                    .unwrap_or_else(|| "the default agent dir".to_owned())
            }));
        }
        placed.extend(literal_entries.iter().cloned());
        return Ok(AddManyOutcome::Described {
            data: Box::new(AddDescribeData {
                source: origin_label,
                members,
                manifest: target.path.display().to_string(),
                reference: spec.label(),
                value: "*".to_owned(),
                note: Some(format!("lands into: {}", placed.join(", "))),
                // A git import has no server endpoint to disclose.
            }),
            yes_argv,
        });
    }

    // ---- APPLY ---- the fetch above succeeded, so any standing refusal against this source is
    // out of date (see the bare reference arm); then the rows, then the bytes (a partial landing
    // therefore leaves demand the next update converges).
    crate::forge_check::forget(ctx.fs, &ctx.layout, &origin_label);
    if target.scope == ManifestScope::Project {
        crate::sidecar::ensure_project_store(ctx.fs, &target.dir)?;
    }
    // The SELECTION the rows carry: the slugs this invocation aimed at (a `*` fan-out is the
    // detected set, spelled out — a row must say what it means, not re-resolve `*` later),
    // recorded as their scope-correct dest dirs, plus the literal `--dest` folders.
    let chosen: Vec<String> = agent_opts.iter().flatten().cloned().collect();
    let mut dest = medit::dest_for_selected_agents(&chosen, target.scope);
    if agents.is_empty() && !dests.is_empty() {
        // Only literal folders were named — the row freezes to exactly those.
        dest.clear();
    }
    for entry in &literal_entries {
        if !dest.contains(entry) {
            dest.push(entry.clone());
        }
    }
    let mut out = Vec::with_capacity(skill_opts.len() * slot_opts.len());
    for s in &skill_opts {
        for (slot_agent, slot_root) in &slot_opts {
            let opts = super::AddRemoteOpts {
                skill: s.clone(),
                harness: slot_agent.clone(),
                dest_root: slot_root.clone(),
                global,
            };
            let mut data = super::add::add_remote_fetched(&sctx, &targz, &spec, &roots, &opts)?;
            // Each landed (skill × destination) records its manifest line like the single-select
            // path — carrying the WHOLE selection (so the second combination of one skill
            // rewrites the identical row rather than narrowing it) and the same dedup courtesy,
            // judged against ITS resolved subdir.
            medit::note_added_remote(ctx, &mut data, &target, &dest)?;
            if !dest.is_empty() {
                data.dest = dest.clone();
            }
            let imported_subdir = data.origin.as_ref().and_then(|o| o.subdir.clone());
            data.governed_copy = super::add::governed_copy_suggestion(
                ctx,
                connect,
                &spec,
                imported_subdir.as_deref(),
            );
            out.push(data);
        }
    }
    Ok(AddManyOutcome::Applied(out))
}

/// The harness slugs DETECTED on this machine (the same registry discovery `list` uses) that have a
/// skills directory at the chosen scope — the fan-out set for `add -a '*'`. Deduped + sorted; a
/// harness with no writable dir at this scope is dropped (so the loop never fails on it).
fn detected_harness_slugs(roots: &super::DiscoveryRoots, global: bool) -> Vec<String> {
    let scope = if global {
        topos_harness::registry::SkillScope::User
    } else {
        topos_harness::registry::SkillScope::Project
    };
    let mut slugs: Vec<String> =
        topos_harness::registry::discover_all(&roots.home, roots.cwd.as_deref())
            .into_iter()
            .map(|d| d.harness_slug)
            .filter(|slug| {
                topos_harness::registry::skills_root(slug, scope, &roots.home, roots.cwd.as_deref())
                    .is_some()
            })
            .collect();
    slugs.sort();
    slugs.dedup();
    slugs
}

/// The `value` a describe prints (the same spelling the row will carry).
fn value_spelling(value: &EntryValue) -> String {
    match value {
        EntryValue::Star => "*".to_owned(),
        EntryValue::Off => "off".to_owned(),
        EntryValue::Pin(p) => p.clone(),
        EntryValue::Fields(f) => match (&f.version, &f.subdir) {
            (Some(v), Some(s)) => format!("{{ version = \"{v}\", subdir = \"{s}\" }}"),
            (None, Some(s)) => format!("{{ subdir = \"{s}\" }}"),
            (Some(v), None) => v.clone(),
            (None, None) => "*".to_owned(),
        },
    }
}

/// Resolve the fourth key segment to a DISCOVERED skill directory's leaf name: an exact leaf wins;
/// otherwise a `SKILL.md` frontmatter name that uniquely names one is accepted and canonicalized.
/// A leaf that maps to two directories refuses with both, pointing at the `subdir` field.
fn canonical_leaf(
    extracted: &crate::git_source::ExtractedRepo,
    discovered: &[String],
    want: &str,
    subdir: Option<&str>,
    repo: &str,
    source_label: &str,
) -> Result<String, ClientError> {
    if discovered.iter().any(|d| d == want) {
        // The name resolves — but it must resolve to exactly ONE directory.
        return match extracted.select(subdir, Some(want), repo, source_label) {
            Ok(_) => Ok(want.to_owned()),
            Err(ClientError::DuplicateSkillName { paths, .. }) => {
                Err(ClientError::InvalidArgument(format!(
                    "'{want}' names {} directories in {source_label} ({}) — a four-segment key \
                     takes a skill's leaf directory name, so name the repo alone and let the \
                     `subdir = \"…\"` field pick the one you mean",
                    paths.len(),
                    paths.join(", ")
                )))
            }
            Err(e) => Err(e),
        };
    }
    // A frontmatter name: accepted only when exactly one discovered skill declares it.
    let mut hits: Vec<String> = Vec::new();
    for candidate in discovered {
        let Ok(selected) = extracted.select(subdir, Some(candidate), repo, source_label) else {
            continue;
        };
        let declared = selected
            .files
            .iter()
            .find(|f| f.path == "SKILL.md")
            .and_then(|f| crate::scan::frontmatter_name(&f.bytes));
        if declared.as_deref() == Some(want) {
            hits.push(candidate.clone());
        }
    }
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(ClientError::SkillNotInRepo {
            skill: want.to_owned(),
            src: source_label.to_owned(),
            available: discovered.to_vec(),
        }),
        many => Err(ClientError::InvalidArgument(format!(
            "'{want}' is declared by {} skills in {source_label} ({}) — name one by its directory",
            many.len(),
            many.join(", ")
        ))),
    }
}

/// The discovery roots a forge install resolves its destination against: a project row lands in
/// the demanding checkout, a global row in the machine's own dirs.
fn discovery_roots(ctx: &Ctx<'_>, target: &EditTarget) -> Option<super::DiscoveryRoots> {
    let roots = ctx.roots.as_ref()?;
    let cwd = match target.scope {
        ManifestScope::Project => Some(target.dir.clone()),
        ManifestScope::Global => roots.cwd.clone(),
    };
    Some(super::DiscoveryRoots {
        home: roots.home.clone(),
        cwd,
    })
}

// ---------------------------------------------------------------------------------------------
// The session write lane (shared by the governance verbs) + the governance rewrite
// ---------------------------------------------------------------------------------------------

/// A resolved SESSION write lane — which workspace a governance/write verb acts in, with the
/// transports built under that session's OWN credential.
pub(crate) struct WriteLane {
    pub host: String,
    pub workspace_name: String,
    pub workspace_id: String,
    pub transports: SessionTransports,
}

/// Resolve the session a write verb signs in: `None` when this installation has no session at all
/// (the caller keeps its own path). A DELIVERED bundle's own workspace wins (the pointer scope
/// must be the bundle's, never an ambient guess); else the one live session, or the
/// `--workspace`-selected one (several without a name refuse typed).
///
/// # Errors
/// [`ClientError::SessionRequired`] / [`ClientError::WorkspaceSelection`] from the selection.
pub(crate) fn resolve_session_lane(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    explicit: Option<&str>,
    skill_id: Option<&str>,
) -> Result<Option<WriteLane>, ClientError> {
    let all = crate::sessions::read_sessions(ctx.fs, &ctx.layout)?;
    if all.sessions.is_empty() {
        return Ok(None);
    }
    let delivered_ws = skill_id.and_then(|sid| {
        use crate::plane::FollowSource;
        super::reconcile::CacheFollow::load(ctx.fs, &ctx.layout)
            .followed()
            .into_iter()
            .find(|(id, _)| id == sid)
            .map(|(_, fc)| fc.workspace_id)
    });
    let session = match delivered_ws {
        // LIVE sessions only — an ended row must refuse toward `login`, never lend its dead
        // credential to a write.
        Some(ws) => all
            .sessions
            .iter()
            .find(|s| s.workspace_id == ws && s.status != crate::sessions::SESSION_ENDED)
            .cloned()
            .ok_or_else(|| ClientError::SessionRequired {
                address: "<workspace-address>".to_owned(),
                message: format!(
                    "this bundle's workspace has no live session on this installation — run \
                     `topos login <workspace-address>` first (workspace id {ws})"
                ),
            })?,
        None => all.resolve_target(explicit)?.clone(),
    };
    let transports = connect(&session);
    Ok(Some(WriteLane {
        host: session.host.clone(),
        workspace_name: session.workspace_name.clone(),
        workspace_id: session.workspace_id.clone(),
        transports,
    }))
}

/// The facts a governance transfer's receipt names.
pub(crate) struct GovernedRewrite {
    pub manifest: String,
    pub canonical: String,
    pub from: String,
}

/// What [`rewrite_to_governed`] concluded.
pub(crate) enum GovernedOutcome {
    /// The path line was rewritten to the canonical workspace reference.
    Rewritten(GovernedRewrite),
    /// A path line was found, but was GONE by the time the manifest's writer lock was held — a
    /// concurrent `topos remove` completed in the window. NOTHING was written: a completed
    /// removal is never silently undone (the publish stands catalog-side; the receipt disclosed
    /// it with this manifest path).
    RowRemoved { manifest: String },
    /// No manifest references the bundle by path (an already-governed republish).
    None,
}

/// The READ-ONLY probe behind [`rewrite_to_governed`] (the describe predicts with it): the nearest
/// manifest whose LOCAL-PATH row resolves to one of the bundle's placement dirs, with the row's
/// spelling. Mutates nothing.
///
/// # Errors
/// A manifest read/parse failure.
pub(crate) fn find_path_line(
    ctx: &Ctx<'_>,
    skill_dirs: &[std::path::PathBuf],
) -> Result<Option<(std::path::PathBuf, crate::manifest::scopes::PlanRow)>, ClientError> {
    // Canonicalize the placement dirs once (macOS `/var` → `/private/var`); a dir that no longer
    // exists keeps its lexical form.
    let dirs: Vec<std::path::PathBuf> = skill_dirs
        .iter()
        .map(|d| d.canonicalize().unwrap_or_else(|_| d.clone()))
        .collect();
    for (path, _scope, rows) in medit::local_rows(ctx)? {
        let manifest_dir = path.parent().map(std::path::Path::to_path_buf);
        let hit = rows.iter().find(|row| {
            let KeyShape::LocalPath { raw } = &row.shape else {
                return false;
            };
            let literal = std::path::Path::new(raw);
            let resolved = if literal.is_absolute() {
                literal.to_path_buf()
            } else {
                match &manifest_dir {
                    Some(dir) => dir.join(raw.trim_start_matches("./")),
                    None => return false,
                }
            };
            let resolved = resolved.canonicalize().unwrap_or(resolved);
            dirs.contains(&resolved)
        });
        let Some(row) = hit else { continue };
        return Ok(Some((path, row.clone())));
    }
    Ok(None)
}

/// The BUNDLE KIND the manifest records for a local bundle: the `kind` field on the nearest
/// local-path row that resolves to one of `skill_dirs`. This is the only place a local folder's
/// kind is written down — a path has no catalog to ask — so `publish` reads it here to decide what
/// it is shipping. `None` = no such row, or a row that names no kind (an ordinary skill).
///
/// # Errors
/// A manifest read/parse failure.
pub(crate) fn path_row_kind(
    ctx: &Ctx<'_>,
    skill_dirs: &[std::path::PathBuf],
) -> Result<Option<String>, ClientError> {
    let dirs: Vec<std::path::PathBuf> = skill_dirs
        .iter()
        .map(|d| d.canonicalize().unwrap_or_else(|_| d.clone()))
        .collect();
    for (path, _scope, rows) in medit::local_rows(ctx)? {
        let manifest_dir = path.parent().map(std::path::Path::to_path_buf);
        for row in &rows {
            let KeyShape::LocalPath { raw } = &row.shape else {
                continue;
            };
            let literal = std::path::Path::new(raw);
            let resolved = if literal.is_absolute() {
                literal.to_path_buf()
            } else {
                match &manifest_dir {
                    Some(dir) => dir.join(raw.trim_start_matches("./")),
                    None => continue,
                }
            };
            let resolved = resolved.canonicalize().unwrap_or(resolved);
            if dirs.contains(&resolved)
                && let Some(kind) = row.fields().kind.clone()
            {
                return Ok(Some(kind));
            }
        }
    }
    Ok(None)
}

/// The governance-transfer rewrite a LANDED publish performs by default: the nearest manifest row
/// whose LOCAL PATH resolves to one of THIS bundle's placement dirs is rewritten to the canonical
/// workspace reference — the local copy becomes a managed placement of the governed bundle, one
/// act. The match is by RESOLVED PATH, never by name (two dirs may share a basename); an
/// already-present canonical row keeps its own value.
///
/// THE ROW'S SETTINGS SURVIVE THE TRANSFER. Governance moves; local state does not. A path row
/// frozen to two destinations is the person's standing decision about where this machine puts the
/// bytes, and the workspace taking over the VERSION history says nothing about that — so `dest`
/// rides onto the new row and the next reconcile plans the same folders. The fields that were
/// about the LOCAL folder do not ride: `kind` is catalog-borne once a workspace serves the bundle,
/// and `name` is the row key's own leaf now.
///
/// LOCK, THEN RESOLVE. The row this rewrite acts on is a decision read FROM a file, and it is
/// only true of the file the writer lock now guards — so the probe that names the file runs
/// first (a lock needs a path), the lock is taken, and the row is RE-RESOLVED under it. A row a
/// concurrent `topos remove` dropped in that window answers [`GovernedOutcome::RowRemoved`]:
/// no workspace row is written (either serialization order the person could have observed —
/// remove-then-publish, publish-then-remove — ends with the row gone, and "removed, then quietly
/// re-added" is neither). A re-resolve that lands in a DIFFERENT file re-locks against it,
/// bounded.
///
/// # Errors
/// A manifest read/write failure.
/// The value the governed row takes over from the path row it replaces: the placement freeze, and
/// only that. A workspace bundle's legal fields are `version` / `dest` / `name`, and of those only
/// `dest` is a LOCAL decision — the version is now the workspace's to serve, and the name is the
/// key. No freeze at all leaves the plain `"*"` this rewrite has always written.
fn carried_value(row: &crate::manifest::scopes::PlanRow) -> EntryValue {
    match row.fields().dest.filter(|d| !d.is_empty()) {
        Some(dest) => EntryValue::Fields(crate::manifest::document::EntryFields {
            dest: Some(dest),
            ..Default::default()
        }),
        None => EntryValue::Star,
    }
}

pub(crate) fn rewrite_to_governed(
    ctx: &Ctx<'_>,
    skill_name: &str,
    host: &str,
    workspace_name: &str,
    skill_dirs: &[std::path::PathBuf],
) -> Result<GovernedOutcome, ClientError> {
    let canonical = format!("{host}/{workspace_name}/{skill_name}");
    let Some((mut path, _)) = find_path_line(ctx, skill_dirs)? else {
        return Ok(GovernedOutcome::None);
    };
    for _ in 0..4 {
        // The governance rewrite is a read-modify-write of someone's manifest, so it takes the
        // same writer lock as `add`/`remove` — a publish's transfer must not silently drop a row
        // an agent added while the publish was in flight.
        let _guard = medit::lock_manifest(ctx, &path)?;
        // RE-RESOLVE under the lock: the unlocked probe above only chose which file to lock.
        let found = find_path_line(ctx, skill_dirs)?;
        let Some((found_path, row)) = found else {
            // The row is gone — a completed concurrent removal. Write nothing.
            return Ok(GovernedOutcome::RowRemoved {
                manifest: path.display().to_string(),
            });
        };
        let from = row.reference.clone();
        if found_path != path {
            // The row moved to a different file (removed here, spelled there) — lock THAT one.
            path = found_path;
            continue;
        }
        let scope = if path.starts_with(ctx.layout.home()) {
            ManifestScope::Global
        } else {
            ManifestScope::Project
        };
        let Some(text) = medit::read_text(ctx, &path)? else {
            return Ok(GovernedOutcome::RowRemoved {
                manifest: path.display().to_string(),
            });
        };
        let mut editor = crate::manifest::document::ManifestEditor::open(&text, scope)
            .map_err(|e| ClientError::Corrupt(format!("{}: {e}", path.display())))?;
        let already_governed = editor.row(&canonical).is_some();
        editor.remove_row(&from);
        if !already_governed {
            editor
                .set_row(&canonical, &carried_value(&row))
                .map_err(|e| ClientError::InvalidArgument(e.message))?;
        }
        match editor.write(ctx.fs, &path) {
            Ok(()) => {}
            // The editor's compare-and-swap saw an OUTSIDE write land in the beat before its
            // rename (topos's own writers hold the lock — this is a person's editor or a
            // `sed`). Nothing was written; retry the whole re-resolve, bounded, and fall out to
            // the pending transfer if the file will not hold still (the next update converges it
            // idempotently) — a landed publish must not turn into a hard error over a rewrite
            // it can retry later.
            Err(ClientError::ManifestChanged { .. }) => continue,
            Err(e) => return Err(e),
        }
        return Ok(GovernedOutcome::Rewritten(GovernedRewrite {
            manifest: path.display().to_string(),
            canonical,
            from,
        }));
    }
    // The row kept hopping between files while we chased the lock — leave the transfer pending
    // (the next update converges it idempotently) rather than writing under the wrong lock.
    Ok(GovernedOutcome::None)
}
