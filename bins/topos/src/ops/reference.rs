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
//! The two SCOPES are unblended: without `-g` the row lands in the nearest project `topos.toml`
//! (created at the enclosing git root when none is in reach); with `-g` it lands in this machine's
//! global file, whose absence means "one feed row per connected workspace" — so a bare `add -g X`
//! of something the feed already delivers writes NOTHING and says so.
//!
//! One CONSENT gate lives here: a git origin the target scope's own STORE does not yet track is
//! fetched read-only, described (what it holds, what would be written where), and applied only
//! under `--yes`. Trust is a store fact — a tracked import of the origin — never a file fact: a
//! manifest row is demand anyone could have committed, so a row's existence never skips the
//! member-listing describe (the describe names it instead). Every other arm is a self-scoped
//! file edit that applies immediately with an undo-led receipt.

use topos_types::results::{AddData, AddDescribeData};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::git_source::GitTarballSource;
use crate::manifest::document::{EntryFields, EntryValue, ManifestScope};
use crate::manifest::keys::{self, InputRef, KeyShape};
use crate::sessions::Session;
use crate::source::{GitHost, RemoteSpec};

use super::manifest_edit::{self as medit, EditTarget};
use super::reconcile::{SessionConnect, SessionTransports};

/// What `add <reference>` did — or, for a first-trust git source, what it WOULD do.
pub(crate) enum AddRefOutcome {
    /// The row is written (and, where it delivers bytes, they landed). Boxed: `AddData` dwarfs
    /// the describe variant.
    Applied(Box<AddData>),
    /// A git source this machine has never used: what it holds and what would be written.
    Described {
        data: AddDescribeData,
        yes_argv: Vec<String>,
    },
}

/// `topos add <reference> [-g] [--yes]`.
///
/// # Errors
/// [`ClientError::SessionRequired`] for a workspace this installation is not logged into;
/// [`ClientError::InvalidArgument`] for a reference the grammar (or the target file) refuses;
/// [`ClientError::NotAvailable`] / [`ClientError::Plane`] from the catalog read; the remote-import
/// family from a git source; a filesystem failure.
pub(crate) fn add_reference(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn GitTarballSource>,
    raw: &str,
    global: bool,
    yes: bool,
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
        KeyShape::Feed { host, workspace } => add_feed(ctx, &host, &workspace, global),
        KeyShape::WorkspaceBundle { .. } | KeyShape::Channel { .. } => {
            add_workspace(ctx, connect, &parsed, global)
        }
        KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. } => {
            add_forge(ctx, git, &parsed, raw, global, yes)
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
        data.reference = Some(reference.clone());
        medit::push_note(
            &mut data,
            format!("already adopting {workspace}'s feed here — nothing changed"),
        );
        return Ok(AddRefOutcome::Applied(Box::new(data)));
    }
    medit::write_row(ctx, &mut data, &target, &reference, &EntryValue::Star)?;
    medit::push_note(
        &mut data,
        format!(
            "this machine now takes whatever {workspace} gives you; `topos update` delivers it"
        ),
    );
    Ok(AddRefOutcome::Applied(Box::new(data)))
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

fn add_workspace(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    parsed: &InputRef,
    global: bool,
) -> Result<AddRefOutcome, ClientError> {
    let resolved = resolve_workspace(ctx, connect, &parsed.shape)?;
    let mut data = match &resolved.entry {
        Some(e) => AddData {
            skill_id: e.skill_id.clone(),
            name: e.name.clone(),
            version_id: e.version_id.clone(),
            bundle_digest: e.bundle_digest.clone(),
            ..set_data(&resolved.name)
        },
        None => set_data(&resolved.name),
    };
    let pin = parsed.pin.clone();
    let value = match &pin {
        Some(p) => EntryValue::Pin(p.clone()),
        None => EntryValue::Star,
    };

    if global {
        let target = medit::global_target(ctx);
        // A standing `"off"` switch is the row's own inverse: adding it back DELETES the switch
        // (never a second, redundant positive row beside it).
        if let Some(text) = medit::read_text(ctx, &target.path)?
            && let Ok(mut editor) =
                crate::manifest::document::ManifestEditor::open(&text, ManifestScope::Global)
            && editor
                .row(&resolved.canonical)
                .is_some_and(|r| r.value == EntryValue::Off)
        {
            editor.remove_row(&resolved.canonical);
            editor.write(ctx.fs, &target.path)?;
            data.manifest = Some(target.path.display().to_string());
            data.reference = Some(resolved.canonical.clone());
            data.undo = medit::undo_add(&resolved.canonical, true);
            medit::push_note(
                &mut data,
                format!(
                    "the `off` switch is gone — '{}' flows from {}'s feed again",
                    resolved.name, resolved.session.workspace_name
                ),
            );
            return Ok(finish_workspace(ctx, connect, data, &resolved));
        }
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
            return Ok(AddRefOutcome::Applied(Box::new(data)));
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
        return Ok(finish_workspace(ctx, connect, data, &resolved));
    }

    let Some(target) = medit::project_target(ctx)? else {
        return Err(ClientError::InvalidArgument(
            "cannot resolve the current directory — run `topos add` inside the folder the \
             manifest should govern, or use `-g` for your machine-wide file"
                .into(),
        ));
    };
    medit::write_row(ctx, &mut data, &target, &resolved.canonical, &value)?;
    Ok(finish_workspace(ctx, connect, data, &resolved))
}

/// Deliver what the new row asks for, in the same invocation — the SAME reconcile the sweep runs,
/// narrowed to this name. Best-effort by contract: the demand is durably recorded, so a transport
/// hiccup just moves the byte landing to the next sweep.
fn finish_workspace(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    data: AddData,
    resolved: &Resolved,
) -> AddRefOutcome {
    let targets = match resolved.entry {
        // A channel expands server-side into member names a targeted filter could not match.
        None => Vec::new(),
        Some(_) => vec![resolved.name.clone()],
    };
    let _ = super::reconcile::manifest_update(
        ctx,
        connect,
        None,
        &super::reconcile::ManifestUpdateOpts {
            targets,
            ack_notices: false,
            ..Default::default()
        },
    );
    AddRefOutcome::Applied(Box::new(data))
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
        skill_id: String::new(),
        name: name.to_owned(),
        version_id: "0".repeat(64),
        bundle_digest: "0".repeat(64),
        tracked: true,
        harness: None,
        harness_slug: None,
        currency: None,
        triggers: Vec::new(),
        origin: None,
        manifest: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
        note: None,
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
    let Some(target) = medit::edit_target(ctx, global)? else {
        return Err(ClientError::InvalidArgument(
            "cannot resolve the current directory — run `topos add` inside the folder the \
             manifest should govern, or use `-g` for your machine-wide file"
                .into(),
        ));
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
    // FIRST TRUST is a MACHINE fact: the origin counts as known ONLY when this machine's own
    // trust registry (the HOME sidecar — `crate::forge_trust`) granted it at a prior add's
    // consent moment. Never a store fact — a project `.topos/` store travels with the checkout,
    // so committed store contents could otherwise smuggle trust in — and never a manifest ROW
    // (demand anyone could have committed). The predicate is evaluated HERE, before any write,
    // so this add's own row can never satisfy the gate it is behind.
    let trusted = crate::forge_trust::is_trusted(ctx, &source_label);

    // ONE fetch serves both the describe and the apply (read-only either way).
    let targz = git.fetch(&spec)?;
    let extracted = crate::git_source::extract_tree(&targz)?;
    let discovered = extracted.skill_names(parsed.subdir_hint.as_deref(), &repo);
    if discovered.is_empty() {
        return Err(ClientError::NoSkillInSource { src: source_label });
    }

    // The row's KEY: the repo (every skill it holds) or ONE discovered skill by its LEAF
    // directory name — a frontmatter name is accepted as input when it uniquely names one.
    let (reference, members) = match &want_skill {
        None => (source_label.clone(), discovered.clone()),
        Some(want) => {
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
    };

    // The row's VALUE: `"*"` tracks the default branch; a hex ref pins straight through; a NAMED
    // ref is a deliberate freeze, so the RESOLVED commit is what the row records.
    let pin = match (&parsed.pin, &parsed.ref_hint) {
        (Some(p), _) => Some(p.clone()),
        (None, Some(r)) if medit::is_commit(r) => Some(r.clone()),
        (None, Some(_)) => extracted.commit.clone().filter(|c| medit::is_commit(c)),
        (None, None) => None,
    };
    let value = match (&pin, &parsed.subdir_hint) {
        (_, Some(sub)) => EntryValue::Fields(EntryFields {
            version: pin.clone().or_else(|| Some("*".to_owned())),
            subdir: Some(sub.clone()),
            ..EntryFields::default()
        }),
        (Some(p), None) => EntryValue::Pin(p.clone()),
        (None, None) => EntryValue::Star,
    };

    // FIRST TRUST: an origin the scope's store does not track is described, never installed,
    // until `--yes` — however many manifests spell its row.
    if !trusted && !yes {
        let mut yes_argv = vec!["topos".to_owned(), "add".to_owned(), raw.to_owned()];
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
            data: AddDescribeData {
                source: source_label,
                members,
                manifest: target.path.display().to_string(),
                reference,
                value: value_spelling(&value),
                note,
            },
            yes_argv,
        });
    }

    // ---- APPLY ----
    // The CONSENT lands FIRST: reaching here IS the consent moment (a granted registry, or the
    // accepted describe's `--yes`), and the machine registry records it durably before any other
    // write — so a partial landing retries as a trusted origin, and the reconcile's forge arms
    // may converge it.
    crate::forge_trust::grant(ctx, &source_label)?;
    // Then the DEMAND: with the row durably recorded, a member-install failure part-way leaves a
    // CONVERGENT state — the manifest asks for exactly what the consent covered, and the next
    // explicit update (or a re-run of this add) finishes the landing — instead of installed
    // members no manifest row asks for.
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
    for member in &members {
        let opts = super::AddRemoteOpts {
            skill: Some(member.clone()),
            harness: None,
            global,
        };
        match super::add::add_remote_fetched(&sctx, &targz, &spec, &roots, &opts) {
            Ok(d) => landed.push(d),
            // Already here: the row still records the demand; the receipt names what was skipped.
            Err(ClientError::AlreadyTracked { .. } | ClientError::AlreadyTrackedName { .. }) => {
                skipped.push(member.clone());
            }
            // A per-member failure never unwinds the batch: the receipt names the partial
            // landing, and the recorded row is what converges it.
            Err(e) => failed.push((member.clone(), crate::render::safe_message(&e))),
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
    Ok(AddRefOutcome::Applied(Box::new(data)))
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
    pub base_url: String,
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
        base_url: session.base_url.clone(),
        transports,
    }))
}

/// The facts a governance transfer's receipt names.
pub(crate) struct GovernedRewrite {
    pub manifest: String,
    pub canonical: String,
    pub from: String,
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
) -> Result<Option<(std::path::PathBuf, String)>, ClientError> {
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
        return Ok(Some((path, row.reference.clone())));
    }
    Ok(None)
}

/// The governance-transfer rewrite a LANDED publish performs by default: the nearest manifest row
/// whose LOCAL PATH resolves to one of THIS bundle's placement dirs is rewritten to the canonical
/// workspace reference — the local copy becomes a managed placement of the governed bundle, one
/// act. The match is by RESOLVED PATH, never by name (two dirs may share a basename); an
/// already-present canonical row keeps its own value.
///
/// # Errors
/// A manifest read/write failure.
pub(crate) fn rewrite_to_governed(
    ctx: &Ctx<'_>,
    skill_name: &str,
    host: &str,
    workspace_name: &str,
    skill_dirs: &[std::path::PathBuf],
) -> Result<Option<GovernedRewrite>, ClientError> {
    let canonical = format!("{host}/{workspace_name}/{skill_name}");
    let Some((path, from)) = find_path_line(ctx, skill_dirs)? else {
        return Ok(None);
    };
    let scope = if path.starts_with(ctx.layout.home()) {
        ManifestScope::Global
    } else {
        ManifestScope::Project
    };
    let Some(text) = medit::read_text(ctx, &path)? else {
        return Ok(None);
    };
    let mut editor = crate::manifest::document::ManifestEditor::open(&text, scope)
        .map_err(|e| ClientError::Corrupt(format!("{}: {e}", path.display())))?;
    let already_governed = editor.row(&canonical).is_some();
    editor.remove_row(&from);
    if !already_governed {
        editor
            .set_row(&canonical, &EntryValue::Star)
            .map_err(|e| ClientError::InvalidArgument(e.message))?;
    }
    editor.write(ctx.fs, &path)?;
    Ok(Some(GovernedRewrite {
        manifest: path.display().to_string(),
        canonical,
        from,
    }))
}
