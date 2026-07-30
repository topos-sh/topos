//! The `~/.topos/` layout, the footprint walk, the per-skill writer lock, and the idempotent crash
//! recovery sweep. The client owns this policy; the gitstore knows none of it.

use std::path::{Path, PathBuf};

use crate::atomic::TMP_SUFFIX;
use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, LockGuard};
use crate::id::SkillId;

/// The prefix marking a transient staging directory (`skills/.staging-<id>/`) being assembled by `add`.
const STAGING_PREFIX: &str = ".staging-";

/// The directory a PROJECT's own engine store lives under: `<project>/.topos/`. Self-ignoring (its
/// own `.gitignore` holds `*`, the venv model), so nothing under it ever enters the checkout's
/// commits.
pub(crate) const PROJECT_STORE_DIR: &str = ".topos";
/// The project store's own ignore file bytes — everything under `.topos/` is machine-local state.
const PROJECT_STORE_IGNORE: &[u8] = b"*\n";

/// Resolves every store path from a base directory (injected, so tests get an isolated home). ONE
/// shape serves BOTH scopes: the person scope's `~/.topos/`, and a project's own store at
/// `<project>/.topos/state/<user>/` — so the whole engine (locks, WAL recovery, doc IO, the sync
/// machine) runs unchanged against either.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    home: PathBuf,
    /// The project checkout this layout is the store OF, when it is a project store (`None` = the
    /// person scope). Scope-dependent behavior (the placement self-ignore) keys off this — the
    /// layout IS the scope.
    project_root: Option<PathBuf>,
}

/// The per-skill paths under a base directory (a published `skills/<id>/` or a staging dir).
#[derive(Debug, Clone)]
pub(crate) struct SkillPaths {
    pub store: PathBuf,
    pub lock: PathBuf,
    pub map: PathBuf,
    pub sync: PathBuf,
    /// The durable unresolved-merge-conflict record — present only while a conflict is unresolved (the
    /// publish guard's source of truth + the pre-swap recovery journal). Absent in the common case.
    pub conflict: PathBuf,
    /// The remote-import provenance record — present only for a skill `add` fetched from a source (a GitHub
    /// repo): the origin repo, resolved commit, subdir, and license. A best-effort adjunct written after
    /// adoption (never part of the atomic core), so its absence just means "no recorded upstream."
    pub origin: PathBuf,
}

impl SkillPaths {
    fn under(base: &Path) -> Self {
        Self {
            store: base.join("store"),
            lock: base.join("lock.json"),
            map: base.join("map.json"),
            sync: base.join("sync.json"),
            conflict: base.join("conflict.json"),
            origin: base.join("origin.json"),
        }
    }
}

impl Layout {
    pub(crate) fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            project_root: None,
        }
    }

    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    /// Whether this layout is a PROJECT's store (its placements land inside a checkout and
    /// self-ignore) rather than the person scope's home store.
    pub(crate) fn is_project_scope(&self) -> bool {
        self.project_root.is_some()
    }

    /// The checkout this layout is the store OF — `None` for the person scope's home store. What a
    /// cross-store disclosure names the holder by.
    pub(crate) fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub(crate) fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// `skills/<id>/` — a path join, so the id is the VALIDATED newtype (parse-don't-validate: a raw
    /// plane/document string can never reach this join). Same for every id-keyed builder below.
    pub(crate) fn skill_dir(&self, id: &SkillId) -> PathBuf {
        self.skills_dir().join(id.as_str())
    }

    /// The paths of a published skill (`skills/<id>/…`).
    pub(crate) fn published(&self, id: &SkillId) -> SkillPaths {
        SkillPaths::under(&self.skill_dir(id))
    }

    /// The paths of a skill being staged (`skills/.staging-<id>/…`), published with one directory rename.
    pub(crate) fn staging(&self, id: &SkillId) -> (PathBuf, SkillPaths) {
        let base = self.skills_dir().join(format!("{STAGING_PREFIX}{id}"));
        let paths = SkillPaths::under(&base);
        (base, paths)
    }

    pub(crate) fn locks_dir(&self) -> PathBuf {
        self.home.join("locks")
    }

    pub(crate) fn lock_file(&self, id: &SkillId) -> PathBuf {
        self.locks_dir().join(format!("{id}.lock"))
    }

    /// `locks/identity.lock` — the identity writer lock (a fixed name, not an id join; the host-id
    /// mint, the login WAL, and every `sessions.json` read-merge-write serialize on it).
    pub(crate) fn identity_lock_file(&self) -> PathBuf {
        self.locks_dir().join("identity.lock")
    }

    /// `locks/currency.lock` — the bare-sweep single-flight lock (a fixed name): the quiet hook
    /// TRY-locks it (a held lock means another sweep is in flight → silent no-op), an explicit bare
    /// `update` takes it blocking. Per-skill writer locks still guard every actual placement write —
    /// this lock only stops two whole sweeps from duplicating work.
    pub(crate) fn currency_lock_file(&self) -> PathBuf {
        self.locks_dir().join("currency.lock")
    }

    pub(crate) fn log_path(&self) -> PathBuf {
        self.home.join("log.jsonl")
    }

    pub(crate) fn identity_dir(&self) -> PathBuf {
        self.home.join("identity")
    }

    pub(crate) fn host_path(&self) -> PathBuf {
        self.identity_dir().join("host.json")
    }

    /// `instance.json` — RETIRED (the pre-session pinned-plane doc). Named only so recovery can
    /// delete a leftover on sight.
    pub(crate) fn instance_path(&self) -> PathBuf {
        self.home.join("instance.json")
    }

    /// `follows.json` — RETIRED (the pre-manifest subscription doc). Named only so recovery can
    /// delete a leftover on sight (an old file may still hold a legacy `read_token`).
    pub(crate) fn follows_path(&self) -> PathBuf {
        self.home.join("follows.json")
    }

    /// `identity/credentials.json` — RETIRED (the pre-session machine-wide credential doc, a
    /// secret). Named only so recovery can delete a leftover on sight — a leftover credential is a
    /// secret with no reader.
    pub(crate) fn credentials_path(&self) -> PathBuf {
        self.identity_dir().join("credentials.json")
    }

    /// `identity/sessions.json` — this installation's SESSIONS (one per workspace, each with its
    /// own workspace-scoped bearer credential). A `0600` secret, written whole under the identity
    /// lock.
    pub(crate) fn sessions_path(&self) -> PathBuf {
        self.identity_dir().join("sessions.json")
    }

    /// `identity/user.json` — RETIRED (the pre-session workspace-metadata doc). Named only so
    /// recovery can delete a leftover on sight.
    pub(crate) fn user_path(&self) -> PathBuf {
        self.identity_dir().join("user.json")
    }

    /// `identity/enrollment.json` — the in-flight LOGIN WAL (a `0600` secret: it holds the flow
    /// code and, once redeemed, the workspace credential). Present only between
    /// `login <workspace-address>` and a completed re-invoked `login`; swept by recovery once
    /// expired-and-unredeemed, deleted on promotion.
    pub(crate) fn enrollment_path(&self) -> PathBuf {
        self.identity_dir().join("enrollment.json")
    }

    /// `state/` — plain (non-secret) operational state documents.
    pub(crate) fn state_dir(&self) -> PathBuf {
        self.home.join("state")
    }

    /// `state/sync_status.json` — the per-workspace delivery/report freshness the delivery-driven
    /// reconcile writes and the hook's staleness warning + `auth status` read. A plain doc — it
    /// carries timestamps and the staleness window, never a secret.
    pub(crate) fn sync_status_path(&self) -> PathBuf {
        self.state_dir().join("sync_status.json")
    }

    /// `state/builtin.json` — the built-in `topos` skill's machine-local state: the durable
    /// `remove topos` opt-out + its placement scope. Never a manifest or profile row (the built-in
    /// is not a subscription; the plane never hears of it).
    pub(crate) fn builtin_state_path(&self) -> PathBuf {
        self.state_dir().join("builtin.json")
    }

    /// `state/forge_trust.json` — the MACHINE's forge first-trust registry (see
    /// `crate::forge_trust`): the origins `add`'s consent moment granted. Home store only — a
    /// project store's contents travel with the checkout and must never carry consent.
    pub(crate) fn forge_trust_path(&self) -> PathBuf {
        self.state_dir().join("forge_trust.json")
    }

    /// `state/visited_stores.json` — the machine-local index of project stores the reconcile has
    /// visited (see `crate::visited_stores`): what makes every applied report COMPLETE across
    /// checkouts, whichever one the update runs from. A plain doc — paths, never a secret.
    pub(crate) fn visited_stores_path(&self) -> PathBuf {
        self.state_dir().join("visited_stores.json")
    }

    /// `state/quiet_sweep.json` — when the last bare update sweep completed (epoch millis). The
    /// quiet hook's TTL self-throttle reads it; every completed bare sweep (quiet or explicit)
    /// refreshes it. A plain doc — one timestamp, never a secret.
    pub(crate) fn quiet_sweep_path(&self) -> PathBuf {
        self.state_dir().join("quiet_sweep.json")
    }

    /// `state/stat_cache.json` — the per-placement `(mtime_ns, ctime_ns, size) → sha256` drift-scan
    /// cache (see `crate::stat_cache`). A plain, ADVISORY doc — never a secret, never fail-closed;
    /// a bad or missing cache just means the next scan re-hashes.
    pub(crate) fn stat_cache_path(&self) -> PathBuf {
        self.state_dir().join("stat_cache.json")
    }

    /// `state/version_check.json` — when the passive version check last ATTEMPTED its probe (epoch
    /// millis; stamped BEFORE the probe, so an offline machine still holds the daily cadence). A
    /// plain doc — one timestamp, never a secret.
    pub(crate) fn version_check_path(&self) -> PathBuf {
        self.state_dir().join("version_check.json")
    }

    /// `ops/` — the contribute write-ahead log directory (`ops/<op_id>.json`, one per in-flight op). A
    /// home-level dir (outside `skills/<id>/`, so a publish rename never disturbs an in-flight record), it
    /// is covered by the footprint walk + uninstall like any other `~/.topos/` path.
    pub(crate) fn ops_dir(&self) -> PathBuf {
        self.home.join("ops")
    }

    /// `ops/<op_id>.json` — one contribute op's durable write-ahead record (a `0600` doc, persisted before
    /// the first send so an uncertain write replays the SAME `op_id`).
    pub(crate) fn op_path(&self, op_id: &str) -> PathBuf {
        self.ops_dir().join(format!("{op_id}.json"))
    }
}

/// The per-user segment of a project store (`state/<user>/`) — per-user because checkouts can be
/// shared between accounts. The OS username (`$USER`), sanitized with the same charset discipline
/// placement dir names use; `default` when unset or nothing survives sanitizing.
fn store_user() -> String {
    std::env::var("USER")
        .ok()
        .and_then(|u| topos_harness::sanitize_skill_dir(&u))
        .unwrap_or_else(|| "default".to_owned())
}

/// The [`Layout`] of a project's own engine store: `<project>/.topos/state/<user>/`, laid out
/// exactly like the home store so the whole engine runs against it unchanged. Pure paths — nothing
/// is created (see [`ensure_project_store`]).
pub(crate) fn project_store_layout(project_dir: &Path) -> Layout {
    Layout {
        home: project_dir
            .join(PROJECT_STORE_DIR)
            .join("state")
            .join(store_user()),
        project_root: Some(project_dir.to_path_buf()),
    }
}

/// The project's store layout when its state tree already EXISTS on disk (this user's), else
/// `None` — the read-side probe (recovery, cleaning) that must never mint a store. A `.topos` that
/// does not resolve inside the checkout is refused here too (see [`ensure_project_store`]): the
/// probe decides which stores the applied report and the cleaning sweep visit, and neither should
/// follow a committed symlink out of the tree.
pub(crate) fn existing_project_store(fs: &dyn FsOps, project_dir: &Path) -> Option<Layout> {
    if !crate::placement::within_project(project_dir, &project_dir.join(PROJECT_STORE_DIR)) {
        return None;
    }
    let layout = project_store_layout(project_dir);
    fs.exists(layout.home()).then_some(layout)
}

/// Create a project's store on first write — `<project>/.topos/` with its own self-ignore file
/// (exactly `*`, exclusive-create: never overwrite a file already at that path) and this user's
/// `state/<user>/` tree — and return its [`Layout`]. Idempotent.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] when `<project>/.topos` does not resolve inside the
/// checkout — the SAME containment rail every project placement root passes
/// ([`crate::placement::within_project`]). A repo can commit a `.topos` symlink as easily as a
/// `.claude/skills` one, and a store minted through it would write this machine's engine state
/// (and every managed byte routed through it) wherever that symlink points. The store is refused,
/// not redirected. Otherwise the [`FsOps`] failure creating the tree or writing the ignore file.
pub(crate) fn ensure_project_store(
    fs: &dyn FsOps,
    project_dir: &Path,
) -> Result<Layout, ClientError> {
    let store_dir = project_dir.join(PROJECT_STORE_DIR);
    if !crate::placement::within_project(project_dir, &store_dir) {
        return Err(ClientError::PlacementUnsupported {
            reason: crate::placement::escape_line("the project store", &store_dir),
        });
    }
    let layout = project_store_layout(project_dir);
    fs.create_dir_all(&store_dir)?;
    let ignore = store_dir.join(crate::scan::IGNORE_FILE);
    // TRUE exclusive create (`O_EXCL`), not check-then-write: a file already at the path — a
    // hand-authored ignore, or a concurrent creator's — is NEVER overwritten, and two racing
    // creators get exactly one winner (the loser's AlreadyExists is success: the file exists).
    match fs.write_new(&ignore, PROJECT_STORE_IGNORE) {
        Ok(()) => fs.fsync_dir(&store_dir)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    fs.create_dir_all(layout.home())?;
    Ok(layout)
}

/// Acquire the per-skill writer lock (blocking), held across snapshot → docs → publish. The lock file
/// lives under `locks/` — **outside** `skills/<id>/`, so it never vanishes under the publish rename.
///
/// # Errors
/// The [`FsOps`] failure if the lock cannot be opened/acquired.
pub(crate) fn lock_skill(
    fs: &dyn FsOps,
    layout: &Layout,
    id: &SkillId,
) -> Result<LockGuard, ClientError> {
    Ok(fs.lock_exclusive(&layout.lock_file(id))?)
}

/// The exhaustive set of paths topos owns under `~/.topos/` (every file **and** directory, sorted) — the
/// `--footprint` answer. A literal walk, so it is self-consistent with the real tree by construction
/// (a stray write under the home shows up here; topos never writes the user's source dir).
///
/// # Errors
/// The [`FsOps`] read failure.
pub(crate) fn footprint(fs: &dyn FsOps, layout: &Layout) -> Result<Vec<String>, ClientError> {
    let mut out = Vec::new();
    walk(fs, layout.home(), &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(fs: &dyn FsOps, dir: &Path, out: &mut Vec<String>) -> Result<(), ClientError> {
    for entry in fs.read_dir(dir)? {
        out.push(entry.to_string_lossy().into_owned());
        if entry.is_dir() {
            walk(fs, &entry, out)?;
        }
    }
    Ok(())
}

/// The idempotent recovery sweep, run at the start of every command.
///
/// - repairs a torn `log.jsonl` tail;
/// - abandons an expired, never-redeemed enrollment WAL (`now_millis` is the comparison clock);
/// - removes an incomplete staging dir (`skills/.staging-<id>/`) — but only if no live writer holds its
///   lock (else it is a concurrent `add`, left alone);
/// - removes a published `skills/<id>/` **only** if `lock.json` is absent (an impossible-via-atomic-add
///   half state) — a *present* lock is never deleted, so an unknown/newer schema means "upgrade
///   required", never data loss;
/// - sweeps leftover `*.tmp` files (a faulted atomic write pre-rename) under the per-skill lock.
///
/// The user's source dir is never touched, so a draft (the live source bytes, or a committed version in
/// the store) always survives.
///
/// # Errors
/// An [`FsOps`] failure during the sweep.
pub(crate) fn recover(fs: &dyn FsOps, layout: &Layout, now_millis: i64) -> Result<(), ClientError> {
    crate::logfile::repair_torn_tail(fs, &layout.log_path())?;
    crate::enroll::sweep_expired_wal(fs, layout, now_millis)?;

    // Sweep the retired device-era identity documents on sight: the keypair seed, the pinned
    // instance, the device credential, the membership roster, and the subscription file — the
    // SESSION model (`identity/sessions.json` + manifests + the delivery cache) replaced them
    // all, and a leftover credential is a secret with no reader.
    for dead in [
        layout.identity_dir().join("device.key"),
        layout.instance_path(),
        layout.credentials_path(),
        layout.user_path(),
        layout.follows_path(),
    ] {
        if fs.exists(&dead) {
            fs.remove_file(&dead)?;
        }
    }

    // Sweep any orphaned op-WAL temp (`ops/<op_id>.json.tmp`) a faulted WAL write left — harmless litter
    // (find_pending only matches a `.json` name) but nothing else cleans the ops dir.
    let ops_dir = layout.ops_dir();
    if fs.exists(&ops_dir) {
        for entry in fs.read_dir(&ops_dir)? {
            if entry
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(TMP_SUFFIX))
            {
                fs.remove_file(&entry)?;
            }
        }
    }

    for entry in fs.read_dir(&layout.skills_dir())? {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(id) = name.strip_prefix(STAGING_PREFIX) {
            // A name outside the validated id charset was never minted by topos — leave it alone (the
            // sweep must never lock/delete by a name it can't have created).
            let Ok(id) = SkillId::parse(id) else {
                continue;
            };
            // Incomplete `add`: claim the id; if a live writer holds it, leave it be.
            if let Some(_guard) = fs.try_lock_exclusive(&layout.lock_file(&id))? {
                fs.remove_dir_all(&entry)?;
            }
        } else if entry.is_dir() {
            // Same rule: a dir whose name fails the id parse is not a topos skill dir — never touched.
            let Ok(id) = SkillId::parse(name) else {
                continue;
            };
            recover_published(fs, layout, &id, &entry)?;
        }
    }
    Ok(())
}

fn recover_published(
    fs: &dyn FsOps,
    layout: &Layout,
    id: &SkillId,
    skill_dir: &Path,
) -> Result<(), ClientError> {
    // Claim the id; a held lock means a concurrent writer is mid-publish — leave it.
    let Some(_guard) = fs.try_lock_exclusive(&layout.lock_file(id))? else {
        return Ok(());
    };
    let paths = layout.published(id);
    if fs.read_opt(&paths.lock)?.is_none() {
        // No lock marker: an incomplete dir (can't arise via the atomic staging-rename, but never trust
        // disk). The user's source bytes are untouched, so removing the half-built sidecar is safe.
        fs.remove_dir_all(skill_dir)?;
        return Ok(());
    }
    // A lock marker is present (and, being atomically written, is whole) — never delete it; just sweep any
    // stray temp file a future in-place write might have left.
    for entry in fs.read_dir(skill_dir)? {
        if entry
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(TMP_SUFFIX))
        {
            fs.remove_file(&entry)?;
        }
    }

    // Sweep any placement-side materialization litter (`.topos-staging-<id>` / `.topos-old-<id>` /
    // `.topos-probe-<id>-*` beside the harness skill dir, OUTSIDE `~/.topos/`) a crash mid-pull may have
    // left. The next pull of THIS skill self-cleans it, but recovery runs before EVERY command (including
    // `uninstall`), so doing it here means a hidden, redundant copy of skill bytes is never orphaned when
    // the next command is `list` / `diff` / `uninstall`. Done under this skill's writer lock, by the exact
    // per-skill names, so a concurrent pull of another skill in the same parent is never disturbed.
    if let Some(map) = doc::read_map(fs, &paths.map)? {
        for placement in &map.placements {
            if let Some(parent) = Path::new(placement).parent() {
                for litter in crate::materialize::litter_siblings(parent, id.as_str()) {
                    fs.remove_dir_all(&litter)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "topos-sidecar-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The store's self-ignore is a TRUE exclusive create: a file already at the path — whatever
    /// its bytes — is never overwritten, and a second creator (the race's loser) leaves the
    /// winner's file byte-identical.
    #[test]
    fn ensure_project_store_never_overwrites_an_existing_ignore_file() {
        let proj = scratch("exclusive");
        let store_dir = proj.join(PROJECT_STORE_DIR);
        std::fs::create_dir_all(&store_dir).unwrap();
        let ignore = store_dir.join(crate::scan::IGNORE_FILE);
        std::fs::write(&ignore, b"# hand-authored\nstate/\n").unwrap();

        ensure_project_store(&RealFs, &proj).unwrap();
        assert_eq!(
            std::fs::read(&ignore).unwrap(),
            b"# hand-authored\nstate/\n",
            "an existing ignore file survives byte-identical"
        );

        // A fresh store mints the sentinel once; the second (raced/loser) call is a no-op success.
        let proj2 = scratch("exclusive2");
        ensure_project_store(&RealFs, &proj2).unwrap();
        let ignore2 = proj2.join(PROJECT_STORE_DIR).join(crate::scan::IGNORE_FILE);
        assert_eq!(std::fs::read(&ignore2).unwrap(), PROJECT_STORE_IGNORE);
        ensure_project_store(&RealFs, &proj2).unwrap();
        assert_eq!(std::fs::read(&ignore2).unwrap(), PROJECT_STORE_IGNORE);

        let _ = std::fs::remove_dir_all(&proj);
        let _ = std::fs::remove_dir_all(&proj2);
    }
}
