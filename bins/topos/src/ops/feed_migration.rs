//! The ONE-TIME feed-line upgrade migration — behavior-preserving, silent, run once per machine.
//!
//! Older builds treated an ABSENT global manifest as "one feed row per connected workspace"; now
//! the file is the machine's complete recipe and `topos login` writes a workspace's feed line on
//! the first connection only. A machine that upgraded mid-life is already connected, so with NO
//! file this migration writes the file those builds pretended existed — exactly one feed line per
//! live session — and behavior does not move. A file that EXISTS is never touched: its rows
//! already state this machine's truth, and adding feeds would change it. Pre-upgrade files never
//! carried feed lines this migration would fight, so it cannot clash with a deliberate removal.
//!
//! The done-marker is the `migrated` flag on the connected-workspaces witness
//! (`state/connected.json` — see [`crate::connected`]); the same write unions EVERY workspace the
//! sessions file names (any status) into the witness, so a later re-login never re-adds a line
//! this migration's file (or a person's edit) accounts for. Failures are best-effort-silent — no
//! stderr, no receipt — and leave the marker unset, so the next invocation retries.

use crate::connected;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::MANIFEST_FILE;
use crate::manifest::document::materialized_global;
use crate::sessions::{self, SESSION_ENDED};

/// Run the migration if its marker is not yet set. Called once per invocation, before command
/// dispatch; once done it costs one document read. Silent on every outcome.
pub(crate) fn ensure_feed_migration(ctx: &Ctx<'_>) {
    let _ = run(ctx);
}

fn run(ctx: &Ctx<'_>) -> Result<(), ClientError> {
    // The cheap standing read. An UNDECIPHERABLE witness (corrupt, or a newer build's) is never
    // written over — fail closed, exactly as the login's first-connection gate does.
    match connected::read(ctx.fs, &ctx.layout) {
        Ok(Some(doc)) if doc.migrated => return Ok(()),
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    // The live sessions — the exact set the retired implicit recipe fed.
    let mut live: Vec<(String, String)> = all
        .sessions
        .iter()
        .filter(|s| s.status != SESSION_ENDED)
        .map(|s| (s.host.clone(), s.workspace_name.clone()))
        .collect();
    live.sort();
    live.dedup();
    let path = ctx.layout.home().join(MANIFEST_FILE);
    // ONLY when no file exists (and there is something to preserve): write the file the old
    // builds pretended existed. An existing file is this machine's stated truth — untouched.
    if !live.is_empty() && !ctx.fs.exists(&path) {
        let _guard = super::manifest_edit::lock_manifest(ctx, &path)?;
        ctx.fs.create_dir_all(ctx.layout.home())?;
        // Exclusive create: a file an outside writer landed between the check and this write
        // stands — its truth wins, and the marker below still sets.
        let _ =
            crate::atomic::atomic_write_new(ctx.fs, &path, materialized_global(&live).as_bytes())?;
    }
    // The witness: every workspace the sessions file names (ANY status — an ended session still
    // proves this machine connected once), plus the done-marker. One write, under the doc's lock.
    connected::with_doc(ctx.fs, &ctx.layout, |doc| {
        for s in &all.sessions {
            doc.workspaces
                .insert(connected::key(&s.host, &s.workspace_name));
        }
        doc.migrated = true;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::Ctx;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::sessions::{SESSION_ACTIVE, Session};
    use crate::sidecar::Layout;
    use std::path::{Path, PathBuf};
    use topos_harness::ClaudeCode;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-feedmig-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_ctx<R>(home: &Path, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = crate::plane::InertPlane;
        let follow = crate::plane::InertFollow;
        let harness = ClaudeCode::new(scratch("adapter"));
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
            roots: None,
        };
        f(&ctx)
    }

    fn seed(ctx: &Ctx<'_>, host: &str, ws_id: &str, name: &str, status: &str) {
        crate::sessions::upsert_session(
            ctx.fs,
            &ctx.layout,
            Session {
                host: host.to_owned(),
                base_url: format!("https://{host}/api"),
                workspace_id: ws_id.to_owned(),
                workspace_name: name.to_owned(),
                display_name: name.to_owned(),
                session_id: format!("sn_{ws_id}"),
                credential: format!("cred-{ws_id}"),
                status: status.to_owned(),
                logged_in_at: 1,
            },
        )
        .unwrap();
    }

    #[test]
    fn sessions_and_no_file_births_exactly_those_feed_lines_once() {
        let home = scratch("mig-born");
        with_ctx(&home, |ctx| {
            // UNEQUAL id/name pairs: the file and witness must key on host + NAME.
            seed(ctx, "topos.sh", "w_1", "acme", SESSION_ACTIVE);
            seed(ctx, "topos.example.com", "w_2", "platform", SESSION_ACTIVE);
            // An ENDED session is not fed — but IS witnessed (it connected once).
            seed(
                ctx,
                "topos.sh",
                "w_3",
                "old",
                crate::sessions::SESSION_ENDED,
            );
            ensure_feed_migration(ctx);
            let path = ctx.layout.home().join(MANIFEST_FILE);
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("\"topos.sh/acme\" = \"*\""), "{text}");
            assert!(
                text.contains("\"topos.example.com/platform\" = \"*\""),
                "{text}"
            );
            assert!(
                !text.contains("old"),
                "an ended session feeds nothing: {text}"
            );
            let doc = crate::connected::read(ctx.fs, &ctx.layout)
                .unwrap()
                .unwrap();
            assert!(doc.migrated, "the marker is set");
            assert_eq!(
                doc.workspaces.iter().cloned().collect::<Vec<_>>(),
                vec![
                    "topos.example.com/platform".to_owned(),
                    "topos.sh/acme".to_owned(),
                    "topos.sh/old".to_owned(),
                ],
                "ANY status is witnessed"
            );
            // The second run is a no-op: delete a line and re-run — it stays deleted.
            std::fs::write(&path, "[bundles]\n\"topos.example.com/platform\" = \"*\"\n").unwrap();
            ensure_feed_migration(ctx);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "[bundles]\n\"topos.example.com/platform\" = \"*\"\n",
                "the marker makes the second run a no-op"
            );
        });
    }

    #[test]
    fn an_existing_file_is_untouched_and_the_marker_still_sets() {
        let home = scratch("mig-existing");
        with_ctx(&home, |ctx| {
            seed(ctx, "topos.sh", "w_1", "acme", SESSION_ACTIVE);
            let path = ctx.layout.home().join(MANIFEST_FILE);
            std::fs::create_dir_all(ctx.layout.home()).unwrap();
            let body = "[bundles]\n\"github.com/o/r\" = \"*\"\n";
            std::fs::write(&path, body).unwrap();
            ensure_feed_migration(ctx);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                body,
                "an existing file already states this machine's truth"
            );
            let doc = crate::connected::read(ctx.fs, &ctx.layout)
                .unwrap()
                .unwrap();
            assert!(doc.migrated);
            assert!(doc.workspaces.contains("topos.sh/acme"));
        });
    }

    #[test]
    fn no_sessions_creates_nothing_but_still_marks() {
        let home = scratch("mig-fresh");
        with_ctx(&home, |ctx| {
            ensure_feed_migration(ctx);
            assert!(
                !ctx.layout.home().join(MANIFEST_FILE).exists(),
                "nothing to preserve, nothing born"
            );
            let doc = crate::connected::read(ctx.fs, &ctx.layout)
                .unwrap()
                .unwrap();
            assert!(doc.migrated);
            assert!(doc.workspaces.is_empty());
        });
    }
}
