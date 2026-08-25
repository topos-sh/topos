//! `init [-g] [-a <agent>]... [--gitignore]` — first setup: write a `topos.toml` you can then
//! edit by hand, and pick the agents topos touches at that scope.
//!
//! Without `-g` it creates THIS folder's project manifest from the commented template (the folder
//! IS the scope — `init` never walks up). It is the ONE way a project manifest is born: `add`
//! records rows in a file that already exists and refuses when none covers the folder, so the
//! file always lands where a person put it. With `-g` it creates the machine-wide file with its
//! header alone — no feed rows (`topos login` writes a workspace's row on this machine's first
//! connection, and a row someone deleted must never come back through a birth). The file half is
//! idempotent — an existing file is never overwritten (`created: false`).
//!
//! The PICK half ([`crate::ops::agents`]): `-a <agent>` (repeatable) names the agents to use at
//! this scope and lands everything for them (skills, MCP entries, the built-in bundle, the
//! auto-update hooks) — on a fresh scope the named agents ARE the pick, on one with a file of its
//! own they join it, so `init -a` on a teammate's clone is pick + install, never an error. With
//! no `-a`: a pick already in force is used as it stands (an existing manifest then prints the
//! plain no-op receipt); with none, the ask decides (one agent installed → it, and the receipt
//! says so; several → [`crate::ops::agents_ask`]), and the chosen pick is recorded at this scope
//! BEFORE anything is placed, then read back. `--gitignore` appends the picked agents' folders
//! to the project's `.gitignore` (refused with `-g`).

use std::path::Path;

use topos_types::results::InitData;

use crate::agents_pick::{self, PickScope};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::MANIFEST_FILE;
use crate::manifest::document::{materialized_global, project_template};

use super::agents;
use super::agents_ask::AskInputs;
use super::manifest_edit as medit;
use super::reconcile::SessionConnect;

/// Whether any directory from `cwd` upward holds a `.git` — the one thing the travel note (and
/// the `.gitignore` hint) needs to know. A manifest inside a repo travels with it; one outside
/// stays local to the folder.
pub(super) fn inside_a_git_repo(fs: &dyn crate::fs_seam::FsOps, cwd: &Path) -> bool {
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if fs.exists(&d.join(".git")) {
            return true;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    false
}

/// What one `init` asks for.
pub(crate) struct InitRequest<'a> {
    /// `-g`: the machine-wide file and the machine pick.
    pub global: bool,
    /// The project file's one workspace line (`--workspace`).
    pub workspace: Option<&'a str>,
    /// The agents named with `-a` (empty = none named).
    pub agents: &'a [String],
    /// `--gitignore`.
    pub gitignore: bool,
    /// What the ask decides on when no pick stands and none was named.
    pub ask: AskInputs,
}

/// Create the folder's `topos.toml` (or, with `-g`, materialize `~/.topos/topos.toml`), then pick
/// and install per the request.
///
/// # Errors
/// [`ClientError::InvalidArgument`] when a project `init` has no working directory;
/// [`ClientError::GitignoreNeedsProject`]; the slug refusals; [`ClientError::PickRequired`] from
/// the ask; an io failure; everything [`agents::apply_pick`] can fail with.
pub(crate) fn init(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn crate::git_source::GitTarballSource>,
    req: &InitRequest<'_>,
) -> Result<InitData, ClientError> {
    if req.global && req.gitignore {
        return Err(ClientError::GitignoreNeedsProject);
    }
    agents_pick::validate(req.agents)?;
    let (scope, path) = if req.global {
        (PickScope::Machine, ctx.layout.home().join(MANIFEST_FILE))
    } else {
        let cwd = project_cwd(ctx)?;
        (PickScope::Project(cwd.clone()), cwd.join(MANIFEST_FILE))
    };
    let existing = ctx.fs.exists(&path);
    let project_dir = match &scope {
        PickScope::Machine => None,
        PickScope::Project(dir) => Some(dir.as_path()),
    };
    // WHICH pick this run applies, decided (and recorded) before the file is born: nothing is
    // placed on a run the ask refuses, and the reconcile below reads the file, never a memory.
    let apply = if !req.agents.is_empty() {
        agents::set_for_init(ctx, &scope, req.agents)?;
        true
    } else if agents_pick::effective(ctx.fs, &ctx.layout, project_dir)?.is_some() {
        // A standing pick over an existing file: the plain no-op receipt, as ever.
        !existing
    } else {
        agents::derive_pick_if_missing(ctx, &scope, &req.ask, false)?;
        true
    };
    let mut data = create_manifest(ctx, req.global, req.workspace)?;
    if apply {
        data.pick = Some(agents::apply_pick(
            ctx,
            connect,
            git,
            &scope,
            req.gitignore,
            "topos init --gitignore",
        )?);
    }
    Ok(data)
}

/// The FILE half alone — create the manifest at `global`'s scope and nothing else. The rigs that
/// need a project manifest to record into call this, exactly as the verb's own first step does.
pub(crate) fn create_manifest(
    ctx: &Ctx<'_>,
    global: bool,
    workspace: Option<&str>,
) -> Result<InitData, ClientError> {
    if global {
        init_global(ctx)
    } else {
        init_project(ctx, workspace)
    }
}

/// The folder a project `init` governs: the working directory, never a walk up.
fn project_cwd(ctx: &Ctx<'_>) -> Result<std::path::PathBuf, ClientError> {
    ctx.roots
        .as_ref()
        .and_then(|r| r.cwd.clone())
        .ok_or_else(|| {
            ClientError::InvalidArgument(
                "cannot resolve the current directory — run `topos init` inside the folder the \
                 manifest should govern (or `topos init -g` for your machine-wide file)"
                    .into(),
            )
        })
}

/// Create the folder's `topos.toml`: the project arm's file half.
fn init_project(ctx: &Ctx<'_>, workspace: Option<&str>) -> Result<InitData, ClientError> {
    // The project file's one workspace: the flag when given (validated), else the machine
    // default (`topos workspace use` moves it), else the template's commented line teaches.
    let (ws_line, ws_scheme): (Option<(String, String)>, Option<&'static str>) = match workspace {
        Some(addr) => (
            Some(
                crate::manifest::document::parse_workspace_line(addr)
                    .map_err(|e| ClientError::InvalidArgument(e.message))?,
            ),
            // A spelled scheme (self-hosted `http://`) survives into the written line — the
            // machine-token dial reads it back from there.
            crate::manifest::document::workspace_line_scheme(addr),
        ),
        None => (
            crate::sessions::read_sessions(ctx.fs, &ctx.layout)
                .ok()
                .and_then(|all| {
                    all.default_session()
                        .map(|s| (s.host.clone(), s.workspace_name.clone()))
                }),
            None,
        ),
    };
    let cwd = project_cwd(ctx)?;
    let path = cwd.join(MANIFEST_FILE);
    let travel = (!inside_a_git_repo(ctx.fs, &cwd)).then(|| {
        "outside a git repository — this manifest stays local to this folder (it won't travel \
         with a repo)"
            .to_owned()
    });
    // The same writer lock every manifest mutation takes — `init` is check-then-write, so a
    // concurrent `add`'s row must not be overwritten by the template.
    let _guard = medit::lock_manifest(ctx, &path)?;
    if ctx.fs.exists(&path) {
        return Ok(InitData {
            manifest: path.display().to_string(),
            created: false,
            note: travel,
            pick: None,
        });
    }
    // The lock fences topos's own writers; an OUTSIDE editor can still land the file between the
    // check above and the write — the exclusive create meets it as an existing file (the same
    // clean no-op receipt, the outside bytes standing), never an overwrite.
    let created = match crate::atomic::atomic_write_new(
        ctx.fs,
        &path,
        project_template(
            ws_line.as_ref().map(|(h, w)| (h.as_str(), w.as_str())),
            ws_scheme,
        )
        .as_bytes(),
    )? {
        crate::atomic::NewOutcome::Written => true,
        crate::atomic::NewOutcome::Exists => false,
    };
    // A FRESH file says what having one MEANS, which the lead's "record skills with `topos add`"
    // does not: this is now the file every project add here writes to, and the only one — an
    // `add` records rows, it never creates a manifest of its own.
    let created_note = "this is the file every `topos add` in this folder records into — `add` \
                        writes rows, never a new manifest";
    let note = match (created, travel) {
        (true, Some(t)) => Some(format!("{created_note} · {t}")),
        (true, None) => Some(created_note.to_owned()),
        (false, t) => t,
    };
    Ok(InitData {
        manifest: path.display().to_string(),
        created,
        note,
        pick: None,
    })
}

/// The `-g` arm: create this machine's own recipe, header only (login writes the feed rows).
fn init_global(ctx: &Ctx<'_>) -> Result<InitData, ClientError> {
    let path = ctx.layout.home().join(MANIFEST_FILE);
    ctx.fs.create_dir_all(ctx.layout.home())?;
    let _guard = medit::lock_manifest(ctx, &path)?;
    if ctx.fs.exists(&path) {
        return Ok(InitData {
            manifest: path.display().to_string(),
            created: false,
            note: Some(
                "it already exists and was left untouched — `topos fmt -g` tidies it, and \
                 `topos list -g` shows what it currently delivers"
                    .to_owned(),
            ),
            pick: None,
        });
    }
    let note = Some(
        "the file starts with just its header — `topos login` adds a workspace's feed row on \
         its first connection, and `topos add -g` records lines by hand"
            .to_owned(),
    );
    ctx.fs.create_dir_all(ctx.layout.home())?;
    // Same discipline as the project arm: the exclusive create meets a file an outside writer
    // landed after the check as an EXISTING file — the no-op receipt, never an overwrite.
    match crate::atomic::atomic_write_new(ctx.fs, &path, materialized_global(&[]).as_bytes())? {
        crate::atomic::NewOutcome::Written => Ok(InitData {
            manifest: path.display().to_string(),
            created: true,
            note,
            pick: None,
        }),
        crate::atomic::NewOutcome::Exists => Ok(InitData {
            manifest: path.display().to_string(),
            created: false,
            note: Some(
                "it already exists and was left untouched — `topos fmt -g` tidies it, and \
                 `topos list -g` shows what it currently delivers"
                    .to_owned(),
            ),
            pick: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::AgentRoots;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;
    use std::path::{Path, PathBuf};
    use topos_harness::ClaudeCode;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-init-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `topos init` as typed: bare, in the folder (or `-g`), no agent named.
    fn bare_init(ctx: &Ctx<'_>, global: bool) -> Result<InitData, ClientError> {
        let connect = |_: &crate::sessions::Session| -> super::super::SessionTransports {
            panic!("no session is logged in")
        };
        init(
            ctx,
            &connect,
            None,
            &InitRequest {
                global,
                workspace: None,
                agents: &[],
                gitignore: false,
                ask: AskInputs {
                    stdin_tty: false,
                    in_claude_code: false,
                    json: false,
                    global,
                },
            },
        )
    }

    fn with_ctx<R>(home: &Path, cwd: Option<&Path>, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("adapter"));
        // The machine pick stands (every installed agent — none, in a scratch home), so a bare
        // `init` exercises the file half without the ask.
        crate::agents_pick::write_pick(
            &crate::agents_pick::machine_path(&Layout::new(&home.join(".topos"))),
            &["*"],
        );
        let ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&home.join(".topos")),
            harness: &harness,
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane: &plane,
            follow: &follow,
            roots: cwd.map(|c| AgentRoots {
                home: home.to_path_buf(),
                cwd: Some(c.to_path_buf()),
            }),
        };
        f(&ctx)
    }

    #[test]
    fn init_creates_once_and_reports_the_no_op() {
        let home = scratch("create");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        with_ctx(&home, Some(&repo), |ctx| {
            let first = bare_init(ctx, false).unwrap();
            assert!(first.created);
            // A fresh file says what it is for — and, inside a repo, nothing about travel.
            let note = first
                .note
                .clone()
                .expect("a created file says what it is for");
            assert!(note.contains("every `topos add`"), "{note}");
            assert!(
                !note.contains("git"),
                "inside a repo — no travel note: {note}"
            );
            assert!(std::path::Path::new(&first.manifest).exists());
            // The created file parses as an EMPTY project manifest (the template is comments only).
            let text = std::fs::read_to_string(&first.manifest).unwrap();
            let doc = crate::manifest::document::parse_manifest(
                &text,
                crate::manifest::document::ManifestScope::Project,
            )
            .unwrap();
            assert!(doc.rows.is_empty());
            // Idempotent — the second run is a no-op receipt, never an overwrite.
            std::fs::write(
                &first.manifest,
                "# hand-edited\n[bundles]\n\"./a\" = \"*\"\n",
            )
            .unwrap();
            let second = bare_init(ctx, false).unwrap();
            assert!(!second.created);
            let text = std::fs::read_to_string(&first.manifest).unwrap();
            assert!(text.contains("hand-edited"), "{text}");
        });
    }

    #[test]
    fn init_outside_a_repo_notes_the_file_wont_travel() {
        let home = scratch("norepo");
        let stray = home.join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        with_ctx(&home, Some(&stray), |ctx| {
            let out = bare_init(ctx, false).unwrap();
            assert!(out.created);
            // Both disclosures ride one note: what the file is for, and that it won't travel.
            let note = out.note.clone().expect("a created file carries its note");
            assert!(note.contains("every `topos add`"), "{note}");
            assert!(note.contains("git"), "{note}");
            // The idempotent no-op keeps ONLY the travel note (nothing was created to explain).
            let again = bare_init(ctx, false).unwrap();
            assert!(!again.created);
            let again_note = again.note.clone().expect("the travel note stands");
            assert!(again_note.contains("git"), "{again_note}");
            assert!(!again_note.contains("every `topos add`"), "{again_note}");
        });
    }

    #[test]
    fn init_without_a_working_directory_refuses_typed() {
        let home = scratch("nocwd");
        with_ctx(&home, None, |ctx| {
            let err = bare_init(ctx, false).unwrap_err();
            assert_eq!(err.code(), "INVALID_ARGUMENT");
        });
    }

    #[test]
    fn a_file_appearing_between_the_check_and_the_land_is_never_overwritten() {
        // The racer is an OUTSIDE editor (topos's own writers serialize on the manifest lock):
        // it lands the file at the last catchable instant — after the absence check, with the
        // template already staged, immediately before the no-replace rename. The exclusive
        // create answers it as an existing file: the no-op receipt, the outside bytes standing.
        let home = scratch("race");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let path = repo.join(MANIFEST_FILE);
        let tmp = crate::atomic::temp_path(&path);
        let racing = path.clone();
        let hook_fs = crate::fs_seam::HookFs::before_first_move_of(&tmp, move || {
            std::fs::write(&racing, b"# an outside editor's file\n").unwrap();
        });
        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("race-adapter"));
        crate::agents_pick::write_pick(
            &crate::agents_pick::machine_path(&Layout::new(&home.join(".topos"))),
            &["*"],
        );
        let ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &hook_fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&home.join(".topos")),
            harness: &harness,
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane: &plane,
            follow: &follow,
            roots: Some(AgentRoots {
                home: home.clone(),
                cwd: Some(repo.clone()),
            }),
        };
        let out = bare_init(&ctx, false).unwrap();
        assert!(
            !out.created,
            "the racer's file is an existing file — the receipt is the no-op, never an overwrite"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"# an outside editor's file\n",
            "the outside bytes stand byte-for-byte"
        );
        assert!(!tmp.exists(), "the staged template was discarded");
    }

    #[test]
    fn init_g_materializes_the_machine_file_and_is_idempotent() {
        let home = scratch("global");
        with_ctx(&home, None, |ctx| {
            // With no session, the file is born with just its header — and says so.
            let first = bare_init(ctx, true).unwrap();
            assert!(first.created);
            assert!(
                first.note.as_deref().is_some_and(|n| n.contains("header")),
                "{first:?}"
            );
            let text = std::fs::read_to_string(&first.manifest).unwrap();
            assert!(text.contains("# topos.toml"), "{text}");
            // Idempotent: the second run leaves the bytes alone and says so.
            let second = bare_init(ctx, true).unwrap();
            assert!(!second.created);
            assert!(
                second
                    .note
                    .as_deref()
                    .is_some_and(|n| n.contains("left untouched")),
                "{second:?}"
            );
            assert_eq!(std::fs::read_to_string(&second.manifest).unwrap(), text);
        });
    }
}
