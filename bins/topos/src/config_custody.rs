//! **Config-entry custody, per scope** — who owns which entry in which agent's config file, and the
//! two facts that outlive a single bundle's record.
//!
//! Ownership of a config entry is proven by the `topos-` key prefix PLUS a durable record of what
//! topos LAST WROTE (the drivers in `topos_harness::mcp` are pure and know nothing about ownership).
//! That record is **each bundle's own** [`topos_types::persisted::EntryPlacement`] rows in its
//! `map.json` — the same document that records the dirs a skill bundle owns, because a bundle's
//! targets are one record whatever shape they take.
//!
//! Two things genuinely do NOT belong to any one bundle, and they live in ONE small per-scope
//! document, `state/config_custody.json`:
//!
//! - **The minted-key registry** ([`ConfigCustody::keys`] / [`ConfigCustody::retired`]). A key names
//!   an OAuth trust surface: several harnesses file a sign-in token under the server name, so a key
//!   is IMMUTABLE once minted, and a RETIRED key is never minted for a different bundle — the token
//!   it may still index must never come to point at someone else's server. That reservation must
//!   outlive the bundle's record, so it cannot live inside it.
//! - **The pending-intent journal** ([`ConfigCustody::pending`]). ONE config write covers MANY
//!   bundles' entries, so the intent that guards it spans bundles by construction. It is written
//!   durably BEFORE every config write and cleared after; a crash in between is healed at the next
//!   converge start by OBSERVING each intended file and promoting or dropping per bundle record.
//!
//! This is name-reservation and crash custody. It is NOT placement ownership — that lives in each
//! bundle's record, and [`ScopeEntries`] is the read-modify-write view that presents both halves as
//! the one index the converge asks its ownership questions of.
//!
//! UNDECIPHERABLE IS NOT EMPTY: a corrupt document — or one written by a NEWER build — answers no
//! entries and REFUSES writes; an ownership question fails closed, and an empty re-seed would turn
//! every managed entry foreign.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// `skills/<id>/entries.json` — ONE bundle's config-entry custody, the SIBLING of its `map.json`.
///
/// The record directory is still the one place a bundle's custody lives; it holds two documents
/// because they have two writers. `map.json` (the dirs) is written under the per-skill flock;
/// this one only ever under the scope's `locks/mcp.lock`. Splitting the file is what makes
/// "one writer per file" structural rather than a convention two lock domains have to honour —
/// a targeted verb rewriting dir custody from a snapshot it read before a concurrent converge
/// cannot silently drop the rows that converge just committed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EntryCustody {
    #[serde(default)]
    pub schema_version: u32,
    /// The entries this bundle owns, indexed order (`"<agent>/<key>"` ascending — what the
    /// converge writes), so the on-disk bytes are deterministic.
    #[serde(default)]
    pub entries: Vec<EntryPlacement>,
}

/// One CONFIG-ENTRY placement's durable state — the ownership record for entries topos wrote into
/// a shared config file. The counterpart of `PlacementState` for the other target shape:
/// `fingerprint` is to an entry what `materialized_sha` is to a dir (what topos last wrote, so
/// drift is this row against the file, judged independently per row).
///
/// Ownership of an entry is proven by the `topos-` key prefix PLUS this row: the config drivers
/// are pure, so a `topos-`-looking key with no row here is FOREIGN and is never touched or claimed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EntryPlacement {
    /// The registry slug of the harness whose config file holds this entry.
    pub agent: String,
    /// The config file the entry lives in. A row recorded at another file is not custody of THIS
    /// surface: a surface path that moves leaves a disclosed stale row rather than re-pointed
    /// custody.
    pub file: String,
    /// The immutable config key topos minted for this bundle. Once minted the key never changes —
    /// several harnesses key OAuth tokens to the server name, so a rename would strand a sign-in.
    pub key: String,
    /// The fingerprint topos last wrote (the drivers' drift baseline).
    pub fingerprint: String,
    /// Whole-file ownership: topos created the file and still owned every byte at the last write —
    /// the precondition for deleting the file when the last entry leaves.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owns_file: bool,
    /// The bundle version whose `server.json` the entry was rendered from (provenance).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version_id: String,
}

/// The per-scope document. Every map is a `BTreeMap`, so the on-disk bytes are deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConfigCustody {
    #[serde(default)]
    pub schema_version: u32,
    /// Bundle identity → the minted, immutable config key.
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    /// Retired key → the bundle it belonged to. A key here is NEVER minted for a different bundle;
    /// re-demanding the same bundle takes its key back out.
    #[serde(default)]
    pub retired: BTreeMap<String, String>,
    /// The intent journal: written BEFORE a config write, cleared after (see the module doc).
    #[serde(default)]
    pub pending: BTreeMap<String, PendingIntent>,
    /// Entry custody for demands whose bundle has NO record of its own to carry it — a folder this
    /// scope's store does not track. Its identity is a `local:<name>` spelling, minted by whichever
    /// reader could not resolve the row to a record (the manifest editor's blast-radius read and
    /// its arm resolution, the inventory, the reconcile). There is no `map.json` to hold their
    /// rows, so they ride here under the bundle identity, in exactly the shape a recorded bundle
    /// keeps. Every recorded bundle's rows live in ITS record and never here — this map is empty on
    /// a machine that only ever adopted or subscribed.
    #[serde(default)]
    pub unrecorded: BTreeMap<String, Vec<EntryPlacement>>,
}

/// One journaled intent: the [`EntryPlacement`] state a config write is ABOUT to commit, plus the
/// bundle whose record it belongs to. An empty `fingerprint` intends the entry's ABSENCE (a
/// removal).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingIntent {
    pub bundle_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version_id: String,
    pub file: String,
    /// The intended fingerprint — empty = the entry is intended REMOVED.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owns_file: bool,
}

impl ConfigCustody {
    /// The bundle a config key belongs to — live first, then retired.
    pub(crate) fn bundle_of_key(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(_, k)| k.as_str() == key)
            .map(|(b, _)| b.as_str())
            .or_else(|| self.retired.get(key).map(String::as_str))
    }

    /// Mint (or recall) the immutable config key for `bundle_id`. A key already minted — live or
    /// retired — is returned verbatim (a retired one is revived); otherwise a fresh key is built
    /// from the naming rule (`topos-<workspace_slug>-<name>` / `topos-local-<name>`, sanitized,
    /// ≤ 64 ASCII) and suffixed `-2`, `-3`… past any key bound to ANOTHER bundle.
    pub(crate) fn mint_key(
        &mut self,
        bundle_id: &str,
        name: &str,
        workspace_slug: Option<&str>,
    ) -> String {
        if let Some(key) = self.keys.get(bundle_id) {
            return key.clone();
        }
        if let Some((key, _)) = self
            .retired
            .iter()
            .find(|(_, b)| b.as_str() == bundle_id)
            .map(|(k, b)| (k.clone(), b.clone()))
        {
            // Revive: the bundle is being placed again, and its old key is the one an OAuth
            // token may still be filed under.
            self.retired.remove(&key);
            self.keys.insert(bundle_id.to_owned(), key.clone());
            return key;
        }
        let base = match workspace_slug {
            Some(ws) => sanitize_key(&format!("topos-{ws}-{name}")),
            None => sanitize_key(&format!("topos-local-{name}")),
        };
        let taken = |candidate: &str| -> bool {
            self.keys
                .iter()
                .any(|(b, k)| k == candidate && b != bundle_id)
                || self.retired.get(candidate).is_some_and(|b| b != bundle_id)
        };
        let mut key = base.clone();
        let mut n = 1u32;
        while taken(&key) {
            n += 1;
            let suffix = format!("-{n}");
            let mut stem = base.clone();
            stem.truncate(64 - suffix.len());
            let stem = stem.trim_end_matches('-').to_owned();
            key = format!("{stem}{suffix}");
        }
        self.keys.insert(bundle_id.to_owned(), key.clone());
        key
    }

    /// Retire `bundle_id`'s key — the last placement of the bundle left this scope. The key stays
    /// reserved forever (see the module doc). No-op when the bundle holds no live key.
    pub(crate) fn retire_key(&mut self, bundle_id: &str) {
        if let Some(key) = self.keys.remove(bundle_id) {
            self.retired.insert(key, bundle_id.to_owned());
        }
    }
}

/// Sanitize a minted key to the managed-key contract: lowercase `[a-z0-9-]`, runs of anything
/// else collapsed to one `-`, repeats collapsed, trimmed, capped at 64 ASCII. The `topos-` prefix
/// survives by construction (the inputs start with it and `-` is legal).
fn sanitize_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        let mapped = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            last_dash = false;
            c
        } else if last_dash {
            continue;
        } else {
            last_dash = true;
            '-'
        };
        out.push(mapped);
        if out.len() == 64 {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "topos-mcp".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Read the scope's custody document, or an empty default when absent.
///
/// # Errors
/// The document is UNDECIPHERABLE — corrupt, or written by a newer build
/// ([`ClientError::UnknownSchemaVersion`]). Never `Ok` with an empty document in that case: an
/// empty answer would read every managed entry as foreign AND clear the way for a clobbering
/// write.
pub(crate) fn read(fs: &dyn FsOps, layout: &Layout) -> Result<ConfigCustody, ClientError> {
    Ok(doc::read_doc(fs, &layout.config_custody_path())?.unwrap_or_default())
}

/// Persist the custody document through the ordinary atomic writers (`state/` created on demand).
pub(crate) fn write(
    fs: &dyn FsOps,
    layout: &Layout,
    custody: &ConfigCustody,
) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.state_dir())?;
    let mut out = custody.clone();
    out.schema_version = PERSISTED_SCHEMA_VERSION;
    doc::write_doc(fs, &layout.config_custody_path(), &out)
}

/// The journal key of one entry placement: `"<harness_slug>/<entry_key>"`.
pub(crate) fn placement_key(slug: &str, entry_key: &str) -> String {
    format!("{slug}/{entry_key}")
}

// =================================================================================================
// The scope-wide read-modify-write view.
// =================================================================================================

/// ONE scope's config-entry custody, loaded for a converge: the scope document (keys, retirements,
/// the intent journal) joined onto every recorded bundle's own entry rows.
///
/// The rows are indexed exactly the way the ownership questions are asked — by
/// `"<harness_slug>/<entry_key>"`, the same key the journal uses — while REMEMBERING which bundle's
/// record each row must be written back to. Mutations mark that bundle dirty; [`Self::flush`] writes
/// each dirty record once and the scope document once.
///
/// Loading is bounded by the key registry: a bundle with no minted key has never had an entry in
/// this scope, so only the registry's bundles are read.
#[derive(Debug)]
pub(crate) struct ScopeEntries {
    /// The scope document — the key registry and the journal.
    pub doc: ConfigCustody,
    /// `"<slug>/<key>"` → the bundle it belongs to + the row itself.
    rows: BTreeMap<String, (String, EntryPlacement)>,
    /// Bundles whose rows changed and must be written back.
    dirty: BTreeSet<String>,
    /// The intents a promotion has consumed but not yet made durable IN A RECORD. Held until
    /// [`Self::flush`] proves each bundle's document landed: a record write that FAILS puts its
    /// intents back into the journal, so the scope document written on the failure path still
    /// describes work that has not landed and the next run's recovery finishes it. Without this
    /// the journal would be cleared for a promotion that never reached disk — the one way this
    /// design could lose an entry's custody permanently.
    promoted: BTreeMap<String, PendingIntent>,
    /// Whether the scope document itself changed.
    doc_dirty: bool,
}

impl ScopeEntries {
    /// Load the scope's whole custody picture.
    ///
    /// # Errors
    /// The scope document is undecipherable (see [`read`]) — the converge must then read and write
    /// nothing. A single bundle record that cannot be read is NOT an error: its rows are absent,
    /// which loses no bytes (its entries read as foreign and are left byte-identical) where failing
    /// the whole scope would strand every other bundle.
    pub(crate) fn load(fs: &dyn FsOps, layout: &Layout) -> Result<Self, ClientError> {
        let doc = read(fs, layout)?;
        let mut rows: BTreeMap<String, (String, EntryPlacement)> = BTreeMap::new();
        for bundle_id in doc.keys.keys() {
            for row in read_rows(fs, layout, &doc, bundle_id) {
                rows.insert(
                    placement_key(&row.agent, &row.key),
                    (bundle_id.clone(), row),
                );
            }
        }
        Ok(Self {
            doc,
            rows,
            dirty: BTreeSet::new(),
            promoted: BTreeMap::new(),
            doc_dirty: false,
        })
    }

    /// The rows under one harness slug RECORDED IN `file`, as the driver's `entry_key → fingerprint`
    /// prior map. Rows recorded at a different file are NOT priors for this surface — a fingerprint
    /// proves what topos wrote into THAT file, and treating it as a prior for another would silently
    /// re-point custody after a surface path moves (an env override change), orphaning the live entry
    /// at the old path. [`Self::stale_rows`] names those rows for disclosure instead.
    pub(crate) fn prior_for(&self, slug: &str, file: &Path) -> BTreeMap<String, String> {
        let prefix = format!("{slug}/");
        self.rows
            .iter()
            .filter(|(_, (_, e))| Path::new(&e.file) == file)
            .filter_map(|(k, (_, e))| {
                k.strip_prefix(&prefix)
                    .map(|key| (key.to_owned(), e.fingerprint.clone()))
            })
            .collect()
    }

    /// The `(entry_key, bundle id, recorded file)` rows under `slug` whose recorded file is NOT
    /// `file` — the disclosed stale class a surface-path move leaves behind. The caller warns with
    /// the old path and leaves the rows (and the old files) in place. The BUNDLE rides along
    /// because a person reads a warning about their own bundle, not about the config key topos
    /// minted for it.
    pub(crate) fn stale_rows(&self, slug: &str, file: &Path) -> Vec<(String, String, String)> {
        let prefix = format!("{slug}/");
        self.rows
            .iter()
            .filter(|(_, (_, e))| Path::new(&e.file) != file)
            .filter_map(|(k, (bundle, e))| {
                k.strip_prefix(&prefix)
                    .map(|key| (key.to_owned(), bundle.clone(), e.file.clone()))
            })
            .collect()
    }

    /// The bundle a config key belongs to — live first, then retired.
    pub(crate) fn bundle_of_key(&self, key: &str) -> Option<&str> {
        self.doc.bundle_of_key(key)
    }

    /// Whether any committed row still places `bundle_id`.
    pub(crate) fn has_entries_for(&self, bundle_id: &str) -> bool {
        self.rows.values().any(|(b, _)| b == bundle_id)
    }

    /// The bundles this scope had already placed — read after recovery, so it is the durable record
    /// and not a guess.
    pub(crate) fn placed_bundles(&self) -> BTreeSet<String> {
        self.rows.values().map(|(b, _)| b.clone()).collect()
    }

    /// How many rows this scope holds in total.
    #[cfg(test)]
    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Every row, as `(index key, bundle id, row)`.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &String, &EntryPlacement)> {
        self.rows.iter().map(|(k, (b, e))| (k, b, e))
    }

    /// One row, by its `"<slug>/<key>"` index.
    pub(crate) fn row(&self, custody_key: &str) -> Option<&EntryPlacement> {
        self.rows.get(custody_key).map(|(_, e)| e)
    }

    /// Whether a row exists at `custody_key`.
    pub(crate) fn holds(&self, custody_key: &str) -> bool {
        self.rows.contains_key(custody_key)
    }

    /// Every row under `slug` recorded at `file`, as `(custody_key, row)` — the surface's own rows.
    pub(crate) fn rows_at(&self, slug: &str, file: &Path) -> Vec<(String, EntryPlacement)> {
        let prefix = format!("{slug}/");
        self.rows
            .iter()
            .filter(|(k, (_, e))| k.starts_with(&prefix) && Path::new(&e.file) == file)
            .map(|(k, (_, e))| (k.clone(), e.clone()))
            .collect()
    }

    /// Commit one row into its bundle's record (in memory; [`Self::flush`] persists).
    pub(crate) fn put(&mut self, custody_key: String, bundle_id: String, row: EntryPlacement) {
        if self
            .rows
            .get(&custody_key)
            .is_some_and(|(b, e)| *b == bundle_id && *e == row)
        {
            return;
        }
        if let Some((prior_bundle, _)) = self.rows.get(&custody_key)
            && *prior_bundle != bundle_id
        {
            self.dirty.insert(prior_bundle.clone());
        }
        self.dirty.insert(bundle_id.clone());
        self.rows.insert(custody_key, (bundle_id, row));
    }

    /// Drop one row from its bundle's record.
    pub(crate) fn remove(&mut self, custody_key: &str) {
        if let Some((bundle_id, _)) = self.rows.remove(custody_key) {
            self.dirty.insert(bundle_id);
        }
    }

    /// Clear the whole-file-ownership claim on every row of `slug` recorded at `path` — belt two of
    /// the ownership discipline (see [`crate::mcp_engine`]).
    pub(crate) fn clear_owns_file(&mut self, slug: &str, path: &Path) -> bool {
        let prefix = format!("{slug}/");
        let hits: Vec<String> = self
            .rows
            .iter()
            .filter(|(k, (_, e))| {
                k.starts_with(&prefix) && Path::new(&e.file) == path && e.owns_file
            })
            .map(|(k, _)| k.clone())
            .collect();
        let moved = !hits.is_empty();
        for k in hits {
            if let Some((bundle_id, row)) = self.rows.get_mut(&k) {
                row.owns_file = false;
                self.dirty.insert(bundle_id.clone());
            }
        }
        moved
    }

    /// Mint (or recall) the immutable key for `bundle_id`.
    pub(crate) fn mint_key(
        &mut self,
        bundle_id: &str,
        name: &str,
        workspace_slug: Option<&str>,
    ) -> String {
        let before = self.doc.keys.len();
        let key = self.doc.mint_key(bundle_id, name, workspace_slug);
        if self.doc.keys.len() != before {
            self.doc_dirty = true;
        }
        key
    }

    /// The live key `bundle_id` holds, if any.
    pub(crate) fn key_of(&self, bundle_id: &str) -> Option<&str> {
        self.doc.keys.get(bundle_id).map(String::as_str)
    }

    /// Retire `bundle_id`'s key (see [`ConfigCustody::retire_key`]).
    pub(crate) fn retire_key(&mut self, bundle_id: &str) {
        if self.doc.keys.contains_key(bundle_id) {
            self.doc.retire_key(bundle_id);
            self.doc_dirty = true;
        }
    }

    /// The bundles holding a live key — the retirement scan's candidate set.
    pub(crate) fn keyed_bundles(&self) -> Vec<String> {
        self.doc.keys.keys().cloned().collect()
    }

    /// Journal `intents` durably as the intents a config write is about to commit.
    ///
    /// # Errors
    /// The scope document write failed — the caller must abandon the config write (the intent that
    /// would have made it recoverable is not on disk).
    ///
    /// REPLACES the journal wholesale, which is only sound while it is empty: intents left by an
    /// earlier surface describe work that has NOT landed in a record, and writing over them loses
    /// the only description of it. [`Self::has_pending`] is the caller's guard — see
    /// `mcp_engine::journaled_write`.
    pub(crate) fn journal(
        &mut self,
        fs: &dyn FsOps,
        layout: &Layout,
        intents: BTreeMap<String, PendingIntent>,
    ) -> Result<(), ClientError> {
        self.doc.pending = intents;
        self.doc_dirty = true;
        write(fs, layout, &self.doc)
    }

    /// Whether outstanding intents stand — work an earlier surface (or an earlier run) journalled
    /// that has not reached a record yet. A surface must never journal over them.
    pub(crate) fn has_pending(&self) -> bool {
        !self.doc.pending.is_empty()
    }

    /// Drop the journalled intents (the config write did not land) — best-effort durably.
    pub(crate) fn drop_journal(&mut self, fs: &dyn FsOps, layout: &Layout) {
        self.doc.pending.clear();
        self.doc_dirty = true;
        let _ = write(fs, layout, &self.doc);
    }

    /// Promote the journalled intents into rows — exactly what [`Self::recover`] would do by
    /// observation, run directly because this process just made the write land.
    pub(crate) fn promote_journal(&mut self) {
        let pending = std::mem::take(&mut self.doc.pending);
        self.doc_dirty = true;
        for (custody_key, intent) in pending {
            self.apply_intent(&custody_key, intent);
        }
    }

    /// Recovery over the intent journal, at converge start: for each pending intent, OBSERVE the
    /// intended file — the intended state present means the config write landed (promote the intent
    /// into the bundle's record); anything else means it did not (drop the intent, the standing row
    /// stays authoritative). `dialect_of` maps the pending key's harness slug to the dialect that
    /// file speaks in THIS scope (`None` = an unknown slug: the intent is dropped — the standing
    /// rows stay, which fails toward keeping what is provable).
    pub(crate) fn recover(
        &mut self,
        fs: &dyn FsOps,
        dialect_of: &dyn Fn(&str) -> Option<topos_harness::mcp::McpDialect>,
    ) -> bool {
        if self.doc.pending.is_empty() {
            return false;
        }
        let pending = std::mem::take(&mut self.doc.pending);
        self.doc_dirty = true;
        for (custody_key, intent) in pending {
            let Some((slug, entry_key)) = custody_key.split_once('/') else {
                continue;
            };
            let Some(dialect) = dialect_of(slug) else {
                continue;
            };
            // A read ERROR is not absence: whether the write landed is unknowable, so the intent
            // drops and the STANDING row stays authoritative (fail toward keeping — treating an IO
            // error as an empty file would let a removal intent count as landed and orphan a live
            // entry the config may still hold).
            let Ok(bytes) = fs.read_opt(Path::new(&intent.file)) else {
                continue;
            };
            let observed = topos_harness::mcp::observe(dialect, bytes.as_deref());
            let landed = if intent.fingerprint.is_empty() {
                // A removal intent: landed when the key is gone (an unparseable file answers
                // "unknowable", which keeps the standing row — fail toward keeping).
                observed.parseable && !observed.entries.contains_key(entry_key)
            } else {
                observed.entries.get(entry_key) == Some(&intent.fingerprint)
            };
            if landed {
                self.apply_intent(&custody_key, intent);
            }
            // else: the write never landed — the intent just drops.
        }
        true
    }

    /// One promoted intent, applied to the rows — and REMEMBERED until its record write lands (see
    /// [`Self::promoted`]).
    fn apply_intent(&mut self, custody_key: &str, intent: PendingIntent) {
        let Some((slug, entry_key)) = custody_key.split_once('/') else {
            return;
        };
        self.promoted.insert(custody_key.to_owned(), intent.clone());
        if intent.fingerprint.is_empty() {
            self.remove(custody_key);
            return;
        }
        self.put(
            custody_key.to_owned(),
            intent.bundle_id,
            EntryPlacement {
                agent: slug.to_owned(),
                file: intent.file,
                key: entry_key.to_owned(),
                fingerprint: intent.fingerprint,
                owns_file: intent.owns_file,
                version_id: intent.version_id,
            },
        );
    }

    /// Persist everything that moved: each dirty bundle's document once, then the scope document.
    ///
    /// A record write that FAILS puts that bundle's outstanding intents back into the journal
    /// before the scope document is written, so the durable journal always describes exactly the
    /// work that has not landed in a record. That is what makes the promise in [`journaled_write`]
    /// and at converge start true: the next run's recovery re-observes the file and promotes the
    /// row again. Best-effort per record — a document that cannot be written is reported, never
    /// retried into a half state; the returned lines are the caller's warnings.
    ///
    /// [`journaled_write`]: crate::mcp_engine
    pub(crate) fn flush(&mut self, fs: &dyn FsOps, layout: &Layout) -> Vec<topos_types::Message> {
        let mut warnings = Vec::new();
        let dirty = std::mem::take(&mut self.dirty);
        let promoted = std::mem::take(&mut self.promoted);
        for bundle_id in dirty {
            let rows: Vec<EntryPlacement> = self
                .rows
                .values()
                .filter(|(b, _)| *b == bundle_id)
                .map(|(_, e)| e.clone())
                .collect();
            let Err(e) = write_rows(fs, layout, &mut self.doc, &bundle_id, rows) else {
                self.doc_dirty = true;
                continue;
            };
            warnings.push(crate::message::failure(
                "MCP_CUSTODY_WRITE_FAILED",
                format!(
                    "{bundle_id}: topos could not save its record of this bundle's MCP entries \
                     ({}). The next 'topos update' finishes it.",
                    e.detail()
                ),
            ));
            // The rows never reached this bundle's document — so the intents that produced them
            // are still outstanding work. Back into the journal they go, and the scope document
            // written below carries them.
            for (custody_key, intent) in &promoted {
                if intent.bundle_id == bundle_id {
                    self.doc.pending.insert(custody_key.clone(), intent.clone());
                    self.doc_dirty = true;
                }
            }
        }
        if self.doc_dirty
            && let Err(e) = write(fs, layout, &self.doc)
        {
            warnings.push(crate::message::failure(
                "MCP_CUSTODY_WRITE_FAILED",
                format!(
                    "topos could not save its record of which MCP config entries it owns ({}). \
                     The next 'topos update' finishes it.",
                    e.detail()
                ),
            ));
        }
        self.doc_dirty = false;
        warnings
    }
}

// =================================================================================================
// Where one bundle's rows live: its own record, else the scope document's unrecorded map.
// =================================================================================================

/// Whether this scope's store holds a RECORD DIRECTORY for `bundle_id` — the one predicate that
/// decides where its rows live, asked identically by every reader and writer. A record dir is the
/// home of `entries.json`; anything else (an identity that is not a record id at all — a
/// `local:<name>` spelling — or a record removed under us) rides the scope document's
/// [`ConfigCustody::unrecorded`] map.
fn record_home(
    fs: &dyn FsOps,
    layout: &Layout,
    bundle_id: &str,
) -> Option<crate::sidecar::SkillPaths> {
    let sid = crate::id::SkillId::parse(bundle_id).ok()?;
    fs.exists(&layout.skill_dir(&sid))
        .then(|| layout.published(&sid))
}

/// One bundle's recorded entry rows, read WHERE THEY LIVE: its own `entries.json` when this scope's
/// store holds a record directory for it, else the scope document's unrecorded map. The two are
/// exclusive and [`write_rows`] keeps them so.
///
/// An unreadable document answers NO rows — never an error: its entries then read as foreign and
/// are left byte-identical, where failing the scope would strand every other bundle.
fn read_rows(
    fs: &dyn FsOps,
    layout: &Layout,
    doc: &ConfigCustody,
    bundle_id: &str,
) -> Vec<EntryPlacement> {
    match record_home(fs, layout, bundle_id) {
        Some(sp) => crate::doc::read_doc::<EntryCustody>(fs, &sp.entries)
            .ok()
            .flatten()
            .map(|d| d.entries)
            .unwrap_or_default(),
        None => doc.unrecorded.get(bundle_id).cloned().unwrap_or_default(),
    }
}

/// Write one bundle's entry rows back where [`read_rows`] looks for them — its own `entries.json`
/// under a record directory, else the scope document, which the caller flushes. A record that now
/// exists claims the rows and the unrecorded husk is dropped in the same pass, so the two homes
/// never both answer.
///
/// # Errors
/// The document could not be written.
fn write_rows(
    fs: &dyn FsOps,
    layout: &Layout,
    doc: &mut ConfigCustody,
    bundle_id: &str,
    rows: Vec<EntryPlacement>,
) -> Result<(), ClientError> {
    let Some(sp) = record_home(fs, layout, bundle_id) else {
        // No record to carry them (a `local:<name>` identity, a hand-written row, or a
        // record removed under us): keep the rows where they can still be found rather than
        // dropping custody of live entries.
        if rows.is_empty() {
            doc.unrecorded.remove(bundle_id);
        } else {
            doc.unrecorded.insert(bundle_id.to_owned(), rows);
        }
        return Ok(());
    };
    doc.unrecorded.remove(bundle_id);
    if rows.is_empty() {
        // Nothing left to own: drop the document rather than leave an empty husk beside the record.
        return match fs.remove_file(&sp.entries) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        };
    }
    crate::doc::write_doc(
        fs,
        &sp.entries,
        &EntryCustody {
            schema_version: PERSISTED_SCHEMA_VERSION,
            entries: rows,
        },
    )
}

/// The entry rows `bundle_id` owns in `layout`'s scope — the ONE record, read where it lives (the
/// bundle's own `map.json`, or the scope document for a bundle with no record). The read-only
/// counterpart of [`ScopeEntries`] for the surfaces that only need to ANSWER what a bundle owns
/// (`list`'s deep dive, the dest-root resolution, the still-held proofs) rather than converge it.
/// Best-effort: an unreadable record answers no rows.
pub(crate) fn entries_of(fs: &dyn FsOps, layout: &Layout, bundle_id: &str) -> Vec<EntryPlacement> {
    if let Some(sp) = record_home(fs, layout, bundle_id) {
        return crate::doc::read_doc::<EntryCustody>(fs, &sp.entries)
            .ok()
            .flatten()
            .map(|d| d.entries)
            .unwrap_or_default();
    }
    read(fs, layout)
        .ok()
        .and_then(|d| d.unrecorded.get(bundle_id).cloned())
        .unwrap_or_default()
}

/// Move `bundle_id`'s SURVIVING rows out of its record and into [`ConfigCustody::unrecorded`] —
/// called when the RECORD ITSELF is about to be deleted (a classic `remove` of a config-placed
/// bundle). A drifted entry is never clobbered, so a removal legitimately leaves rows standing;
/// deleting the record with them inside would strand those entries in the person's config files
/// forever, with nothing left to prove they were ever topos's. Moved here they stay disclosable,
/// the bundle keeps its key (it still has entries, so retirement has not fired), and a later sweep
/// still removes them once the hand-edit is reverted.
///
/// Answers whether any row moved. Must be called under the scope's converge lock.
///
/// # Errors
/// The scope document could not be read or written.
pub(crate) fn detach_to_unrecorded(
    fs: &dyn FsOps,
    layout: &Layout,
    bundle_id: &str,
) -> Result<bool, ClientError> {
    let Some(sp) = record_home(fs, layout, bundle_id) else {
        return Ok(false); // already unrecorded — nothing to move
    };
    let rows = crate::doc::read_doc::<EntryCustody>(fs, &sp.entries)
        .ok()
        .flatten()
        .map(|d| d.entries)
        .unwrap_or_default();
    if rows.is_empty() {
        return Ok(false);
    }
    let mut doc = read(fs, layout)?;
    doc.unrecorded.insert(bundle_id.to_owned(), rows);
    write(fs, layout, &doc)?;
    Ok(true)
}

/// The rows the FIRST of `ids` that owns any answers — a bundle is filed under ONE of the spellings
/// its row could carry, never several at once.
pub(crate) fn entries_of_any(
    fs: &dyn FsOps,
    layout: &Layout,
    ids: &[String],
) -> Vec<EntryPlacement> {
    ids.iter()
        .map(|id| entries_of(fs, layout, id))
        .find(|rows| !rows.is_empty())
        .unwrap_or_default()
}

/// The engine's "which dialect does this slug's file speak in this scope" recovery lookup, derived
/// from the MCP-capable registry rows (see [`ScopeEntries::recover`]).
pub(crate) fn dialect_lookup<'a>(
    descriptors: &'a [&'static topos_harness::registry::KnownHarness],
    project: bool,
) -> impl Fn(&str) -> Option<topos_harness::mcp::McpDialect> + 'a {
    move |slug: &str| {
        let h = descriptors.iter().find(|h| h.slug == slug)?;
        if project {
            h.mcp().and_then(|m| m.project).map(|(_, d)| d)
        } else {
            // The intent journal records the driver surface FILE — for the plugin dir that is
            // its `.mcp.json`, which observes through the same dialect as every other surface.
            h.mcp().and_then(|m| m.user).map(|s| s.dialect)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> Layout {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-cust-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Layout::new(&dir)
    }

    #[test]
    fn keys_mint_sanitized_collide_with_suffixes_and_never_reuse_retired() {
        let mut l = ConfigCustody::default();
        // The workspace form, sanitized (uppercase + illegal runs collapse).
        assert_eq!(
            l.mint_key("s_1", "Linear MCP!", Some("acme")),
            "topos-acme-linear-mcp"
        );
        // Minted once — the same bundle always answers the same key.
        assert_eq!(
            l.mint_key("s_1", "renamed", Some("other")),
            "topos-acme-linear-mcp"
        );
        // A DIFFERENT bundle colliding on the natural spelling suffixes.
        assert_eq!(
            l.mint_key("s_2", "linear-mcp", Some("acme")),
            "topos-acme-linear-mcp-2"
        );
        assert_eq!(
            l.mint_key("s_3", "linear mcp", Some("acme")),
            "topos-acme-linear-mcp-3"
        );
        // The local form.
        assert_eq!(l.mint_key("local:x", "x", None), "topos-local-x");
        // Retire s_1; its key is reserved — a NEW bundle can never take it…
        l.retire_key("s_1");
        assert_eq!(
            l.mint_key("s_9", "linear-mcp", Some("acme")),
            "topos-acme-linear-mcp-4"
        );
        // …but the SAME bundle revives it.
        assert_eq!(
            l.mint_key("s_1", "whatever", Some("acme")),
            "topos-acme-linear-mcp"
        );
        assert!(!l.retired.contains_key("topos-acme-linear-mcp"));
        // Length cap: 64 ASCII, suffix included.
        let long = "x".repeat(100);
        let k = l.mint_key("s_long", &long, Some("acme"));
        assert!(k.len() <= 64, "{k}");
        let k2 = l.mint_key("s_long2", &long, Some("acme"));
        assert!(k2.len() <= 64 && k2.ends_with("-2"), "{k2}");
    }

    #[test]
    fn a_publish_transfer_keeps_the_local_key() {
        // A local row minted `topos-local-*`; the SAME bundle later demanded as a workspace row
        // (a landed publish keeps the skill id) keeps the key — OAuth tokens key to the name.
        let mut l = ConfigCustody::default();
        assert_eq!(l.mint_key("s_x", "notion", None), "topos-local-notion");
        assert_eq!(
            l.mint_key("s_x", "notion", Some("acme")),
            "topos-local-notion"
        );
    }

    #[test]
    fn the_doc_round_trips_and_fails_closed_on_a_newer_schema() {
        let fs = RealFs;
        let layout = scratch("doc");
        let mut l = ConfigCustody::default();
        l.mint_key("s_1", "a", Some("ws"));
        write(&fs, &layout, &l).unwrap();
        let back = read(&fs, &layout).unwrap();
        assert_eq!(back.keys, l.keys);
        // A NEWER build's document refuses (never an empty answer).
        std::fs::write(
            layout.config_custody_path(),
            format!("{{\"schema_version\": {}}}", PERSISTED_SCHEMA_VERSION + 1),
        )
        .unwrap();
        assert!(matches!(
            read(&fs, &layout),
            Err(ClientError::UnknownSchemaVersion { .. })
        ));
    }

    /// An UNRECORDED bundle (a `local:<name>` identity — no store record to carry its
    /// rows) round-trips through the scope document, and clearing its rows drops the map entry
    /// rather than leaving an empty list behind.
    #[test]
    fn an_unrecorded_bundles_rows_ride_the_scope_document() {
        let fs = RealFs;
        let layout = scratch("unrecorded");
        let mut doc = ConfigCustody::default();
        doc.mint_key("local:weather", "weather", None);
        write(&fs, &layout, &doc).unwrap();

        let mut scope = ScopeEntries::load(&fs, &layout).unwrap();
        let row = EntryPlacement {
            agent: "cursor".to_owned(),
            file: "/home/x/.cursor/mcp.json".to_owned(),
            key: "topos-local-weather".to_owned(),
            fingerprint: "fp".to_owned(),
            owns_file: true,
            version_id: String::new(),
        };
        scope.put(
            placement_key("cursor", "topos-local-weather"),
            "local:weather".to_owned(),
            row.clone(),
        );
        assert!(scope.flush(&fs, &layout).is_empty());

        let back = ScopeEntries::load(&fs, &layout).unwrap();
        assert!(back.has_entries_for("local:weather"));
        assert_eq!(
            back.prior_for("cursor", Path::new("/home/x/.cursor/mcp.json"))
                .get("topos-local-weather")
                .map(String::as_str),
            Some("fp")
        );

        let mut back = back;
        back.remove(&placement_key("cursor", "topos-local-weather"));
        assert!(back.flush(&fs, &layout).is_empty());
        let empty = read(&fs, &layout).unwrap();
        assert!(
            !empty.unrecorded.contains_key("local:weather"),
            "an emptied bundle leaves no husk behind"
        );
    }

    /// A store written by a PREVIOUS release — a `state/mcp_ledger.json` holding keys, entries and
    /// a journal, beside a `map.json` written before config custody existed — is neither read nor
    /// tripped over. The old document is not this build's; its keys reserve nothing, its entries
    /// claim nothing, and the record beside it owns no config entries at all.
    #[test]
    fn a_previous_releases_ledger_and_map_are_ignored_not_consulted() {
        let fs = RealFs;
        let layout = scratch("old-state");
        std::fs::create_dir_all(layout.state_dir()).unwrap();
        // The retired document, in its exact old shape.
        std::fs::write(
            layout.state_dir().join("mcp_ledger.json"),
            br#"{
  "schema_version": 1,
  "keys": { "s_old": "topos-acme-old" },
  "retired": { "topos-acme-gone": "s_gone" },
  "entries": {
    "cursor/topos-acme-old": {
      "bundle_id": "s_old",
      "version_id": "v1",
      "file": "/home/x/.cursor/mcp.json",
      "fingerprint": "fp-old",
      "owns_file": true
    }
  },
  "pending": {}
}
"#,
        )
        .unwrap();
        // A v2 record beside it: one dir placement, no config-entry half.
        let sid = crate::id::SkillId::parse("topos_00000000000000000000000000000001").unwrap();
        let sp = layout.published(&sid);
        std::fs::create_dir_all(sp.map.parent().unwrap()).unwrap();
        std::fs::write(
            &sp.map,
            br#"{
  "schema_version": 2,
  "placements": ["/home/x/.claude/skills/old"],
  "applied_commit": "0000000000000000000000000000000000000000000000000000000000000000",
  "materialized_sha": "1111111111111111111111111111111111111111111111111111111111111111",
  "swap_capability": "unsupported",
  "placement_state": [
    { "kind": "native", "agent": "claude-code", "swap_capability": "unsupported" }
  ]
}
"#,
        )
        .unwrap();

        // The scope's custody: empty. No key is reserved, no retirement is honored, no row stands.
        let scope = ScopeEntries::load(&fs, &layout).unwrap();
        assert!(
            scope.keyed_bundles().is_empty(),
            "no key survives the break"
        );
        assert_eq!(scope.row_count(), 0, "no entry row survives the break");
        assert!(scope.doc.retired.is_empty(), "no reservation survives it");
        assert!(scope.bundle_of_key("topos-acme-old").is_none());
        assert!(!scope.has_entries_for("s_old"));

        // The old document is left exactly where it is — ignoring is not deleting.
        assert!(layout.state_dir().join("mcp_ledger.json").exists());

        // The record still loads as pure dir custody, and owns no config entries: no
        // `entries.json` was ever written beside it.
        let map = crate::doc::read_map(&fs, &sp.map)
            .unwrap()
            .expect("the record loads");
        assert_eq!(map.placements.len(), 1);
        assert!(!sp.entries.exists(), "an old record has no entry document");
        assert!(entries_of(&fs, &layout, sid.as_str()).is_empty());
    }

    /// Priors are scoped to the FILE they were recorded at; a row at another file is named for
    /// disclosure and never treated as this surface's custody.
    #[test]
    fn priors_are_scoped_to_the_recorded_file_and_stale_rows_are_named() {
        let fs = RealFs;
        let layout = scratch("stale");
        let mut doc = ConfigCustody::default();
        doc.keys.insert("local:a".into(), "topos-ws-a".into());
        doc.unrecorded.insert(
            "local:a".into(),
            vec![EntryPlacement {
                agent: "cursor".into(),
                file: "/old/home/.cursor/mcp.json".into(),
                key: "topos-ws-a".into(),
                fingerprint: "fp-old".into(),
                owns_file: false,
                version_id: "v1".into(),
            }],
        );
        write(&fs, &layout, &doc).unwrap();
        let scope = ScopeEntries::load(&fs, &layout).unwrap();

        let here = Path::new("/new/home/.cursor/mcp.json");
        assert!(
            scope.prior_for("cursor", here).is_empty(),
            "a row recorded at another file is no prior for this surface"
        );
        assert_eq!(
            scope
                .prior_for("cursor", Path::new("/old/home/.cursor/mcp.json"))
                .get("topos-ws-a")
                .map(String::as_str),
            Some("fp-old")
        );
        // The BUNDLE rides along with the key and the old file: the disclosure a caller writes
        // from this is about a bundle a person owns, not about the config key topos minted.
        assert_eq!(
            scope.stale_rows("cursor", here),
            vec![(
                "topos-ws-a".to_owned(),
                "local:a".to_owned(),
                "/old/home/.cursor/mcp.json".to_owned()
            )]
        );
        assert!(
            scope
                .stale_rows("cursor", Path::new("/old/home/.cursor/mcp.json"))
                .is_empty()
        );
    }
}
