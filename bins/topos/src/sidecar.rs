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

    /// `locks/visited-stores.lock` — the visited-store index's writer lock (a fixed name). The
    /// index is a read-union-write over one machine-local document, so two sweeps running from
    /// two checkouts would each write a set built from bytes the other had already replaced, and
    /// the loser's checkout would vanish from every later applied report with no error anywhere.
    pub(crate) fn visited_stores_lock_file(&self) -> PathBuf {
        self.locks_dir().join("visited-stores.lock")
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

    /// `state/park_journal.json` — the PARK JOURNAL: every placement/sidecar directory an
    /// operation moves aside under a UNIQUE name (`.topos-refresh-old-*`, `.topos-retiring-*`,
    /// a failed import's park) is journaled here — durably, BEFORE the rename — so a crash
    /// between the park and the operation's conclusion strands no bytes invisibly: recovery
    /// ([`recover`]) restores each leftover park to its original path, or preserves + discloses
    /// it when the original has been re-taken. A plain doc — paths, never a secret.
    pub(crate) fn park_journal_path(&self) -> PathBuf {
        self.state_dir().join("park_journal.json")
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
    // RE-PROVE containment at the WRITE boundary: the check above and the creates below it are
    // separate syscalls, and an ancestor that became a symlink between them routes the created
    // tree — and every engine byte that would flow through this layout — wherever it points.
    // With the full path now existing, the proof covers every component (lstat-walked + the
    // canonical containment), so a mid-create swap is caught here and the store is REFUSED, not
    // used. Nothing is deleted: only empty directories were created, and removing through a
    // symlink is exactly the class of write this rail exists to refuse.
    if !crate::placement::within_project(project_dir, layout.home()) {
        return Err(ClientError::PlacementUnsupported {
            reason: crate::placement::escape_line("the project store", layout.home()),
        });
    }
    Ok(layout)
}

// =================================================================================================
// The park journal — crash recovery for uniquely-named parks
// =================================================================================================

/// The durable record of parks in flight (`state/park_journal.json`). A park under a UNIQUE name
/// (`park_aside`'s ladder) is invisible to every name-keyed sweep by design — which is exactly why
/// a crash between the park and its operation's conclusion must leave a journal, not hope: the
/// next run's recovery reads this and puts the bytes back (or preserves + discloses them).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ParkJournal {
    pub schema_version: u32,
    #[serde(default)]
    pub parks: Vec<ParkEntry>,
}

/// One journaled park: where the tree sits now, and where it must return if the operation that
/// moved it never concludes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ParkEntry {
    /// The park path (where the tree was moved TO).
    pub park: String,
    /// The path the tree was moved FROM — recovery's restore target.
    pub original: String,
    /// Whether recovery may RESTORE the park to `original` when that path is free (`true` — the
    /// ordinary case: the bytes belong there and the operation simply never concluded). `false`
    /// marks a park whose restore could contradict state the operation may already have
    /// established (a refresh's old sidecar record after the new one landed): recovery PRESERVES
    /// + discloses it instead — a human decision, never a guess.
    #[serde(default = "restore_default")]
    pub restore: bool,
}

fn restore_default() -> bool {
    true
}

/// Record one park in the journal — durably, BEFORE the rename that creates it (the caller's
/// contract). An unreadable/newer journal REFUSES the write (and therefore the park): an older
/// binary must not clobber a newer document, and a park that cannot be journaled is a park a
/// crash would strand invisibly.
///
/// # Errors
/// The journal read/write failure (fail-closed on an unknown schema).
pub(crate) fn journal_park(
    fs: &dyn FsOps,
    layout: &Layout,
    original: &Path,
    park: &Path,
    restore: bool,
) -> Result<(), ClientError> {
    let path = layout.park_journal_path();
    let mut journal: ParkJournal = crate::doc::read_doc(fs, &path)?.unwrap_or_default();
    journal.schema_version = topos_types::PERSISTED_SCHEMA_VERSION;
    let park_s = park.to_string_lossy().into_owned();
    journal.parks.retain(|e| e.park != park_s);
    journal.parks.push(ParkEntry {
        park: park_s,
        original: original.to_string_lossy().into_owned(),
        restore,
    });
    fs.create_dir_all(&layout.state_dir())?;
    crate::doc::write_doc(fs, &path, &journal)
}

/// Clear one park's journal entry — the operation concluded (the park was dropped, or restored).
/// Best-effort by contract: a failure leaves a stale entry, which recovery resolves harmlessly
/// (an absent park is a concluded one).
pub(crate) fn settle_park_journal(fs: &dyn FsOps, layout: &Layout, park: &Path) {
    let path = layout.park_journal_path();
    let Ok(Some(mut journal)) = crate::doc::read_doc::<ParkJournal>(fs, &path) else {
        return;
    };
    let before = journal.parks.len();
    journal.parks.retain(|e| Path::new(&e.park) != park);
    if journal.parks.len() != before {
        let _ = crate::doc::write_doc(fs, &path, &journal);
    }
}

/// Recovery's half of the journal: every entry whose park still exists is RESTORED to its
/// original path (the map/manifest rows that reference it find their bytes again), or — when the
/// original has since been re-taken, or the restore fails — PRESERVED as a `.topos-kept-*`
/// sibling and disclosed in the log. An absent park is a concluded operation (its entry is
/// dropped); an undecipherable journal is left untouched (never "unreadable = empty").
fn recover_park_journal(
    fs: &dyn FsOps,
    layout: &Layout,
    now_millis: i64,
) -> Result<(), ClientError> {
    let path = layout.park_journal_path();
    let journal = match crate::doc::read_doc::<ParkJournal>(fs, &path) {
        Ok(Some(j)) => j,
        Ok(None) => return Ok(()),
        // Fail closed: an unknown/newer journal deletes nothing and is not rewritten.
        Err(_) => return Ok(()),
    };
    if journal.parks.is_empty() {
        return Ok(());
    }
    let mut remaining: Vec<ParkEntry> = Vec::new();
    for entry in journal.parks {
        let park = PathBuf::from(&entry.park);
        let original = PathBuf::from(&entry.original);
        if !fs.exists(&park) {
            continue; // concluded: dropped or restored before the crash cleared the entry
        }
        if entry.restore && !fs.exists(&original) && fs.rename(&park, &original).is_ok() {
            let _ = crate::logfile::append_event(
                fs,
                &layout.log_path(),
                &serde_json::json!({
                    "action": "park_restored",
                    "park": entry.park,
                    "restored_to": entry.original,
                    "at": now_millis,
                }),
            );
            continue;
        }
        // The original was re-created (or the restore failed): preserve + disclose, never delete.
        match crate::materialize::preserve_park(fs, &park) {
            Some(kept) => {
                let _ = crate::logfile::append_event(
                    fs,
                    &layout.log_path(),
                    &serde_json::json!({
                        "action": "park_preserved",
                        "park": entry.park,
                        "kept_at": kept.to_string_lossy(),
                        "original": entry.original,
                        "at": now_millis,
                    }),
                );
            }
            None => remaining.push(entry), // stuck under its own name: retried next run
        }
    }
    let rewritten = ParkJournal {
        schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
        parks: remaining,
    };
    crate::doc::write_doc(fs, &path, &rewritten)
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
/// - restores (or preserves + discloses) the parks the PARK JOURNAL records — an interrupted
///   refresh/retire/import left its only bytes under a unique park name;
/// - JUDGES leftover placement-side parks (`.topos-staging-*` / `.topos-old-*`): accounted bytes
///   drop, anything else is preserved as a `.topos-kept-*` sibling — never deleted on the name;
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
    // Restore (or preserve + disclose) the parks an interrupted operation journaled — BEFORE the
    // per-skill sweeps below, so a restored tree is back where its map/manifest rows expect it.
    recover_park_journal(fs, layout, now_millis)?;

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
    //
    // The staging/graveyard names are PARKS, and a crash between the swap and its
    // `verify_parked_old` strands the OLD tree there — raced edits included — so they are JUDGED,
    // never deleted on their name alone: bytes this skill's own records account for (the lock's
    // current digest, a placement's recorded baseline) drop; anything else — an unaccounted tree,
    // an unreadable one, an unparsable lock — is PRESERVED as a `.topos-kept-*` sibling and
    // disclosed in the log. Recovery has no snapshotter, so preservation is its absorb. Only the
    // probe dirs (throwaway empties the materializer mints) are removed blind.
    if let Some(map) = doc::read_map(fs, &paths.map)? {
        let mut accounted: Vec<String> = map
            .placement_state
            .iter()
            .filter_map(|st| st.materialized_sha.clone())
            .collect();
        accounted.push(map.materialized_sha.clone());
        if let Ok(Some(lock)) =
            crate::doc::read_doc::<topos_types::persisted::Lock>(fs, &paths.lock)
        {
            accounted.push(lock.bundle_digest);
        }
        for placement in &map.placements {
            let Some(parent) = Path::new(placement).parent() else {
                continue;
            };
            for litter in crate::materialize::litter_siblings(parent, id.as_str()) {
                if !fs.exists(&litter) {
                    continue;
                }
                let name = litter.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with(".topos-probe-") {
                    fs.remove_dir_all(&litter)?;
                    continue;
                }
                let benign = crate::scan::scan(&litter).is_ok_and(|scanned| {
                    let hex = topos_core::digest::to_hex(&scanned.bundle_digest);
                    accounted.contains(&hex)
                });
                if benign {
                    fs.remove_dir_all(&litter)?;
                } else if let Some(kept) = crate::materialize::preserve_park(fs, &litter) {
                    let _ = crate::logfile::append_event(
                        fs,
                        &layout.log_path(),
                        &serde_json::json!({
                            "action": "park_preserved",
                            "park": litter.to_string_lossy(),
                            "kept_at": kept.to_string_lossy(),
                            "skill_id": id.as_str(),
                        }),
                    );
                }
                // A park that can neither be accounted for nor moved stays under its own name —
                // the materializer's litter judge refuses over it rather than deleting blind.
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

    /// THE PARK JOURNAL: a crash between a journaled park and its conclusion strands the tree's
    /// only bytes under a unique name no sweep knows — recovery reads the journal and puts them
    /// back (restore), or preserves + discloses them (no-restore / original retaken). Never a
    /// deletion anywhere.
    #[test]
    fn recovery_restores_or_preserves_journaled_parks_never_deletes() {
        let home = scratch("journal");
        let layout = Layout::new(&home);
        std::fs::create_dir_all(layout.skills_dir()).unwrap();
        let fs = RealFs;

        // (a) RESTORE: the ordinary interrupted operation — the original path is free again.
        let orig_a = home.join("place-a").join("demo");
        std::fs::create_dir_all(&orig_a).unwrap();
        std::fs::write(orig_a.join("SKILL.md"), b"# a draft worth keeping\n").unwrap();
        let park_a =
            crate::materialize::park_aside_journaled(&fs, &layout, &orig_a, "retiring", true)
                .unwrap();
        assert!(!orig_a.exists());

        // (b) NO-RESTORE: the operation may already have re-established state (a refresh's old
        // sidecar record) — recovery must PRESERVE, not restore.
        let orig_b = home.join("place-b").join("demo");
        std::fs::create_dir_all(&orig_b).unwrap();
        std::fs::write(orig_b.join("SKILL.md"), b"# old record\n").unwrap();
        let park_b =
            crate::materialize::park_aside_journaled(&fs, &layout, &orig_b, "refresh-old", false)
                .unwrap();

        // (c) ORIGINAL RETAKEN: something re-created the original — the park must not clobber it.
        let orig_c = home.join("place-c").join("demo");
        std::fs::create_dir_all(&orig_c).unwrap();
        std::fs::write(orig_c.join("SKILL.md"), b"# first\n").unwrap();
        let park_c =
            crate::materialize::park_aside_journaled(&fs, &layout, &orig_c, "retiring", true)
                .unwrap();
        std::fs::create_dir_all(&orig_c).unwrap();
        std::fs::write(orig_c.join("SKILL.md"), b"# the newcomer\n").unwrap();

        recover(&fs, &layout, 1).unwrap();

        // (a) restored whole; its journal entry settled.
        assert_eq!(
            std::fs::read(orig_a.join("SKILL.md")).unwrap(),
            b"# a draft worth keeping\n"
        );
        assert!(!park_a.exists());
        // (b) preserved as a kept sibling; the original path stays free.
        assert!(!orig_b.exists(), "a no-restore park is never restored");
        assert!(!park_b.exists(), "…but it left its park name");
        let kept_b = park_b.with_file_name(format!(
            ".topos-kept-{}",
            park_b.file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(
            std::fs::read(kept_b.join("SKILL.md")).unwrap(),
            b"# old record\n",
            "the bytes sit whole under the kept name"
        );
        // (c) the newcomer keeps the path; the parked bytes are preserved beside it.
        assert_eq!(
            std::fs::read(orig_c.join("SKILL.md")).unwrap(),
            b"# the newcomer\n"
        );
        let kept_c = park_c.with_file_name(format!(
            ".topos-kept-{}",
            park_c.file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(
            std::fs::read(kept_c.join("SKILL.md")).unwrap(),
            b"# first\n"
        );
        // The journal is settled (no entry left), and the disclosures are on the log.
        let journal: ParkJournal = crate::doc::read_doc(&fs, &layout.park_journal_path())
            .unwrap()
            .unwrap();
        assert!(journal.parks.is_empty(), "{:?}", journal.parks);
        let log = std::fs::read_to_string(layout.log_path()).unwrap();
        assert!(log.contains("park_restored"), "{log}");
        assert!(log.contains("park_preserved"), "{log}");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// THE LITTER JUDGE: recovery finds a `.topos-staging-*`/`.topos-old-*` park a crashed apply
    /// left mid-judgment. Bytes the skill's own records account for drop; NOVEL bytes (an edit
    /// raced in before the swap, stranded by the crash) are preserved as a `.topos-kept-*`
    /// sibling and disclosed — never deleted on the name alone.
    #[test]
    fn recovery_judges_placement_parks_preserving_novel_bytes() {
        use topos_types::PERSISTED_SCHEMA_VERSION;
        use topos_types::persisted::{
            Lock, LockedFile, PlacementKind, PlacementMap, PlacementState, SwapCapability,
        };
        let home = scratch("litter");
        let parent = scratch("litter-place");
        let layout = Layout::new(&home);
        let fs = RealFs;
        let id = SkillId::parse("topos_litter1").unwrap();
        let placement = parent.join("demo");
        std::fs::create_dir_all(&placement).unwrap();
        std::fs::write(placement.join("SKILL.md"), b"# current\n").unwrap();

        // The skill's own records: lock at the current digest, map with the placement + baseline.
        let baseline = {
            let scanned = crate::scan::scan(&placement).unwrap();
            topos_core::digest::to_hex(&scanned.bundle_digest)
        };
        let sp = layout.published(&id);
        std::fs::create_dir_all(&sp.store).unwrap();
        crate::doc::write_doc(
            &fs,
            &sp.lock,
            &Lock {
                schema_version: PERSISTED_SCHEMA_VERSION,
                skill_id: id.to_string(),
                name: "demo".into(),
                base_commit: "1".repeat(64),
                bundle_digest: baseline.clone(),
                files: Vec::<LockedFile>::new(),
            },
        )
        .unwrap();
        crate::doc::write_map(
            &fs,
            &sp.map,
            &PlacementMap {
                schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
                placements: vec![placement.to_string_lossy().into_owned()],
                applied_commit: "1".repeat(64),
                materialized_sha: baseline.clone(),
                pre_existing_sha: None,
                swap_capability: SwapCapability::Unsupported,
                placement_state: vec![PlacementState {
                    kind: PlacementKind::Native,
                    agent: None,
                    materialized_sha: Some(baseline.clone()),
                    pre_existing_sha: None,
                    swap_capability: SwapCapability::Unsupported,
                }],
                harness: None,
                harness_layer: None,
                harness_slug: None,
            },
        )
        .unwrap();

        // Two stranded parks: one holding exactly the baseline (accounted — droppable), one
        // holding an edit nothing captured (novel — must survive).
        let accounted_park = parent.join(".topos-staging-topos_litter1");
        std::fs::create_dir_all(&accounted_park).unwrap();
        std::fs::write(accounted_park.join("SKILL.md"), b"# current\n").unwrap();
        let novel_park = parent.join(".topos-old-topos_litter1");
        std::fs::create_dir_all(&novel_park).unwrap();
        std::fs::write(novel_park.join("SKILL.md"), b"# a raced edit\n").unwrap();

        recover(&fs, &layout, 1).unwrap();

        assert!(
            !accounted_park.exists(),
            "an accounted park is dropped as before"
        );
        assert!(!novel_park.exists(), "the novel park left its swept name…");
        let kept = parent.join(".topos-kept-.topos-old-topos_litter1");
        assert_eq!(
            std::fs::read(kept.join("SKILL.md")).unwrap(),
            b"# a raced edit\n",
            "…and its bytes survive whole under the kept name"
        );
        let log = std::fs::read_to_string(layout.log_path()).unwrap();
        assert!(log.contains("park_preserved"), "{log}");

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&parent);
    }
}
