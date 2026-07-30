//! The MANIFEST half of `add` / `remove` — which `topos.toml` a verb edits, how that file is
//! BORN, and the row-level recording every receipt names.
//!
//! Two scopes, unblended. The GLOBAL file (`~/.topos/topos.toml`, the `-g` arm) is this machine's
//! personal recipe: absent, it behaves exactly as if it held one feed row per connected
//! workspace; present, it is the COMPLETE recipe. A PROJECT file is a repo fact — the NEAREST
//! `topos.toml` covering the working directory, else a fresh one at the enclosing git root (the
//! npm-init precedent: the file should travel with the repo).
//!
//! FILE BIRTH is one rule, everywhere: any path topos brings into existence is written
//! MATERIALIZED first — the global file gets the header plus one feed row per connected
//! workspace (spelling out what an absent file already means), a project file the commented
//! template — and only THEN does the requested edit run. A file a person wrote by hand is never
//! materialized. A born file STAYS even when a consent gate refuses the act that birthed it, and
//! the receipt says so.
//!
//! `remove` is the strict inverse: it edits FILES ONLY (machine facts). What a workspace GIVES a
//! person is a server fact, managed on the web — the CLI's one machine-local negative is the
//! `"off"` row in the global file.

use std::path::{Path, PathBuf};

use topos_types::results::{AddData, RemoveData, RemoveItem, RemoveKind};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::MANIFEST_FILE;
use crate::manifest::document::{
    EntryValue, ManifestEditor, ManifestError, ManifestScope, materialized_global, project_template,
};
use crate::manifest::keys::{self, KeyShape};
use crate::manifest::scopes::{self, PlanRow, ScopePlan};
use crate::sessions::{self, SESSION_ENDED, Session};

use super::reconcile::SessionConnect;
use super::remove::RemoveOutcome;

// ---------------------------------------------------------------------------------------------
// Sessions — what this installation is connected to
// ---------------------------------------------------------------------------------------------

/// The LIVE sessions (never an ended row).
pub(super) fn live_sessions(ctx: &Ctx<'_>) -> Result<Vec<Session>, ClientError> {
    Ok(sessions::read_sessions(ctx.fs, &ctx.layout)?
        .sessions
        .into_iter()
        .filter(|s| s.status != SESSION_ENDED)
        .collect())
}

/// Every connected `(host, workspace)` — the feed rows a global file is born with.
pub(crate) fn connected_workspaces(ctx: &Ctx<'_>) -> Vec<(String, String)> {
    live_sessions(ctx)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.host, s.workspace_name))
        .collect()
}

/// The ONE connected host the `@ws` sugar resolves at — `None` when zero or several are
/// connected (several is never guessed; [`several_hosts`] refuses, naming them).
pub(super) fn default_host(ctx: &Ctx<'_>) -> Option<String> {
    let mut hosts: Vec<String> = live_sessions(ctx)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.host)
        .collect();
    hosts.sort();
    hosts.dedup();
    match hosts.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// The host the `@ws` sugar resolves at, for the composition root's shape classification — the
/// same answer the verbs use.
pub(crate) fn manifest_host(ctx: &Ctx<'_>) -> Option<String> {
    default_host(ctx)
}

/// Whether a token is REFERENCE-SHAPED — a `@`-led workspace sugar, or a `/`-joined key. Such a
/// token that the grammar refuses surfaces ITS refusal; it is never retried as a filesystem path
/// or a discovered name, whose answers would name the wrong fix.
pub(crate) fn reference_shaped(token: &str) -> bool {
    let token = token.trim();
    if token.starts_with('@') || token.starts_with('#') {
        return true;
    }
    // A path spelling is a path, not a reference (`./x`, `../x`, `~/x`, `/abs`).
    if token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with('/')
    {
        return false;
    }
    token.contains('/')
}

/// The typed refusal for an `@ws` sugar with more than one connected host — the sugar cannot
/// pick, so the message names every host and the full spelling.
pub(super) fn several_hosts(ctx: &Ctx<'_>, token: &str) -> Option<ClientError> {
    let mut hosts: Vec<String> = live_sessions(ctx)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.host)
        .collect();
    hosts.sort();
    hosts.dedup();
    (hosts.len() > 1).then(|| {
        ClientError::InvalidArgument(format!(
            "`{token}` names a workspace at your connected host, and this machine is connected to \
             several ({}) — spell the reference in full (`<host>/<workspace>…`)",
            hosts.join(", ")
        ))
    })
}

// ---------------------------------------------------------------------------------------------
// Where an edit lands
// ---------------------------------------------------------------------------------------------

/// The manifest an edit targets: the file, the folder it governs, and which scope it reads as.
#[derive(Debug, Clone)]
pub(crate) struct EditTarget {
    pub path: PathBuf,
    /// The folder the manifest governs (a relative path row resolves against it).
    pub dir: PathBuf,
    pub scope: ManifestScope,
}

/// The GLOBAL target — `~/.topos/topos.toml`, always resolvable (it lives in the sidecar, not the
/// machine roots).
pub(crate) fn global_target(ctx: &Ctx<'_>) -> EditTarget {
    EditTarget {
        path: ctx.layout.home().join(MANIFEST_FILE),
        dir: ctx.layout.home().to_path_buf(),
        scope: ManifestScope::Global,
    }
}

/// The PROJECT target: the NEAREST `topos.toml` covering the working directory, else a fresh one
/// at the enclosing git root (else the working directory itself). `None` when no working
/// directory is known — the caller skips the edit honestly rather than guessing a folder.
///
/// # Errors
/// An [`crate::fs_seam::FsOps`] read failure while walking.
pub(crate) fn project_target(ctx: &Ctx<'_>) -> Result<Option<EditTarget>, ClientError> {
    let Some(roots) = &ctx.roots else {
        return Ok(None);
    };
    let Some(cwd) = roots.cwd.as_deref() else {
        return Ok(None);
    };
    if let Some(dir) = scopes::nearest_manifest_dir(ctx.fs, cwd, Some(&roots.home)) {
        return Ok(Some(EditTarget {
            path: dir.join(MANIFEST_FILE),
            dir,
            scope: ManifestScope::Project,
        }));
    }
    let dir = init_dir(ctx.fs, cwd);
    // The project walk STOPS below `$HOME` (a manifest there would shadow the personal scope
    // confusingly), so a project edit must never land AT or above the home directory — the file
    // would be dead on arrival, read by nothing. Route it to the global file instead (the receipt
    // names that path, so the routing is disclosed).
    //
    // Both sides are canonicalized the SAME way before comparing (canonicalizing only one makes a
    // symlinked prefix — macOS `/var` → `/private/var` — a spurious match), AND the route fires
    // only when the walk from cwd would actually stop at $HOME (cwd at/under home): with cwd
    // ABOVE home, the file at `dir` is on the live upward path, never dead.
    let dir_c = canonical_or(&dir);
    let home_c = canonical_or(&roots.home);
    let cwd_c = canonical_or(cwd);
    let cwd_at_or_under_home = cwd_c == home_c || cwd_c.starts_with(&home_c);
    let edit_at_or_above_home = dir_c == home_c || home_c.starts_with(&dir_c);
    if cwd_at_or_under_home && edit_at_or_above_home {
        return Ok(Some(global_target(ctx)));
    }
    Ok(Some(EditTarget {
        path: dir.join(MANIFEST_FILE),
        dir,
        scope: ManifestScope::Project,
    }))
}

/// The target a scope flag selects (`-g` = the global file).
///
/// # Errors
/// As [`project_target`].
pub(crate) fn edit_target(ctx: &Ctx<'_>, global: bool) -> Result<Option<EditTarget>, ClientError> {
    if global {
        return Ok(Some(global_target(ctx)));
    }
    project_target(ctx)
}

/// The folder a fresh project manifest is created in when none is in reach: the enclosing git
/// repository's root (npm-init precedent — the file travels with the repo), else the working
/// directory itself.
pub(crate) fn init_dir(fs: &dyn crate::fs_seam::FsOps, cwd: &Path) -> PathBuf {
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if fs.exists(&d.join(".git")) {
            return d;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    cwd.to_path_buf()
}

/// A path's canonical form, best-effort: symlinks resolved when the path exists, else the path
/// itself. Two paths must be canonicalized the SAME way before comparing — canonicalizing only
/// one side makes a symlinked prefix (macOS `/var` → `/private/var`) a spurious mismatch.
fn canonical_or(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

// ---------------------------------------------------------------------------------------------
// Reading + birthing a manifest
// ---------------------------------------------------------------------------------------------

fn corrupt(path: &Path, e: &ManifestError) -> ClientError {
    ClientError::Corrupt(format!("{}: {e}", path.display()))
}

/// The file's text, or `None` when it does not exist.
///
/// # Errors
/// A read failure, or non-UTF-8 bytes (a manifest is text by contract).
pub(super) fn read_text(ctx: &Ctx<'_>, path: &Path) -> Result<Option<String>, ClientError> {
    match ctx.fs.read_opt(path)? {
        Some(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|_| {
            ClientError::Corrupt(format!("{}: not UTF-8", path.display()))
        })?)),
        None => Ok(None),
    }
}

/// An opened manifest ready to edit — plus whether THIS call brought the file into existence.
pub(super) struct Opened {
    pub editor: ManifestEditor,
    /// `Some(note)` when the file was born here: the disclosure the receipt leads with. A born
    /// file stays even if the act that birthed it is then refused.
    pub born: Option<String>,
}

/// Open the target for editing, BIRTHING the file first when it does not exist: the global file
/// is born MATERIALIZED (the header + one feed row per connected workspace — exactly what an
/// absent file already meant), a project file from the commented template. The birth is a real,
/// committed write before the requested edit runs.
///
/// # Errors
/// A filesystem failure, or [`ClientError::Corrupt`] when the existing file does not parse.
pub(super) fn open_for_edit(ctx: &Ctx<'_>, target: &EditTarget) -> Result<Opened, ClientError> {
    if let Some(text) = read_text(ctx, &target.path)? {
        let editor =
            ManifestEditor::open(&text, target.scope).map_err(|e| corrupt(&target.path, &e))?;
        return Ok(Opened { editor, born: None });
    }
    let (seed, note) = match target.scope {
        ManifestScope::Global => {
            let connected = connected_workspaces(ctx);
            let note = if connected.is_empty() {
                format!(
                    "created {} — no workspace is connected yet, so it starts with just its header",
                    target.path.display()
                )
            } else {
                format!(
                    "created {} — materialized with one feed row per connected workspace ({})",
                    target.path.display(),
                    connected
                        .iter()
                        .map(|(_, w)| w.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            (materialized_global(&connected), note)
        }
        ManifestScope::Project => (
            project_template(),
            format!("created {}", target.path.display()),
        ),
    };
    if let Some(parent) = target.path.parent() {
        ctx.fs.create_dir_all(parent)?;
    }
    crate::atomic::atomic_write(ctx.fs, &target.path, seed.as_bytes())?;
    let editor =
        ManifestEditor::open(&seed, target.scope).map_err(|e| corrupt(&target.path, &e))?;
    Ok(Opened {
        editor,
        born: Some(note),
    })
}

/// The scope PLAN behind a target (what its rows currently demand). The global scope falls back
/// to the IMPLICIT recipe when no file exists — one feed row per connected workspace — so a
/// classification made before a file birth matches the file the birth writes.
///
/// # Errors
/// As [`scopes::person_plan`] / [`scopes::nearest_project_plan`].
pub(super) fn plan_for(ctx: &Ctx<'_>, target: &EditTarget) -> Result<ScopePlan, ClientError> {
    match target.scope {
        ManifestScope::Global => {
            scopes::person_plan(ctx.fs, &ctx.layout, &connected_workspaces(ctx))
        }
        ManifestScope::Project => {
            let Some(text) = read_text(ctx, &target.path)? else {
                return Ok(ScopePlan::default());
            };
            let doc = crate::manifest::document::parse_manifest(&text, ManifestScope::Project)
                .map_err(|e| corrupt(&target.path, &e))?;
            Ok(ScopePlan::from_doc(&doc, Some(target.path.clone())))
        }
    }
}

/// Every manifest that governs anything here — the cwd chain's files (nearest first) plus the
/// global file — as `(path, scope, rows)`. The read-only view the cross-scope hints, the
/// first-trust check, and the governance rewrite share.
///
/// # Errors
/// A read failure, or a manifest the grammar refuses (typed, naming the file).
pub(super) fn local_rows(
    ctx: &Ctx<'_>,
) -> Result<Vec<(PathBuf, ManifestScope, Vec<PlanRow>)>, ClientError> {
    let mut out = Vec::new();
    if let Some(roots) = &ctx.roots
        && let Some(cwd) = roots.cwd.as_deref()
    {
        for dir in scopes::manifest_dirs_up(ctx.fs, cwd, Some(&roots.home)) {
            let path = dir.join(MANIFEST_FILE);
            let Some(text) = read_text(ctx, &path)? else {
                continue;
            };
            let doc = crate::manifest::document::parse_manifest(&text, ManifestScope::Project)
                .map_err(|e| corrupt(&path, &e))?;
            let plan = ScopePlan::from_doc(&doc, Some(path.clone()));
            out.push((path, ManifestScope::Project, all_rows(&plan)));
        }
    }
    let path = ctx.layout.home().join(MANIFEST_FILE);
    if let Some(text) = read_text(ctx, &path)? {
        let doc = crate::manifest::document::parse_manifest(&text, ManifestScope::Global)
            .map_err(|e| corrupt(&path, &e))?;
        let plan = ScopePlan::from_doc(&doc, Some(path.clone()));
        out.push((path, ManifestScope::Global, all_rows(&plan)));
    }
    Ok(out)
}

/// Every explicit row of a plan (things, sets, and `"off"` switches — never the feeds, which are
/// not rows a token can name).
pub(super) fn all_rows(plan: &ScopePlan) -> Vec<PlanRow> {
    plan.things
        .iter()
        .chain(plan.sets.iter())
        .chain(plan.offs.iter())
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The `add` receipt half — recording one row
// ---------------------------------------------------------------------------------------------

/// Append one disclosure to the receipt's note (several disclosures join with ` · `).
pub(super) fn push_note(data: &mut AddData, line: impl Into<String>) {
    let line = line.into();
    data.note = Some(match data.note.take() {
        Some(prev) => format!("{prev} · {line}"),
        None => line,
    });
}

/// The paste-ready inverse of an add: `topos remove [-g] <reference>`.
pub(super) fn undo_add(reference: &str, global: bool) -> Vec<String> {
    let mut argv = vec!["topos".to_owned(), "remove".to_owned()];
    if global {
        argv.push("-g".to_owned());
    }
    argv.push(reference.to_owned());
    argv
}

/// Write ONE row into `target` (birthing the file first when it does not exist) and stamp the
/// receipt: `data.manifest` names the file, `data.reference` the canonical key, `data.undo` the
/// inverse, and `data.note` leads with the birth when this call created the file.
///
/// EXACT-INVERSE DISCIPLINE over a pre-existing row:
/// - the same value again writes NOTHING and says so (and offers no undo — a `remove` would
///   delete a row that predates this add);
/// - a DIFFERENT value applies, the receipt names the prior value, and the undo is offered ONLY
///   in the form that verifiably restores it — `add <ref>` for a prior `"*"`, `add <ref>@<pin>`
///   for a prior pin; a prior fields table has no single restoring command, so no undo is
///   offered and the note says why (no undo beats a wrong one).
///
/// # Errors
/// [`ClientError::InvalidArgument`] when the row is illegal for the file (the editor's own typed
/// refusal — e.g. a feed row while its grouping section stands); a filesystem failure.
pub(super) fn write_row(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    reference: &str,
    value: &EntryValue,
) -> Result<(), ClientError> {
    let global = target.scope == ManifestScope::Global;
    let Opened { mut editor, born } = open_for_edit(ctx, target)?;
    let prior = editor.row(reference).map(|r| r.value);
    if let Some(note) = born {
        push_note(data, note);
    }
    data.manifest = Some(target.path.display().to_string());
    data.reference = Some(reference.to_owned());
    if prior.as_ref() == Some(value) {
        // The redundancy disclosure: the file already spells exactly this row.
        data.undo = Vec::new();
        push_note(
            data,
            format!("`{reference}` is already recorded in this file — nothing changed"),
        );
        return Ok(());
    }
    editor
        .set_row(reference, value)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;
    editor.write(ctx.fs, &target.path)?;
    match &prior {
        None => {
            data.undo = undo_add(reference, global);
        }
        Some(EntryValue::Star) => {
            data.undo = restore_add(reference, None, global);
            push_note(
                data,
                "replaced the row's prior value `\"*\"` — the undo restores it".to_owned(),
            );
        }
        Some(EntryValue::Pin(p)) => {
            data.undo = restore_add(reference, Some(p), global);
            push_note(
                data,
                format!("replaced the row's prior pin `{p}` — the undo restores it"),
            );
        }
        Some(EntryValue::Fields(_) | EntryValue::Off) => {
            // No single command re-spells the prior value — a wrong undo is worse than none.
            data.undo = Vec::new();
            let prior_kind = if matches!(prior, Some(EntryValue::Off)) {
                "`\"off\"` switch".to_owned()
            } else {
                "field table".to_owned()
            };
            push_note(
                data,
                format!(
                    "replaced the row's prior {prior_kind} — no undo is offered because no \
                     single command restores it; edit {} by hand to put it back",
                    target.path.display()
                ),
            );
        }
    }
    Ok(())
}

/// The paste-ready RE-ADD that restores a replaced row's prior value: `topos add [-g] <ref>` for
/// a prior `"*"`, `topos add [-g] <ref>@<pin>` for a prior pin.
fn restore_add(reference: &str, pin: Option<&str>, global: bool) -> Vec<String> {
    let mut argv = vec!["topos".to_owned(), "add".to_owned()];
    if global {
        argv.push("-g".to_owned());
    }
    argv.push(match pin {
        Some(p) => format!("{reference}@{p}"),
        None => reference.to_owned(),
    });
    argv
}

/// Record a workspace/forge/local reference in the manifest a scope flag selects. `pin` writes
/// the explicit version row; `None` tracks what the reference currently serves.
///
/// # Errors
/// As [`write_row`]; `Ok(())` with no target (no working directory) — the caller's adopt already
/// landed, and guessing a folder would be worse than recording nothing.
pub(crate) fn note_added(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    reference: &str,
    pin: Option<&str>,
    global: bool,
) -> Result<(), ClientError> {
    let Some(target) = edit_target(ctx, global)? else {
        return Ok(());
    };
    let value = match pin {
        Some(p) => EntryValue::Pin(p.to_owned()),
        None => EntryValue::Star,
    };
    write_row(ctx, data, &target, reference, &value)
}

/// Record a PATH-adopted bundle: the project manifest with the dir-relative spelling when the
/// source sits under its folder (the committed, travels-with-the-repo form); else — an
/// out-of-tree source, or an explicit `-g` — the GLOBAL file with the absolute path (machine-local
/// by nature).
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_path(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    source: &Path,
    global: bool,
) -> Result<(), ClientError> {
    // Canonicalize best-effort (symlinks resolve; a vanished dir keeps the typed spelling).
    let source_abs = source.canonicalize().unwrap_or_else(|_| {
        if source.is_absolute() {
            source.to_path_buf()
        } else {
            ctx.roots
                .as_ref()
                .and_then(|r| r.cwd.as_ref())
                .map(|c| c.join(source))
                .unwrap_or_else(|| source.to_path_buf())
        }
    });
    if !global && let Some(target) = project_target(ctx)? {
        // Compare BOTH paths canonically: a source under the manifest's folder ALWAYS records the
        // dir-relative `./path`, never an absolute one (the file itself still lands at
        // `target.path` — the dir is canonicalized only to compute the spelling).
        let dir_c = canonical_or(&target.dir);
        if source_abs.starts_with(&dir_c) && target.scope == ManifestScope::Project {
            let reference = path_reference(&dir_c, &source_abs);
            return write_row(ctx, data, &target, &reference, &EntryValue::Star);
        }
    }
    let reference = source_abs.display().to_string();
    let target = global_target(ctx);
    write_row(ctx, data, &target, &reference, &EntryValue::Star)
}

/// The manifest spelling of an adopted local path: relative to the manifest's folder when the
/// source sits under it (`./tools/my-skill`), else the absolute path.
pub(crate) fn path_reference(manifest_dir: &Path, source_abs: &Path) -> String {
    match source_abs.strip_prefix(manifest_dir) {
        Ok(rel) => format!("./{}", rel.display()),
        Err(_) => source_abs.display().to_string(),
    }
}

/// Whether a string is commit-shaped (7–40 hex) — the only pin a forge row takes.
pub(super) fn is_commit(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Record a REMOTE-imported bundle: the forge key is the repo (`<host>/<owner>/<repo>`) for a
/// repo-root import, else the discovered skill's LEAF directory name as the fourth segment (a
/// literal in-repo path is never a key — it rides the `subdir` field). The row tracks the
/// DEFAULT BRANCH unless the input carried an explicit ref: only a deliberately-referenced
/// commit pins (a freeze the person asked for).
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_remote(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    global: bool,
) -> Result<(), ClientError> {
    let Some(origin) = data.origin.clone() else {
        return Ok(());
    };
    let leaf = origin
        .subdir
        .as_deref()
        .and_then(|s| s.trim_end_matches('/').rsplit('/').next())
        .filter(|s| !s.is_empty());
    let reference = match leaf {
        Some(l) => format!("{}/{l}", origin.source),
        None => origin.source.clone(),
    };
    // An explicit ref (a `#<ref>` shorthand or a `/tree/<ref>/` URL) is what pins; a bare add
    // tracks the branch the repo serves.
    let pin = match (&origin.git_ref, &origin.commit) {
        (Some(_), Some(c)) if is_commit(c) => Some(c.as_str()),
        _ => None,
    };
    note_added(ctx, data, &reference, pin, global)
}

// ---------------------------------------------------------------------------------------------
// What the FEED currently delivers (live, else the offline cache)
// ---------------------------------------------------------------------------------------------

/// One bundle a connected workspace's feed carries for this person — or DECLINES on the web.
#[derive(Debug, Clone)]
pub(super) struct FeedItem {
    pub host: String,
    pub workspace: String,
    pub name: String,
    /// The person DECLINED this bundle on the web (it is not delivered; a machine's row may still
    /// bring it here, and the receipt says so).
    pub declined: bool,
}

/// What the connected workspaces give this person right now: the LIVE delivery when a session
/// answers, else that workspace's offline cache (never a claim the network was reached).
pub(super) fn feed_items(ctx: &Ctx<'_>, connect: &SessionConnect<'_>) -> Vec<FeedItem> {
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let mut out = Vec::new();
    for session in live_sessions(ctx).unwrap_or_default() {
        let live = connect(&session)
            .plane
            .fetch_delivery(&session.workspace_id)
            .ok();
        match live {
            Some(snapshot) => {
                for s in &snapshot.skills {
                    out.push(FeedItem {
                        host: session.host.clone(),
                        workspace: session.workspace_name.clone(),
                        name: s.name.clone(),
                        declined: false,
                    });
                }
                for (_, name) in &snapshot.declined {
                    out.push(FeedItem {
                        host: session.host.clone(),
                        workspace: session.workspace_name.clone(),
                        name: name.clone(),
                        declined: true,
                    });
                }
            }
            None => {
                let Some(entry) = cache.workspaces.get(&session.workspace_id) else {
                    continue;
                };
                for ds in entry.delivered.values() {
                    // A manifest-delivered row is this machine's own doing, never the feed's.
                    if ds.withdrawn || ds.via_manifest || ds.name.is_empty() {
                        continue;
                    }
                    out.push(FeedItem {
                        host: session.host.clone(),
                        workspace: session.workspace_name.clone(),
                        name: ds.name.clone(),
                        declined: false,
                    });
                }
                for name in entry.declined.values() {
                    out.push(FeedItem {
                        host: session.host.clone(),
                        workspace: session.workspace_name.clone(),
                        name: name.clone(),
                        declined: true,
                    });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// `remove` — the strict file editors
// ---------------------------------------------------------------------------------------------

/// What a `remove` token resolved to in the target file.
enum Arm {
    /// The file spells this row — drop the line.
    RowDrop { row: PlanRow, name: String },
    /// The workspace FEED row (global file only) — stop adopting that feed here.
    FeedDrop {
        reference: String,
        workspace: String,
        /// The bundles that feed currently delivers here (non-declined) — the loss-guard scans
        /// each for local edits, because dropping the row retires ALL their placements.
        bundles: Vec<String>,
    },
    /// No row, and the workspace's FEED delivers it — write the machine-local `"off"` switch.
    OffWrite {
        reference: String,
        name: String,
        workspace: String,
    },
    /// A SET row (a channel, a repo) delivers it — replace the set line with its current members
    /// minus this one.
    SetSplit {
        set: PlanRow,
        name: String,
        members: Vec<String>,
    },
}

impl Arm {
    fn name(&self) -> &str {
        match self {
            Arm::RowDrop { name, .. } | Arm::OffWrite { name, .. } | Arm::SetSplit { name, .. } => {
                name
            }
            Arm::FeedDrop { workspace, .. } => workspace,
        }
    }
}

/// `topos remove -g <token>…` — edit THIS MACHINE's global file: drop a row, drop a feed row, or
/// write the `"off"` switch for something the feed delivers. Never touches the server: what a
/// workspace gives a person is managed on the web.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a token the file (and the feed) do not carry — with the
/// cross-scope hint when the project manifest does; a filesystem failure.
pub(crate) fn remove_global(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    tokens: &[String],
    yes: bool,
) -> Result<RemoveOutcome, ClientError> {
    let target = global_target(ctx);
    let arms = resolve_arms(ctx, connect, &target, tokens)?;
    let mut resolved = Vec::with_capacity(tokens.len());
    for (token, arm) in tokens.iter().zip(arms) {
        match arm {
            Some(a) => resolved.push(a),
            None => return Err(miss(ctx, token, true)?),
        }
    }
    apply_arms(ctx, &target, tokens, resolved, yes, true)
}

/// `topos remove <token>…` — edit THIS FOLDER's manifest. `Ok(None)` when no token names a
/// manifest row (the caller falls through to the classic tracked/untracked removal).
///
/// # Errors
/// [`ClientError::InvalidArgument`] on a mixed batch (some tokens are manifest rows, some are
/// not), or when a token is a GLOBAL row spelled without `-g`; a filesystem failure.
pub(crate) fn remove_project(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    tokens: &[String],
    yes: bool,
) -> Result<Option<RemoveOutcome>, ClientError> {
    let Some(target) = project_target(ctx)? else {
        return Ok(None);
    };
    if target.scope == ManifestScope::Global {
        // The home-routing guard sent this edit to the global file — a bare `remove` there is
        // still a file edit, but it must not silently claim a token the classic removal owns.
        let arms = resolve_arms(ctx, connect, &target, tokens)?;
        if arms.iter().all(Option::is_none) {
            return Ok(None);
        }
    }
    let arms = resolve_arms(ctx, connect, &target, tokens)?;
    if arms.iter().all(Option::is_none) {
        // Nothing here claims any token — but a GLOBAL row of that name is a near-miss worth
        // naming rather than answering "no such skill" through the classic path.
        for token in tokens {
            if let Some(hint) = cross_scope_hint(ctx, token, false)? {
                return Err(ClientError::InvalidArgument(hint));
            }
        }
        return Ok(None);
    }
    if arms.iter().any(Option::is_none) {
        return Err(ClientError::InvalidArgument(
            "some targets are manifest rows and some are not — remove them in separate \
             invocations"
                .into(),
        ));
    }
    let resolved: Vec<Arm> = arms.into_iter().flatten().collect();
    apply_arms(ctx, &target, tokens, resolved, yes, false).map(Some)
}

/// Resolve every token against the target scope (all-or-none is the caller's discipline).
fn resolve_arms(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    target: &EditTarget,
    tokens: &[String],
) -> Result<Vec<Option<Arm>>, ClientError> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let plan = plan_for(ctx, target)?;
    let host = default_host(ctx);
    // The feed + set expansions are read ONCE per invocation (each may dial).
    let mut feeds: Option<Vec<FeedItem>> = None;
    let mut sets: Option<Vec<(PlanRow, Vec<String>)>> = None;
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let canonical = keys::parse_input(token, host.as_deref())
            .ok()
            .map(|r| r.shape.canonical());
        out.push(resolve_one(
            ctx,
            connect,
            target,
            &plan,
            token,
            canonical.as_deref(),
            &mut feeds,
            &mut sets,
        )?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn resolve_one(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    target: &EditTarget,
    plan: &ScopePlan,
    token: &str,
    canonical: Option<&str>,
    feeds: &mut Option<Vec<FeedItem>>,
    sets: &mut Option<Vec<(PlanRow, Vec<String>)>>,
) -> Result<Option<Arm>, ClientError> {
    // 1. A FEED row this file spells (global only).
    if let Some(c) = canonical
        && let Ok(KeyShape::Feed { host, workspace }) = keys::classify_key(c)
        && plan.has_feed(&host, &workspace)
    {
        // What the feed currently delivers — the loss-guard's scan set (dropping the row
        // retires every one of these placements at the next sweep).
        let items = feeds.get_or_insert_with(|| feed_items(ctx, connect));
        let bundles: Vec<String> = items
            .iter()
            .filter(|i| !i.declined && i.host == host && i.workspace == workspace)
            .map(|i| i.name.clone())
            .collect();
        return Ok(Some(Arm::FeedDrop {
            reference: c.to_owned(),
            workspace,
            bundles,
        }));
    }
    // 2. An explicit row, by canonical reference or by the name it delivers under.
    if let Some(row) = all_rows(plan)
        .into_iter()
        .find(|r| row_matches(token, canonical, r))
    {
        if matches!(row.value, EntryValue::Off) {
            return Err(ClientError::InvalidArgument(format!(
                "'{token}' is already switched off on this machine — `topos add -g {}` turns it \
                 back on",
                row.reference
            )));
        }
        let name = row.display_name();
        return Ok(Some(Arm::RowDrop { row, name }));
    }
    // 3. No row — the workspace FEED delivers it: the one machine-local negative is the `"off"`
    //    switch, and it lives in the global file only.
    if target.scope == ManifestScope::Global {
        let items = feeds.get_or_insert_with(|| feed_items(ctx, connect));
        if let Some(item) = items
            .iter()
            .find(|i| !i.declined && feed_matches(token, canonical, i))
            && plan.has_feed(&item.host, &item.workspace)
        {
            return Ok(Some(Arm::OffWrite {
                reference: format!("{}/{}/{}", item.host, item.workspace, item.name),
                name: item.name.clone(),
                workspace: item.workspace.clone(),
            }));
        }
    }
    // 4. A SET row in this file delivers it — the removal is a rewrite of that line.
    let expansions = sets.get_or_insert_with(|| expand_sets(ctx, connect, target, plan));
    for (row, members) in expansions.iter() {
        let want = member_of(row, canonical);
        if let Some(member) = members
            .iter()
            .find(|m| m.as_str() == token || Some(m.as_str()) == want.as_deref())
        {
            return Ok(Some(Arm::SetSplit {
                set: row.clone(),
                name: member.clone(),
                members: members.clone(),
            }));
        }
    }
    Ok(None)
}

/// The member NAME a canonical token names inside `set`'s namespace (`host/ws/<name>` under a
/// channel's workspace, `host/owner/repo/<leaf>` under a repo set), if it belongs there at all.
fn member_of(set: &PlanRow, canonical: Option<&str>) -> Option<String> {
    let c = canonical?;
    let shape = keys::classify_key(c).ok()?;
    match (&set.shape, &shape) {
        (
            KeyShape::Channel {
                host: sh,
                workspace: sw,
                ..
            },
            KeyShape::WorkspaceBundle {
                host,
                workspace,
                bundle,
            },
        ) if sh == host && sw == workspace => Some(bundle.clone()),
        (
            KeyShape::RepoSet {
                host: sh,
                owner: so,
                repo: sr,
            },
            KeyShape::RepoSkill {
                host,
                owner,
                repo,
                skill,
            },
        ) if sh == host && so == owner && sr == repo => Some(skill.clone()),
        _ => None,
    }
}

/// Whether `token` names `row`: the exact canonical reference, the reference as typed, or the
/// name the row delivers under (its `name` override, else the reference's leaf).
fn row_matches(token: &str, canonical: Option<&str>, row: &PlanRow) -> bool {
    row.reference == token
        || canonical == Some(row.reference.as_str())
        || row.display_name() == token
        || row.shape.leaf_name() == token
}

/// Whether `token` names a feed-delivered bundle (by name, or by its canonical reference).
fn feed_matches(token: &str, canonical: Option<&str>, item: &FeedItem) -> bool {
    if item.name == token {
        return true;
    }
    canonical.is_some_and(|c| c == format!("{}/{}/{}", item.host, item.workspace, item.name))
}

/// Expand each SET row of a plan into the member NAMES it currently delivers: a channel's
/// members from the live channel index (else the offline cache's `via` attribution), a repo's
/// from the imports the TARGET SCOPE's store tracks with that origin (a project row's imports
/// live in the project's own `.topos/` store, a global row's in the home store).
fn expand_sets(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    target: &EditTarget,
    plan: &ScopePlan,
) -> Vec<(PlanRow, Vec<String>)> {
    let store_layout = match target.scope {
        ManifestScope::Project => crate::sidecar::project_store_layout(&target.dir),
        ManifestScope::Global => ctx.layout.clone(),
    };
    let sctx = super::pull::ctx_with_layout(ctx, &store_layout);
    let mut out = Vec::new();
    for row in &plan.sets {
        let members = match &row.shape {
            KeyShape::Channel {
                host,
                workspace,
                channel,
            } => channel_members(ctx, connect, host, workspace, channel),
            KeyShape::RepoSet { host, owner, repo } => {
                let mut names: Vec<String> = super::reconcile::tracked_repo_members(
                    &sctx,
                    &format!("{host}/{owner}/{repo}"),
                )
                .into_iter()
                .map(|(_, lock, _)| lock.name)
                .collect();
                names.sort();
                names.dedup();
                names
            }
            _ => Vec::new(),
        };
        out.push((row.clone(), members));
    }
    out
}

/// A channel's current members, live if a session answers, else from the delivery cache's `via`
/// attribution (never a claim the network was reached).
fn channel_members(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    host: &str,
    workspace: &str,
    channel: &str,
) -> Vec<String> {
    for session in live_sessions(ctx).unwrap_or_default() {
        if session.host != host || session.workspace_name != workspace {
            continue;
        }
        if let Ok(index) = connect(&session)
            .directory
            .channels_index(&session.workspace_id)
            && let Some(entry) = index.channels.iter().find(|c| c.name == channel)
        {
            return entry.skills.iter().map(|s| s.name.clone()).collect();
        }
    }
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let mut out = Vec::new();
    for entry in cache.workspaces.values() {
        if entry.host.as_deref() != Some(host) || entry.workspace_name.as_deref() != Some(workspace)
        {
            continue;
        }
        for ds in entry.delivered.values() {
            if ds.via_channels.iter().any(|c| c == channel) && !ds.name.is_empty() {
                out.push(ds.name.clone());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The typed miss for a token no arm claims — with the CROSS-SCOPE hint when the other file
/// carries it.
fn miss(ctx: &Ctx<'_>, token: &str, global: bool) -> Result<ClientError, ClientError> {
    if let Some(hint) = cross_scope_hint(ctx, token, global)? {
        return Ok(ClientError::InvalidArgument(hint));
    }
    Ok(ClientError::InvalidArgument(format!(
        "'{token}' is not in {} — `topos status` lists what each file asks for",
        if global {
            "your machine-wide manifest, and no connected workspace's feed delivers it".to_owned()
        } else {
            "this folder's manifest".to_owned()
        }
    )))
}

/// The other scope's row of that name, phrased as the way to remove it.
fn cross_scope_hint(
    ctx: &Ctx<'_>,
    token: &str,
    global: bool,
) -> Result<Option<String>, ClientError> {
    let host = default_host(ctx);
    let canonical = keys::parse_input(token, host.as_deref())
        .ok()
        .map(|r| r.shape.canonical());
    for (path, scope, rows) in local_rows(ctx)? {
        let hit = rows
            .iter()
            .any(|r| row_matches(token, canonical.as_deref(), r));
        if !hit {
            continue;
        }
        return Ok(match (global, scope) {
            (true, ManifestScope::Project) => Some(format!(
                "'{token}' is not in your machine-wide manifest — it is in the project manifest at \
                 {}; run `topos remove {token}` there, without `-g`",
                path.display()
            )),
            (false, ManifestScope::Global) => Some(format!(
                "'{token}' is not in this folder's manifest — it is in your machine-wide manifest \
                 ({}); run `topos remove -g {token}`",
                path.display()
            )),
            _ => None,
        });
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// The loss guard — a row whose bundle carries local edits stays describe-first
// ---------------------------------------------------------------------------------------------

/// What a draft scan concluded about a bundle's working copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftState {
    /// No local edits anywhere (or nothing local at all).
    Clean,
    /// At least one copy carries local edits.
    Draft,
    /// A copy could not be classified — the guard fails TOWARD the gate.
    Indeterminate,
}

/// Whether the bundle a row delivers carries local edits, over the store that owns it (the home
/// store, or the project store of a checkout on the cwd chain).
fn draft_state(ctx: &Ctx<'_>, name: &str) -> DraftState {
    let (layout, sid, _lock) = match super::resolve_skill_stored(ctx, name, None) {
        Ok(v) => v,
        Err(ClientError::NoSuchSkill { .. }) => return DraftState::Clean,
        // An ambiguous name (two stores hold it) cannot be classified — fail toward the gate.
        Err(_) => return DraftState::Indeterminate,
    };
    let sctx = super::pull::ctx_with_layout(ctx, &layout);
    let map = match crate::doc::read_map(sctx.fs, &layout.published(&sid).map) {
        Ok(Some(m)) => m,
        Ok(None) => return DraftState::Clean,
        Err(_) => return DraftState::Indeterminate,
    };
    let Ok(scans) = crate::placement::scan_placements(&sctx, &map) else {
        return DraftState::Indeterminate;
    };
    let mut state = DraftState::Clean;
    for scan in &scans {
        match scan.status {
            crate::placement::ScanStatus::Modified { .. } => return DraftState::Draft,
            crate::placement::ScanStatus::Unscannable => state = DraftState::Indeterminate,
            _ => {}
        }
    }
    state
}

// ---------------------------------------------------------------------------------------------
// The apply half
// ---------------------------------------------------------------------------------------------

/// Describe or apply the resolved arms: a batch that mixes gated and ungated arms gates WHOLE.
fn apply_arms(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    tokens: &[String],
    arms: Vec<Arm>,
    yes: bool,
    global: bool,
) -> Result<RemoveOutcome, ClientError> {
    if arms.is_empty() {
        return Err(ClientError::InvalidArgument(
            "remove needs something to remove — a skill name, a reference, or `-g @<workspace>`"
                .into(),
        ));
    }
    // The gate: a file edit is reversible, so it applies immediately — EXCEPT where local work
    // could be lost (an affected bundle's draft, or a copy that cannot be classified) or where
    // the edit rewrites a curated line into its members (a set split changes what future
    // curation reaches this file). EVERY arm that retires placements runs the loss-guard —
    // the explicit row drop, the feed-row drop (all of that feed's bundles leave), and the
    // `"off"` switch alike — and a scan that cannot classify fails TOWARD the gate.
    let mut gated = false;
    let mut notes: Vec<Option<String>> = Vec::with_capacity(arms.len());
    for arm in &arms {
        let note = match arm {
            Arm::RowDrop { name, .. } | Arm::OffWrite { name, .. } => {
                let removal_note = match arm {
                    Arm::OffWrite { workspace, .. } => Some(format!(
                        "'{name}' stays assigned to you in {workspace} — this switch is \
                         machine-local; declining it on the web stops it everywhere"
                    )),
                    _ => None,
                };
                match draft_state(ctx, name) {
                    DraftState::Draft => {
                        gated = true;
                        Some(join_notes(
                            format!(
                                "'{name}' has local edits that are not shared — this removal \
                                 takes it out of this scope's agent dirs (the edits are \
                                 snapshotted first, never deleted)"
                            ),
                            removal_note,
                        ))
                    }
                    DraftState::Indeterminate => {
                        gated = true;
                        Some(join_notes(
                            format!(
                                "'{name}''s files could not be read to check for local edits — \
                                 describing first rather than assuming there are none"
                            ),
                            removal_note,
                        ))
                    }
                    DraftState::Clean => removal_note,
                }
            }
            Arm::SetSplit { set, name, .. } => {
                gated = true;
                let (_, carried, dropped) = split_carriage(set);
                let mut note = format!(
                    "'{name}' comes from the {} line `{}` — removing it replaces that one line \
                     with its current members, so later additions by whoever curates it stop \
                     arriving through this file",
                    set.shape.noun(),
                    set.reference
                );
                if !carried.is_empty() {
                    note.push_str(&format!(
                        "; the line's {} settings carry onto each member row",
                        carried.join("/")
                    ));
                }
                if !dropped.is_empty() {
                    note.push_str(&format!(
                        "; its {} cannot ride a member row and is dropped",
                        dropped.join("/")
                    ));
                }
                Some(note)
            }
            Arm::FeedDrop {
                workspace, bundles, ..
            } => {
                let removal_note = format!(
                    "{workspace}'s feed no longer delivers here; its skills leave this machine \
                     at the next update, and the rows you spelled yourself survive"
                );
                let mut drafted: Vec<&str> = Vec::new();
                let mut unreadable = false;
                for bundle in bundles {
                    match draft_state(ctx, bundle) {
                        DraftState::Draft => drafted.push(bundle),
                        DraftState::Indeterminate => unreadable = true,
                        DraftState::Clean => {}
                    }
                }
                if !drafted.is_empty() {
                    gated = true;
                    Some(format!(
                        "{} of its skills carry local edits that are not shared ({}) — dropping \
                         the feed row takes them out of this scope's agent dirs (the edits are \
                         snapshotted first, never deleted) · {removal_note}",
                        drafted.len(),
                        drafted.join(", ")
                    ))
                } else if unreadable {
                    gated = true;
                    Some(format!(
                        "some of its skills' files could not be read to check for local edits — \
                         describing first rather than assuming there are none · {removal_note}"
                    ))
                } else {
                    Some(removal_note)
                }
            }
        };
        notes.push(note);
    }

    let items: Vec<RemoveItem> = arms
        .iter()
        .zip(notes.iter())
        .map(|(arm, note)| RemoveItem {
            name: arm.name().to_owned(),
            kind: match arm {
                Arm::OffWrite { .. } => RemoveKind::ManifestExcluded,
                _ => RemoveKind::ManifestRemoved,
            },
            manifest: Some(target.path.display().to_string()),
            workspace_id: None,
            agent_dirs: Vec::new(),
            bytes_kept: true,
            note: note.clone(),
        })
        .collect();

    if gated && !yes {
        let mut yes_argv = vec!["topos".to_owned(), "remove".to_owned()];
        if global {
            yes_argv.push("-g".to_owned());
        }
        yes_argv.extend(tokens.iter().cloned());
        yes_argv.push("--yes".to_owned());
        return Ok(RemoveOutcome::Described {
            data: RemoveData {
                items,
                applied: false,
                undo: Vec::new(),
            },
            yes_argv,
        });
    }

    // ---- APPLY ----
    let Opened { mut editor, born } = open_for_edit(ctx, target)?;
    for arm in &arms {
        match arm {
            Arm::RowDrop { row, .. } => {
                editor.remove_row(&row.reference);
            }
            Arm::FeedDrop { reference, .. } => {
                editor.remove_row(reference);
            }
            Arm::OffWrite { reference, .. } => {
                editor
                    .set_row(reference, &EntryValue::Off)
                    .map_err(|e| ClientError::InvalidArgument(e.message))?;
            }
            Arm::SetSplit {
                set, name, members, ..
            } => {
                editor.remove_row(&set.reference);
                // Each surviving member row carries the SET line's pin/fields where the member's
                // shape allows them (disclosed in the describe) — a split must not silently
                // strip the line's placement/harness discipline.
                let (member_value, _, _) = split_carriage(set);
                for member in members {
                    if member == name {
                        continue;
                    }
                    let Some(reference) = member_reference(set, member) else {
                        continue;
                    };
                    editor
                        .set_row(&reference, &member_value)
                        .map_err(|e| ClientError::InvalidArgument(e.message))?;
                }
            }
        }
    }
    editor.write(ctx.fs, &target.path)?;

    let mut items = items;
    if let Some(note) = born
        && let Some(first) = items.first_mut()
    {
        first.note = Some(match first.note.take() {
            Some(prev) => format!("{note} · {prev}"),
            None => note,
        });
    }
    Ok(RemoveOutcome::Applied(RemoveData {
        undo: undo_for(&arms, global),
        items,
        applied: true,
    }))
}

/// Join a loss-guard line with an arm's own removal note (` · `-separated when both exist).
fn join_notes(lead: String, tail: Option<String>) -> String {
    match tail {
        Some(t) => format!("{lead} · {t}"),
        None => lead,
    }
}

/// What a set split writes on each surviving member row, and how the describe words it: the set
/// line's pin/fields filtered to what the member's shape legally carries —
/// `(member value, carried field names, dropped field names)`. Today every set-legal field
/// (`path`, `harness`, a repo set's commit pin) is member-legal too, so `dropped` stays empty;
/// the filter is still real, so a future set-only field is DISCLOSED rather than silently lost.
fn split_carriage(set: &PlanRow) -> (EntryValue, Vec<&'static str>, Vec<&'static str>) {
    use crate::manifest::document::{EntryFields, legal_fields};
    // A representative member shape: a channel's members are workspace bundles, a repo set's
    // are repo skills.
    let member_shape = match &set.shape {
        KeyShape::Channel {
            host, workspace, ..
        } => KeyShape::WorkspaceBundle {
            host: host.clone(),
            workspace: workspace.clone(),
            bundle: "member".to_owned(),
        },
        KeyShape::RepoSet { host, owner, repo } => KeyShape::RepoSkill {
            host: host.clone(),
            owner: owner.clone(),
            repo: repo.clone(),
            skill: "member".to_owned(),
        },
        _ => return (EntryValue::Star, Vec::new(), Vec::new()),
    };
    let legal = legal_fields(&member_shape);
    match &set.value {
        EntryValue::Star | EntryValue::Off => (EntryValue::Star, Vec::new(), Vec::new()),
        EntryValue::Pin(p) => {
            // A repo set's commit pin is a legal member pin verbatim; nothing else pins a set.
            if matches!(member_shape, KeyShape::RepoSkill { .. }) {
                (EntryValue::Pin(p.clone()), vec!["version"], Vec::new())
            } else {
                (EntryValue::Star, Vec::new(), vec!["version"])
            }
        }
        EntryValue::Fields(f) => {
            let mut out = EntryFields::default();
            let mut carried = Vec::new();
            let mut dropped = Vec::new();
            let mut take = |field: &'static str, present: bool, copy: &mut dyn FnMut()| {
                if !present {
                    return;
                }
                if legal.contains(&field) {
                    copy();
                    carried.push(field);
                } else {
                    dropped.push(field);
                }
            };
            take("version", f.version.is_some(), &mut || {
                out.version = f.version.clone();
            });
            take("path", f.path.is_some(), &mut || out.path = f.path.clone());
            take("harness", f.harness.is_some(), &mut || {
                out.harness = f.harness.clone();
            });
            take("name", f.name.is_some(), &mut || out.name = f.name.clone());
            take("subdir", f.subdir.is_some(), &mut || {
                out.subdir = f.subdir.clone();
            });
            take("kind", f.kind.is_some(), &mut || out.kind = f.kind.clone());
            let value = if out == EntryFields::default() {
                EntryValue::Star
            } else {
                EntryValue::Fields(out)
            };
            (value, carried, dropped)
        }
    }
}

/// One member's own row key inside a set's namespace.
fn member_reference(set: &PlanRow, member: &str) -> Option<String> {
    match &set.shape {
        KeyShape::Channel {
            host, workspace, ..
        } => Some(format!("{host}/{workspace}/{member}")),
        KeyShape::RepoSet { host, owner, repo } => Some(format!("{host}/{owner}/{repo}/{member}")),
        _ => None,
    }
}

/// The literal inverse — offered ONLY when it restores the whole prior state. A batch of one
/// whose row carried nothing but a version pin re-adds exactly what left; a set split, a fields
/// row the `add` grammar cannot respell, or a multi-target batch offers none (no undo beats a
/// wrong one).
fn undo_for(arms: &[Arm], global: bool) -> Vec<String> {
    let [arm] = arms else {
        return Vec::new();
    };
    let spelling = match arm {
        Arm::RowDrop { row, .. } => match &row.value {
            EntryValue::Star => row.reference.clone(),
            EntryValue::Pin(p) => format!("{}@{p}", row.reference),
            // Fields the `add` grammar cannot respell (a path, a harness list, a name override):
            // the re-add would silently drop them.
            EntryValue::Fields(_) | EntryValue::Off => return Vec::new(),
        },
        Arm::FeedDrop { reference, .. } | Arm::OffWrite { reference, .. } => reference.clone(),
        Arm::SetSplit { .. } => return Vec::new(),
    };
    let mut argv = vec!["topos".to_owned(), "add".to_owned()];
    if global {
        argv.push("-g".to_owned());
    }
    argv.push(spelling);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_reference_is_relative_inside_the_manifest_dir_absolute_outside() {
        let dir = PathBuf::from("/repo");
        assert_eq!(
            path_reference(&dir, Path::new("/repo/tools/my-skill")),
            "./tools/my-skill"
        );
        assert_eq!(
            path_reference(&dir, Path::new("/elsewhere/x")),
            "/elsewhere/x"
        );
    }

    #[test]
    fn only_a_commit_shaped_string_pins_a_forge_row() {
        assert!(is_commit("8c1f0a2"));
        assert!(is_commit(&"a".repeat(40)));
        assert!(!is_commit("main"));
        assert!(!is_commit("abc"));
        assert!(!is_commit(&"a".repeat(41)));
    }

    #[test]
    fn the_undo_is_withheld_where_it_cannot_restore_the_whole_row() {
        let row = |value: EntryValue| PlanRow {
            reference: "topos.sh/acme/deploy".into(),
            shape: keys::classify_key("topos.sh/acme/deploy").unwrap(),
            value,
        };
        let star = Arm::RowDrop {
            row: row(EntryValue::Star),
            name: "deploy".into(),
        };
        assert_eq!(
            undo_for(std::slice::from_ref(&star), false),
            vec!["topos", "add", "topos.sh/acme/deploy"]
        );
        assert_eq!(
            undo_for(std::slice::from_ref(&star), true),
            vec!["topos", "add", "-g", "topos.sh/acme/deploy"]
        );
        let digest = "0123456789abcdef".repeat(4);
        let pinned = Arm::RowDrop {
            row: row(EntryValue::Pin(digest.clone())),
            name: "deploy".into(),
        };
        assert_eq!(
            undo_for(std::slice::from_ref(&pinned), false),
            vec!["topos", "add", &format!("topos.sh/acme/deploy@{digest}")]
        );
        // A fields row cannot be respelled by `add` — no undo at all.
        let fields = Arm::RowDrop {
            row: row(EntryValue::Fields(crate::manifest::document::EntryFields {
                harness: Some(vec!["claude-code".into()]),
                ..Default::default()
            })),
            name: "deploy".into(),
        };
        assert!(undo_for(&[fields], false).is_empty());
        // A batch of several offers none (a partial inverse would misstate what it restores).
        assert!(
            undo_for(
                &[
                    Arm::RowDrop {
                        row: row(EntryValue::Star),
                        name: "a".into()
                    },
                    Arm::RowDrop {
                        row: row(EntryValue::Star),
                        name: "b".into()
                    },
                ],
                false
            )
            .is_empty()
        );
    }

    #[test]
    fn a_set_split_carries_what_the_member_shape_legally_takes() {
        use crate::manifest::document::{EntryFields, PathSpec};
        // A repo set's commit PIN rides each member row verbatim.
        let repo_pinned = PlanRow {
            reference: "github.com/o/r".into(),
            shape: keys::classify_key("github.com/o/r").unwrap(),
            value: EntryValue::Pin("abc1234".into()),
        };
        let (value, carried, dropped) = split_carriage(&repo_pinned);
        assert_eq!(value, EntryValue::Pin("abc1234".into()));
        assert_eq!(carried, vec!["version"]);
        assert!(dropped.is_empty());

        // A channel line's fields (`path`, `harness`) carry onto its workspace-bundle members.
        let channel_fields = PlanRow {
            reference: "topos.sh/acme/channels/backend".into(),
            shape: keys::classify_key("topos.sh/acme/channels/backend").unwrap(),
            value: EntryValue::Fields(EntryFields {
                path: Some(PathSpec::One(".agents/skills".into())),
                harness: Some(vec!["codex".into()]),
                ..Default::default()
            }),
        };
        let (value, carried, dropped) = split_carriage(&channel_fields);
        match value {
            EntryValue::Fields(f) => {
                assert_eq!(f.path, Some(PathSpec::One(".agents/skills".into())));
                assert_eq!(f.harness.as_deref(), Some(&["codex".to_owned()][..]));
            }
            other => panic!("fields carry: {other:?}"),
        }
        assert_eq!(carried, vec!["path", "harness"]);
        assert!(dropped.is_empty());

        // A bare `"*"` set stays a `"*"` member.
        let star = PlanRow {
            reference: "github.com/o/r".into(),
            shape: keys::classify_key("github.com/o/r").unwrap(),
            value: EntryValue::Star,
        };
        assert_eq!(split_carriage(&star).0, EntryValue::Star);
    }

    #[test]
    fn a_token_names_a_row_by_reference_or_by_the_name_it_delivers() {
        let row = PlanRow {
            reference: "topos.sh/acme/deploy".into(),
            shape: keys::classify_key("topos.sh/acme/deploy").unwrap(),
            value: EntryValue::Star,
        };
        assert!(row_matches("deploy", None, &row));
        assert!(row_matches("topos.sh/acme/deploy", None, &row));
        assert!(row_matches(
            "@acme/deploy",
            Some("topos.sh/acme/deploy"),
            &row
        ));
        assert!(!row_matches("other", None, &row));
    }
}
