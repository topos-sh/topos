//! The MANIFEST half of `add` / `remove` — which `topos.toml` a verb edits, how that file is
//! BORN, and the row-level recording every receipt names.
//!
//! Two scopes, unblended, and no verb crosses the line on its own: an edit acts on WHERE YOU
//! STAND, `-g` means the machine. The GLOBAL file (`~/.topos/topos.toml`, the `-g` arm) is this
//! machine's COMPLETE personal recipe: only its rows deliver, and with no file nothing is
//! demanded machine-wide (`topos login` writes a workspace's feed row on this machine's first
//! connection; a row someone deleted stays deleted). A PROJECT file is a repo fact — the NEAREST
//! `topos.toml` covering the working directory, and nothing else. With none in reach a
//! project-scoped edit REFUSES ([`ClientError::NoManifest`]): creating the file is `topos init`'s
//! job (the cargo/pnpm/git precedent), and rerouting the edit to the other scope would write in a
//! file nobody named.
//!
//! FILE BIRTH is one rule, everywhere: any path topos brings into existence is written
//! MATERIALIZED first — the global file gets its header alone (no feed rows: login is the only
//! automatic feed-row author, so a deleted row can never come back through a birth), a project
//! file the commented template — and only THEN does the requested edit run. A file a person
//! wrote by hand is never materialized. A born file STAYS even when a consent gate refuses the
//! act that birthed it, and the receipt says so. Only `-g` births a file this way now: the
//! project scope has nothing to birth, because it refuses when there is nothing to edit.
//!
//! `remove` is the strict inverse: it edits FILES ONLY (machine facts). What a workspace GIVES a
//! person is a server fact, managed on the web — the CLI's one machine-local negative is the
//! `"off"` row in the global file.
//!
//! ## The outside-writer window (the manifest CAS + its accepted residual)
//!
//! Every manifest mutation is a read-modify-write under the file's writer lock — which fences
//! topos's OWN writers and nothing else. A person's editor or a `sed` can land between the read
//! an edit was built from and the rename that would apply it, and a document written from the
//! older reading silently erases theirs. Two rails close that window from both sides: the arms
//! are RE-PROVEN against a fresh read before the editor opens (`prove_unchanged`), and the
//! editor's write itself is a COMPARE-AND-SWAP — immediately before its atomic rename the file
//! is re-read and byte-compared against the exact text the editor was built from, and any drift
//! refuses typed (`MANIFEST_CHANGED`, staged document discarded, nothing overwritten; the
//! re-run reads the file as it now is). The accepted residual is the syscall PAIR between that
//! final compare and the rename: an outside write landing inside those two syscalls is still
//! overwritten — no sequence of path syscalls closes it, and unlike the byte-landing side there
//! is no directory-handle primitive for "rename unless the file changed". It is the manifest
//! twin of the materializer's fd-window note, and it is why the lock stays load-bearing for
//! every writer topos controls. FILE BIRTH has no such residual: a birth is a claim the file
//! does not exist, and it lands by EXCLUSIVE no-replace create
//! (`crate::atomic::atomic_write_new` — kernel `RENAME_NOREPLACE` where the filesystem speaks
//! it), so a file an outside editor wrote after the absence check refuses typed
//! (`MANIFEST_EXISTS`) with the outside bytes standing.

use std::path::{Path, PathBuf};

use topos_types::results::{AddData, DestChange, RemoveData, RemoveItem, RemoveKind};

use crate::bundle_kind::BundleKind;
use crate::ctx::Ctx;
use crate::error::{ClientError, TargetCandidate};
use crate::id::SkillId;
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
// The manifest writer lock — every mutation is a read-modify-write
// ---------------------------------------------------------------------------------------------

/// Serialize the mutations of ONE manifest file. Every manifest edit in this binary is a
/// read-modify-write over the whole document (open → set/remove rows → atomic write), so two
/// concurrent topos processes editing the same file — an agent's `add` and a session-start sweep's
/// governance rewrite, two agents in one checkout — would each write a document built from bytes
/// the other had already replaced, and the loser's row would vanish with no error anywhere.
///
/// The lock is the ordinary [`crate::fs_seam::FsOps::lock_exclusive`] `flock`, held across the
/// whole read→edit→write, and keyed by a STABLE HASH of the manifest's path so one `locks/`
/// directory serves every file a machine touches.
///
/// It lives in the HOME store's `locks/` — a documented divergence from "the owning store's". The
/// project store's own `locks/` would give no extra serialization (a project store is per-user,
/// `state/<user>/`, exactly like the home one), and reaching for it would mint `<project>/.topos/`
/// on edits that otherwise only write `topos.toml` — a new directory in someone's repo as the price
/// of a lock nobody else would contend on.
pub(super) fn lock_manifest(
    ctx: &Ctx<'_>,
    path: &Path,
) -> Result<crate::fs_seam::LockGuard, ClientError> {
    let key = topos_core::digest::to_hex(&topos_core::digest::sha256(
        path.as_os_str().as_encoded_bytes(),
    ));
    let locks = ctx.layout.locks_dir();
    ctx.fs.create_dir_all(&locks)?;
    Ok(ctx
        .fs
        .lock_exclusive(&locks.join(format!("manifest-{}.lock", &key[..16])))?)
}

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

/// Every connected `(host, workspace)` — what the live sessions name.
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

/// The ONE workspace-QUALIFIED display rule every receipt speaks: `@<workspace>/<name>` at the
/// machine's one connected host ([`default_host`]), the full `<host>/<workspace>/<name>`
/// everywhere else. Written once — the add/remove receipts and the update receipt's rows all call
/// this, so their spellings can never drift apart.
pub(super) fn qualify_display(
    default_host: Option<&str>,
    host: &str,
    workspace: &str,
    name: &str,
) -> String {
    if default_host == Some(host) {
        format!("@{workspace}/{name}")
    } else {
        format!("{host}/{workspace}/{name}")
    }
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

/// Whether a token can ONLY be a manifest row's spelling: a reference (see [`reference_shaped`])
/// or a PATH (`./x`, `../x`, `~/x`, `/abs`, the adopt-in-place spelling a row records). Such a
/// token names nothing the classic tracked/untracked ladder knows, so with no manifest in reach it
/// refuses toward the scopes instead of falling through to a "no such skill".
fn row_spelled(token: &str) -> bool {
    let t = token.trim();
    reference_shaped(t)
        || t.starts_with("./")
        || t.starts_with("../")
        || t.starts_with("~/")
        || t.starts_with('/')
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

/// The PROJECT target: the NEAREST `topos.toml` covering the working directory — nothing else.
/// `None` when no manifest covers the cwd (or no working directory is known); every verb acts on
/// where it stands, and a file that does not exist is `topos init`'s to create (the cargo/pnpm/git
/// precedent), never an `add`'s to invent somewhere the person did not name.
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
    let Some(dir) = scopes::nearest_manifest_dir(ctx.fs, cwd, Some(&roots.home)) else {
        return Ok(None);
    };
    Ok(Some(EditTarget {
        path: dir.join(MANIFEST_FILE),
        dir,
        scope: ManifestScope::Project,
    }))
}

/// The target a scope flag selects (`-g` = the global file, always resolvable).
///
/// # Errors
/// As [`project_target`].
pub(crate) fn edit_target(ctx: &Ctx<'_>, global: bool) -> Result<Option<EditTarget>, ClientError> {
    if global {
        return Ok(Some(global_target(ctx)));
    }
    project_target(ctx)
}

/// The SCOPE an `add` acts in, decided BEFORE any byte moves: the manifest file the row lands in,
/// and the STORE that scope's version history belongs to.
///
/// Custody follows the scope, not the machine: a project add's history belongs to the checkout's
/// own `.topos/state/<user>/` — the store that scope's `update` converges — so the decision has to
/// be made before the adopt, never after history has already landed in the home store. `-g` keeps
/// the home store and the machine-wide file.
///
/// # Errors
/// [`ClientError::NoManifest`] when no `topos.toml` covers the working directory and `-g` was not
/// asked for — refused before anything lands, never rerouted to the other scope; a filesystem
/// failure minting the project store.
pub(crate) fn add_scope(ctx: &Ctx<'_>, global: bool) -> Result<AddScope, ClientError> {
    if global {
        return Ok(AddScope {
            target: global_target(ctx),
            layout: ctx.layout.clone(),
        });
    }
    let target = project_target(ctx)?.ok_or(ClientError::NoManifest)?;
    let layout = crate::sidecar::ensure_project_store(ctx.fs, &target.dir)?;
    Ok(AddScope { target, layout })
}

/// Where an `add` records its row and where it keeps the bytes' history — see [`add_scope`].
pub(crate) struct AddScope {
    pub target: EditTarget,
    pub layout: crate::sidecar::Layout,
}

/// Every store this invocation does NOT write, each with the manifest file a reader would go to.
///
/// The machine store AND every project store up the cwd chain except the invoked one — a nested
/// checkout is not one scope, it is two, and the outer one's sweep converges the folders its own
/// rows name. Taking only the nearest pair would let an inner add double-adopt a folder the outer
/// project already manages.
///
/// THE ONE OWNERSHIP PROBE both folder doors ask: the claim (`add <path> --as <bundle>`) and the
/// plain path adopt. Neither store knows about the other's rows, so a folder recorded over there
/// would be converged by two engines at once — the state neither door may create.
///
/// `ctx` is the OUTER context (its layout is the machine store); `scope` is what this invocation
/// resolved to write.
pub(crate) fn other_scope_stores(
    ctx: &Ctx<'_>,
    scope: &AddScope,
) -> Vec<(crate::sidecar::Layout, String)> {
    let mut out: Vec<(crate::sidecar::Layout, String)> = Vec::new();
    if scope.target.scope == ManifestScope::Project {
        out.push((ctx.layout.clone(), "~/.topos/topos.toml".to_owned()));
    }
    let Some(roots) = &ctx.roots else {
        return out;
    };
    let Some(cwd) = roots.cwd.as_deref() else {
        return out;
    };
    let invoked = (scope.target.scope == ManifestScope::Project).then(|| scope.target.dir.clone());
    for dir in crate::manifest::scopes::manifest_dirs_up(ctx.fs, cwd, Some(&roots.home)) {
        if invoked.as_deref() == Some(dir.as_path()) {
            continue; // the scope this invocation IS acting in — its own store answers first
        }
        if let Some(layout) = crate::sidecar::existing_project_store(ctx.fs, &dir) {
            // Spelled as the machine file beside it is — the reader compares two paths in one
            // sentence, and only one of them being abbreviated makes them look unrelated.
            out.push((
                layout,
                crate::ops::inventory::pretty(ctx, &dir.join("topos.toml")),
            ));
        }
    }
    out
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

/// The typed refusal a manifest the grammar rejects earns — always the MANIFEST family, never
/// corrupt-state (a `topos.toml` is a user-authored file, and its faults are the file's, not
/// topos's own state), and it closes the TTY with `nothing changed`.
fn corrupt(path: &Path, e: &ManifestError) -> ClientError {
    ClientError::ManifestInvalid(format!("{}: {e}", path.display()))
}

/// The file's text, or `None` when it does not exist.
///
/// # Errors
/// A read failure, or non-UTF-8 bytes (a manifest is text by contract).
pub(super) fn read_text(ctx: &Ctx<'_>, path: &Path) -> Result<Option<String>, ClientError> {
    match ctx.fs.read_opt(path)? {
        Some(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|_| {
            ClientError::ManifestInvalid(format!("{}: not UTF-8", path.display()))
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
/// is born with its header alone (NO feed rows — login is the only automatic feed-row author,
/// so a birth can never resurrect a row someone deleted), a project file from the commented
/// template. The birth is a real, committed write before the requested edit runs.
///
/// # Errors
/// A filesystem failure, or the typed manifest refusal ([`ClientError::ManifestInvalid`]) when
/// the existing file does not parse.
pub(super) fn open_for_edit(ctx: &Ctx<'_>, target: &EditTarget) -> Result<Opened, ClientError> {
    if let Some(text) = read_text(ctx, &target.path)? {
        let editor =
            ManifestEditor::open(&text, target.scope).map_err(|e| corrupt(&target.path, &e))?;
        return Ok(Opened { editor, born: None });
    }
    let (seed, note) = match target.scope {
        ManifestScope::Global => (
            materialized_global(&[]),
            format!(
                "created {} — it starts with just its header; `topos login` adds a workspace's \
                 feed row on its first connection",
                target.path.display()
            ),
        ),
        ManifestScope::Project => (
            project_template(None, None),
            format!("created {}", target.path.display()),
        ),
    };
    if let Some(parent) = target.path.parent() {
        ctx.fs.create_dir_all(parent)?;
    }
    // The birth is a claim the file does not exist, landed as an EXCLUSIVE create: a file an
    // outside editor wrote between the absence read above and this landing refuses typed with
    // its bytes standing (the lock fences only topos's own writers) — never overwritten by the
    // seed.
    match crate::atomic::atomic_write_new(ctx.fs, &target.path, seed.as_bytes())? {
        crate::atomic::NewOutcome::Written => {}
        crate::atomic::NewOutcome::Exists => {
            return Err(ClientError::ManifestExists {
                path: target.path.display().to_string(),
            });
        }
    }
    let editor =
        ManifestEditor::open(&seed, target.scope).map_err(|e| corrupt(&target.path, &e))?;
    Ok(Opened {
        editor,
        born: Some(note),
    })
}

/// The scope PLAN behind a target (what its rows currently demand). With no global file the plan
/// is the empty default — exactly what a header-only birth writes, so a classification made
/// before a file birth matches the file the birth writes.
///
/// # Errors
/// As [`scopes::person_plan`] / [`scopes::nearest_project_plan`].
pub(super) fn plan_for(ctx: &Ctx<'_>, target: &EditTarget) -> Result<ScopePlan, ClientError> {
    match target.scope {
        ManifestScope::Global => scopes::person_plan(ctx.fs, &ctx.layout),
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
/// global file — as `(path, scope, rows)`. The read-only view the cross-scope hints and the
/// governance rewrite share.
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
pub(crate) fn push_note(data: &mut AddData, line: impl Into<String>) {
    let line = line.into();
    data.note = Some(match data.note.take() {
        Some(prev) => format!("{prev} · {line}"),
        None => line,
    });
}

/// The same, on a LINE OF ITS OWN. The `·` fold above reads well for two short asides about the
/// row that was just written; it reads as a wall the moment a fact is a sentence, and the MCP
/// receipt's facts all are — what the server is, that it reached nobody, and why. Every renderer
/// already splits a note on newlines and indents the tail ([`crate::render::add_tty`]), so the
/// fifteen per-config lines below it were already one to a line and only the FIRST was jammed onto
/// the end of whatever came before.
pub(crate) fn push_note_line(data: &mut AddData, line: impl Into<String>) {
    let line = line.into();
    data.note = Some(match data.note.take() {
        Some(prev) => format!("{prev}\n{line}"),
        None => line,
    });
}

/// THE FILE WAS UNTOUCHED AND BYTES MOVED ANYWAY — a first placement at a destination the row
/// already named, a merge onto a new current. The redundancy answer is retracted whole: its flag
/// AND the sentence that carried it, because `nothing changed` printed beside a line naming what
/// was just installed is a receipt contradicting itself. Every other disclosure on the note stays.
pub(crate) fn clear_unchanged(data: &mut AddData) {
    data.unchanged = false;
    let Some(note) = data.note.take() else {
        return;
    };
    let kept: Vec<&str> = note
        .split(" · ")
        .filter(|clause| !clause.ends_with("nothing changed"))
        .collect();
    data.note = (!kept.is_empty()).then(|| kept.join(" · "));
}

/// Which of the two manifests a receipt is about — the wire word behind the one spelling every
/// add answer names its file by.
pub(super) fn receipt_scope(target: &EditTarget) -> topos_types::results::ReceiptScope {
    match target.scope {
        ManifestScope::Global => topos_types::results::ReceiptScope::Machine,
        ManifestScope::Project => topos_types::results::ReceiptScope::Project,
    }
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
/// DESTINATIONS EXTEND, they never replace ([`extend_dest`]): `-a`/`--dest` ADDS to what the row
/// already names, and to the DEFAULT-REACH token when it named nothing, so an add can never cost
/// the row an agent. The undo subtracts only what this add put there. Narrowing is
/// `remove -a`/`--dest`, and a hand edit of the file still replaces outright.
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
    kind: crate::bundle_kind::BundleKind,
) -> Result<(), ClientError> {
    let global = target.scope == ManifestScope::Global;
    // Held across read → edit → write: another process's row must not be born and lost inside it.
    let _guard = lock_manifest(ctx, &target.path)?;
    let Opened { mut editor, born } = open_for_edit(ctx, target)?;
    let prior = editor.row(reference).map(|r| r.value);
    if let Some(note) = born {
        push_note(data, note);
    }
    data.manifest = Some(target.path.display().to_string());
    data.scope = Some(receipt_scope(target));
    data.reference = Some(reference.to_owned());
    // The `source:` line, derived ONCE for every arm that records a row: a reference IS the
    // source wherever a workspace or a forge governs the bundle. A path row's source is the
    // folder the caller already resolved (canonical, links followed) — never overwritten here,
    // because the row's `./…` spelling means nothing away from the file that holds it.
    data.source.get_or_insert_with(|| reference.to_owned());
    // A project file's FIRST workspace row writes the `workspace = ` line it resolves against —
    // `init` writes it up front, but a file born before the workspace was known (or hand-made)
    // adopts the one workspace here rather than refusing toward a hand edit.
    if target.scope == ManifestScope::Project
        && editor.workspace().is_none()
        && let Ok(shape) = crate::manifest::keys::classify_key(reference)
        && let Some(ws_key) = shape.workspace_key()
        && let Some((host, ws)) = ws_key.split_once('/')
    {
        editor
            .set_workspace(host, ws)
            .map_err(|e| ClientError::InvalidArgument(e.message))?;
        push_note(
            data,
            format!(
                "this project now uses `workspace = \"{ws_key}\"` — its bare names resolve there"
            ),
        );
    }
    let in_reach = reach_already_held(ctx, target, reference, prior.as_ref(), value)?;
    let (value, extend) = extend_dest(prior.as_ref(), value, &in_reach);
    let value = &value;
    if prior.as_ref() == Some(value) {
        // The redundancy disclosure: the file already spells exactly this row — which is also how
        // a `-a` naming only destinations the row already has says that it changed nothing. It is
        // the WHOLE answer, so the receipt leads with it rather than heading an install that did
        // not happen with a note underneath retracting it.
        data.undo = Vec::new();
        data.unchanged = true;
        push_note(
            data,
            format!("`{reference}` is already recorded in this file — nothing changed"),
        );
        return Ok(());
    }
    editor
        .set_row(reference, value, kind)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;
    editor.write(ctx.fs, &target.path)?;
    // An EXTEND is not a replace, so it does not read as one: the row kept everything it had and
    // the receipt leads with the destinations it gained.
    //
    // THE INVERSE IS OFFERED ONLY WHERE IT VERIFIABLY RESTORES. Subtracting the new entries puts
    // the row's own `dest` back exactly as it was, whether it named destinations before or stood
    // for its default reach (the subtraction collapses the row back to that reach — see
    // [`narrow_one`]). A channel's `mcp_dest` is the one array no `remove` flag subtracts from, so
    // its inverse is the prior value's own restore (`"*"`, or the pin it carried), and where no
    // single command spells the prior value there is no undo at all.
    if let Some(extend) = extend {
        data.undo = match (extend.mcp_dest, &prior) {
            (false, _) => undo_dest_add(reference, global, &extend.change.added),
            (true, Some(EntryValue::Star)) => restore_add(reference, None, global),
            (true, Some(EntryValue::Pin(p))) => restore_add(reference, Some(p), global),
            (true, _) => Vec::new(),
        };
        data.dest_change = Some(extend.change);
        if data.undo.is_empty() {
            push_note(
                data,
                format!(
                    "no single command re-spells the prior value of `{reference}` — no undo is \
                     offered; edit {} by hand to put it back",
                    target.path.display()
                ),
            );
        }
        return Ok(());
    }
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

/// DESTINATIONS EXTEND. A `-a`/`--dest` add over a standing row unions its entries onto what the
/// row already names, kind-correct array by kind-correct array (`dest` for a bundle's folders or
/// an mcp bundle's config files, `mcp_dest` for a channel's mcp members) and preserving the file's
/// own entry order — appending is the only edit; nothing already recorded is dropped.
///
/// AN ADD NEVER NARROWS. A row that names NO destinations reaches every agent, now and later, so
/// the entries join the DEFAULT-REACH token rather than replacing what the row already had:
/// `dest = ["*", <new>]` keeps that reach as a live plan-time answer (a machine that installs
/// another agent tomorrow still gets the bundle) and adds the named destinations on top. Writing
/// out the resolved set instead — a list of folders one machine happened to hold on one day —
/// froze the row to that day.
///
/// AN `"off"` SWITCH IS NOT A DESTINATION SET. It is the row's own negation — the bundle is not
/// delivered here at all — so an add over one is a re-activation, never an extend, and it takes
/// the ordinary replaced-prior path whose answer already says no single command restores an
/// `"off"`. Calling it an extend advertised `remove --dest <new>` as the inverse, and that removes
/// the last destination, drops the row, and resumes the feed EVERYWHERE — the opposite of the
/// state it claimed to restore.
///
/// A DESTINATION THE ROW ALREADY REACHES IS NOT AN ADDITION. `in_reach` is the row's own default
/// reach ([`reach_already_held`]), non-empty only while the row still stands for it, and an asked
/// entry inside it is a request for nothing: recording it would leave the receipt's undo a
/// NARROWING, because subtracting an in-reach entry is exactly what makes the collapse materialize
/// the token into an exact list. Such an entry drops like a string the line already spells, and an
/// ask that drops whole is the ordinary redundancy no-op.
///
/// Returns the value to write and, when this WAS a destination-only act over a standing row, what
/// it changed.
fn extend_dest(
    prior: Option<&EntryValue>,
    value: &EntryValue,
    in_reach: &[String],
) -> (EntryValue, Option<DestExtend>) {
    let (Some(prior), EntryValue::Fields(fields)) = (prior, value) else {
        return (value.clone(), None);
    };
    if fields.dest.is_none() && fields.mcp_dest.is_none() {
        return (value.clone(), None);
    }
    if matches!(prior, EntryValue::Off) {
        return (value.clone(), None);
    }
    let prior_fields = prior.fields();
    // AN EXTEND KEEPS THE ROW. It starts from what the file already spells and overlays only the
    // fields this add actually names, so a hand-written `subdir`, a channel's `mcp_dest`, or any
    // other standing setting survives an add that was only ever about destinations.
    let mut merged = crate::manifest::document::EntryFields {
        version: fields.version.clone().or(prior_fields.version.clone()),
        dest: prior_fields.dest.clone(),
        mcp_dest: prior_fields.mcp_dest.clone(),
        name: fields.name.clone().or(prior_fields.name.clone()),
        subdir: fields.subdir.clone().or(prior_fields.subdir.clone()),
        kind: fields.kind.clone().or(prior_fields.kind.clone()),
    };
    let mut added: Vec<String> = Vec::new();
    let mut mcp_dest_touched = false;
    for (asked, standing, is_row_dest) in [
        (&fields.dest, &prior_fields.dest, true),
        (&fields.mcp_dest, &prior_fields.mcp_dest, false),
    ] {
        let Some(asked) = asked else { continue };
        let mut base = match standing {
            Some(standing) => standing.clone(),
            // A row's OWN `dest` starts from the token that stands for what it reaches today, so
            // the add is purely additive. A channel's `mcp_dest` is a FILTER over members, not a
            // reach the token could speak for, and starts empty exactly as it always has.
            None if is_row_dest => vec![crate::manifest::dest::DEFAULT_REACH.to_owned()],
            None => Vec::new(),
        };
        for entry in asked {
            if is_row_dest && in_reach.contains(entry) {
                continue;
            }
            if !base.contains(entry) {
                base.push(entry.clone());
                added.push(entry.clone());
            }
        }
        if is_row_dest {
            merged.dest = Some(base);
        } else {
            mcp_dest_touched = true;
            merged.mcp_dest = Some(base);
        }
    }
    // A MIXED MUTATION IS NOT A DESTINATION-ONLY ACT. `add <ref>@<new-pin> --dest B` moves the pin
    // AND adds a folder; a receipt that spoke only of the folder left the moved pin unsaid, and
    // its `remove --dest B` inverse left it moved. Anything beyond the destinations having changed
    // hands the whole write back to the ordinary replaced-prior path, which names the prior value
    // and offers only an undo that verifiably restores it.
    let mut without_dest = merged.clone();
    without_dest.dest = prior_fields.dest.clone();
    without_dest.mcp_dest = prior_fields.mcp_dest.clone();
    let dest_only = without_dest == prior_fields;
    if added.is_empty() {
        // NOTHING NEW TO RECORD IS A TRUE NO-OP, and the row goes back exactly as it stood — the
        // asked entry is already on the line. (`write_row`'s redundancy arm then answers
        // `nothing changed`, which is the documented promise for naming a destination a line
        // already has.) A MIXED add still carries its other change through.
        return (
            if dest_only {
                prior.clone()
            } else {
                EntryValue::Fields(merged)
            },
            None,
        );
    }
    if !dest_only {
        return (EntryValue::Fields(merged), None);
    }
    let default_reach = merged
        .dest
        .as_deref()
        .is_some_and(crate::manifest::dest::carries_default_reach);
    (
        EntryValue::Fields(merged),
        Some(DestExtend {
            change: DestChange {
                added,
                default_reach,
            },
            mcp_dest: mcp_dest_touched,
        }),
    )
}

/// What a destination-only add did, and whether its inverse can be a subtraction.
struct DestExtend {
    change: DestChange,
    /// This add extended a channel's `mcp_dest`, which `remove --dest` does not subtract from —
    /// there is no argv that writes that array. The exact inverse is then the prior VALUE's own
    /// restore, where one is spellable at all, and otherwise there is none.
    mcp_dest: bool,
}

/// The inverse of a destination EXTEND: `topos remove [-g] <ref> --dest <new>` naming ONLY what
/// this add put there. The whole-row `remove` would drop destinations that predate it — no undo
/// beats a wrong one, and this one is exact.
fn undo_dest_add(reference: &str, global: bool, added: &[String]) -> Vec<String> {
    let mut argv = vec!["topos".to_owned(), "remove".to_owned()];
    if global {
        argv.push("-g".to_owned());
    }
    argv.push(reference.to_owned());
    for entry in added {
        argv.push("--dest".to_owned());
        argv.push(entry.clone());
    }
    argv
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

/// Record a workspace/forge/local reference in `target`. `pin` writes the explicit version row;
/// `None` tracks what the reference currently serves.
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_in(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    reference: &str,
    pin: Option<&str>,
) -> Result<(), ClientError> {
    let value = match pin {
        Some(p) => EntryValue::Pin(p.to_owned()),
        None => EntryValue::Star,
    };
    write_row(
        ctx,
        data,
        target,
        reference,
        &value,
        value
            .declared_kind()
            .unwrap_or(crate::bundle_kind::BundleKind::Skill),
    )
}

/// Record a PATH-adopted bundle in `target`: the dir-relative `./` spelling when the source sits
/// under the manifest's own folder (the committed, travels-with-the-repo form), else the ABSOLUTE
/// path — written into that same file either way.
///
/// The scope is the CALLER's (the folder it stands in, or `-g`) and is never rerouted here: an
/// out-of-tree source under a project manifest is a machine-local fact spelled absolutely, in the
/// file the person asked for. Sending it to the global file instead used to edit a document
/// nobody had named.
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_path_in(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    source: &Path,
) -> Result<(), ClientError> {
    note_added_path_kind_in(ctx, data, target, source, None)
}

/// [`note_added_path_in`] carrying a `-a`/`--dest` SELECTION: the row becomes
/// `{ dest = [...] }`, so the adopted folder's copies are frozen to exactly those destinations —
/// the adopted source dir itself stays the user's own and is never retired by the freeze.
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_path_dest_in(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    source: &Path,
    dest: &[String],
) -> Result<(), ClientError> {
    note_added_path_row_in(ctx, data, target, source, None, Some(dest))
}

/// [`note_added_path_in`] carrying a BUNDLE KIND. `kind` is `None` for the ordinary skill adopt
/// (the row's value stays the bare `"*"`, exactly as before) and `Some(BundleKind::Mcp)` for an
/// MCP server folder, whose row becomes the inline table `{ kind = "mcp" }` — the one place the
/// manifest records what a local folder IS, because a local path has no catalog to ask.
///
/// The value is still ONE row at ONE key, so `remove` stays `add`'s exact file inverse: it drops
/// the whole row, kind and all.
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_path_kind_in(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    source: &Path,
    kind: Option<BundleKind>,
) -> Result<(), ClientError> {
    note_added_path_row_in(ctx, data, target, source, kind, None)
}

/// [`note_added_path_kind_in`] carrying a `-a`/`--dest` selection too — the MCP door's row write
/// (`{ kind = "mcp", dest = [...] }`), so the converge narrows to exactly the named config files.
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_path_kind_dest_in(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    source: &Path,
    kind: Option<BundleKind>,
    dest: &[String],
) -> Result<(), ClientError> {
    let dest = (!dest.is_empty()).then_some(dest);
    note_added_path_row_in(ctx, data, target, source, kind, dest)
}

/// The shared core of the path-row writers: kind and/or dest onto ONE row at ONE key.
///
/// # Errors
/// As [`write_row`].
fn note_added_path_row_in(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    source: &Path,
    kind: Option<BundleKind>,
    dest: Option<&[String]>,
) -> Result<(), ClientError> {
    // Canonicalize best-effort (symlinks resolve; a vanished dir keeps the typed spelling), then
    // follow a LINK SHELL to the folder it stands for — the row must name the folder the adopt
    // actually recorded, or the next reconcile would demand a directory topos does not track.
    let source_abs = match source.canonicalize() {
        Ok(canon) => super::origin_dir_or_self(&canon),
        Err(_) if source.is_absolute() => source.to_path_buf(),
        Err(_) => ctx
            .roots
            .as_ref()
            .and_then(|r| r.cwd.as_ref())
            .map(|c| c.join(source))
            .unwrap_or_else(|| source.to_path_buf()),
    };
    // The `source:` line for a path row is the FOLDER, canonical — never the row's portable
    // `./…` spelling, which is only meaningful beside the file that holds it.
    data.source = Some(source_abs.display().to_string());
    let reference = path_row_key(target, &source_abs);
    let value = match (kind, dest) {
        (None, None) => EntryValue::Star,
        (kind, dest) => EntryValue::Fields(crate::manifest::document::EntryFields {
            kind: kind.map(|k| k.as_str().to_owned()),
            dest: dest.map(<[String]>::to_vec),
            ..crate::manifest::document::EntryFields::default()
        }),
    };
    write_row(
        ctx,
        data,
        target,
        &reference,
        &value,
        value
            .declared_kind()
            .unwrap_or(crate::bundle_kind::BundleKind::Skill),
    )
}

/// [`note_added_path_in`] resolving the target from the scope flag — the BACKSTOP for a caller
/// that has not already resolved it. The composition root always has (`add_scope`, which refuses
/// before a byte moves), so this exists for the fixture rig and its own tests.
///
/// # Errors
/// [`ClientError::NoManifest`] when no `topos.toml` covers the working directory and `-g` was not
/// asked for; otherwise as [`write_row`].
#[cfg(any(test, feature = "test-fixtures"))]
pub(crate) fn note_added_path(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    source: &Path,
    global: bool,
) -> Result<(), ClientError> {
    let target = edit_target(ctx, global)?.ok_or(ClientError::NoManifest)?;
    note_added_path_in(ctx, data, &target, source)
}

/// The KEY a local-path row takes in `target`'s file — the ONE derivation the row write and every
/// later lookup of that row share.
///
/// Compared BOTH ways canonically: a source under the manifest's folder ALWAYS records the
/// dir-relative `./path`, never an absolute one (the file itself still lands at `target.path` —
/// the dir is canonicalized only to compute the spelling).
pub(super) fn path_row_key(target: &EditTarget, source_abs: &Path) -> String {
    let dir_c = canonical_or(&target.dir);
    if target.scope == ManifestScope::Project && source_abs.starts_with(&dir_c) {
        path_reference(&dir_c, source_abs)
    } else {
        source_abs.display().to_string()
    }
}

/// The VALUE `target`'s file already spells for `reference`, or `None` when it spells no such row
/// (or has no file at all).
///
/// # Errors
/// A read failure, or a manifest the grammar refuses (typed, naming the file).
pub(super) fn row_value(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    reference: &str,
) -> Result<Option<EntryValue>, ClientError> {
    let Some(text) = read_text(ctx, &target.path)? else {
        return Ok(None);
    };
    let doc = crate::manifest::document::parse_manifest(&text, target.scope)
        .map_err(|e| corrupt(&target.path, &e))?;
    Ok(doc
        .rows
        .into_iter()
        .find(|row| row.reference == reference)
        .map(|row| row.value))
}

/// A path in the scope's own `dest` dialect — see [`dest_display`].
pub(super) fn dest_spelling(ctx: &Ctx<'_>, target: &EditTarget, path: &Path) -> String {
    dest_display(ctx, target, path)
}

/// The manifest spelling of an adopted local path: relative to the manifest's folder when the
/// source sits under it (`./tools/my-skill`), else the absolute path.
pub(crate) fn path_reference(manifest_dir: &Path, source_abs: &Path) -> String {
    match source_abs.strip_prefix(manifest_dir) {
        Ok(rel) => format!("./{}", rel.display()),
        Err(_) => source_abs.display().to_string(),
    }
}

/// Whether `target`'s file already carries a local-path row pointing at `source_abs` — the
/// question that separates "this folder is recorded here" from "a record outliving the row that
/// asked for it". Keys resolve the way delivery resolves them (`~/` against the machine home, an
/// absolute key verbatim, anything else against the folder the manifest governs) and are compared
/// canonically, so `./tools/x` and the same folder spelled in full are one row, not two.
///
/// # Errors
/// A read failure, or a manifest the grammar refuses (typed, naming the file).
pub(crate) fn path_row_claims(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    source_abs: &Path,
) -> Result<bool, ClientError> {
    let Some(text) = read_text(ctx, &target.path)? else {
        return Ok(false);
    };
    let doc = crate::manifest::document::parse_manifest(&text, target.scope)
        .map_err(|e| corrupt(&target.path, &e))?;
    Ok(doc.rows.iter().any(|row| match &row.shape {
        KeyShape::LocalPath { raw } => canonical_or(&row_dir(ctx, target, raw)) == *source_abs,
        _ => false,
    }))
}

/// The folder a local-path row's key names, before canonicalization.
fn row_dir(ctx: &Ctx<'_>, target: &EditTarget, raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(roots) = &ctx.roots
    {
        return roots.home.join(rest);
    }
    if Path::new(raw).is_absolute() {
        return PathBuf::from(raw);
    }
    target.dir.join(raw.trim_start_matches("./"))
}

/// Whether a row's LOCAL FOLDER is gone: the row names a path on this machine, and nothing is
/// there. It is the one fact that settles a row drop outright — there are no files to scan for
/// local edits, so nothing can be lost, and `topos add <that path>` can never restore it, so no
/// undo is offered either.
fn row_folder_gone(ctx: &Ctx<'_>, target: &EditTarget, row: &PlanRow) -> bool {
    match &row.shape {
        KeyShape::LocalPath { raw } => !ctx.fs.exists(&row_dir(ctx, target, raw)),
        _ => false,
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
/// `dest` carries the import's `-a` SELECTION onto the row, as destination dirs. A selector
/// decides where the bytes land, and a decision only this invocation remembers is not a
/// decision at all — the next `update` would re-land the copy in the default agent dir. Written
/// as the `dest` field (legal on every forge shape), it is a standing fact the reconcile's
/// forge arms plan against; an empty slice records the plain value exactly as before.
///
/// # Errors
/// As [`write_row`].
pub(crate) fn note_added_remote(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    target: &EditTarget,
    dest: &[String],
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
    if dest.is_empty() {
        return note_added_in(ctx, data, target, &reference, pin);
    }
    // The fields must be LEGAL for the reference's own shape — a whole-repo row takes `dest`
    // and nothing else, so a pinned repo-root import has no spelling that carries both. The PIN
    // wins there (it decides which bytes; the selection only decides where they sit) and the
    // receipt says the selection could not be recorded, rather than a row the file would refuse
    // or a pin quietly dropped.
    let shape = crate::manifest::keys::classify_key(&reference)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;
    let version_legal = crate::manifest::document::legal_fields(&shape).contains(&"version");
    if pin.is_some() && !version_legal {
        note_added_in(ctx, data, target, &reference, pin)?;
        push_note(
            data,
            format!(
                "the `-a` selection is not recorded on `{reference}` — a whole-repo row cannot \
                 carry both a commit pin and a dest list; name the skill (`{reference}/<skill>`) \
                 to keep it"
            ),
        );
        return Ok(());
    }
    let value = EntryValue::Fields(crate::manifest::document::EntryFields {
        version: pin.filter(|_| version_legal).map(str::to_owned),
        dest: Some(dest.to_vec()),
        ..Default::default()
    });
    write_row(
        ctx,
        data,
        target,
        &reference,
        &value,
        value
            .declared_kind()
            .unwrap_or(crate::bundle_kind::BundleKind::Skill),
    )
}

/// The `dest` spellings a `-a` selection records for its slugs — the scope-correct skills-root
/// DEFAULT spelling per slug (`~/`-abbreviated in the global file, the relative project dir in
/// a project file), deduped in selection order. A slug with no dir at this scope contributes
/// nothing (the import itself already refused or resolved it).
pub(crate) fn dest_for_selected_agents(slugs: &[String], scope: ManifestScope) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for slug in slugs {
        if let Some(d) = crate::manifest::dest::skills_dest_spelling(slug, scope)
            && !out.contains(&d)
        {
            out.push(d);
        }
    }
    out
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
                // BOTH KINDS. The delivery answer keeps connected servers in their own list, and
                // reading only the file bundles made every caller here kind-blind: an
                // `add <host>/<ws>/<server>` of a server the feed already delivers wrote a
                // redundant row, while the same add of a skill correctly wrote nothing and said
                // so. The offline branch below has always counted both — the two must agree.
                // A REVOKED revision still counts: the wire's own word for it is "pulled back
                // after publication — still delivered, and disclosed". The offline branch's skip
                // is `withdrawn`, which is the opposite fact (upstream stopped delivering it).
                for m in &snapshot.mcp_servers {
                    out.push(FeedItem {
                        host: session.host.clone(),
                        workspace: session.workspace_name.clone(),
                        name: m.name.clone(),
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
// A handful of these exist per invocation, each carrying the row it speaks for; boxing a variant
// would buy indirection and nothing else.
#[allow(clippy::large_enum_variant)]
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
    /// minus the removed ones. `names` is a LIST because a set can only be split ONCE: the rewrite
    /// is "this line, replaced by its members", and applying two per-target splits in sequence
    /// would have the second one re-write the member the first had just removed.
    SetSplit {
        set: PlanRow,
        names: Vec<String>,
        members: Vec<String>,
        /// Survivors that ALREADY have their own explicit row in this file — explicit beats set,
        /// so the split leaves those rows untouched and writes new rows only for the rest.
        keeps_own: Vec<String>,
    },
    /// A `-a`/`--dest` NARROWING: subtract the named destination(s) from the row's `dest` and
    /// retire that copy — the row survives with the rest. A row that named no destination of its
    /// own — no `dest`, or the DEFAULT-REACH token — first materializes its CURRENT resolved set
    /// (the only reading under which subtraction means anything), so the result is an exact row of
    /// the remaining destinations; a remainder that still holds the whole default reach collapses
    /// back onto the token instead of freezing.
    DestNarrow {
        /// The standing row (`None` = a feed-delivered bundle with no row — the narrow writes a
        /// fresh dest-frozen row).
        row: Option<PlanRow>,
        reference: String,
        name: String,
        /// `(host, workspace)` for a workspace bundle — the receipt's qualification + the undo's
        /// `@ws` sugar.
        workspace: Option<(String, String)>,
        /// The row's value AFTER the subtraction.
        value: EntryValue,
        /// The destination entries leaving (dest spellings — what the receipt names).
        subtract: Vec<String>,
        /// How many destinations the row still names — `None` when it names none of its own and
        /// stands for its DEFAULT REACH, which is not a number.
        remaining: Option<usize>,
        /// Whether the row's current resolved set had to be written out because the row named no
        /// destination of its own (the materialize disclosure).
        materialized: bool,
        /// What the bundle IS — the vocabulary its destinations are spelled in.
        kind: BundleKind,
    },
    /// A `-a`/`--dest` DETACH of a CLAIMED folder — one the person brought under management with
    /// `add <path> --as <bundle>`. The record forgets the folder and NOT ONE BYTE MOVES: topos
    /// never created it, so there is nothing of its making to retire. The row is touched only to
    /// take back the destination the claim itself put on it, which is the claim's exact inverse.
    ClaimDetach {
        /// The standing row the claim's own destination entry rides — `None` for a bundle no row
        /// of this file spells (then nothing here is edited at all).
        row: Option<PlanRow>,
        /// The row key, for the write and the reproof.
        reference: String,
        name: String,
        /// The record whose placements are dropped.
        record: SkillId,
        /// The claimed placement dirs, as recorded.
        dirs: Vec<String>,
        /// The row's value after subtracting the claim's entries — `None` leaves the file
        /// untouched, which is the whole answer for a row that never named destinations.
        value: Option<EntryValue>,
        /// The bundle's own re-claim spelling, for the receipt's inverse.
        origin: Option<String>,
    },
}

impl Arm {
    fn name(&self) -> String {
        match self {
            Arm::RowDrop { name, .. }
            | Arm::OffWrite { name, .. }
            | Arm::DestNarrow { name, .. }
            | Arm::ClaimDetach { name, .. } => name.clone(),
            Arm::FeedDrop { workspace, .. } => workspace.clone(),
            Arm::SetSplit { names, .. } => names.join(", "),
        }
    }
}

/// Fold every SetSplit against the SAME set line into ONE arm: the split writes the members minus
/// ALL the removed targets, and the describe reflects that combined result. Applied sequentially,
/// two splits of one line would each rebuild it from the full member list and the earlier
/// removal would come straight back. Other arms pass through in order.
fn coalesce_set_splits(arms: Vec<Arm>) -> Vec<Arm> {
    let mut out: Vec<Arm> = Vec::with_capacity(arms.len());
    for arm in arms {
        let Arm::SetSplit {
            set,
            names,
            members,
            keeps_own,
        } = arm
        else {
            out.push(arm);
            continue;
        };
        let existing = out
            .iter_mut()
            .find(|a| matches!(a, Arm::SetSplit { set: s, .. } if s.reference == set.reference));
        match existing {
            Some(Arm::SetSplit {
                names: have,
                keeps_own: have_keeps,
                ..
            }) => {
                have.extend(names);
                have.dedup();
                have_keeps.extend(keeps_own);
                have_keeps.dedup();
                // A survivor that is ITSELF being removed is no survivor.
                let removed = have.clone();
                have_keeps.retain(|m| !removed.contains(m));
            }
            _ => {
                let mut keeps_own = keeps_own;
                keeps_own.retain(|m| !names.contains(m));
                out.push(Arm::SetSplit {
                    set,
                    names,
                    members,
                    keeps_own,
                });
            }
        }
    }
    out
}

/// `topos remove -g <token>…` — edit THIS MACHINE's global file: drop a row, drop a feed row, or
/// write the `"off"` switch for something the feed delivers — or, with `-a`/`--dest`, subtract
/// just those destinations from a row. Never touches the server: what a workspace gives a person
/// is managed on the web.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a token the file (and the feed) do not carry — with the
/// cross-scope hint when the project manifest does; the narrowing refusals
/// ([`ClientError::SharedCopyOnly`] / [`ClientError::SelectionRefused`]); a filesystem failure.
pub(crate) fn remove_global(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    tokens: &[String],
    via: Option<&str>,
    yes: bool,
    selection: &super::dest_select::Selection,
) -> Result<RemoveOutcome, ClientError> {
    let target = global_target(ctx);
    // THE LOCK COMES FIRST — before the read that decides the edit, not just around the write.
    // Every arm below is computed FROM the file (which rows exist, which survivors of a split
    // already have their own row), so a read outside the lock and a write inside it are two
    // different documents: the second writer would rebuild the set line from members the first
    // writer had already re-homed, and silently drop that row.
    let guard = lock_manifest(ctx, &target.path)?;
    let arms = resolve_arms(ctx, connect, &target, tokens, via)?;
    let mut resolved = Vec::with_capacity(tokens.len());
    for (token, arm) in tokens.iter().zip(arms) {
        match arm {
            Some(a) => resolved.push(a),
            None => return Err(miss(ctx, token, true)?),
        }
    }
    if !selection.is_empty() {
        resolved = narrow_arms(ctx, &target, resolved, selection)?;
    }
    // A row still claims these records, so a retirement marker on one is stale — lifted before the
    // plan below reads the store, and before the eager rails walk it (see [`revive_rowed_records`]).
    if yes {
        revive_rowed_records(ctx, &target, &resolved);
    }
    let eager = eager_plan(ctx, &target, &resolved);
    let mut outcome = apply_arms(ctx, &target, tokens, resolved, via, selection, yes, true)?;
    // Row edits uninstall EAGERLY: with the row durably changed, the same machine-scope
    // update/cleanup the sweep runs converges NOW, so the copies the edit no longer demands
    // leave in this invocation (edited copies kept in place) and the receipt says what actually
    // moved. AFTER the writer lock releases — the reconcile is a reader of this file and may
    // itself take the manifest lock (the pending-governance converge), which would deadlock a
    // still-held guard. Best-effort by contract: the row edit already landed, so a transport
    // hiccup just moves the byte landing to the next sweep.
    drop(guard);
    if let RemoveOutcome::Applied(data) = &mut outcome {
        data.uninstalled = eager_cleanup(
            ctx,
            connect,
            &target,
            super::reconcile::UpdateScope::Machine,
            &eager,
            &mut data.items,
        );
    }
    Ok(outcome)
}

/// The canonical reference of a standing `"off"` row in the GLOBAL file that `token` names —
/// `Ok(None)` when no row answers to it.
///
/// This is what keeps the switch's inverse spellable the way the switch was written. `remove -g
/// <bare-name>` resolves a bare name (see [`resolve_one`]) and writes `"off"` under the canonical
/// reference; the reference grammar does not own bare names, so without this the inverse `add -g
/// <bare-name>` would fall through to the local adopt ladder and answer with something about
/// re-forking or an already-tracked directory — never about the switch it was asked to lift.
/// Resolving here hands `add`'s reference arm the spelling it understands, and the off-lift it
/// already implements does the rest.
///
/// # Errors
/// [`ClientError::AmbiguousName`] when two workspaces' rows are both switched off under that name
/// (each qualified spelling listed — picking one on alphabetical luck would turn the wrong feed
/// back on); a read failure, or a manifest the grammar refuses.
pub(crate) fn off_row_for(ctx: &Ctx<'_>, token: &str) -> Result<Option<String>, ClientError> {
    let target = global_target(ctx);
    let plan = plan_for(ctx, &target)?;
    let mut refs: Vec<String> = plan
        .offs
        .iter()
        .filter(|row| row_matches(token, None, row))
        .map(|row| row.reference.clone())
        .collect();
    refs.dedup();
    match refs.len() {
        0 => Ok(None),
        1 => Ok(Some(refs.remove(0))),
        // A switch lives in the machine-wide file only, and this lookup reads exactly that file —
        // so every re-spelling it offers is a `-g` one, whatever else the invocation carried.
        _ => Err(ambiguous(
            token,
            refs.into_iter().map(TargetCandidate::plain),
            true,
        )),
    }
}

/// `topos remove <token>…` — edit THIS FOLDER's manifest. `Ok(None)` when no token names a
/// manifest row (the caller falls through to the classic tracked/untracked removal).
///
/// # Errors
/// [`ClientError::NoManifest`] when no manifest covers this folder and a token is spelled as a
/// manifest ROW (a reference, or a path) — such a token names nothing else, so the classic ladder's
/// answers would name the wrong fix; [`ClientError::InvalidArgument`] on a mixed batch (some tokens
/// are manifest rows, some are not), or when a token is a GLOBAL row spelled without `-g`; a
/// filesystem failure.
pub(crate) fn remove_project(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    tokens: &[String],
    via: Option<&str>,
    yes: bool,
    selection: &super::dest_select::Selection,
) -> Result<Option<RemoveOutcome>, ClientError> {
    let Some(target) = project_target(ctx)? else {
        // No file here to edit. A ROW-SPELLED token (a reference, or a path) only ever means a
        // manifest line, so it refuses toward the two scopes rather than falling through to the
        // classic ladder, whose "no such skill" would name the wrong fix. A BARE NAME still falls
        // through — an untracked copy, the built-in, or the machine-delivered refusal.
        if tokens.iter().any(|t| row_spelled(t)) {
            return Err(ClientError::NoManifest);
        }
        return Ok(None);
    };
    // Held across the resolve AND the apply — see [`remove_global`].
    let guard = lock_manifest(ctx, &target.path)?;
    let arms = resolve_arms(ctx, connect, &target, tokens, via)?;
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
    let mut resolved: Vec<Arm> = arms.into_iter().flatten().collect();
    if !selection.is_empty() {
        resolved = narrow_arms(ctx, &target, resolved, selection)?;
    }
    // The stale-marker lift — see [`remove_global`].
    if yes {
        revive_rowed_records(ctx, &target, &resolved);
    }
    let eager = eager_plan(ctx, &target, &resolved);
    let mut outcome = apply_arms(ctx, &target, tokens, resolved, via, selection, yes, false)?;
    // The eager uninstall, after the writer lock releases — see [`remove_global`].
    drop(guard);
    if let RemoveOutcome::Applied(data) = &mut outcome {
        data.uninstalled = eager_cleanup(
            ctx,
            connect,
            &target,
            super::reconcile::UpdateScope::Here,
            &eager,
            &mut data.items,
        );
    }
    Ok(Some(outcome))
}

/// Resolve every token against the target scope (all-or-none is the caller's discipline).
fn resolve_arms(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    target: &EditTarget,
    tokens: &[String],
    via: Option<&str>,
) -> Result<Vec<Option<Arm>>, ClientError> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let plan = plan_for(ctx, target)?;
    let host = default_host(ctx);
    // `--via` names WHICH set line a member removal rewrites — validated once, canonically: it
    // must be a channel or repo reference (the only shapes that ARE lines with members).
    let via_ref: Option<String> = match via {
        None => None,
        Some(v) => {
            let canonical = keys::parse_input(v, host.as_deref())
                .map_err(|e| ClientError::InvalidArgument(e.message))?
                .shape
                .canonical();
            if !matches!(
                keys::classify_key(&canonical),
                Ok(KeyShape::Channel { .. } | KeyShape::RepoSet { .. })
            ) {
                return Err(ClientError::InvalidArgument(format!(
                    "--via must name a channel or repo line (`@ws/channels/<name>`, \
                     `owner/repo`) — `{v}` is neither"
                )));
            }
            Some(canonical)
        }
    };
    // The feed + set expansions are read ONCE per invocation (each may dial).
    let mut feeds: Option<Vec<FeedItem>> = None;
    let mut sets: Option<Vec<(PlanRow, Vec<String>)>> = None;
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let canonical = keys::parse_input(token, host.as_deref())
            .ok()
            .map(|r| r.shape.canonical())
            // A PATH-shaped token names a directory, and a directory has many spellings while the
            // row `add` wrote holds exactly one of them. Re-spell the token as the reference of
            // the row whose folder it IS, so every way of naming that folder reaches the same row.
            .map(|c| path_row_reference(ctx, target, &plan, token).unwrap_or(c));
        out.push(resolve_one(
            ctx,
            connect,
            target,
            &plan,
            token,
            canonical.as_deref(),
            via_ref.as_deref(),
            &mut feeds,
            &mut sets,
        )?);
    }
    Ok(out)
}

/// Resolve ONE token against everything the target file (and the feed it adopts) offers.
///
/// EVERY source is consulted before anything is decided — the file's feed row, its explicit rows,
/// the bundles the feed delivers, and every SET line's current members — because a token that two
/// of them answer names two different things, and there is no order in which picking one of them
/// is right. The old shape returned at the first hit, so a name carried by two channels removed it
/// from whichever expanded first, and a name the feed delivered hid a repo set that also carried
/// it. Now the candidates are gathered and the count decides: one answers, several refuse with the
/// qualified references, and the caller re-spells the one it meant.
///
/// Candidates are keyed by the QUALIFIED SPELLING they act on and deduped by it, in the order
/// below — one bundle reached two ways (its own row, and the feed that also delivers it) is ONE
/// candidate, and the order picks the arm that edits this file most directly. A SET member is
/// keyed by its own reference AND the line it comes from: two channels carrying one bundle are two
/// different edits of two different lines, and splitting whichever expanded first leaves the other
/// line delivering it — a removal that appears to do nothing.
///
/// An EXACT spelling is always an answer, never an ambiguity: a token that literally is a row's
/// reference names that row, even when a set line also carries the bundle. That is what keeps the
/// refusal actionable — the candidates it lists can be typed back. And where two SET lines both
/// carry the bundle, no bare spelling can pick one — `--via <channel-ref>` names the line, so the
/// set-versus-set refusal lists the exact `--via` invocations that each resolve.
#[allow(clippy::too_many_arguments)]
fn resolve_one(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    target: &EditTarget,
    plan: &ScopePlan,
    token: &str,
    canonical: Option<&str>,
    via: Option<&str>,
    feeds: &mut Option<Vec<FeedItem>>,
    sets: &mut Option<Vec<(PlanRow, Vec<String>)>>,
) -> Result<Option<Arm>, ClientError> {
    // `--via` narrows the resolution to ONE set line's member rewrite — the person named the
    // line, so nothing else (a row, the feed, another set) competes, and a miss is a typed
    // refusal rather than a fall-through to arms the flag does not select.
    if let Some(via_ref) = via {
        return resolve_via(ctx, connect, target, plan, token, canonical, via_ref, sets);
    }
    let mut found: Vec<Candidate> = Vec::new();
    let is_exact = |reference: &str| reference == token || canonical == Some(reference);

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
        found.push(Candidate {
            candidate: TargetCandidate::plain(c),
            exact: true,
            arm: Arm::FeedDrop {
                reference: c.to_owned(),
                workspace,
                bundles,
            },
        });
    }
    // 2. An explicit row, by canonical reference or by the name it delivers under. A BARE NAME can
    //    name more than one row (two workspaces, or a workspace and a forge repo, that each carry a
    //    `deploy`) — taking the first would remove someone's row on alphabetical luck.
    for row in all_rows(plan)
        .into_iter()
        .filter(|r| row_matches(token, canonical, r))
    {
        if matches!(row.value, EntryValue::Off) {
            return Err(ClientError::InvalidArgument(format!(
                "'{token}' is already switched off on this machine — `topos add -g {}` turns it \
                 back on",
                row.reference
            )));
        }
        let name = row.display_name();
        found.push(Candidate {
            candidate: TargetCandidate::plain(row.reference.clone()),
            exact: is_exact(&row.reference),
            arm: Arm::RowDrop { row, name },
        });
    }
    // 3. The workspace FEED delivers it: the one machine-local negative is the `"off"` switch, and
    //    it lives in the global file only. Same discipline — two connected workspaces can both
    //    deliver a `deploy`, and switching off the wrong one is silent.
    if target.scope == ManifestScope::Global {
        let items = feeds.get_or_insert_with(|| feed_items(ctx, connect));
        for item in items.iter().filter(|i| {
            !i.declined && feed_matches(token, canonical, i) && plan.has_feed(&i.host, &i.workspace)
        }) {
            let reference = format!("{}/{}/{}", item.host, item.workspace, item.name);
            found.push(Candidate {
                candidate: TargetCandidate::plain(reference.clone()),
                exact: is_exact(&reference),
                arm: Arm::OffWrite {
                    reference,
                    name: item.name.clone(),
                    workspace: item.workspace.clone(),
                },
            });
        }
    }
    // 4. A SET row in this file delivers it — the removal is a rewrite of that line. A member is
    //    qualified by the reference its OWN row would carry (`<ws>/<bundle>`, `<repo>/<skill>`),
    //    which is both the paste-ready spelling and the identity the dedup compares.
    let expansions = sets.get_or_insert_with(|| expand_sets(ctx, connect, target, plan));
    let rows = all_rows(plan);
    for (row, members) in expansions.iter() {
        let want = member_of(row, canonical);
        let Some(member) = members
            .iter()
            .find(|m| m.as_str() == token || Some(m.as_str()) == want.as_deref())
        else {
            continue;
        };
        // Explicit beats set: a survivor whose own row this file already spells KEEPS it —
        // the split must never replace a stronger pin/fields row with the set's value.
        let keeps_own: Vec<String> = members
            .iter()
            .filter(|m| m.as_str() != member.as_str())
            .filter(|m| {
                member_reference(row, m).is_some_and(|r| rows.iter().any(|pr| pr.reference == r))
            })
            .cloned()
            .collect();
        let member_ref =
            member_reference(row, member).unwrap_or_else(|| format!("{}/{member}", row.reference));
        found.push(Candidate {
            // The EXACT invocation that selects this line's rewrite — what the ambiguity refusal
            // offers, STRUCTURED (`--via` names the set line the split targets). The member
            // reference stays its own field, so a path-shaped one is never re-tokenized.
            candidate: TargetCandidate::via(member_ref, row.reference.clone()),
            // A set member is never spelled in the file — the token cannot BE it.
            exact: false,
            arm: Arm::SetSplit {
                set: row.clone(),
                names: vec![member.clone()],
                members: members.clone(),
                keeps_own,
            },
        });
    }

    // The DECISION. An exact spelling settles it; otherwise one distinct thing answers, or the
    // token names several and refuses with all of them.
    if found.iter().any(|c| c.exact) {
        found.retain(|c| c.exact);
    }
    // Dedup on the RENDERED spelling: two candidates that would print (and run) identically are
    // one way out, however they were reached.
    let mut seen: Vec<TargetCandidate> = Vec::new();
    found.retain(|c| {
        let first = !seen.iter().any(|s| s.spelling() == c.candidate.spelling());
        if first {
            seen.push(c.candidate.clone());
        }
        first
    });
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found.remove(0).arm)),
        // The candidates are lines of THIS target's file, so the offered re-spellings must resolve
        // against the same one: `-g` where this edit is the machine-wide file, bare where it is
        // this folder's.
        _ => Err(ambiguous(
            token,
            seen,
            target.scope == ManifestScope::Global,
        )),
    }
}

/// One thing a `remove` token could name, with the QUALIFIED CANDIDATE the refusal would offer and
/// whether the token spelled it exactly (see [`resolve_one`]).
struct Candidate {
    candidate: TargetCandidate,
    exact: bool,
    arm: Arm,
}

/// The `--via <set-ref>` resolution: exactly ONE set line's member-minus-one rewrite. The person
/// named the line, so this is a selection, never a search — the line must exist in this file and
/// the token must be one of its current members; each miss is its own typed refusal (the flag
/// must not fall through to arms it does not select).
#[allow(clippy::too_many_arguments)]
fn resolve_via(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    target: &EditTarget,
    plan: &ScopePlan,
    token: &str,
    canonical: Option<&str>,
    via_ref: &str,
    sets: &mut Option<Vec<(PlanRow, Vec<String>)>>,
) -> Result<Option<Arm>, ClientError> {
    let expansions = sets.get_or_insert_with(|| expand_sets(ctx, connect, target, plan));
    let Some((row, members)) = expansions.iter().find(|(row, _)| row.reference == via_ref) else {
        return Err(ClientError::InvalidArgument(format!(
            "--via {via_ref} names no channel/repo line in this file — `topos list` shows what \
             each scope delivers"
        )));
    };
    let want = member_of(row, canonical);
    let Some(member) = members
        .iter()
        .find(|m| m.as_str() == token || Some(m.as_str()) == want.as_deref())
    else {
        return Err(ClientError::InvalidArgument(format!(
            "'{token}' does not come from the line `{via_ref}`{}",
            if members.is_empty() {
                String::new()
            } else {
                format!(" (its current members: {})", members.join(", "))
            }
        )));
    };
    let rows = all_rows(plan);
    // Explicit beats set — identical to the bare resolution's split arm.
    let keeps_own: Vec<String> = members
        .iter()
        .filter(|m| m.as_str() != member.as_str())
        .filter(|m| {
            member_reference(row, m).is_some_and(|r| rows.iter().any(|pr| pr.reference == r))
        })
        .cloned()
        .collect();
    Ok(Some(Arm::SetSplit {
        set: row.clone(),
        names: vec![member.clone()],
        members: members.clone(),
        keeps_own,
    }))
}

// ---------------------------------------------------------------------------------------------
// The `-a`/`--dest` NARROWING — subtraction over a row's destination set
// ---------------------------------------------------------------------------------------------

/// Rewrite each resolved arm into its NARROWED form (see [`Arm::DestNarrow`]): the selection's
/// entries are subtracted from the row's `dest` — materialized first from the CURRENT resolved
/// destination set when the row names none. Removing the last destination keeps the arm's
/// whole-removal shape (a row is never left bare by subtraction — bare means default and would
/// resurrect copies).
///
/// # Errors
/// [`ClientError::SharedCopyOnly`] when the only copy is a shared folder several agents read;
/// [`ClientError::SelectionRefused`] for an arm subtraction cannot narrow (a feed line, a
/// channel/repo member) or a destination the bundle has no copy at; the selection's own
/// resolution errors.
fn narrow_arms(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    arms: Vec<Arm>,
    selection: &super::dest_select::Selection,
) -> Result<Vec<Arm>, ClientError> {
    let mut out = Vec::with_capacity(arms.len());
    for arm in arms {
        out.push(narrow_one(ctx, target, arm, selection)?);
    }
    Ok(out)
}

fn narrow_one(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    arm: Arm,
    selection: &super::dest_select::Selection,
) -> Result<Arm, ClientError> {
    let global = target.scope == ManifestScope::Global;
    // A CLAIMED folder is not a destination the engine fills — it is the person's own directory,
    // recorded as one of the bundle's places. Subtracting it must not route through the narrow's
    // materialize-and-freeze arm, which would write out a destination set the claim never asked
    // for and leave the add/remove pair no longer exact inverses.
    if let Some(detach) = claim_detach(ctx, target, &arm, selection)? {
        return Ok(detach);
    }
    match arm {
        Arm::FeedDrop { reference, .. } => Err(ClientError::SelectionRefused(format!(
            "`{reference}` is a whole feed and reaches every agent — narrow a single skill \
             instead (`topos remove -g <skill> -a <agent>`)"
        ))),
        Arm::SetSplit { set, names, .. } => Err(ClientError::SelectionRefused(format!(
            "'{}' comes from the {} line `{}` — a line's members cannot be narrowed per-agent; \
             give the member its own row first (`topos add{} <member-reference> -a <agent>`)",
            names.join("', '"),
            set.shape.noun(),
            set.reference,
            if global { " -g" } else { "" },
        ))),
        Arm::RowDrop { row, name } => {
            let ws = match &row.shape {
                KeyShape::WorkspaceBundle {
                    host, workspace, ..
                } => Some((host.clone(), workspace.clone())),
                _ => None,
            };
            let kind = row_kind(ctx, target, Some(&row), ws.as_ref(), &name);
            let entries = if kind.is_mcp() {
                selection.mcp_entries(target.scope)?
            } else {
                selection.skill_entries(target.scope)?
            };
            let row_fields = row.fields();
            // NARROWING IS EXACT. A row that names no destination of its own — no `dest` at all,
            // or the DEFAULT-REACH token standing for whatever this machine resolves today — has
            // its CURRENT resolved set materialized first, because that is the only reading under
            // which subtracting one destination means anything.
            let stood_for_default = row_fields
                .dest
                .as_deref()
                .is_none_or(crate::manifest::dest::carries_default_reach);
            let row_dest = if stood_for_default {
                let mut set = current_dest_roots(ctx, target, Some(&row), &name, kind)?;
                for entry in row_fields
                    .dest
                    .as_deref()
                    .map(crate::manifest::dest::named_entries)
                    .unwrap_or_default()
                {
                    if !set.contains(&entry) {
                        set.push(entry);
                    }
                }
                set
            } else {
                row_fields.dest.clone().unwrap_or_default()
            };
            let (subtract, remaining) = split_dest(
                ctx,
                target,
                selection,
                &name,
                ws.as_ref(),
                &row_dest,
                &entries,
                &record_copies(ctx, target, Some(&row), &name),
            )?;
            // THE COLLAPSE. A row carrying the token gives it up only when the subtraction
            // actually took something the DEFAULT REACH still holds — otherwise the removal took
            // a named addition and nothing else, and freezing the row would cost it every agent
            // installed after today for a subtraction that never touched them. The token stays,
            // with whatever the row still names beside it, which is what makes an `-a` add and its
            // `remove -a` undo exact inverses.
            let carries_token = row_fields
                .dest
                .as_deref()
                .is_some_and(crate::manifest::dest::carries_default_reach);
            let default_reach = carries_token
                .then(|| default_reach_roots(ctx, target, Some(&row), &name, kind))
                .transpose()?
                .unwrap_or_default();
            let back_to_default =
                carries_token && !subtract.iter().any(|s| default_reach.contains(s));
            if !back_to_default && remaining.is_empty() {
                // The last destination: the whole row goes (never a bare row).
                return Ok(Arm::RowDrop { row, name });
            }
            let mut new_dest: Vec<String> = if back_to_default {
                // What the ROW named, minus what left — never the materialized set, whose other
                // entries are the token's own reach written out.
                let mut v = vec![crate::manifest::dest::DEFAULT_REACH.to_owned()];
                v.extend(
                    row_fields
                        .dest
                        .as_deref()
                        .map(crate::manifest::dest::named_entries)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| remaining.contains(e)),
                );
                v
            } else {
                remaining.clone()
            };
            new_dest = crate::manifest::dest::normal_dest(&new_dest);
            let named_left = crate::manifest::dest::named_entries(&new_dest).len();
            let mut fields = row_fields;
            fields.dest = Some(new_dest.clone());
            // A value-level pin survives the narrow as the `version` field where its shape
            // carries one; a whole-repo pin cannot ride beside `dest` and is dropped, disclosed
            // by the ordinary write path refusing — kept simple: the pin is preserved only where
            // legal.
            if let EntryValue::Pin(p) = &row.value
                && crate::manifest::document::legal_fields(&row.shape).contains(&"version")
            {
                fields.version = Some(p.clone());
            }
            // A row left saying nothing but "my default reach" says exactly what the plain value
            // row says — the normal form's own collapse, applied at write time so an `-a` add and
            // its `remove -a` undo land the file back byte for byte. THE COLLAPSE NEVER DROPS THE
            // ROW: a subtraction edits where a row delivers, and deleting the demand itself is
            // `remove <name>`'s job alone. Dropping it here cost a row that PREDATED the add its
            // undo was inverting — a pin outlives a `dest` subtraction, and so does the line a
            // person wrote by hand.
            let value = match fields.version.as_deref() {
                _ if named_left > 0 || !plain_default_row(&fields) => EntryValue::Fields(fields),
                Some("*") | None => EntryValue::Star,
                Some(pin) => EntryValue::Pin(pin.to_owned()),
            };
            crate::manifest::document::check_row(&row.reference, target.scope, &value)
                .map_err(|e| ClientError::InvalidArgument(e.message))?;
            Ok(Arm::DestNarrow {
                reference: row.reference.clone(),
                row: Some(row),
                name,
                workspace: ws,
                value,
                subtract,
                // The row keeping its default reach names no count to state — "1 folder remains"
                // over a row that reaches every agent is a smaller number than the truth.
                remaining: (!back_to_default).then_some(remaining.len()),
                materialized: stood_for_default && !back_to_default,
                kind,
            })
        }
        Arm::OffWrite {
            reference,
            name,
            workspace,
        } => {
            // Feed-delivered, no row: the narrow MATERIALIZES the current resolved set into a
            // fresh dest-frozen row minus the subtracted one. Subtracting the only destination
            // keeps the `"off"` switch — the honest whole-machine end.
            let host = keys::classify_key(&reference)
                .ok()
                .and_then(|s| match s {
                    KeyShape::WorkspaceBundle { host, .. } => Some(host),
                    _ => None,
                })
                .unwrap_or_default();
            let ws = Some((host, workspace.clone()));
            let kind = row_kind(ctx, target, None, ws.as_ref(), &name);
            let entries = if kind.is_mcp() {
                selection.mcp_entries(target.scope)?
            } else {
                selection.skill_entries(target.scope)?
            };
            let current = current_dest_roots(ctx, target, None, &name, kind)?;
            // NO recorded copies at all: nothing to subtract FROM. This arm is FEED-delivered —
            // no row exists, so `split_dest`'s "its row was added" zero-state would be a
            // fabrication here; the honest variant names what stands (the feed delivers it) and
            // the whole-machine end that exists (`topos remove -g <name>` writes the `"off"`
            // switch).
            if current.is_empty() {
                return Err(ClientError::SelectionRefused(format!(
                    "'{name}' is feed-delivered and has no recorded copies in this scope yet — \
                     run `topos update` first, or `topos remove -g {name}` to switch it off \
                     entirely"
                )));
            }
            let (subtract, remaining) = split_dest(
                ctx,
                target,
                selection,
                &name,
                ws.as_ref(),
                &current,
                &entries,
                &record_copies(ctx, target, None, &name),
            )?;
            if remaining.is_empty() {
                return Ok(Arm::OffWrite {
                    reference,
                    name,
                    workspace,
                });
            }
            let value = EntryValue::Fields(crate::manifest::document::EntryFields {
                dest: Some(remaining.clone()),
                ..Default::default()
            });
            crate::manifest::document::check_row(&reference, target.scope, &value)
                .map_err(|e| ClientError::InvalidArgument(e.message))?;
            Ok(Arm::DestNarrow {
                row: None,
                reference,
                name,
                workspace: ws,
                value,
                subtract,
                remaining: Some(remaining.len()),
                materialized: true,
                kind,
            })
        }
        narrowed @ (Arm::DestNarrow { .. } | Arm::ClaimDetach { .. }) => Ok(narrowed),
    }
}

/// The CLAIM-DETACH read of a narrowing selection: does every folder it names belong to this
/// bundle because someone CLAIMED it (`add <path> --as <bundle>`)?
///
/// `Ok(None)` for everything else — an ordinary destination, an MCP bundle (whose destinations are
/// config files, which no claim is about), a bundle with no record here, and a MIXED selection
/// naming both a claimed folder and an engine-filled destination, which stays the ordinary
/// narrow's business.
///
/// The record's OWN source folder is never a candidate: for a local-path row that folder IS the
/// row, and dropping its placement would leave the row demanding a bundle with nowhere to be.
///
/// # Errors
/// The selection's own resolution errors; a store read failure.
fn claim_detach(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    arm: &Arm,
    selection: &super::dest_select::Selection,
) -> Result<Option<Arm>, ClientError> {
    let (row, name, ws) = match arm {
        Arm::RowDrop { row, name } => {
            let ws = match &row.shape {
                KeyShape::WorkspaceBundle {
                    host, workspace, ..
                } => Some((host.clone(), workspace.clone())),
                _ => None,
            };
            (Some(row), name, ws)
        }
        Arm::OffWrite {
            reference,
            name,
            workspace,
        } => {
            let host = match keys::classify_key(reference) {
                Ok(KeyShape::WorkspaceBundle { host, .. }) => host,
                _ => String::new(),
            };
            (None, name, Some((host, workspace.clone())))
        }
        // A SET line (a channel, a whole repo) delivers the bundle and no row of its own spells
        // it — but a CLAIM is scope-store state, not a line, so its inverse is reachable here and
        // nowhere else: the split refusal below would otherwise leave a documented inverse with no
        // command behind it. ONE named bundle only; a split naming several has no single folder.
        Arm::SetSplit { set, names, .. } => {
            let [one] = names.as_slice() else {
                return Ok(None);
            };
            let ws = match &set.shape {
                KeyShape::Channel {
                    host, workspace, ..
                } => Some((host.clone(), workspace.clone())),
                _ => None,
            };
            (None, one, ws)
        }
        _ => return Ok(None),
    };
    let kind = row_kind(ctx, target, row, ws.as_ref(), name);
    if kind.is_mcp() {
        return Ok(None);
    }
    // NAMING AN EXISTING COPY, not writing a row: the tokens go through the resolution that
    // question already has ([`super::dest_select::copy_tokens`] + [`names_copy`]), so a pasted
    // absolute path, the `~/`-spelled root, and the claimed folder's own path all name the copy
    // they obviously name. The manifest dialect is the wrong question here — it asks what a FILE
    // may record, and nothing is being recorded about a folder that only stops being managed.
    let tokens = selection.copy_tokens(target.scope)?;
    let Some(sctx) = scope_store_ctx(ctx, target) else {
        return Ok(None);
    };
    let Some(sid) = row_record(ctx, target, row, name).and_then(|s| SkillId::parse(&s).ok()) else {
        return Ok(None);
    };
    let Some(map) = crate::doc::read_map(sctx.fs, &sctx.layout.published(&sid).map)? else {
        return Ok(None);
    };
    // The row's OWN folder, for a local-path row — the one adopted dir a detach may never drop.
    let source = match row.map(|r| &r.shape) {
        Some(KeyShape::LocalPath { raw }) => Some(canonical_or(&row_dir(ctx, target, raw))),
        _ => None,
    };
    let mut dirs: Vec<String> = Vec::new();
    // The entries the CLAIMS THEMSELVES put on the row — never merely the roots they sit under.
    let mut added: Vec<String> = Vec::new();
    let mut matched: Vec<&String> = Vec::new();
    for (p, st) in map.placements.iter().zip(&map.placement_state) {
        let Some(claim) = &st.claim else {
            continue; // topos's own dir, or the bundle's adopted source — not a claim
        };
        let dir = Path::new(p);
        if source.as_deref() == Some(canonical_or(dir).as_path()) {
            continue;
        }
        let spelling = super::dest_select::copy_spellings(&sctx, dir);
        let hits: Vec<&String> = tokens
            .iter()
            .filter(|t| super::dest_select::names_copy(t, dir, &spelling))
            .collect();
        if hits.is_empty() {
            continue;
        }
        for t in hits {
            if !matched.contains(&t) {
                matched.push(t);
            }
        }
        dirs.push(p.clone());
        if let Some(entry) = claim.added_dest.clone()
            && !added.contains(&entry)
        {
            added.push(entry);
        }
    }
    // MIXED, or nothing claimed at all: the ordinary narrow owns the answer. A token that names no
    // claimed folder is somebody else's question, and answering half of one would be worse.
    if dirs.is_empty() || matched.len() != tokens.len() {
        return Ok(None);
    }
    let reference = row.map_or_else(
        || {
            arm_reference(arm)
                .map(str::to_owned)
                .unwrap_or_else(|| name.clone())
        },
        |r| r.reference.clone(),
    );
    // THE ROW LOSES EXACTLY WHAT THE CLAIM PUT ON IT — read off each placement's own record, not
    // inferred from the row naming the folder's root. A row that already named the root was not
    // changed by the claim: subtracting it would drop copies the claim never touched, and where it
    // was the row's only destination the ordinary narrow would then drop the ROW, uninstalling
    // every copy of a bundle the person only asked to stop managing one folder of.
    let value = match row.and_then(|r| r.fields().dest) {
        Some(dest) if dest.iter().any(|d| added.contains(d)) => {
            let remaining: Vec<String> = dest
                .iter()
                .filter(|d| !added.contains(d))
                .cloned()
                .collect();
            // Unreachable by construction — a claim only ever APPENDS to a non-empty `dest`, so
            // what it added can never be the whole set. Fail toward touching nothing rather than
            // toward a bare row (which would resurrect copies everywhere) or a silent row drop.
            if remaining.is_empty() {
                None
            } else {
                let mut fields = row.expect("a dest came from a row").fields();
                fields.dest = Some(remaining);
                Some(EntryValue::Fields(fields))
            }
        }
        _ => None,
    };
    Ok(Some(Arm::ClaimDetach {
        origin: claim_origin(ctx, &sctx, &sid),
        row: row.cloned(),
        reference,
        name: name.clone(),
        record: sid,
        dirs,
        value,
    }))
}

/// The bundle's recorded copies as a receipt spells them — every placement, including the adopted
/// and claimed folders no destination set speaks for. Best-effort: this feeds a refusal's list, and
/// an unreadable record simply names none.
fn record_copies(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    row: Option<&PlanRow>,
    name: &str,
) -> Vec<String> {
    let Some(sctx) = scope_store_ctx(ctx, target) else {
        return Vec::new();
    };
    let Some(sid) = row_record(ctx, target, row, name).and_then(|s| SkillId::parse(&s).ok()) else {
        return Vec::new();
    };
    crate::doc::read_map(sctx.fs, &sctx.layout.published(&sid).map)
        .ok()
        .flatten()
        .map(|map| {
            map.placements
                .iter()
                .map(|p| super::dest_select::copy_spellings(&sctx, Path::new(p)).display)
                .collect()
        })
        .unwrap_or_default()
}

/// The row key an arm names, for the shapes that carry one.
fn arm_reference(arm: &Arm) -> Option<&str> {
    match arm {
        Arm::RowDrop { row, .. } => Some(&row.reference),
        Arm::OffWrite { reference, .. } | Arm::DestNarrow { reference, .. } => Some(reference),
        _ => None,
    }
}

/// The `--as` value a detached folder's RE-CLAIM would carry — the record's own source spelling.
fn claim_origin(ctx: &Ctx<'_>, sctx: &Ctx<'_>, sid: &SkillId) -> Option<String> {
    let lock = crate::doc::read_doc::<topos_types::persisted::Lock>(
        sctx.fs,
        &sctx.layout.published(sid).lock,
    )
    .ok()
    .flatten()?;
    match super::add::record_origin(ctx, sctx, sid, &lock)? {
        super::add::RecordOrigin::Reference(r) => Some(r),
        super::add::RecordOrigin::Folder(dir) => Some(dir.to_string_lossy().into_owned()),
    }
}

/// Split a destination set into `(subtract, remaining)` against the selection's entries — every
/// selected entry must name a destination the set holds, or the refusal says why (the
/// shared-copy answer when the coverage table explains the miss).
///
/// `copies` are the bundle's recorded copies as a receipt spells them — what the zero-state
/// refusal names when the resolved destination set is empty but copies DO stand (every one of
/// them a folder the person adopted or claimed, which no destination set speaks for). Saying
/// "nothing has synced" there is a plain falsehood, and `list` one line up says otherwise.
#[allow(clippy::too_many_arguments)]
fn split_dest(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    selection: &super::dest_select::Selection,
    name: &str,
    ws: Option<&(String, String)>,
    dest: &[String],
    entries: &[String],
    copies: &[String],
) -> Result<(Vec<String>, Vec<String>), ClientError> {
    let global = target.scope == ManifestScope::Global;
    // NO destination set to subtract FROM, and the ordinary refusal below would trail off into an
    // empty list. Which zero-state it is depends on whether anything stands: copies the row's
    // destinations do not speak for get named, so the reader can retype one; nothing at all is
    // the honest not-yet-synced answer, with both ways out.
    if dest.is_empty() {
        if !copies.is_empty() {
            return Err(ClientError::SelectionRefused(format!(
                "no copy of '{name}' is in {} — its copies are: {}",
                selection.argv_tail().join(" "),
                copies.join(", ")
            )));
        }
        return Err(ClientError::SelectionRefused(format!(
            "'{name}' has no recorded copies in this scope yet — its row was added but nothing \
             has synced; run `topos update` first, or remove the whole row (`topos remove{} \
             {name}`)",
            if global { " -g" } else { "" },
        )));
    }
    for entry in entries {
        if dest.contains(entry) {
            continue;
        }
        // The miss: a shared folder several agents read may be the reason — the honest refusal
        // names it and the two ways out.
        let shared_root = match target.scope {
            ManifestScope::Global => "~/.agents/skills".to_owned(),
            ManifestScope::Project => ".agents/skills".to_owned(),
        };
        let removed_slug = selection
            .agents
            .iter()
            .find(|slug| {
                crate::manifest::dest::skills_dest_spelling(slug, target.scope).as_deref()
                    == Some(entry.as_str())
            })
            .cloned();
        if dest.contains(&shared_root)
            && let Some(slug) = &removed_slug
            && topos_harness::coverage::shared_dir_support(slug).covered()
        {
            return Err(shared_copy_refusal(ctx, target, name, slug, ws, global));
        }
        return Err(ClientError::SelectionRefused(format!(
            "'{name}' has no copy at {entry} — its destinations here are {}",
            crate::manifest::dest::quoted_list(dest)
        )));
    }
    let subtract: Vec<String> = dest
        .iter()
        .filter(|d| entries.contains(d))
        .cloned()
        .collect();
    let remaining: Vec<String> = dest
        .iter()
        .filter(|d| !entries.contains(d))
        .cloned()
        .collect();
    Ok((subtract, remaining))
}

/// The [`ClientError::SharedCopyOnly`] refusal, fully spelled: the one shared copy, the
/// whole-removal command, and the per-agent re-add for the OTHER covered agents.
fn shared_copy_refusal(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    name: &str,
    removed_slug: &str,
    ws: Option<&(String, String)>,
    global: bool,
) -> ClientError {
    // The shared COPY dir (the placement itself, not its root), best-effort from the record.
    let copy = shared_copy_dir(ctx, target, name).unwrap_or_else(|| match target.scope {
        ManifestScope::Global => format!("~/.agents/skills/{name}"),
        ManifestScope::Project => format!(".agents/skills/{name}"),
    });
    let mut remove_argv = vec!["topos".to_owned(), "remove".to_owned()];
    if global {
        remove_argv.push("-g".to_owned());
    }
    remove_argv.push(name.to_owned());
    let mut readd_argv = vec!["topos".to_owned(), "add".to_owned()];
    if global {
        readd_argv.push("-g".to_owned());
    }
    readd_argv.push(sugar_reference(ctx, ws, name));
    // The agents that read the shared copy per the coverage table, minus the one being removed —
    // detected here, alphabetical.
    let mut others: Vec<String> = ctx
        .roots
        .as_ref()
        .map(|roots| {
            topos_harness::registry::detected_harnesses(&roots.home, roots.cwd.as_deref())
                .iter()
                .map(|h| h.slug.to_owned())
                .filter(|slug| slug != removed_slug)
                .filter(|slug| topos_harness::coverage::shared_dir_support(slug).covered())
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    others.dedup();
    for slug in &others {
        readd_argv.push("-a".to_owned());
        readd_argv.push(slug.clone());
    }
    ClientError::SharedCopyOnly {
        name: name.to_owned(),
        agent: removed_slug.to_owned(),
        copy,
        remove_argv,
        readd_argv,
    }
}

/// The recorded SHARED placement dir for `name` in this scope's store (pretty-spelled), when one
/// stands.
fn shared_copy_dir(ctx: &Ctx<'_>, target: &EditTarget, name: &str) -> Option<String> {
    let sctx = scope_store_ctx(ctx, target)?;
    let (sid, _lock) = super::resolve_skill(&sctx, name).ok()?;
    let map = crate::doc::read_map(sctx.fs, &sctx.layout.published(&sid).map).ok()??;
    map.placements
        .iter()
        .zip(&map.placement_state)
        .find(|(_, st)| st.kind == topos_types::persisted::PlacementKind::Shared)
        .map(|(p, _)| dest_display(ctx, target, Path::new(p)))
}

/// The `@ws/<name>` sugar at the machine's one connected host, else the canonical spelling, else
/// the bare name (a non-workspace bundle).
fn sugar_reference(ctx: &Ctx<'_>, ws: Option<&(String, String)>, name: &str) -> String {
    qualified_name(ctx, ws, name)
}

/// The workspace-QUALIFIED display a destination receipt leads with — [`qualify_display`], with
/// the bare name for a non-workspace bundle.
fn qualified_name(ctx: &Ctx<'_>, ws: Option<&(String, String)>, name: &str) -> String {
    match ws {
        Some((host, workspace)) => {
            qualify_display(default_host(ctx).as_deref(), host, workspace, name)
        }
        None => name.to_owned(),
    }
}

/// The CURRENT resolved destination set of a bundle in this scope — each recorded placement's
/// PARENT root (the adopted-in-place source dir excluded: it is the person's own and no freeze
/// manages it), spelled in the scope's dest dialect. For an MCP bundle: the config FILES its
/// per-agent entries live in.
///
/// THE ROW NAMES THE RECORD, not the bare name ([`row_record`]). Two names collide more often than
/// they should and the cost was not symmetric: a bare-name lookup that hit an AMBIGUITY answered
/// with the empty set, and an empty current set reads as "this bundle reaches nowhere" — so a
/// freeze materialized from it would have quietly dropped every copy the bundle really had. On
/// the MCP side the bare name went the other way and swept in EVERY cached record of that name,
/// so another workspace's bundle contributed its config files to this one's set.
fn current_dest_roots(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    row: Option<&PlanRow>,
    name: &str,
    kind: BundleKind,
) -> Result<Vec<String>, ClientError> {
    // MATCHED, not tested: a third kind's "where do its copies live" answer is a hole the
    // compiler names, not a silent fall into the skills-folder branch.
    let mcp = match kind {
        BundleKind::Mcp => true,
        BundleKind::Skill => false,
    };
    if mcp {
        // The bundle's own custody rows name the config files its entries live in — filtered by
        // every identity THIS bundle may be filed under: the id the row resolves to, and — for a
        // local row — the line-keyed local identity.
        let Some(layout) = mcp_scope_store(ctx, target) else {
            return Ok(Vec::new());
        };
        let mut ids: Vec<String> = Vec::new();
        if let Some(sid) = row_record(ctx, target, row, name) {
            ids.push(sid);
        }
        if let Some(r) = row.filter(|r| matches!(r.shape, KeyShape::LocalPath { .. })) {
            ids.push(crate::config_custody::local_identity(&r.reference));
        }
        let mut files: Vec<String> = crate::config_custody::entries_of_any(ctx.fs, &layout, &ids)
            .iter()
            .map(|e| dest_display(ctx, target, Path::new(&e.file)))
            .collect();
        files.sort();
        files.dedup();
        return Ok(files);
    }
    let Some(sctx) = scope_store_ctx(ctx, target) else {
        return Ok(Vec::new());
    };
    let Some(sid) = row_record(ctx, target, row, name).and_then(|s| SkillId::parse(&s).ok()) else {
        return Ok(Vec::new());
    };
    let Some(map) = crate::doc::read_map(sctx.fs, &sctx.layout.published(&sid).map)? else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    for (p, st) in map.placements.iter().zip(&map.placement_state) {
        if st.adopted_source {
            continue;
        }
        let Some(parent) = Path::new(p).parent() else {
            continue;
        };
        let spelled = dest_display(ctx, target, parent);
        if !out.contains(&spelled) {
            out.push(spelled);
        }
    }
    Ok(out)
}

/// THE DEFAULT REACH of a bundle in this scope — what the row resolves to carrying no `dest` at
/// all, which is exactly what the `"*"` token stands for. Asked of the ONE planner the reconcile
/// asks (`placement::default_reach_plan` for a skill; the scope's engaged MCP surfaces for a
/// config-placed bundle), so a narrowing and the sweep that follows it cannot disagree about which
/// destinations the token still holds.
///
/// A narrowing asks this ONE question of it: did the subtraction take a destination the token
/// still stands for? Empty — nothing in this scope answers for the bundle — reads as no, which is
/// the honest answer when there is no plan to compare against, and leaves the token where the
/// person put it.
///
/// # Errors
/// A store read failure.
fn default_reach_roots(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    row: Option<&PlanRow>,
    name: &str,
    kind: BundleKind,
) -> Result<Vec<String>, ClientError> {
    let Some(sctx) = scope_store_ctx(ctx, target) else {
        return Ok(Vec::new());
    };
    if kind.is_mcp() {
        let plan = crate::placement::entries_plan(&sctx, sctx.layout.project_root(), None);
        let mut files: Vec<String> = plan
            .entries()
            .map(|e| dest_display(ctx, target, &e.file))
            .collect();
        files.sort();
        files.dedup();
        return Ok(files);
    }
    let Some(sid) = row_record(ctx, target, row, name).and_then(|s| SkillId::parse(&s).ok()) else {
        return Ok(Vec::new());
    };
    let sp = sctx.layout.published(&sid);
    let Some(lock) = crate::doc::read_doc::<topos_types::persisted::Lock>(sctx.fs, &sp.lock)?
    else {
        return Ok(Vec::new());
    };
    // PLANNED OVER NO PRIOR RECORD, deliberately. The prior-stability rule keeps every recorded
    // placement, and the row's OWN named entries put copies there — so a plan built over the
    // record would hand back the very destinations the token is being asked about, and no
    // subtraction would ever look like it left the default half alone.
    let plan = crate::placement::default_reach_plan(
        &sctx,
        sid.as_str(),
        topos_harness::PlacementNaming {
            name: Some(&lock.name),
            workspace_slug: None,
        },
        super::followed_workspace(&sctx, sid.as_str()).is_some(),
        &empty_placement_map(),
        None,
    );
    let mut out: Vec<String> = Vec::new();
    for dir in plan.dirs() {
        let Some(parent) = dir.dir.parent() else {
            continue;
        };
        let spelled = dest_display(ctx, target, parent);
        if !out.contains(&spelled) {
            out.push(spelled);
        }
    }
    Ok(out)
}

/// WHAT AN ADD MUST NOT RECORD on a standing row: its DEFAULT REACH, asked only while the row
/// still stands for it (no `dest` at all, or a `dest` carrying the token). Empty everywhere else —
/// a row born by this add records what it was asked for, an `"off"` switch is no destination set,
/// and a row naming its own destinations reaches exactly what it names, so every asked entry there
/// is a real addition.
///
/// The reach is [`default_reach_roots`]' answer, the same one the collapse consults, so an add and
/// the `remove` its receipt prints cannot disagree about which entries the token already holds.
///
/// # Errors
/// A store read failure, or a manifest the grammar refuses.
fn reach_already_held(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    reference: &str,
    prior: Option<&EntryValue>,
    value: &EntryValue,
) -> Result<Vec<String>, ClientError> {
    let (Some(prior), EntryValue::Fields(fields)) = (prior, value) else {
        return Ok(Vec::new());
    };
    if fields.dest.is_none() || matches!(prior, EntryValue::Off) {
        return Ok(Vec::new());
    }
    if !prior
        .fields()
        .dest
        .as_deref()
        .is_none_or(crate::manifest::dest::carries_default_reach)
    {
        return Ok(Vec::new());
    }
    let Ok(shape) = crate::manifest::keys::classify_key(reference) else {
        return Ok(Vec::new());
    };
    let ws = match &shape {
        KeyShape::WorkspaceBundle {
            host, workspace, ..
        } => Some((host.clone(), workspace.clone())),
        _ => None,
    };
    let row = PlanRow {
        reference: reference.to_owned(),
        shape,
        section: match prior.declared_kind() {
            Some(BundleKind::Mcp) => crate::manifest::document::SectionKind::Mcp,
            _ => crate::manifest::document::SectionKind::Skills,
        },
        value: prior.clone(),
    };
    let name = row.display_name();
    let kind = row_kind(ctx, target, Some(&row), ws.as_ref(), &name);
    default_reach_roots(ctx, target, Some(&row), &name, kind)
}

/// WHAT A SET LINE CAN NEVER REACH: the asked destinations that are no agent's own — a folder a
/// person named, which nothing but a row can demand. Everything else is dropped: a destination the
/// bundle's DEFAULT REACH already holds is a request for nothing, and one belonging to a harness
/// this machine has not installed yet joins that reach the day it is (detection is answered at plan
/// time, on every run), so neither needs a line in the file.
///
/// The reach is [`default_reach_roots`]' answer — the same one the collapse consults — over the
/// row this add would write, and the harness question is the [`super::dest_select`] resolution the
/// selection itself was validated with.
///
/// # Errors
/// A store read failure, or a manifest the grammar refuses.
pub(super) fn outside_default_reach(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    reference: &str,
    name: &str,
    kind: BundleKind,
    asked: &[String],
) -> Result<Vec<String>, ClientError> {
    let Ok(shape) = crate::manifest::keys::classify_key(reference) else {
        return Ok(Vec::new());
    };
    let row = PlanRow {
        reference: reference.to_owned(),
        shape,
        section: match kind {
            BundleKind::Mcp => crate::manifest::document::SectionKind::Mcp,
            _ => crate::manifest::document::SectionKind::Skills,
        },
        value: EntryValue::Star,
    };
    let reach = default_reach_roots(ctx, target, Some(&row), name, kind)?;
    // ANY harness claiming the entry is enough — a folder several of them read is still theirs,
    // and the one-claimant question the undo spelling asks is a different one.
    let claimed = |entry: &str| match kind {
        BundleKind::Mcp => crate::manifest::dest::mcp_slug_for_dest(entry, target.scope).is_some(),
        BundleKind::Skill => topos_harness::registry::known_harnesses().iter().any(|h| {
            crate::manifest::dest::skills_dest_spelling(h.slug, target.scope).as_deref()
                == Some(entry)
        }),
    };
    Ok(asked
        .iter()
        .filter(|entry| !reach.contains(entry) && !claimed(entry))
        .cloned()
        .collect())
}

/// A record-free placement map — the "nothing has been placed yet" input a default-reach plan is
/// asked over.
fn empty_placement_map() -> topos_types::persisted::PlacementMap {
    topos_types::persisted::PlacementMap {
        schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
        placements: Vec::new(),
        applied_commit: String::new(),
        materialized_sha: String::new(),
        placement_state: Vec::new(),
        harness: None,
        harness_slug: None,
    }
}

/// Whether a narrowed row's remaining fields carry nothing a plain value row cannot say: no
/// name/subdir/kind override and no `mcp_dest`. Such a row plus a `dest` naming only the
/// default-reach token IS the plain value row, spelled the long way.
fn plain_default_row(fields: &crate::manifest::document::EntryFields) -> bool {
    fields.mcp_dest.is_none()
        && fields.name.is_none()
        && fields.subdir.is_none()
        && fields.kind.is_none()
}

/// WHICH SET LINE in `target`'s file delivers the workspace bundle `<host>/<workspace>/<bundle>`,
/// spelled as this scope names it: `channels/<name>` for the channel row it arrives through, the
/// workspace reference for the FEED row.
///
/// `None` whenever the answer is not PROVEN: the file names the bundle explicitly (an explicit row
/// beats every set, and an `"off"` switch withholds it), no set line covers it, the offline
/// delivery cache cannot speak for it, or this scope's store holds no record of it. A caller acts
/// on this answer by writing NOTHING, or (for an ask the set cannot reach — [`outside_default_reach`])
/// by writing the one line that can, so an unproven set is no set: the ordinary row write is what
/// an unproven answer falls back to.
///
/// # Errors
/// A read failure, or a manifest the grammar refuses.
pub(super) fn set_delivering(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    host: &str,
    workspace: &str,
    bundle: &str,
) -> Result<Option<String>, ClientError> {
    let plan = plan_for(ctx, target)?;
    if plan.explicit_claims(host, workspace, bundle)
        || plan.off_for(host, workspace, bundle).is_some()
    {
        return Ok(None);
    }
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let Some(entry) = cache.workspaces.values().find(|e| {
        e.host.as_deref() == Some(host) && e.workspace_name.as_deref() == Some(workspace)
    }) else {
        return Ok(None);
    };
    let Some((sid, delivered)) = entry
        .delivered
        .iter()
        .find(|(_, d)| d.name == bundle && !d.withdrawn)
    else {
        return Ok(None);
    };
    // THE SCOPE'S OWN STORE has to hold the record: the delivery cache is the machine's, and a
    // scope that has never taken the bundle has nothing standing to converge.
    let Some(sctx) = scope_store_ctx(ctx, target) else {
        return Ok(None);
    };
    let Ok(id) = SkillId::parse(sid) else {
        return Ok(None);
    };
    if crate::doc::read_doc::<topos_types::persisted::Lock>(
        sctx.fs,
        &sctx.layout.published(&id).lock,
    )?
    .is_none()
    {
        return Ok(None);
    }
    // A CHANNEL ROW answers first, then the feed — the order the resolution reads rows in.
    let channel = plan.sets.iter().find_map(|set| match &set.shape {
        KeyShape::Channel {
            host: h,
            workspace: w,
            channel,
        } if h == host && w == workspace && delivered.via_channels.contains(channel) => {
            Some(channel.clone())
        }
        _ => None,
    });
    if let Some(channel) = channel {
        return Ok(Some(format!("channels/{channel}")));
    }
    // The FEED row delivers whatever the workspace serves this person — but a channel-row member
    // is excluded from everything feed-shaped, exactly as the offline resolution excludes it.
    if !delivered.via_manifest && plan.has_feed(host, workspace) {
        return Ok(Some(format!("{host}/{workspace}")));
    }
    Ok(None)
}

/// Whether a CHANNEL row in this scope's file carries `bundle` as a member. This is the one
/// condition under which a bare `remove <name>` resolves to a chooser — the explicit row plus the
/// set line's member rewrite both answer — so a receipt whose undo must run as printed spells the
/// full reference instead. Unprovable states (an unreadable plan or cache) answer `false`, which
/// keeps the bare-name spelling: the same state would have failed the surrounding verb first.
pub(super) fn channel_carries(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    host: &str,
    workspace: &str,
    bundle: &str,
) -> bool {
    let Ok(plan) = plan_for(ctx, target) else {
        return false;
    };
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let Some(entry) = cache.workspaces.values().find(|e| {
        e.host.as_deref() == Some(host) && e.workspace_name.as_deref() == Some(workspace)
    }) else {
        return false;
    };
    let Some((_, delivered)) = entry
        .delivered
        .iter()
        .find(|(_, d)| d.name == bundle && !d.withdrawn)
    else {
        return false;
    };
    plan.sets.iter().any(|set| match &set.shape {
        KeyShape::Channel {
            host: h,
            workspace: w,
            channel,
        } => h == host && w == workspace && delivered.via_channels.contains(channel),
        _ => false,
    })
}

/// THE RECORD A ROW NAMES, resolved by the row's own QUALIFIED identity — the id, as a string,
/// or `None` when nothing in this scope answers for it.
///
/// A bare name is the wrong key for this question. Two bundles can share one, and the two failure
/// modes are both silent: a skills lookup collapses to "no copies anywhere" (which a freeze then
/// materializes from, dropping every real copy), and an MCP lookup collapses the other way and
/// pulls in the other bundle's config files. Each row shape carries a key that cannot collide:
///
/// - a WORKSPACE row is `<host>/<ws>/<bundle>` — the delivery cache holds exactly one record for
///   that triple;
/// - a FORGE row's record is the one whose recorded upstream is that repo, under that name;
/// - a LOCAL PATH row's record is the one whose adopted-in-place folder IS the row's folder.
///
/// The bare-name resolve is the last resort, for a row shape with no identity of its own (and for
/// a caller with no row at all) — where it is no worse than what it replaced.
fn row_record(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    row: Option<&PlanRow>,
    name: &str,
) -> Option<String> {
    let sctx = scope_store_ctx(ctx, target)?;
    match row.map(|r| &r.shape) {
        Some(KeyShape::WorkspaceBundle {
            host,
            workspace,
            bundle,
        }) => {
            let cache = crate::sync_status::read(ctx.fs, &ctx.layout).ok()?;
            cache.workspaces.values().find_map(|entry| {
                (entry.host.as_deref() == Some(host.as_str())
                    && entry.workspace_name.as_deref() == Some(workspace.as_str()))
                .then(|| {
                    entry
                        .delivered
                        .iter()
                        .find(|(_, d)| d.name == *bundle)
                        .map(|(sid, _)| sid.clone())
                })
                .flatten()
            })
        }
        Some(KeyShape::LocalPath { raw }) => {
            let dir = canonical_or(&row_dir(ctx, target, raw));
            super::tracked_skill_at(&sctx, &dir).ok().flatten()
        }
        // A forge row, a channel, a feed, or no row at all: the name is the only key there is.
        _ => super::resolve_skill(&sctx, name)
            .ok()
            .map(|(sid, _)| sid.as_str().to_owned()),
    }
}

/// A path in the scope's dest dialect: `~/`-abbreviated under the machine home for the global
/// file, checkout-relative for a project file, verbatim otherwise.
fn dest_display(ctx: &Ctx<'_>, target: &EditTarget, path: &Path) -> String {
    if target.scope == ManifestScope::Project
        && let Ok(rel) = path.strip_prefix(&target.dir)
    {
        return rel.display().to_string();
    }
    if let Some(roots) = &ctx.roots
        && let Ok(rest) = path.strip_prefix(&roots.home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// A per-skill store ctx for the scope this edit acts in (`None` when a project scope has no
/// store yet — then nothing was ever placed, and the current set is honestly empty).
fn scope_store_ctx<'a>(ctx: &'a Ctx<'a>, target: &EditTarget) -> Option<Ctx<'a>> {
    match target.scope {
        ManifestScope::Global => Some(super::pull::ctx_with_layout(ctx, &ctx.layout.clone())),
        ManifestScope::Project => crate::sidecar::existing_project_store(ctx.fs, &target.dir)
            .map(|layout| super::pull::ctx_with_layout(ctx, &layout)),
    }
}

/// What the bundle a manifest arm acts on IS — the answer that picks a destination VOCABULARY
/// (config files vs skills folders) and the noun a receipt uses. The row's own `kind` field
/// answers first: a locally declared server folder has no catalog to ask, and a hand-written
/// row may point outside every store. Otherwise the scope store's record answers
/// through THE chain ([`crate::bundle_kind::classify`]).
///
/// Nothing answering is the ordinary skill. No bytes move on this answer — an arm that would
/// place or destroy them asks the engine, which refuses rather than guesses.
fn row_kind(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    row: Option<&PlanRow>,
    ws: Option<&(String, String)>,
    name: &str,
) -> BundleKind {
    if let Some(kind) = row
        .and_then(|r| r.fields().kind)
        .and_then(|word| BundleKind::parse(&word))
    {
        return kind;
    }
    let Some(sctx) = scope_store_ctx(ctx, target) else {
        return BundleKind::Skill;
    };
    let Some(sid) = qualified_record(&sctx, ws, name) else {
        return BundleKind::Skill;
    };
    let placements = crate::doc::read_map(sctx.fs, &sctx.layout.published(&sid).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default();
    crate::bundle_kind::classify(&sctx, sid.as_str(), &placements).or_skill()
}

/// The scope store's record for a row's bundle, QUALIFIED by the row's own workspace. A bare name
/// is not an identity: two workspaces may each publish a `linear`, and asking for that name alone
/// answers "ambiguous" — which would leave a qualified row resolving nothing and falling back to
/// the wrong vocabulary. So a workspace row finds its record through the delivery cache's
/// (host, workspace, name) → id map first. The cache answers WHICH record; the marker answers WHAT
/// it is — kind never comes from here.
fn qualified_record(sctx: &Ctx<'_>, ws: Option<&(String, String)>, name: &str) -> Option<SkillId> {
    if let Some((host, workspace)) = ws {
        let hit = crate::sync_status::read(sctx.fs, &sctx.layout)
            .ok()
            .and_then(|cache| {
                cache.workspaces.values().find_map(|e| {
                    (e.host.as_deref() == Some(host.as_str())
                        && e.workspace_name.as_deref() == Some(workspace.as_str()))
                    .then(|| {
                        e.delivered
                            .iter()
                            .find(|(_, d)| d.name == *name)
                            .map(|(id, _)| id.clone())
                    })
                    .flatten()
                })
            });
        if let Some(id) = hit {
            return SkillId::parse(&id).ok();
        }
    }
    // Unqualified (a local/forge row), or a workspace the cache has no row for yet: the bare name
    // is the only handle there is, and an ambiguous one answers nothing rather than guessing.
    super::resolve_skill(sctx, name).ok().map(|(sid, _)| sid)
}

/// The typed refusal for a token this file answers more than once — the paste-ready qualified
/// references ride the error, so the caller re-spells one instead of guessing which was meant.
/// Deduped + sorted, so the same ambiguity always reads the same way.
///
/// `global` is the SCOPE THIS EDIT RESOLVED IN, and every rebuilt command the refusal offers
/// carries it: each candidate names a line in ONE file, and the same token re-spelled without `-g`
/// would be resolved against the other one.
fn ambiguous(
    token: &str,
    candidates: impl IntoIterator<Item = TargetCandidate>,
    global: bool,
) -> ClientError {
    let mut candidates: Vec<TargetCandidate> = candidates.into_iter().collect();
    // Ordered structurally (which is spelling order) and deduped on what the surfaces show: two
    // candidates that would print and run identically are ONE way out.
    candidates.sort();
    candidates.dedup_by(|a, b| a.spelling() == b.spelling());
    ClientError::AmbiguousTarget {
        name: token.to_owned(),
        candidates,
        global,
    }
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

/// The reference of the local-path row whose FOLDER `token` names, when `token` is path-shaped and
/// some row in this file points at that folder. `None` for every other token, and for a path that
/// resolves to no row (the ordinary miss then reports the token as typed).
///
/// `add ./deploy` records the folder's canonicalized absolute path, so `remove ./deploy` — the
/// documented exact inverse — matched nothing: the row's one spelling and the person's own were
/// different strings for one directory. Both sides are resolved and canonicalized here, which
/// makes every spelling of a folder (`./deploy`, `../repo/deploy`, `~/repo/deploy`, the absolute
/// path) reach the row, rather than only the spelling that happened to be stored. A relative TOKEN
/// resolves against the CWD (that is what the person typed it against); a relative ROW resolves
/// against the folder its manifest governs.
fn path_row_reference(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    plan: &ScopePlan,
    token: &str,
) -> Option<String> {
    if !matches!(keys::classify_key(token), Ok(KeyShape::LocalPath { .. })) {
        return None;
    }
    let roots = ctx.roots.as_ref()?;
    let resolve = |raw: &str, base: &Path| -> Option<PathBuf> {
        let joined = match raw.strip_prefix("~/") {
            Some(rest) => roots.home.join(rest),
            None if Path::new(raw).is_absolute() => PathBuf::from(raw),
            None => base.join(raw.trim_start_matches("./")),
        };
        std::fs::canonicalize(joined).ok()
    };
    let wanted = resolve(token, roots.cwd.as_deref()?)?;
    plan.things
        .iter()
        .find(|r| match &r.shape {
            KeyShape::LocalPath { raw } => resolve(raw, &target.dir).as_ref() == Some(&wanted),
            _ => false,
        })
        .map(|r| r.reference.clone())
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
                .map(|i| i.lock.name)
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
        "'{token}' is not in {} — `topos list` shows what each scope delivers",
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
#[derive(Debug, Clone, PartialEq, Eq)]
enum DraftState {
    /// No local edits anywhere (or nothing local at all).
    Clean,
    /// At least one copy carries local edits — the folders holding them, so the note can say
    /// WHERE the work a person is about to stop managing will still be. It used to point at
    /// `topos list <name>`, which answers "not managed on this machine" the moment the row is
    /// gone: the one command the note named could never show the copy it was about.
    Draft(Vec<String>),
    /// A copy could not be classified — the guard fails TOWARD the gate.
    Indeterminate,
}

/// Whether the bundle a row delivers carries local edits, over the store that owns it (the home
/// store, or the project store of a checkout on the cwd chain).
fn draft_state(ctx: &Ctx<'_>, name: &str) -> DraftState {
    let (layout, sid) = match super::resolve_skill_stored(ctx, name, None, super::StoreScope::Here)
    {
        Ok(hit) => (hit.layout, hit.id),
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
    let mut drafted: Vec<String> = Vec::new();
    for scan in &scans {
        match scan.status {
            crate::placement::ScanStatus::Modified { .. } => {
                drafted.push(super::inventory::pretty(&sctx, &scan.dir));
            }
            crate::placement::ScanStatus::Unscannable => state = DraftState::Indeterminate,
            _ => {}
        }
    }
    // A draft anywhere is a draft: the folders are the answer, and an unreadable SIBLING copy
    // does not make a proven edit unprovable.
    if !drafted.is_empty() {
        return DraftState::Draft(drafted);
    }
    state
}

// ---------------------------------------------------------------------------------------------
// The apply half
// ---------------------------------------------------------------------------------------------

/// Describe or apply the resolved arms: a batch that mixes gated and ungated arms gates WHOLE.
///
/// THE CALLER HOLDS THE FILE'S WRITER LOCK across the resolve that produced `arms` and this apply
/// — the arms are decisions about a document, and they are only true of the document they were
/// read from.
#[allow(clippy::too_many_arguments)]
fn apply_arms(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    tokens: &[String],
    arms: Vec<Arm>,
    via: Option<&str>,
    selection: &super::dest_select::Selection,
    yes: bool,
    global: bool,
) -> Result<RemoveOutcome, ClientError> {
    if arms.is_empty() {
        return Err(ClientError::InvalidArgument(
            "remove needs something to remove — a skill name, a reference, or `-g @<workspace>`"
                .into(),
        ));
    }
    // Several targets inside ONE set line are ONE split, resolved against that line once.
    let arms = coalesce_set_splits(arms);
    // The gate: a file edit is reversible, so it applies immediately — EXCEPT where local work
    // could be lost (a copy that cannot be classified fails TOWARD the gate) or where the edit
    // rewrites a curated line into its members (a set split changes what future curation reaches
    // this file). An EDITED copy is no longer a gate on the row drops: the eager uninstall keeps
    // every edited copy in place (disclosed with a kept line), so nothing is lost by applying.
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
                // A row whose FOLDER IS GONE is settled before the loss guard runs: there is
                // nothing on disk to scan, so the scan's own hedge ("could not be read —
                // describing first") named the wrong fact and gated an edit that can lose
                // nothing. State what is true instead, and apply.
                if let Arm::RowDrop { row, .. } = arm
                    && row_folder_gone(ctx, target, row)
                {
                    notes.push(Some(join_notes(
                        "its folder is gone, so there are no files to check — dropping the line \
                         changes nothing else"
                            .to_owned(),
                        removal_note,
                    )));
                    continue;
                }
                match draft_state(ctx, name) {
                    // The FOLDERS, named. Once the row is gone `topos list <name>` answers "not
                    // managed on this machine" and stops — so the command this note used to
                    // offer could not show the edited copy it was about. One folder reads
                    // inline; several are counted and listed, the same convention every other
                    // destination follows.
                    DraftState::Draft(dirs) => Some(join_notes(
                        match dirs.as_slice() {
                            [one] => format!(
                                "'{name}' has local edits that are not shared — they stay in \
                                 {one}"
                            ),
                            many => format!(
                                "'{name}' has local edits that are not shared — they stay in {}",
                                many.join(", ")
                            ),
                        },
                        removal_note,
                    )),
                    DraftState::Indeterminate => {
                        gated = true;
                        Some(join_notes(
                            format!(
                                "{name}'s files could not be read to check for local edits — \
                                 describing first rather than assuming there are none"
                            ),
                            removal_note,
                        ))
                    }
                    DraftState::Clean => removal_note,
                }
            }
            // A detach loses nothing — the folder and every byte in it stay — so there is no
            // loss guard to trip and nothing the stock line does not already say.
            Arm::ClaimDetach { .. } => None,
            Arm::DestNarrow {
                name,
                subtract,
                remaining,
                materialized,
                ..
            } => {
                let materialize_note = materialized.then(|| {
                    format!(
                        "the row reached every agent, so its current set was written out \
                         minus {} — {} remain",
                        crate::manifest::dest::quoted_list(subtract),
                        remaining.unwrap_or_default()
                    )
                });
                match draft_state(ctx, name) {
                    DraftState::Indeterminate => {
                        gated = true;
                        Some(join_notes(
                            format!(
                                "{name}'s files could not be read to check for local edits — \
                                 describing first rather than assuming there are none"
                            ),
                            materialize_note,
                        ))
                    }
                    _ => materialize_note,
                }
            }
            Arm::SetSplit {
                set,
                names,
                members,
                keeps_own,
            } => {
                gated = true;
                let mut note = format!(
                    "'{}' {} from the {} line `{}` — removing {} replaces that one line \
                     with its current members, so later additions by whoever curates it stop \
                     arriving through this file",
                    names.join("', '"),
                    if names.len() == 1 { "comes" } else { "come" },
                    set.shape.noun(),
                    set.reference,
                    if names.len() == 1 { "it" } else { "them" }
                );
                // Which survivors get NEW rows and which keep their own (explicit beats set —
                // an already-explicit row is never overwritten by the split).
                let new_rows: Vec<&str> = members
                    .iter()
                    .filter(|m| !names.contains(m) && !keeps_own.contains(m))
                    .map(String::as_str)
                    .collect();
                if !new_rows.is_empty() {
                    note.push_str(&format!(
                        "; new rows are written for {}",
                        new_rows.join(", ")
                    ));
                }
                if !keeps_own.is_empty() {
                    note.push_str(&format!(
                        "; {} already {} own row here, kept untouched",
                        keeps_own.join(", "),
                        if keeps_own.len() == 1 {
                            "has its"
                        } else {
                            "have their"
                        }
                    ));
                }
                // The carriage, said PER KIND — one clause each — because a channel's two
                // destination arrays go to different survivors: `dest` to the skills, `mcp_dest`
                // to the mcp servers (as their own row `dest`). A reader who is told only "the
                // line's settings carry" cannot tell which of their rows got which array.
                let kinds: Vec<BundleKind> = new_rows
                    .iter()
                    .map(|m| member_kind(ctx, target, set, m))
                    .collect();
                let mut carried_any: Vec<&'static str> = Vec::new();
                let mut dropped: Vec<&'static str> = Vec::new();
                let mut note_kind = |kind: BundleKind, note: &mut String| {
                    if !kinds.contains(&kind) {
                        return;
                    }
                    let (_, carried, lost) = split_carriage(set, kind);
                    carried_any.extend(carried.iter().copied());
                    dropped.extend(lost);
                    if kind.is_mcp() {
                        note.push_str(if carried.contains(&"mcp_dest") {
                            "; its mcp_dest settings carry onto each new mcp row as that row's \
                             dest"
                        } else {
                            "; the line names no mcp_dest, so each new mcp row carries no dest"
                        });
                    } else if !carried.is_empty() {
                        note.push_str(&format!(
                            "; the line's {} settings carry onto each new skill row",
                            carried.join("/")
                        ));
                    }
                };
                note_kind(BundleKind::Skill, &mut note);
                note_kind(BundleKind::Mcp, &mut note);
                // What NO survivor's kind could take is disclosed rather than silently lost.
                let mut said: Vec<&'static str> = Vec::new();
                dropped.retain(|f| {
                    !carried_any.contains(f) && !said.contains(f) && {
                        said.push(f);
                        true
                    }
                });
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
                    "{workspace}'s feed no longer delivers here; the copies it placed leave this \
                     machine now — an edited copy stays in place — and the rows you spelled \
                     yourself survive"
                );
                // An EDITED copy is kept in place (disclosed on the receipt), so a draft is no
                // loss and no gate. A copy that cannot be READ is the one indeterminate state:
                // the whole remove fails toward the gate, and `--yes` then applies with it
                // treated as edited (kept).
                let unreadable = bundles
                    .iter()
                    .any(|b| draft_state(ctx, b) == DraftState::Indeterminate);
                if unreadable {
                    gated = true;
                    Some(format!(
                        "some of its skills' files could not be read to check for local edits — \
                         describing first rather than assuming there are none (an edited copy is \
                         kept in place) · {removal_note}"
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
            name: arm.name(),
            kind: match arm {
                Arm::OffWrite { .. } => RemoveKind::ManifestExcluded,
                Arm::FeedDrop { .. } => RemoveKind::FeedRemoved,
                Arm::DestNarrow { .. } => RemoveKind::ManifestNarrowed,
                Arm::ClaimDetach { .. } => RemoveKind::ClaimDetached,
                _ => RemoveKind::ManifestRemoved,
            },
            // A DETACH that edits no row must not name a file it never opened.
            manifest: match arm {
                Arm::ClaimDetach { value: None, .. } => None,
                _ => Some(target.path.display().to_string()),
            },
            workspace_id: None,
            dest_dirs: Vec::new(),
            // A row edit names no folders in either direction — what left rides
            // `RemoveData::uninstalled`, and what stayed is an edited copy the uninstall reports
            // as kept. The adopted-source disclosure belongs to the classic arm, which is the one
            // that deletes folders at all — and to the detach, whose whole point is the folder
            // that stays.
            kept_dirs: match arm {
                Arm::ClaimDetach { dirs, .. } => dirs.iter().map(|d| pretty_path(ctx, d)).collect(),
                _ => Vec::new(),
            },
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
        yes_argv.extend(selection.argv_tail());
        if let Some(v) = via {
            yes_argv.push("--via".to_owned());
            yes_argv.push(v.to_owned());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(RemoveOutcome::Described {
            data: RemoveData {
                items,
                applied: false,
                undo: Vec::new(),
                uninstalled: Vec::new(),
            },
            yes_argv,
        });
    }

    // ---- APPLY ----
    // The lock fences topos's own writers; it cannot fence a person's editor or a `sed`. So the
    // arms are RE-PROVEN against the file as it is right now, and a document that no longer
    // matches the one they were read from refuses rather than writing a plan built from bytes
    // that are gone — most sharply for a set split, whose survivor rows would otherwise be
    // rebuilt from a member list that predates someone else's row.
    //
    // ONE READ serves both the proof and the editor: proving against one read and editing a
    // second would leave a window between them where an external edit is proven against but not
    // edited (or edited but not proven against) — the reproof must hold for the EXACT document
    // instance the write below emits, so that instance is read once, proven, and then opened.
    //
    // The reproof alone is NOT a compare-and-swap — an edit landing after this read and before
    // the write's rename would still be overwritten — so the editor's write closes that half
    // too: it re-reads the file immediately before its atomic rename and byte-compares against
    // this very text, refusing typed on any drift (see the module doc's outside-writer note for
    // the accepted compare-to-rename residual).
    let text = read_text(ctx, &target.path)?;
    prove_unchanged(target, &arms, text.as_deref())?;
    let Opened { mut editor, born } = match &text {
        Some(t) => Opened {
            editor: ManifestEditor::open(t, target.scope).map_err(|e| corrupt(&target.path, &e))?,
            born: None,
        },
        // A file that does not exist yet is BORN by the edit — nothing to prove against.
        None => open_for_edit(ctx, target)?,
    };
    // A DETACH that edits no row must not rewrite the file: it would fail on a manifest this act
    // has no business opening, and a successful one would touch a document the receipt says it
    // left alone. Every other arm sets this; the record-only detach is the one that does not.
    let mut edited = false;
    for arm in &arms {
        match arm {
            Arm::RowDrop { row, .. } => {
                edited = true;
                editor.remove_row(&row.reference);
            }
            Arm::FeedDrop { reference, .. } => {
                edited = true;
                editor.remove_row(reference);
            }
            Arm::OffWrite { reference, .. } => {
                edited = true;
                editor
                    .set_row(
                        reference,
                        &EntryValue::Off,
                        crate::bundle_kind::BundleKind::Skill,
                    )
                    .map_err(|e| ClientError::InvalidArgument(e.message))?;
            }
            Arm::DestNarrow {
                reference, value, ..
            } => {
                edited = true;
                editor
                    .set_row(
                        reference,
                        value,
                        value
                            .declared_kind()
                            .unwrap_or(crate::bundle_kind::BundleKind::Skill),
                    )
                    .map_err(|e| ClientError::InvalidArgument(e.message))?;
            }
            // The row is touched ONLY where the claim put something on it; the record-side drop
            // happens after the write, below.
            Arm::ClaimDetach {
                reference,
                value: Some(value),
                ..
            } => {
                edited = true;
                editor
                    .set_row(
                        reference,
                        value,
                        value
                            .declared_kind()
                            .unwrap_or(crate::bundle_kind::BundleKind::Skill),
                    )
                    .map_err(|e| ClientError::InvalidArgument(e.message))?;
            }
            Arm::ClaimDetach { value: None, .. } => {}
            Arm::SetSplit {
                set,
                names,
                members,
                keeps_own,
            } => {
                edited = true;
                editor.remove_row(&set.reference);
                // Each NEW member row carries the SET line's pin/fields where the member's
                // shape allows them (disclosed in the describe) — a split must not silently
                // strip the line's placement/harness discipline. Explicit beats set: a survivor
                // that already has its own explicit row keeps it untouched (its pin/path/
                // harness/name settings are stronger facts than the set's value).
                //
                // The carriage is resolved PER MEMBER, because a channel's `dest` and `mcp_dest`
                // are two vocabularies: writing one shared value would hand an MCP survivor the
                // channel's skill folders as its row `dest` and cost it every agent.
                for member in members {
                    if names.contains(member) || keeps_own.contains(member) {
                        continue;
                    }
                    let Some(reference) = member_reference(set, member) else {
                        continue;
                    };
                    let (member_value, _, _) =
                        split_carriage(set, member_kind(ctx, target, set, member));
                    editor
                        .set_row(
                            &reference,
                            &member_value,
                            member_value
                                .declared_kind()
                                .unwrap_or(crate::bundle_kind::BundleKind::Skill),
                        )
                        .map_err(|e| ClientError::InvalidArgument(e.message))?;
                }
            }
        }
    }
    if edited {
        editor.write(ctx.fs, &target.path)?;
    }

    // THE DETACH's own half, after the file is settled: the record forgets the claimed folders.
    // No bytes move — topos never created those directories — so there is nothing to park, snapshot,
    // or verify; the map simply stops managing them.
    for arm in &arms {
        let Arm::ClaimDetach { record, dirs, .. } = arm else {
            continue;
        };
        if let Some(sctx) = scope_store_ctx(ctx, target) {
            super::reconcile::detach_placements(&sctx, record, dirs)?;
        }
    }

    let mut items = items;
    // An MCP bundle's row drop (or off-switch) converges the affected scope's CONFIG entries
    // INLINE, so the receipt names the per-agent removals — and discloses a hand-edited entry
    // left behind — instead of deferring the fact to the next sweep. Best-effort: the row edit
    // already landed, and the next sweep's removal convergence reaches the same end state.
    converge_removed_mcp(ctx, target, &arms, &mut items);
    // A manifest file this very edit brought into existence leads the receipt, so the row's
    // inverse is read against the world the edit found rather than the one it made.
    if let Some(note) = born
        && let Some(first) = items.first_mut()
    {
        first.note = Some(match first.note.take() {
            Some(prev) => format!("{note} · {prev}"),
            None => note,
        });
    }
    // A feed-line drop's EAGER uninstall runs in [`remove_global`], after the writer lock
    // releases — the receipt's `uninstalled` block is filled there.
    // NO UNDO BEATS A WRONG ONE: `topos add <path>` cannot re-adopt a folder that is not there,
    // so a row whose folder is gone gets no inverse rather than one that must fail.
    let undo = match arms.as_slice() {
        [Arm::RowDrop { row, .. }] if row_folder_gone(ctx, target, row) => Vec::new(),
        _ => undo_for(&arms, global, manifest_host(ctx).as_deref()),
    };
    Ok(RemoveOutcome::Applied(RemoveData {
        undo,
        items,
        applied: true,
        uninstalled: Vec::new(),
    }))
}

/// The eager-uninstall plan, read off the resolved arms BEFORE the apply consumes them: which
/// dropped FEEDS (their whole delivery leaves), which row-edited BUNDLES (a dropped/off/narrowed
/// row's copies leave), and whether the follow-up reconcile can run TARGETED (bundle names) or
/// must sweep the scope whole (a feed drop).
struct EagerPlan {
    feeds: Vec<String>,
    bundles: Vec<EagerBundle>,
}

struct EagerBundle {
    name: String,
    display: String,
    kind: BundleKind,
    /// `(host, workspace)` for a workspace bundle's row — the identity every record lookup for
    /// this bundle is cross-checked against (see [`scope_record`]). `None` for a local/forge row.
    workspace: Option<(String, String)>,
    /// A NARROWED bundle: the subtracted dest entries + how many destinations the row still names
    /// (`None` when it names none of its own and stands for its default reach).
    narrow: Option<(Vec<String>, Option<u64>)>,
    /// The MACHINE scope store's record for a whole-row edit, resolved BEFORE the apply — the
    /// verb's own retire rail needs it when the reconcile's cache/forge walks miss the record
    /// (and the same reconcile's orphan pass may retire it mid-run, after which the name no
    /// longer resolves). `None` for a narrow, an mcp bundle, a project edit, an unresolvable
    /// name (an ambiguity fails toward not deleting), or a record another workspace delivered.
    record: Option<SkillId>,
    /// A LOCAL row's line-keyed custody identity ([`crate::config_custody::local_identity`]) — the
    /// second spelling its reconcile rows can arrive under when no store record answers for the
    /// folder. `None` for every other shape.
    local: Option<String>,
}

/// THE ROW IS THE DEMAND, so a record one of these arms still names is not retired — whatever
/// marker it carries. A retirement marker records ONE conclusion, reached by the sweep's one-time
/// resolution: that NOTHING claimed the record. A row claiming it now is the standing proof that
/// conclusion has lapsed, and a stale marker must never outrank a live row — every store walker
/// skips a marked record, so the eager uninstall below would enumerate nothing and this removal's
/// receipt would claim copies left that are still sitting on disk.
///
/// Only a LOCAL-PATH row revives, and only through its OWN FOLDER: the record is identified by the
/// directory the row is keyed on, which nothing else can answer to. A name lookup would be the
/// wrong instrument here — a retired record and a later fresh one can share a name, and reviving
/// the wrong one would put settled history back on every surface for the next sweep to resolve a
/// second time, with a second closing statement for one record. Best-effort, like every revive.
///
/// Read (and applied) BEFORE the apply consumes the arms, and only where the removal APPLIES: a
/// describe changes nothing on disk, marker included.
fn revive_rowed_records(ctx: &Ctx<'_>, target: &EditTarget, arms: &[Arm]) {
    let Some(sctx) = scope_store_ctx(ctx, target) else {
        return;
    };
    for arm in arms {
        let row = match arm {
            Arm::RowDrop { row, .. } | Arm::DestNarrow { row: Some(row), .. } => row,
            _ => continue,
        };
        let KeyShape::LocalPath { raw } = &row.shape else {
            continue;
        };
        let dir = canonical_or(&row_dir(ctx, target, raw));
        let Ok(Some(id)) = super::tracked_skill_at(&sctx, &dir) else {
            continue;
        };
        let Ok(sid) = SkillId::parse(&id) else {
            continue;
        };
        crate::sidecar::revive_record(sctx.fs, &sctx.layout, &sid);
    }
}

fn eager_plan(ctx: &Ctx<'_>, target: &EditTarget, arms: &[Arm]) -> EagerPlan {
    let mut plan = EagerPlan {
        feeds: Vec::new(),
        bundles: Vec::new(),
    };
    // The pre-apply record lookup for a MACHINE whole-row edit (see [`EagerBundle::record`]).
    let record_for =
        |name: &str, kind: BundleKind, ws: Option<&(String, String)>| -> Option<SkillId> {
            if kind.is_mcp() || target.scope != ManifestScope::Global {
                return None;
            }
            let sctx = scope_store_ctx(ctx, target)?;
            scope_record(&sctx, ws, name)
        };
    for arm in arms {
        match arm {
            Arm::FeedDrop { workspace, .. } => plan.feeds.push(workspace.clone()),
            Arm::RowDrop { row, name } => {
                let ws = match &row.shape {
                    KeyShape::WorkspaceBundle {
                        host, workspace, ..
                    } => Some((host.clone(), workspace.clone())),
                    _ => None,
                };
                let kind = row_kind(ctx, target, Some(row), ws.as_ref(), name);
                plan.bundles.push(EagerBundle {
                    display: qualified_name(ctx, ws.as_ref(), name),
                    kind,
                    record: record_for(name, kind, ws.as_ref()),
                    local: matches!(row.shape, KeyShape::LocalPath { .. })
                        .then(|| crate::config_custody::local_identity(&row.reference)),
                    name: name.clone(),
                    narrow: None,
                    workspace: ws,
                });
            }
            Arm::OffWrite {
                reference, name, ..
            } => {
                let ws = match keys::classify_key(reference) {
                    Ok(KeyShape::WorkspaceBundle {
                        host, workspace, ..
                    }) => Some((host, workspace)),
                    _ => None,
                };
                let kind = row_kind(ctx, target, None, ws.as_ref(), name);
                plan.bundles.push(EagerBundle {
                    display: qualified_name(ctx, ws.as_ref(), name),
                    kind,
                    record: record_for(name, kind, ws.as_ref()),
                    local: None,
                    name: name.clone(),
                    narrow: None,
                    workspace: ws,
                });
            }
            Arm::DestNarrow {
                name,
                workspace,
                subtract,
                remaining,
                kind,
                ..
            } => {
                plan.bundles.push(EagerBundle {
                    display: qualified_name(ctx, workspace.as_ref(), name),
                    kind: *kind,
                    name: name.clone(),
                    narrow: Some((
                        subtract.clone(),
                        remaining.map(|n| u64::try_from(n).unwrap_or(u64::MAX)),
                    )),
                    record: None,
                    local: None,
                    // The row's own workspace identity: the narrow's retirement rail resolves the
                    // record through it, so a same-named record another workspace delivered can
                    // never answer for this row (see [`scope_record`]).
                    workspace: workspace.clone(),
                });
            }
            // A split re-homes its survivors to their own rows — nothing leaves. Neither does a
            // detach: the folder is the person's own, and no copy of anything is uninstalled.
            Arm::SetSplit { .. } | Arm::ClaimDetach { .. } => {}
        }
    }
    plan
}

/// The scope store's record for `name`, CROSS-CHECKED against the row's workspace identity. The
/// store resolves a BARE NAME, so a workspace row whose own bundle has no record here would
/// otherwise answer with a same-named record ANOTHER workspace delivered — and the eager rails
/// would retire that record's placements and put them on this row's receipt. A recorded origin
/// that disagrees resolves to nothing: no deletion, no claim (the fallback fails TOWARD keeping).
/// An UNRECORDED origin (a local adopt, a forge import, a cache entry that is gone) is exactly the
/// case the fallback exists for and still resolves, as does every local/forge row (`ws = None`).
fn scope_record(sctx: &Ctx<'_>, ws: Option<&(String, String)>, name: &str) -> Option<SkillId> {
    let (sid, _) = super::resolve_skill(sctx, name).ok()?;
    if let Some((host, workspace)) = ws
        && let Some((rec_host, rec_ws)) = record_origin(sctx, &sid)
        && (&rec_host != host || &rec_ws != workspace)
    {
        return None;
    }
    Some(sid)
}

/// The `(host, workspace)` a stored record was delivered by, as this scope's delivery cache
/// records it — `None` when nothing local names an origin for it.
fn record_origin(sctx: &Ctx<'_>, sid: &SkillId) -> Option<(String, String)> {
    let cache = crate::sync_status::read(sctx.fs, &sctx.layout).ok()?;
    cache.workspaces.values().find_map(|e| {
        (e.delivered.contains_key(sid.as_str()))
            .then(|| Some((e.host.clone()?, e.workspace_name.clone()?)))
            .flatten()
    })
}

/// The in-invocation cleanup a row edit drives: ONE reconcile of the edited scope (the exact
/// machinery the background sweep runs — nothing bespoke), then the receipt's uninstall facts are
/// read back off its rows: what left which destinations, and which edited copies stayed. Filtered
/// to the dropped feeds' and edited bundles' rows, so a receipt never claims another source's
/// movement. A NARROWED bundle's entry speaks in the subtracted dest entries and carries the
/// remaining count — kept only when the reconcile actually reported the copies moving; when it
/// did not (offline, a failed converge), the matching item's own line discloses the next-sweep
/// path instead of claiming a removal that never ran.
///
/// A WHOLE-ROW machine edit the reconcile moved nothing for gets one more rail: the sweep's
/// cleaner discovers records through the delivery cache and the forge imports, so a record
/// neither enumerates (a local-path row's; a workspace row's under a dark plane or a gone cache
/// entry) is retired HERE, through the same by-choice rail — the verb witnessed the intent, and
/// the machine set is re-resolved first so a name the feed still delivers moves nothing.
fn eager_cleanup(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    target: &EditTarget,
    scope: super::reconcile::UpdateScope,
    plan: &EagerPlan,
    items: &mut [RemoveItem],
) -> Vec<topos_types::results::UninstalledBundle> {
    use topos_types::results::{PullAction, UninstalledBundle};
    if plan.feeds.is_empty() && plan.bundles.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = live_sessions(ctx)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| plan.feeds.contains(&s.workspace_name))
        .map(|s| s.workspace_id)
        .collect();
    let removed: Vec<topos_types::results::PullSkill> = super::reconcile::manifest_update(
        ctx,
        connect,
        None,
        // UNTARGETED: a dropped row's undemanded clean (and a dropped feed's whole delivery)
        // is only seen by the full scope sweep — the same machinery the background sweep runs.
        &super::reconcile::ManifestUpdateOpts {
            scope,
            ..Default::default()
        },
    )
    .map(|out| {
        out.data
            .skills
            .into_iter()
            .filter(|r| r.action == PullAction::Removed)
            .collect()
    })
    .unwrap_or_default();
    let mut out: Vec<UninstalledBundle> = Vec::new();
    // What the MACHINE recipe still delivers AFTER the edit (and this run's cache refresh) —
    // the fallback rail's freeze: a name a surviving row or the cached feed still claims must
    // not lose its copies to the retire below. Resolved lazily (offline by construction), once,
    // and keyed name-and-reference → THE LINE that delivers it (the feed address when a
    // workspace feed row does, `None` for any other line), so the copies-stay note names the
    // real deliverer instead of assuming the dropped row's own workspace fed it.
    let mut lines: Option<Option<std::collections::HashMap<String, Option<String>>>> = None;
    let mut demand = |ctx: &Ctx<'_>, name: &str| -> Demand {
        let resolved = lines.get_or_insert_with(|| {
            let (all, cache) = super::inventory::read_sources(ctx).ok()?;
            let resolved = super::inventory::resolve(ctx, &all, &cache).ok()?;
            let mut map: std::collections::HashMap<String, Option<String>> =
                std::collections::HashMap::new();
            for row in resolved.machine().rows.iter().filter(|r| r.bundle) {
                for key in [row.name.clone(), row.reference.clone()] {
                    map.entry(key).or_insert_with(|| row.feed.clone());
                }
            }
            Some(map)
        });
        match resolved.as_ref() {
            None => Demand::Unreadable,
            Some(map) => match map.get(name) {
                Some(feed) => Demand::Still(feed.clone()),
                None => Demand::Gone,
            },
        }
    };
    // The dropped feeds' bundles (attributed by workspace id, exactly as before).
    for r in removed.iter().filter(|r| {
        r.workspace_id
            .as_deref()
            .is_some_and(|w| ids.iter().any(|i| i == w))
    }) {
        out.push(UninstalledBundle {
            name: r.display.clone().unwrap_or_else(|| r.skill.clone()),
            destinations: r.destinations.clone(),
            kind: r.kind.clone(),
            kept: r.kept.clone(),
            remaining: None,
        });
    }
    // The row-edited bundles, by name — and by the scope store's identity for the same bundle
    // (a config-placed bundle's removal row can carry the tracked id when no delivery ever
    // named it).
    for b in &plan.bundles {
        // Cross-checked like the retire rail's own record: a same-named record another workspace
        // delivered must not put ITS reconcile movements on this row's receipt.
        let store_id: Option<String> = scope_store_ctx(ctx, target)
            .and_then(|sctx| scope_record(&sctx, b.workspace.as_ref(), &b.name))
            .map(|sid| sid.as_str().to_owned());
        let rows: Vec<&topos_types::results::PullSkill> = removed
            .iter()
            .filter(|r| {
                r.skill == b.name
                    || store_id.as_deref() == Some(r.skill.as_str())
                    || b.local.as_deref() == Some(r.skill.as_str())
            })
            .collect();
        let mut destinations: Vec<String> = Vec::new();
        let mut kept: Vec<String> = Vec::new();
        for r in &rows {
            destinations.extend(r.destinations.iter().cloned());
            kept.extend(r.kept.iter().cloned());
        }
        destinations.dedup();
        kept.dedup();
        match &b.narrow {
            // The narrow's receipt speaks in the SUBTRACTED dest entries — the row's own
            // spellings — plus what remains. GATED on the reconcile reporting this bundle's
            // copies actually moving (a matching `Removed` row), like the whole-row arm below:
            // offline, the shrink never ran, and `- … removed (…)` would claim a copy left that
            // is still on disk — the row edit stays the receipt, with the next-sweep path on its
            // own line.
            Some((subtract, remaining)) => {
                if rows.is_empty() {
                    // THE SWEEP DID NOT RETIRE IT, so the verb does. A row left standing for its
                    // DEFAULT REACH plans every agent-less placement it finds recorded, which is
                    // what keeps an adopted folder — and would keep the de-listed one forever,
                    // under a receipt promising the next update takes it. The verb knows exactly
                    // which folders left the row, so it retires those and nothing else, through
                    // the same by-choice rail the whole-row arm runs (edited copies kept in
                    // place). Best-effort, like the whole eager contract.
                    if let Some(clean) = retire_subtracted(ctx, target, b, subtract) {
                        // The receipt speaks in the row's own spellings, and only where a copy
                        // really left: an entry whose only copy was KEPT (edited in place) makes
                        // no removal claim, so a mixed retire names exactly the roots that
                        // emptied and nothing beside them.
                        // The removed list speaks pretty spellings (`~`-abbreviated), so an entry
                        // matches through its own spelling first and its resolved root second.
                        let destinations: Vec<String> = subtract
                            .iter()
                            .filter(|entry| {
                                let root = dest_root(ctx, target, entry);
                                clean.removed.iter().any(|p| {
                                    let parent = Path::new(p).parent();
                                    parent == Some(Path::new(entry.as_str()))
                                        || (root.is_some() && parent == root.as_deref())
                                })
                            })
                            .cloned()
                            .collect();
                        out.push(UninstalledBundle {
                            name: b.display.clone(),
                            destinations,
                            kind: b.kind.tag(),
                            kept: clean.kept,
                            remaining: *remaining,
                        });
                        continue;
                    }
                    let keeps = match remaining {
                        Some(n) => crate::actions::Subject::of_kind(b.kind.tag().as_deref())
                            .targets(usize::try_from(*n).unwrap_or(usize::MAX)),
                        None => "its default reach".to_owned(),
                    };
                    let note = format!(
                        "the destination left its row, but its copy could not be uninstalled \
                         this run — it leaves on the next `topos update`; the row keeps {keeps}"
                    );
                    if let Some(item) = items
                        .iter_mut()
                        .find(|it| it.kind == RemoveKind::ManifestNarrowed && it.name == b.name)
                    {
                        item.note = Some(match item.note.take() {
                            Some(prev) => format!("{prev} · {note}"),
                            None => note,
                        });
                    }
                    continue;
                }
                out.push(UninstalledBundle {
                    name: b.display.clone(),
                    destinations: subtract.clone(),
                    kind: b.kind.tag(),
                    kept,
                    remaining: *remaining,
                });
            }
            None => {
                if destinations.is_empty() && kept.is_empty() {
                    // The reconcile moved nothing for this whole-row edit. FIRST: does the
                    // machine set STILL deliver the bundle (the standing feed row, a surviving
                    // line)? Then the copies CORRECTLY stay — the item's own line says so,
                    // naming the feed and the off switch, so the receipt's stock copies-leave
                    // sentence can never lie about them — and nothing below may move. An
                    // unreadable resolution keeps the copies too, claiming nothing.
                    if target.scope == ManifestScope::Global {
                        match demand(ctx, &b.name) {
                            Demand::Still(feed) => {
                                note_still_delivered(b, feed.as_deref(), items);
                                continue;
                            }
                            Demand::Unreadable => continue,
                            Demand::Gone => {}
                        }
                    }
                    // Otherwise the sweep's cleaner walks the delivery cache and the forge
                    // imports — a record neither enumerates (a local-path row's, a workspace
                    // row's while the plane is dark or its cache entry is gone) would keep
                    // every placed copy while the receipt claims otherwise. The verb witnessed
                    // the intent, so the copies leave HERE, through the same by-choice rail the
                    // sweep runs (park-then-verify; edited and unreadable copies kept in place;
                    // adopted sources and in-checkout placements never touched). The record was
                    // resolved BEFORE the apply: the same reconcile's orphan pass may have
                    // retired it just now, which hides the name, not the record.
                    let Some(sid) = b.record.as_ref().filter(|_| !b.kind.is_mcp()) else {
                        continue;
                    };
                    let Some(sctx) = scope_store_ctx(ctx, target) else {
                        continue;
                    };
                    // Best-effort like the whole eager contract: a failed retire leaves the
                    // copies for the sweep, claiming nothing.
                    if let Ok(Some(clean)) = super::reconcile::clean_by_choice(&sctx, sid, true) {
                        if let Some(item) = items.iter_mut().find(|it| {
                            it.name == b.name
                                && matches!(
                                    it.kind,
                                    RemoveKind::ManifestRemoved | RemoveKind::ManifestExcluded
                                )
                        }) {
                            item.dest_dirs = clean.removed.clone();
                        }
                        out.push(UninstalledBundle {
                            name: b.display.clone(),
                            destinations: clean.removed,
                            kind: None,
                            kept: clean.kept,
                            remaining: None,
                        });
                    }
                    continue;
                }
                out.push(UninstalledBundle {
                    name: b.display.clone(),
                    destinations,
                    kind: b.kind.tag(),
                    kept,
                    remaining: None,
                });
            }
        }
    }
    out
}

/// Retire the copies a NARROW just de-listed, in the invocation that narrowed the row — the rail
/// [`eager_cleanup`] falls back to when the reconcile reported nothing moving (see the comment at
/// its call site). `None` when there is nothing to speak for: no store here, no record of this
/// bundle, a config-placed bundle (whose entries are the converge's own to retire), a dest entry
/// that resolves nowhere, or no retirable copy under any of them.
fn retire_subtracted(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    b: &EagerBundle,
    subtract: &[String],
) -> Option<super::reconcile::ByChoiceClean> {
    if b.kind.is_mcp() {
        return None;
    }
    let sctx = scope_store_ctx(ctx, target)?;
    let sid = scope_record(&sctx, b.workspace.as_ref(), &b.name)?;
    let roots: Vec<PathBuf> = subtract
        .iter()
        .filter_map(|entry| dest_root(ctx, target, entry))
        .collect();
    if roots.is_empty() {
        return None;
    }
    super::reconcile::clean_dest_roots(&sctx, &sid, &roots)
        .ok()
        .flatten()
}

/// Where one `dest` entry resolves on this machine — the same reading the placement planner gives
/// it: checkout-relative for a project row, `~/`-expanded (else absolute) for the machine's.
/// `None` when the entry cannot resolve here (a `~/` entry on a machine with no known home).
fn dest_root(ctx: &Ctx<'_>, target: &EditTarget, entry: &str) -> Option<PathBuf> {
    if target.scope == ManifestScope::Project {
        return Some(target.dir.join(entry.trim_start_matches("./")));
    }
    match entry.strip_prefix("~/") {
        Some(rest) => Some(ctx.roots.as_ref()?.home.join(rest)),
        None => Some(PathBuf::from(entry)),
    }
}

/// What the POST-EDIT machine resolution says about a bundle whose row just left (see the
/// fallback rail in [`eager_cleanup`]).
enum Demand {
    /// A surviving line still delivers it, so the copies stay: the `<host>/<workspace>` address
    /// when a workspace FEED row is that line, `None` for any other (a channel set, another
    /// workspace's row, a local line).
    Still(Option<String>),
    /// Provably nothing in the machine recipe demands it any more.
    Gone,
    /// The resolution could not be read — nothing is claimed and nothing moves.
    Unreadable,
}

/// The copies-stay disclosure for a WHOLE-ROW removal whose bundle the machine set STILL
/// delivers: the row edit landed, but a standing line keeps demanding the bundle, so its copies
/// deliberately stay in place. Said on the item's own line (the note wins over the stock
/// sentence).
///
/// `feed` is the delivering line AS THE RESOLUTION REPORTS IT, never as the dropped row's shape
/// suggests: a workspace feed address earns the named-feed wording plus the off switch (`topos
/// remove -g <name>` IS the off write for a feed-delivered bundle), and every other line gets the
/// honest generic sentence — naming a feed that does not carry this bundle would point the reader
/// at a switch that does not reach it, so the deep read is what the note offers instead.
fn note_still_delivered(b: &EagerBundle, feed: Option<&str>, items: &mut [RemoveItem]) {
    let Some(item) = items
        .iter_mut()
        .find(|it| it.kind == RemoveKind::ManifestRemoved && it.name == b.name)
    else {
        return;
    };
    let note = match feed.and_then(|f| f.rsplit('/').next()) {
        Some(ws) => format!(
            "the row is gone, but {ws}'s feed still delivers it here; the copies stay in place \
             (`topos remove -g {}` switches it off)",
            b.name
        ),
        None => format!(
            "the row is gone, but another line here still delivers it; the copies stay in place \
             (`topos list {}` shows which)",
            b.name
        ),
    };
    item.note = Some(match item.note.take() {
        Some(prev) => format!("{prev} · {note}"),
        None => note,
    });
}

/// The SCOPE STORE one manifest-edit target's config custody lives in, with the project root its
/// config surfaces resolve against — the ONE resolution every MCP entry point shares (`add`'s
/// inline converge, the dest-root read, the removal converge), so no two of them can disagree
/// about which store a row's entries belong to. The home layout for a GLOBAL target; the
/// checkout's own EXISTING store otherwise — none of these paths mints one, because a scope that
/// never placed has nothing to converge. `None` = there is no such store, so nothing was placed.
pub(crate) fn mcp_scope_target(
    ctx: &Ctx<'_>,
    target: &EditTarget,
) -> Option<(crate::sidecar::Layout, Option<PathBuf>)> {
    match target.scope {
        ManifestScope::Global => Some((ctx.layout.clone(), None)),
        ManifestScope::Project => crate::sidecar::existing_project_store(ctx.fs, &target.dir)
            .map(|l| (l, Some(target.dir.clone()))),
    }
}

/// [`mcp_scope_target`] when only the store is wanted.
pub(crate) fn mcp_scope_store(
    ctx: &Ctx<'_>,
    target: &EditTarget,
) -> Option<crate::sidecar::Layout> {
    mcp_scope_target(ctx, target).map(|(l, _)| l)
}

/// The inline MCP removal convergence a `remove` apply runs (see the call site): for each dropped
/// row (or written `"off"` switch) whose bundle is a `kind = "mcp"` one, run
/// [`crate::mcp_engine::remove_bundle`] against the edited scope and fold the per-agent outcomes
/// into that item's receipt note.
fn converge_removed_mcp(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    arms: &[Arm],
    items: &mut [RemoveItem],
) {
    let Some(roots) = ctx.roots.clone() else {
        return;
    };
    let Some((layout, project_root)) = mcp_scope_target(ctx, target) else {
        return;
    };
    if !ctx.fs.exists(&layout.config_custody_path()) {
        return; // nothing was ever config-placed in this scope
    }
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let detected: std::collections::BTreeSet<String> = topos_harness::registry::detected_harnesses(
        &roots.home,
        project_root.as_deref().or(roots.cwd.as_deref()),
    )
    .iter()
    .map(|h| h.slug.to_owned())
    .collect();
    let io = crate::mcp_engine::ScopeIo {
        fs: ctx.fs,
        runtimes: &crate::mcp_render::PathRuntimes,
        relay_program: crate::mcp_engine::relay_program(),
        layout: &layout,
        home: roots.home.clone(),
        project_root: project_root.clone(),
    };
    for (arm, item) in arms.iter().zip(items.iter_mut()) {
        let Some(bundle_id) = mcp_bundle_of_arm(ctx, target, &cache, arm) else {
            continue;
        };
        let outcome = crate::mcp_engine::remove_bundle(
            &io,
            &topos_harness::mcp::descriptor::mcp_harnesses_for_teardown(),
            &detected,
            &bundle_id,
            &item.name,
        );
        let mut lines: Vec<String> = Vec::new();
        // ORDER IS THE MESSAGE. A hand-edited entry SURVIVING a removal is the one fact a
        // person must not miss, and it used to sit at whatever position the harness table
        // happened to order it into — position four of eleven, in a `·`-joined run-on. The
        // clauses that say "something of yours is still there" lead; the ordinary departures
        // follow.
        let mut kept: Vec<String> = Vec::new();
        for removed in &outcome.removed {
            // Keyed by the config FILE the entry lived in — receipts speak in destinations,
            // never agents.
            let file = removed
                .state
                .file
                .as_deref()
                .map_or_else(|| "its config".to_owned(), |f| pretty_path(ctx, f));
            match removed.state.state {
                topos_types::results::TargetOutcome::Drifted => {
                    kept.push(format!(
                        "{file}: the entry you edited by hand is left in place."
                    ));
                }
                _ => lines.push(format!("{file}: the server's entry was removed.")),
            }
        }
        let mut lines = {
            kept.extend(lines);
            kept
        };
        for w in outcome.notices.iter().chain(&outcome.warnings) {
            lines.push(w.text.clone());
        }
        if lines.is_empty() {
            continue;
        }
        // ONE CLAUSE PER LINE. The renderer leads with the first and indents the rest, so a
        // six-agent removal reads as a list instead of one sentence nobody finishes.
        let folded = lines.join("\n");
        item.note = Some(match item.note.take() {
            Some(prev) => format!("{prev}\n{folded}"),
            None => folded,
        });
    }
}

/// The mcp bundle one removal arm drops, when it drops one: a `kind = "mcp"` local path row (its
/// tracked identity), or a workspace bundle the delivery cache knows as `mcp` (its skill id).
/// `None` for everything else — a skill row, a feed drop, a set split (a split member's config
/// entries leave through the next sweep's removal convergence, like a feed drop's skills leave
/// its dirs).
fn mcp_bundle_of_arm(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    cache: &crate::sync_status::SyncStatus,
    arm: &Arm,
) -> Option<String> {
    let ws_bundle = |host: &str, workspace: &str, bundle: &str| -> Option<String> {
        cache
            .workspaces
            .iter()
            .find(|(_, e)| {
                e.host.as_deref() == Some(host) && e.workspace_name.as_deref() == Some(workspace)
            })
            .and_then(|(_, e)| {
                e.delivered
                    .iter()
                    .find(|(_, d)| {
                        d.name == bundle
                            && BundleKind::of_tag(d.kind.as_deref()) == Some(BundleKind::Mcp)
                    })
                    .map(|(id, _)| id.clone())
            })
    };
    match arm {
        Arm::RowDrop { row, .. } => match &row.shape {
            KeyShape::WorkspaceBundle {
                host,
                workspace,
                bundle,
            } => ws_bundle(host, workspace, bundle),
            KeyShape::LocalPath { raw } if row.value.declared_kind() == Some(BundleKind::Mcp) => {
                // Resolved the way every local-path key resolves ([`row_dir`], the `~/` arm
                // included), so a home-spelled row canonicalizes to the identity its adopt
                // recorded instead of falling back to a `local:` guess no minted key answers.
                let dir = row_dir(ctx, target, raw);
                // The tracked identity: the scope's own store first (a project row's custody
                // lives in the checkout), then the home store — the same identity the demand
                // side minted the config key under.
                let tracked = dir.canonicalize().ok().and_then(|canonical| {
                    let project = crate::sidecar::existing_project_store(ctx.fs, &target.dir)
                        .and_then(|playout| {
                            let pctx = crate::ops::pull::ctx_with_layout(ctx, &playout);
                            crate::ops::add::tracked_skill_at(&pctx, &canonical)
                                .ok()
                                .flatten()
                        });
                    project.or_else(|| {
                        crate::ops::add::tracked_skill_at(ctx, &canonical)
                            .ok()
                            .flatten()
                    })
                });
                Some(
                    tracked
                        .unwrap_or_else(|| crate::config_custody::local_identity(&row.reference)),
                )
            }
            _ => None,
        },
        Arm::OffWrite { reference, .. } => match keys::classify_key(reference) {
            Ok(KeyShape::WorkspaceBundle {
                host,
                workspace,
                bundle,
            }) => ws_bundle(&host, &workspace, &bundle),
            _ => None,
        },
        Arm::FeedDrop { .. }
        | Arm::SetSplit { .. }
        | Arm::DestNarrow { .. }
        | Arm::ClaimDetach { .. } => None,
    }
}

/// Re-prove every arm against the EXACT document instance about to be edited — the caller reads
/// the file once and hands that one text to both this proof and the editor, so there is no
/// second read an external edit could slip between.
///
/// The arms carry decisions the file decided: which row to drop, at which value, and — for a set
/// split — which survivors already have their own row and which need one written. Each of those is
/// falsified by an edit this process did not make, and applying a falsified plan is how a
/// concurrent row disappears without an error anywhere. So a document that has moved refuses; the
/// re-run reads the file as it is.
///
/// # Errors
/// [`ClientError::ManifestChanged`] when any arm no longer describes the document; a manifest the
/// grammar now refuses.
fn prove_unchanged(
    target: &EditTarget,
    arms: &[Arm],
    text: Option<&str>,
) -> Result<(), ClientError> {
    // A file that does not exist yet is about to be BORN by this call — there is nothing to have
    // changed, and the birth is part of the edit.
    let Some(text) = text else {
        return Ok(());
    };
    let doc = crate::manifest::document::parse_manifest(text, target.scope)
        .map_err(|e| corrupt(&target.path, &e))?;
    let plan = ScopePlan::from_doc(&doc, Some(target.path.clone()));
    let rows = all_rows(&plan);
    let holds = |arm: &Arm| -> bool {
        match arm {
            Arm::RowDrop { row, .. } => rows
                .iter()
                .any(|r| r.reference == row.reference && r.value == row.value),
            Arm::FeedDrop { reference, .. } => {
                matches!(keys::classify_key(reference), Ok(KeyShape::Feed { host, workspace })
                    if plan.has_feed(&host, &workspace))
            }
            // The switch is written BECAUSE no row claims the bundle; a row that appeared since
            // would have to be dropped instead.
            Arm::OffWrite { reference, .. } => !rows.iter().any(|r| r.reference == *reference),
            // A narrow is a decision about the row's exact value (or its absence, for the
            // feed-materialized form) — any drift refuses toward a re-read.
            Arm::DestNarrow { row, reference, .. } => match row {
                Some(row) => rows
                    .iter()
                    .any(|r| r.reference == row.reference && r.value == row.value),
                None => !rows.iter().any(|r| r.reference == *reference),
            },
            // A detach that edits no row proves nothing about the file — it is a record act. One
            // that DOES is a decision about that row's exact value, and drift refuses, exactly as
            // it does for the narrow beside it.
            Arm::ClaimDetach { value: None, .. } => true,
            Arm::ClaimDetach { row, .. } => row.as_ref().is_some_and(|row| {
                rows.iter()
                    .any(|r| r.reference == row.reference && r.value == row.value)
            }),
            Arm::SetSplit {
                set,
                names,
                members,
                keeps_own,
            } => {
                let line_stands = rows
                    .iter()
                    .any(|r| r.reference == set.reference && r.value == set.value);
                let now: Vec<String> = members
                    .iter()
                    .filter(|m| !names.contains(m))
                    .filter(|m| {
                        member_reference(set, m)
                            .is_some_and(|r| rows.iter().any(|pr| pr.reference == r))
                    })
                    .cloned()
                    .collect();
                line_stands && now == *keeps_own
            }
        }
    };
    if arms.iter().all(holds) {
        return Ok(());
    }
    Err(ClientError::ManifestChanged {
        path: target.path.display().to_string(),
    })
}

/// A path as a person reads it — `~`-abbreviated under the machine home.
fn pretty_path(ctx: &Ctx<'_>, path: &str) -> String {
    super::inventory::pretty(ctx, Path::new(path))
}

/// Join a loss-guard line with an arm's own removal note (` · `-separated when both exist).
fn join_notes(lead: String, tail: Option<String>) -> String {
    match tail {
        Some(t) => format!("{lead} · {t}"),
        None => lead,
    }
}

/// What a set split writes on ONE surviving member row of `kind`, and how the describe words it:
/// the set line's pin/fields filtered to what that member legally carries —
/// `(member value, carried field names, dropped field names)`.
///
/// Carriage is PER KIND, because a channel's two destination arrays mean different things: `dest`
/// names skill placement FOLDERS and `mcp_dest` names MCP CONFIG FILES, while a member row has one
/// `dest` whose meaning is decided by that row's own kind. So a SKILL survivor takes the line's
/// `dest`, an MCP survivor takes the line's `mcp_dest` AS its row `dest`, and whichever array the
/// survivor's kind cannot read is disclosed as dropped rather than silently written onto it —
/// which would freeze the row to destinations of the wrong vocabulary and cost it every agent. A
/// channel with no `mcp_dest` leaves its MCP survivors with no `dest` at all (they reach every
/// MCP-capable agent, which is what the unnarrowed line did for them). A repo set's members are
/// always skills, and its commit pin is a legal member pin verbatim.
fn split_carriage(
    set: &PlanRow,
    kind: BundleKind,
) -> (EntryValue, Vec<&'static str>, Vec<&'static str>) {
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
            // The two destination arrays, resolved against THIS member's kind: the one written in
            // the survivor's own vocabulary becomes its row `dest`, the other is disclosed as
            // dropped. Legality is asked of the field the value LANDS in (`dest` on the member
            // row), never of the set-line spelling it came from.
            let (mine, theirs) = if kind.is_mcp() {
                (("mcp_dest", &f.mcp_dest), ("dest", &f.dest))
            } else {
                (("dest", &f.dest), ("mcp_dest", &f.mcp_dest))
            };
            take("dest", mine.1.is_some(), &mut || out.dest = mine.1.clone());
            take("name", f.name.is_some(), &mut || out.name = f.name.clone());
            take("subdir", f.subdir.is_some(), &mut || {
                out.subdir = f.subdir.clone();
            });
            take("kind", f.kind.is_some(), &mut || out.kind = f.kind.clone());
            // The set-line spelling is what the describe names, so a carried `mcp_dest` reads as
            // itself rather than as the `dest` it became.
            if mine.1.is_some() {
                for c in &mut carried {
                    if *c == "dest" {
                        *c = mine.0;
                    }
                }
            }
            if theirs.1.is_some() {
                dropped.push(theirs.0);
            }
            let value = if out == EntryFields::default() {
                EntryValue::Star
            } else {
                EntryValue::Fields(out)
            };
            (value, carried, dropped)
        }
    }
}

/// WHAT one set member is — the kind that decides which of a channel's two destination arrays its
/// survivor row carries (see [`split_carriage`]). A channel member is resolved through the same
/// chain an explicit row's kind is (`row_kind`, qualified by the channel's own workspace); a repo
/// set's members are always skills, because `kind = "mcp"` on a GitHub row refuses at load.
fn member_kind(ctx: &Ctx<'_>, target: &EditTarget, set: &PlanRow, member: &str) -> BundleKind {
    match &set.shape {
        KeyShape::Channel {
            host, workspace, ..
        } => row_kind(
            ctx,
            target,
            None,
            Some(&(host.clone(), workspace.clone())),
            member,
        ),
        _ => BundleKind::Skill,
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
/// whose row carried nothing but a version pin re-adds exactly what left; a fields row carrying
/// ONLY destinations (and a pin) re-adds with its `-a`/`--dest` reconstruction (`-a` where a
/// folder maps back to the registry table, `--dest` otherwise); a set split, a fields row the
/// `add` grammar cannot respell (a name override, a subdir, a kind), or a multi-target batch
/// offers none (no undo beats a wrong one). A FEED line's inverse spells the `@ws` sugar when
/// `sugar_host` (this machine's one connected host) resolves it — the same default-host rule
/// every verb's sugar uses; a dest reconstruction sugars a workspace bundle the same way.
fn undo_for(arms: &[Arm], global: bool, sugar_host: Option<&str>) -> Vec<String> {
    let [arm] = arms else {
        return Vec::new();
    };
    let scope = if global {
        ManifestScope::Global
    } else {
        ManifestScope::Project
    };
    let mut tail: Vec<String> = Vec::new();
    let spelling = match arm {
        Arm::RowDrop { row, .. } => match &row.value {
            EntryValue::Star => row.reference.clone(),
            EntryValue::Pin(p) => format!("{}@{p}", row.reference),
            // A fields row carrying ONLY destinations (and a pin) is respellable — the `-a`/
            // `--dest` flags reconstruct exactly the set that left. Anything else the `add`
            // grammar cannot respell (a name override, a subdir, a kind, a channel's `mcp_dest` —
            // no flag writes that array), so no undo.
            EntryValue::Fields(f) => {
                let respellable = f.name.is_none()
                    && f.subdir.is_none()
                    && f.kind.is_none()
                    && f.mcp_dest.is_none();
                let Some(dest) = f.dest.as_ref().filter(|_| respellable) else {
                    return Vec::new();
                };
                // A row the `add` grammar can respell carries no `kind`, so its destinations
                // are skills folders by construction.
                tail = super::dest_select::undo_tail(dest, scope, BundleKind::Skill);
                let base = sugar_row_reference(&row.shape, sugar_host)
                    .unwrap_or_else(|| row.reference.clone());
                match &f.version {
                    Some(v) => format!("{base}@{v}"),
                    None => base,
                }
            }
            EntryValue::Off => return Vec::new(),
        },
        Arm::FeedDrop {
            reference,
            workspace,
            ..
        } => match keys::classify_key(reference) {
            Ok(KeyShape::Feed { host, .. }) if sugar_host == Some(host.as_str()) => {
                format!("@{workspace}")
            }
            _ => reference.clone(),
        },
        Arm::OffWrite { reference, .. } => reference.clone(),
        Arm::DestNarrow {
            reference,
            name,
            workspace,
            subtract,
            kind,
            ..
        } => {
            // The narrow's inverse: re-add exactly the subtracted destination(s).
            tail = super::dest_select::undo_tail(subtract, scope, *kind);
            match workspace {
                Some((host, ws)) if sugar_host == Some(host.as_str()) => format!("@{ws}/{name}"),
                _ => reference.clone(),
            }
        }
        // A DETACH's inverse is the claim that made it: the folder, and the bundle it is a copy
        // of. It restores the whole prior state — the placement row with its baseline re-derived
        // from the same bytes, and the row entry the claim itself added — so it is offered. ONE
        // folder only: two claims in one command have no single re-claim spelling.
        Arm::ClaimDetach { dirs, origin, .. } => {
            let ([dir], Some(origin)) = (dirs.as_slice(), origin.as_ref()) else {
                return Vec::new();
            };
            tail = vec!["--as".to_owned(), origin.clone()];
            dir.clone()
        }
        Arm::SetSplit { .. } => return Vec::new(),
    };
    let mut argv = vec!["topos".to_owned(), "add".to_owned()];
    if global {
        argv.push("-g".to_owned());
    }
    argv.push(spelling);
    argv.extend(tail);
    argv
}

/// The `@ws/<bundle>` sugar for a workspace-bundle row at the machine's one connected host.
fn sugar_row_reference(shape: &KeyShape, sugar_host: Option<&str>) -> Option<String> {
    match shape {
        KeyShape::WorkspaceBundle {
            host,
            workspace,
            bundle,
        } if sugar_host == Some(host.as_str()) => Some(format!("@{workspace}/{bundle}")),
        _ => None,
    }
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
            section: crate::manifest::document::SectionKind::Skills,
            reference: "topos.sh/acme/deploy".into(),
            shape: keys::classify_key("topos.sh/acme/deploy").unwrap(),
            value,
        };
        let star = Arm::RowDrop {
            row: row(EntryValue::Star),
            name: "deploy".into(),
        };
        assert_eq!(
            undo_for(std::slice::from_ref(&star), false, None),
            vec!["topos", "add", "topos.sh/acme/deploy"]
        );
        assert_eq!(
            undo_for(std::slice::from_ref(&star), true, None),
            vec!["topos", "add", "-g", "topos.sh/acme/deploy"]
        );
        let digest = "0123456789abcdef".repeat(4);
        let pinned = Arm::RowDrop {
            row: row(EntryValue::Pin(digest.clone())),
            name: "deploy".into(),
        };
        assert_eq!(
            undo_for(std::slice::from_ref(&pinned), false, None),
            vec!["topos", "add", &format!("topos.sh/acme/deploy@{digest}")]
        );
        // A fields row carrying ONLY destinations is respellable now — the undo reconstructs
        // the set as `-a` sugar where the folder maps back to the registry table.
        let fields = Arm::RowDrop {
            row: row(EntryValue::Fields(crate::manifest::document::EntryFields {
                dest: Some(vec!["~/.claude/skills".into()]),
                ..Default::default()
            })),
            name: "deploy".into(),
        };
        assert_eq!(
            undo_for(&[fields], true, Some("topos.sh")),
            vec!["topos", "add", "-g", "@acme/deploy", "-a", "claude-code"]
        );
        // A fields row with a NAME OVERRIDE has no single restoring command — no undo at all.
        let named = Arm::RowDrop {
            row: row(EntryValue::Fields(crate::manifest::document::EntryFields {
                dest: Some(vec!["~/.claude/skills".into()]),
                name: Some("other".into()),
                ..Default::default()
            })),
            name: "other".into(),
        };
        assert!(undo_for(&[named], false, None).is_empty());
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
                false,
                None
            )
            .is_empty()
        );
    }

    #[test]
    fn a_feed_drops_undo_spells_the_sugar_at_the_default_host() {
        let feed = |host: &str| Arm::FeedDrop {
            reference: format!("{host}/acme"),
            workspace: "acme".into(),
            bundles: Vec::new(),
        };
        // The machine's ONE connected host resolves the `@ws` sugar — the undo the final receipt
        // prints, byte for byte.
        assert_eq!(
            undo_for(
                std::slice::from_ref(&feed("topos.sh")),
                true,
                Some("topos.sh")
            ),
            vec!["topos", "add", "-g", "@acme"]
        );
        // Another host (or no resolvable sugar) keeps the full spelling.
        assert_eq!(
            undo_for(
                std::slice::from_ref(&feed("other.example")),
                true,
                Some("topos.sh")
            ),
            vec!["topos", "add", "-g", "other.example/acme"]
        );
        assert_eq!(
            undo_for(std::slice::from_ref(&feed("topos.sh")), true, None),
            vec!["topos", "add", "-g", "topos.sh/acme"]
        );
    }

    #[test]
    fn a_set_split_carries_what_the_member_shape_legally_takes() {
        use crate::manifest::document::EntryFields;
        // A repo set's commit PIN rides each member row verbatim.
        let repo_pinned = PlanRow {
            section: crate::manifest::document::SectionKind::Skills,
            reference: "github.com/o/r".into(),
            shape: keys::classify_key("github.com/o/r").unwrap(),
            value: EntryValue::Pin("abc1234".into()),
        };
        let (value, carried, dropped) = split_carriage(&repo_pinned, BundleKind::Skill);
        assert_eq!(value, EntryValue::Pin("abc1234".into()));
        assert_eq!(carried, vec!["version"]);
        assert!(dropped.is_empty());

        // A channel line's `dest` carries onto its workspace-bundle members.
        let channel_fields = PlanRow {
            section: crate::manifest::document::SectionKind::Channels,
            reference: "topos.sh/acme/channels/backend".into(),
            shape: keys::classify_key("topos.sh/acme/channels/backend").unwrap(),
            value: EntryValue::Fields(EntryFields {
                dest: Some(vec![".agents/skills".into()]),
                ..Default::default()
            }),
        };
        let (value, carried, dropped) = split_carriage(&channel_fields, BundleKind::Skill);
        match value {
            EntryValue::Fields(f) => {
                assert_eq!(f.dest.as_deref(), Some(&[".agents/skills".to_owned()][..]));
            }
            other => panic!("fields carry: {other:?}"),
        }
        assert_eq!(carried, vec!["dest"]);
        assert!(dropped.is_empty());

        // A bare `"*"` set stays a `"*"` member.
        let star = PlanRow {
            section: crate::manifest::document::SectionKind::Skills,
            reference: "github.com/o/r".into(),
            shape: keys::classify_key("github.com/o/r").unwrap(),
            value: EntryValue::Star,
        };
        assert_eq!(split_carriage(&star, BundleKind::Skill).0, EntryValue::Star);
    }

    /// A MIXED channel splits PER KIND: the skill survivor's row takes the line's `dest` (skill
    /// folders), the mcp survivor's takes its `mcp_dest` — as that row's own `dest`, the only
    /// field a member row has — and neither ever receives the other's array.
    #[test]
    fn a_mixed_channel_split_carries_each_array_to_the_kind_that_reads_it() {
        use crate::manifest::document::EntryFields;
        let channel = PlanRow {
            section: crate::manifest::document::SectionKind::Channels,
            reference: "topos.sh/acme/channels/backend".into(),
            shape: keys::classify_key("topos.sh/acme/channels/backend").unwrap(),
            value: EntryValue::Fields(EntryFields {
                dest: Some(vec!["~/.claude/skills".into()]),
                mcp_dest: Some(vec!["~/.cursor/mcp.json".into()]),
                ..Default::default()
            }),
        };
        let dest_of = |kind| match split_carriage(&channel, kind) {
            (EntryValue::Fields(f), carried, dropped) => (f.dest, carried, dropped),
            other => panic!("fields carry: {other:?}"),
        };
        assert_eq!(
            dest_of(BundleKind::Skill),
            (
                Some(vec!["~/.claude/skills".to_owned()]),
                vec!["dest"],
                vec!["mcp_dest"]
            )
        );
        assert_eq!(
            dest_of(BundleKind::Mcp),
            (
                Some(vec!["~/.cursor/mcp.json".to_owned()]),
                vec!["mcp_dest"],
                vec!["dest"]
            )
        );
        // A channel with no `mcp_dest` leaves its mcp survivors unnarrowed rather than handing
        // them the skill folders.
        let skills_only = PlanRow {
            section: crate::manifest::document::SectionKind::Skills,
            value: EntryValue::Fields(EntryFields {
                dest: Some(vec!["~/.claude/skills".into()]),
                ..Default::default()
            }),
            ..channel.clone()
        };
        assert_eq!(
            split_carriage(&skills_only, BundleKind::Mcp).0,
            EntryValue::Star
        );
    }

    #[test]
    fn a_token_names_a_row_by_reference_or_by_the_name_it_delivers() {
        let row = PlanRow {
            section: crate::manifest::document::SectionKind::Skills,
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
