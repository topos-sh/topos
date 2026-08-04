//! `status` — the offline HEALTH panel: the binary version, the server + signed-in state, the
//! sessions, the auto-update trigger states, and per shown SCOPE what governs it, the regimes,
//! the disclosure notes, and the ATTENTION COUNTS — each count one line ending in the exact
//! command that resolves it. NO skill inventory: that is `list`'s. Computed ENTIRELY from local
//! state through the shared resolution ([`super::inventory`]). No network, no writes.
//!
//! The scope rule every verb follows: the body is the HERE-scope (the nearest `topos.toml`
//! covering the cwd, else the machine); `-g` is the machine scope alone; `--all` is both. What
//! the body does not show is never invisible — inside a project the machine scope rides ONE
//! summary line with ITS pending counts, ending in `topos status -g`.
//!
//! The bare `topos` invocation renders this same snapshot on a TTY, so a human's first keystroke
//! answers "what is this, and where am I" without dialing anything.

use topos_types::results::{
    AttentionCount, StatusData, StatusScope, StatusScopeSummary, StatusSession,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::sessions;

use super::inventory::{self, ScopeResolution, ScopeView};

/// Assemble the offline status snapshot for `view`. `triggers` stays empty here — the composition
/// root fills it from the read-only probe (`ops::probe_detected`), the same layering the arming
/// receipts use.
///
/// # Errors
/// A read failure or a manifest the grammar refuses (typed, naming the fix).
pub(crate) fn status_snapshot(ctx: &Ctx<'_>, view: ScopeView) -> Result<StatusData, ClientError> {
    let (all, cache) = inventory::read_sources(ctx)?;
    let signed_in = all.live().count() > 0;

    // This installation's SESSIONS (one per logged-into workspace).
    let session_rows: Vec<StatusSession> = all
        .sessions
        .iter()
        .map(|s| StatusSession {
            workspace_id: s.workspace_id.clone(),
            name: s.workspace_name.clone(),
            display_name: s.display_name.clone(),
            host: s.host.clone(),
            session_status: (s.status != sessions::SESSION_ACTIVE).then(|| s.status.clone()),
        })
        .collect();

    let resolved = inventory::resolve(ctx, &all, &cache)?;
    // Counted against the SAME person plan the machine rows resolved from — the count's command
    // (`topos update [-g]`) has to be true of the recipe that is actually governing.
    let awaiting = inventory::awaiting_first_sync(ctx, &all, &cache, &resolved.person_plan);
    let in_project = resolved.project().is_some();

    // Which bodies show in full: the here-scope by default; the machine alone under `-g`; both
    // under `--all`. Outside a project the machine IS the here-scope.
    let show_project = in_project && matches!(view, ScopeView::Here | ScopeView::All);
    let show_machine = !in_project || matches!(view, ScopeView::Machine | ScopeView::All);

    let mut scopes = Vec::new();
    // The sections this invocation SHOWS — what the forge health below is answered over, so `-g`
    // never surfaces a failure belonging to a project the invocation did not ask about.
    let mut shown: Vec<&inventory::ScopeResolution> = Vec::new();
    if show_project {
        let sr = resolved
            .project()
            .expect("in_project implies a project scope");
        scopes.push(scope_body(sr, &resolved, None, in_project));
        shown.push(sr);
    }
    if show_machine {
        scopes.push(scope_body(
            resolved.machine(),
            &resolved,
            awaiting,
            in_project,
        ));
        shown.push(resolved.machine());
    }
    // The machine scope not shown in full rides ONE summary line with ITS pending counts.
    let machine_summary = (!show_machine).then(|| StatusScopeSummary {
        attention: attention(resolved.machine(), awaiting, true, in_project),
        command: "topos status -g".to_owned(),
    });

    Ok(StatusData {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        server: all.sessions.first().map(|s| s.base_url.clone()),
        signed_in,
        sessions: session_rows,
        triggers: Vec::new(),
        forge: inventory::forge_sources(ctx, &shown),
        scopes,
        machine_summary,
    })
}

/// One scope's shown body: the governing file, the regimes (a machine-scope fact), the notes,
/// and the attention counts.
fn scope_body(
    sr: &ScopeResolution,
    resolved: &inventory::Resolved,
    awaiting: Option<u64>,
    in_project: bool,
) -> StatusScope {
    let machine = sr.scope == "machine";
    StatusScope {
        scope: sr.scope.to_owned(),
        manifest: sr.manifest.clone(),
        regimes: if machine {
            resolved.regimes.clone()
        } else {
            Vec::new()
        },
        notes: sr.notes.clone(),
        attention: attention(sr, awaiting, machine, in_project),
    }
}

/// The attention counts for one scope — only non-zero counts appear, each ending in the exact
/// command that resolves it. The command is SCOPE-EXACT: `-g` spells the machine scope only when
/// a project covers the cwd (when the machine IS the here-scope, the plain spelling already acts
/// on it).
fn attention(
    sr: &ScopeResolution,
    awaiting: Option<u64>,
    machine: bool,
    in_project: bool,
) -> Vec<AttentionCount> {
    let flag = if machine && in_project { " -g" } else { "" };
    let mut out = Vec::new();
    let updates = sr.updates_pending();
    if updates > 0 {
        out.push(AttentionCount {
            kind: "updates-pending".to_owned(),
            count: updates,
            command: format!("topos update{flag}"),
        });
    }
    // Assignments are a MACHINE-scope fact (the workspaces assign the person, and feeds deliver
    // into the machine scope); the count is honestly absent when any sync doc was unreadable.
    if machine && let Some(n) = awaiting.filter(|n| *n > 0) {
        out.push(AttentionCount {
            kind: "assignments-not-applied".to_owned(),
            count: n,
            command: format!("topos update{flag}"),
        });
    }
    let drafts = sr.drafts_ahead();
    if drafts > 0 {
        out.push(AttentionCount {
            kind: "drafts-ahead".to_owned(),
            count: drafts,
            command: format!("topos list{flag}"),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::enroll;
    use crate::fs_seam::RealFs;
    use crate::ops::inventory::testkit::{TempHome, assigned, with_ctx};
    use crate::sidecar::Layout;
    use topos_types::PERSISTED_SCHEMA_VERSION;

    fn snapshot(home: &TempHome, cwd: &std::path::Path, view: ScopeView) -> StatusData {
        with_ctx(home, Some(cwd), |ctx| {
            status_snapshot(ctx, view).expect("status snapshot")
        })
    }

    /// The body is the HERE-scope; the machine scope rides ONE summary with ITS counts, ending
    /// in the exact expanding command. The assignments count comes from the cache (delivered,
    /// never applied here).
    #[test]
    fn inside_a_project_the_body_is_the_project_and_the_machine_rides_a_summary() {
        let home = TempHome::new();
        let repo = home.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(crate::manifest::MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/api-only\" = \"*\"\n",
        )
        .unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", Some("Dana"))],
            Vec::new(),
        );

        let d = snapshot(&home, &repo, ScopeView::Here);
        assert_eq!(d.scopes.len(), 1, "{:?}", d.scopes);
        assert_eq!(d.scopes[0].scope, "project");
        assert!(
            d.scopes[0]
                .manifest
                .as_deref()
                .is_some_and(|m| m.ends_with("topos.toml")),
            "{:?}",
            d.scopes[0].manifest
        );
        // The project scope carries no regimes (a machine-scope fact) and nothing pending.
        assert!(d.scopes[0].regimes.is_empty());
        assert!(
            d.scopes[0].attention.is_empty(),
            "{:?}",
            d.scopes[0].attention
        );
        // The machine summary carries ITS pending count with the scope-exact spelling.
        let summary = d
            .machine_summary
            .expect("a machine summary inside a project");
        assert_eq!(summary.command, "topos status -g");
        assert_eq!(summary.attention.len(), 1, "{:?}", summary.attention);
        assert_eq!(summary.attention[0].kind, "assignments-not-applied");
        assert_eq!(summary.attention[0].count, 1);
        assert_eq!(summary.attention[0].command, "topos update -g");
    }

    /// `-g` inside a project: the machine body alone, no project body, no summary — and its
    /// commands keep the `-g` spelling (the plain one would act on the project).
    #[test]
    fn dash_g_shows_the_machine_body_alone_with_scope_exact_commands() {
        let home = TempHome::new();
        let repo = home.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(crate::manifest::MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/api-only\" = \"*\"\n",
        )
        .unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None)],
            Vec::new(),
        );

        let d = snapshot(&home, &repo, ScopeView::Machine);
        assert_eq!(d.scopes.len(), 1);
        assert_eq!(d.scopes[0].scope, "machine");
        assert!(d.machine_summary.is_none());
        let a = &d.scopes[0].attention;
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].kind, "assignments-not-applied");
        assert_eq!(a[0].command, "topos update -g");
        // The regimes ride the machine body.
        assert_eq!(d.scopes[0].regimes.len(), 1);
        assert_eq!(d.scopes[0].regimes[0].regime, "adopting all assigned");
    }

    /// `--all`: both bodies in full, no summary.
    #[test]
    fn all_shows_both_bodies_and_no_summary() {
        let home = TempHome::new();
        let repo = home.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(crate::manifest::MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/api-only\" = \"*\"\n",
        )
        .unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );

        let d = snapshot(&home, &repo, ScopeView::All);
        let names: Vec<&str> = d.scopes.iter().map(|s| s.scope.as_str()).collect();
        assert_eq!(names, vec!["project", "machine"]);
        assert!(d.machine_summary.is_none());
    }

    /// Outside a project the machine IS the here-scope: its body shows in full with the PLAIN
    /// command spellings, and there is no summary.
    #[test]
    fn outside_a_project_the_machine_body_is_the_here_scope() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None)],
            Vec::new(),
        );

        let d = snapshot(&home, &cwd, ScopeView::Here);
        assert_eq!(d.scopes.len(), 1);
        assert_eq!(d.scopes[0].scope, "machine");
        assert!(d.machine_summary.is_none());
        let a = &d.scopes[0].attention;
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].command, "topos update", "machine IS the here-scope");
    }

    /// The updates-pending count reads a BEHIND row (applied version behind the last-known
    /// served target); drafts-ahead reads a MODIFIED placement. Both offline, both ending in the
    /// command that resolves them.
    #[test]
    fn behind_and_draft_rows_land_as_attention_counts() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None), assigned("notes", None)],
            Vec::new(),
        );
        // deploy: applied at an OLDER version than served — behind.
        home.store_applied(
            "topos_dddddddddddddddddddddddddddddddd",
            "deploy",
            &"c".repeat(64),
            &[],
        );
        // notes: applied at the served version, with a placement whose bytes have drifted — draft.
        let placed = home.0.join("placed-notes");
        std::fs::create_dir_all(&placed).unwrap();
        std::fs::write(placed.join("SKILL.md"), b"# edited\n").unwrap();
        home.store_applied(
            "topos_nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
            "notes",
            &"d".repeat(64),
            &[placed.to_string_lossy().as_ref()],
        );

        let d = snapshot(&home, &cwd, ScopeView::Here);
        let a = &d.scopes[0].attention;
        let of = |kind: &str| a.iter().find(|c| c.kind == kind);
        let updates = of("updates-pending").expect("a behind row counts");
        assert_eq!(updates.count, 1);
        assert_eq!(updates.command, "topos update");
        let drafts = of("drafts-ahead").expect("a modified placement counts");
        assert_eq!(drafts.count, 1);
        assert_eq!(drafts.command, "topos list");
        assert!(of("assignments-not-applied").is_none(), "{a:?}");
    }

    /// Every file under `dir`, as `relative path → bytes` — the byte-identity oracle.
    fn tree_bytes(dir: &PathBuf) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        fn walk(
            root: &PathBuf,
            dir: &PathBuf,
            out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
        ) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, std::fs::read(&path).unwrap_or_default());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    /// The read-only promise, proven byte-for-byte: a status run over a sidecar holding a
    /// PENDING-RECOVERY fixture (an expired enrollment WAL the ordinary start-of-command sweep
    /// would reap) leaves every byte in place — the snapshot AND the trigger probe (the exact
    /// pre-recovery pair the composition root's fast path runs) write nothing, and the same
    /// fixture is then shown POTENT: the recovery sweep the fast path skips does mutate it.
    #[test]
    fn status_leaves_a_pending_recovery_sidecar_byte_identical() {
        let home = TempHome::new();
        let fs = RealFs;
        let layout = Layout::new(&home.0);
        enroll::write_wal(
            &fs,
            &layout,
            &enroll::PendingEnrollment {
                schema_version: PERSISTED_SCHEMA_VERSION,
                host: String::new(),
                base_url: "https://topos.sh/api".to_owned(),
                preselect: "acme".to_owned(),
                intent: enroll::EnrollIntentDoc::Session,
                device_code: "dc_expired".to_owned(),
                user_code: "XXXX-YYYY".to_owned(),
                verification_uri: "https://topos.sh/verify".to_owned(),
                interval_secs: 5,
                // Long expired — recovery would reap this WAL on any ordinary command.
                loopback: false,
                expires_at_millis: 1_000,
            },
        )
        .unwrap();

        let before = tree_bytes(&home.0);
        assert!(!before.is_empty(), "the fixture is on disk");

        // The exact pair the composition root's pre-recovery fast path runs. `with_bare_ctx`
        // wires the layout at the home root and NO roots — the fixture's original shape.
        let harness = topos_harness::ClaudeCode::new(home.0.join(".claude"), &fs);
        let ctx = crate::ctx::Ctx {
            progress: crate::progress::silent(),
            fs: &fs,
            ids: &crate::ids::RealIds,
            clock: &crate::ids::RealClock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: &harness,
            plane: &crate::plane::InertPlane,
            follow: &crate::plane::InertFollow,
            roots: None,
        };
        let data = status_snapshot(&ctx, ScopeView::Here).expect("status snapshot");
        assert!(data.sessions.is_empty());
        let _ = crate::ops::probe_detected(&home.0, None, &harness, &fs);
        assert_eq!(
            before,
            tree_bytes(&home.0),
            "a status run must leave the sidecar byte-identical"
        );

        // The fixture is potent: the sweep the fast path skips DOES mutate it (the WAL is reaped).
        crate::sidecar::recover(&fs, &layout, i64::MAX, &mut Vec::new()).unwrap();
        assert_ne!(
            before,
            tree_bytes(&home.0),
            "the recovery sweep reaps the expired WAL — proving status really skipped it"
        );
    }

    #[test]
    fn a_fresh_install_reads_not_connected_with_nothing_pending() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let data = snapshot(&home, &cwd, ScopeView::Here);
        assert!(data.sessions.is_empty() && !data.signed_in);
        assert_eq!(data.server, None);
        assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
        assert!(data.triggers.is_empty());
        assert_eq!(data.scopes.len(), 1);
        assert_eq!(data.scopes[0].scope, "machine");
        assert!(data.scopes[0].attention.is_empty());
        assert!(data.scopes[0].notes.is_empty());
        assert!(data.machine_summary.is_none());
    }
}
