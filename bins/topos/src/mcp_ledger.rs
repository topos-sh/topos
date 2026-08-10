//! `state/mcp_ledger.json` — one SCOPE's MCP config-placement ownership ledger.
//!
//! The MCP drivers (`topos_harness::mcp`) are pure: ownership of a config entry is proven by the
//! `topos-` key prefix PLUS this ledger's `key → fingerprint` record of what topos LAST WROTE.
//! The ledger is therefore trust-critical state, per scope (the home store, or a checkout's own
//! `.topos/state/<user>/` — the same [`crate::sidecar::Layout`] accessor serves both), written
//! through the ordinary crash-safe [`crate::doc`] writers.
//!
//! Three responsibilities:
//!
//! - **The key registry** ([`McpLedger::keys`] / [`McpLedger::retired`]): each bundle's minted,
//!   IMMUTABLE config key. Once minted for a bundle the key never changes — several harnesses key
//!   OAuth tokens to the server name, so a rename would strand a sign-in — and a retired key is
//!   NEVER minted for a different bundle (the token it may still index must never point at
//!   someone else's server).
//! - **The entry ledger** ([`McpLedger::entries`], keyed `"<harness_slug>/<entry_key>"`): one row
//!   per committed placement — the file it lives in, the fingerprint topos last wrote, and
//!   whether topos owns the WHOLE file (created it and still owned every byte at last write).
//! - **The intent journal** ([`McpLedger::pending`]): written durably BEFORE every config write,
//!   cleared after. A crash between the config write and the ledger promotion would otherwise
//!   leave the file holding a fingerprint the ledger does not know — which every later run reads
//!   as user drift, permanently. Recovery (at converge start) observes each intended file and
//!   promotes or drops each intent by what actually landed.
//!
//! UNDECIPHERABLE IS NOT EMPTY (the [`crate::forge_trust`] posture): a corrupt document — or one
//! written by a NEWER build — answers no entries and REFUSES writes; an ownership question fails
//! closed, and an empty re-seed would turn every managed entry foreign.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// The whole document. Every map is a `BTreeMap`, so the on-disk bytes are deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct McpLedger {
    #[serde(default)]
    pub schema_version: u32,
    /// Bundle identity → the minted, immutable config key.
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    /// Retired key → the bundle it belonged to. A key here is NEVER minted for a different
    /// bundle; re-demanding the same bundle takes its key back out.
    #[serde(default)]
    pub retired: BTreeMap<String, String>,
    /// `"<harness_slug>/<entry_key>"` → a committed placement.
    #[serde(default)]
    pub entries: BTreeMap<String, LedgerEntry>,
    /// The intent journal: written BEFORE a config write, cleared after (crash recovery scans
    /// these — see the module doc).
    #[serde(default)]
    pub pending: BTreeMap<String, PendingIntent>,
}

/// One committed placement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerEntry {
    /// The bundle this entry places (the workspace skill id, or a local row's identity).
    pub bundle_id: String,
    /// The bundle version whose `server.json` the entry was rendered from (provenance).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version_id: String,
    /// The config file the entry lives in.
    pub file: String,
    /// The fingerprint topos last wrote (the drivers' drift baseline).
    pub fingerprint: String,
    /// Whole-file ownership: topos created the file and still owned every byte at last write —
    /// the precondition for deleting the file when the last entry leaves.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owns_file: bool,
}

/// One journaled intent: the [`LedgerEntry`] state a config write is ABOUT to commit. An empty
/// `fingerprint` intends the entry's ABSENCE (a removal).
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

impl McpLedger {
    /// The `entries` under one harness slug RECORDED IN `file`, as the driver's
    /// `entry_key → fingerprint` prior map. Rows recorded at a different file are NOT priors for
    /// this surface — a fingerprint proves what topos wrote into THAT file, and treating it as a
    /// prior for another would silently re-point custody after a surface path moves (an env
    /// override change), orphaning the live entry at the old path. [`Self::stale_rows`] names
    /// those rows for disclosure instead.
    pub(crate) fn prior_for(&self, slug: &str, file: &Path) -> BTreeMap<String, String> {
        let prefix = format!("{slug}/");
        self.entries
            .iter()
            .filter(|(_, e)| Path::new(&e.file) == file)
            .filter_map(|(k, e)| {
                k.strip_prefix(&prefix)
                    .map(|key| (key.to_owned(), e.fingerprint.clone()))
            })
            .collect()
    }

    /// The `(entry_key, recorded file)` rows under `slug` whose recorded file is NOT `file` —
    /// the disclosed stale class a surface-path move leaves behind. The caller warns with the
    /// old path and leaves the rows (and the old files) in place.
    pub(crate) fn stale_rows(&self, slug: &str, file: &Path) -> Vec<(String, String)> {
        let prefix = format!("{slug}/");
        self.entries
            .iter()
            .filter(|(_, e)| Path::new(&e.file) != file)
            .filter_map(|(k, e)| {
                k.strip_prefix(&prefix)
                    .map(|key| (key.to_owned(), e.file.clone()))
            })
            .collect()
    }

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

    /// Whether any committed entry still places `bundle_id`.
    pub(crate) fn has_entries_for(&self, bundle_id: &str) -> bool {
        self.entries.values().any(|e| e.bundle_id == bundle_id)
    }

    /// Recovery over the intent journal, at converge start: for each pending intent, OBSERVE the
    /// intended file — the intended state present means the config write landed (promote the
    /// intent into `entries`); anything else means it did not (drop the intent, the standing
    /// entry stays authoritative). `dialect_of` maps the pending key's harness slug to the
    /// dialect that file speaks in THIS scope (`None` = an unknown slug: the intent is dropped —
    /// the standing entries stay, which fails toward keeping what is provable).
    pub(crate) fn recover_pending(
        &mut self,
        fs: &dyn FsOps,
        dialect_of: &dyn Fn(&str) -> Option<topos_harness::mcp::McpDialect>,
    ) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let pending = std::mem::take(&mut self.pending);
        for (ledger_key, intent) in pending {
            let Some((slug, entry_key)) = ledger_key.split_once('/') else {
                continue;
            };
            let Some(dialect) = dialect_of(slug) else {
                continue;
            };
            // A read ERROR is not absence: whether the write landed is unknowable, so the
            // intent drops and the STANDING entry stays authoritative (fail toward keeping —
            // treating an IO error as an empty file would let a removal intent count as landed
            // and orphan a live entry the config may still hold).
            let Ok(bytes) = fs.read_opt(Path::new(&intent.file)) else {
                continue;
            };
            let observed = topos_harness::mcp::observe(dialect, bytes.as_deref());
            let landed = if intent.fingerprint.is_empty() {
                // A removal intent: landed when the key is gone (an unparseable file answers
                // "unknowable", which keeps the standing entry — fail toward keeping).
                observed.parseable && !observed.entries.contains_key(entry_key)
            } else {
                observed.entries.get(entry_key) == Some(&intent.fingerprint)
            };
            if landed {
                if intent.fingerprint.is_empty() {
                    self.entries.remove(&ledger_key);
                } else {
                    self.entries.insert(
                        ledger_key,
                        LedgerEntry {
                            bundle_id: intent.bundle_id,
                            version_id: intent.version_id,
                            file: intent.file,
                            fingerprint: intent.fingerprint,
                            owns_file: intent.owns_file,
                        },
                    );
                }
            }
            // else: the write never landed — the intent just drops.
        }
        true
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

/// Read the scope's ledger, or an empty default when absent.
///
/// # Errors
/// The document is UNDECIPHERABLE — corrupt, or written by a newer build
/// ([`ClientError::UnknownSchemaVersion`]). Never `Ok` with an empty ledger in that case: an
/// empty answer would read every managed entry as foreign AND clear the way for a clobbering
/// write.
pub(crate) fn read(fs: &dyn FsOps, layout: &Layout) -> Result<McpLedger, ClientError> {
    Ok(doc::read_doc(fs, &layout.mcp_ledger_path())?.unwrap_or_default())
}

/// Persist the ledger through the ordinary atomic writers (`state/` created on demand).
pub(crate) fn write(
    fs: &dyn FsOps,
    layout: &Layout,
    ledger: &McpLedger,
) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.state_dir())?;
    let mut doc_val = ledger.clone();
    doc_val.schema_version = PERSISTED_SCHEMA_VERSION;
    doc::write_doc(fs, &layout.mcp_ledger_path(), &doc_val)
}

/// The ledger key of one placement: `"<harness_slug>/<entry_key>"`.
pub(crate) fn placement_key(slug: &str, entry_key: &str) -> String {
    format!("{slug}/{entry_key}")
}

/// The engine's "which dialect does this slug's file speak in this scope" recovery lookup,
/// derived from a descriptor slice (see [`McpLedger::recover_pending`]).
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
        let dir = std::env::temp_dir().join(format!("topos-mcpl-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Layout::new(&dir)
    }

    #[test]
    fn keys_mint_sanitized_collide_with_suffixes_and_never_reuse_retired() {
        let mut l = McpLedger::default();
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
        let mut l = McpLedger::default();
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
        let mut l = McpLedger::default();
        l.mint_key("s_1", "a", Some("ws"));
        l.entries.insert(
            placement_key("cursor", "topos-ws-a"),
            LedgerEntry {
                bundle_id: "s_1".into(),
                version_id: "v".into(),
                file: "/tmp/x".into(),
                fingerprint: "fp".into(),
                owns_file: true,
            },
        );
        write(&fs, &layout, &l).unwrap();
        let back = read(&fs, &layout).unwrap();
        assert_eq!(back.keys, l.keys);
        assert_eq!(back.entries, l.entries);
        // A NEWER build's document refuses (never an empty answer).
        std::fs::write(
            layout.mcp_ledger_path(),
            format!("{{\"schema_version\": {}}}", PERSISTED_SCHEMA_VERSION + 1),
        )
        .unwrap();
        assert!(matches!(
            read(&fs, &layout),
            Err(ClientError::UnknownSchemaVersion { .. })
        ));
    }

    #[test]
    fn recovery_promotes_a_landed_intent_and_drops_an_unlanded_one() {
        use topos_harness::mcp::{self, AuthHint, McpDialect, McpEntry};
        let fs = RealFs;
        let layout = scratch("recover");
        // A cursor config holding the INTENDED entry (the write landed, the promote crashed).
        let entry = McpEntry {
            key: "topos-ws-a".into(),
            url: "https://mcp.example/a".into(),
            headers: Vec::new(),
            auth: AuthHint::Unknown,
        };
        let out = mcp::apply(McpDialect::CursorJson, None, &[entry], &BTreeMap::new());
        let topos_harness::mcp::EditPlan::Write(bytes) = &out.plan else {
            panic!("fresh apply writes");
        };
        let file = layout.home().join("mcp.json");
        std::fs::write(&file, bytes).unwrap();
        let fp = out.fingerprints[0].1.clone();

        let mut l = McpLedger::default();
        l.pending.insert(
            placement_key("cursor", "topos-ws-a"),
            PendingIntent {
                bundle_id: "s_1".into(),
                version_id: "v1".into(),
                file: file.display().to_string(),
                fingerprint: fp.clone(),
                owns_file: true,
            },
        );
        // A second intent whose write NEVER landed (the file holds no such key).
        l.pending.insert(
            placement_key("cursor", "topos-ws-b"),
            PendingIntent {
                bundle_id: "s_2".into(),
                version_id: "v1".into(),
                file: file.display().to_string(),
                fingerprint: "fp-never-written".into(),
                owns_file: false,
            },
        );
        let dialect_of = |_: &str| Some(McpDialect::CursorJson);
        assert!(l.recover_pending(&fs, &dialect_of));
        assert!(l.pending.is_empty());
        let promoted = &l.entries[&placement_key("cursor", "topos-ws-a")];
        assert_eq!(promoted.fingerprint, fp);
        assert!(promoted.owns_file);
        assert!(
            !l.entries
                .contains_key(&placement_key("cursor", "topos-ws-b"))
        );

        // A REMOVAL intent: the key is gone from the file → the entry drops out of the ledger.
        l.entries.insert(
            placement_key("cursor", "topos-ws-gone"),
            LedgerEntry {
                bundle_id: "s_3".into(),
                version_id: "v1".into(),
                file: file.display().to_string(),
                fingerprint: "old".into(),
                owns_file: false,
            },
        );
        l.pending.insert(
            placement_key("cursor", "topos-ws-gone"),
            PendingIntent {
                bundle_id: "s_3".into(),
                version_id: String::new(),
                file: file.display().to_string(),
                fingerprint: String::new(),
                owns_file: false,
            },
        );
        assert!(l.recover_pending(&fs, &dialect_of));
        assert!(
            !l.entries
                .contains_key(&placement_key("cursor", "topos-ws-gone"))
        );
    }

    #[test]
    fn recovery_keeps_the_standing_entry_when_the_intended_file_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let fs = RealFs;
        let dir = std::env::temp_dir().join(format!(
            "topos-mcpl-unread-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t").len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let protected = dir.join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        let file = protected.join("mcp.json");
        std::fs::write(&file, b"{}").unwrap();
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o000)).unwrap();
        let restore = || {
            let _ = std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&dir);
        };
        if fs.read_opt(&file).is_ok() {
            // Privileges that ignore file modes (root): the read fault cannot be staged here.
            restore();
            return;
        }

        let lk = placement_key("cursor", "topos-ws-a");
        let mut l = McpLedger::default();
        l.entries.insert(
            lk.clone(),
            LedgerEntry {
                bundle_id: "s_1".into(),
                version_id: "v1".into(),
                file: file.display().to_string(),
                fingerprint: "fp-standing".into(),
                owns_file: false,
            },
        );
        // A REMOVAL intent against a file whose read ERRORS: whether the write landed is
        // unknowable, so the intent must drop and the standing entry must stay — an IO error
        // read as "file absent" would count the removal as landed and orphan a live entry.
        l.pending.insert(
            lk.clone(),
            PendingIntent {
                bundle_id: "s_1".into(),
                version_id: String::new(),
                file: file.display().to_string(),
                fingerprint: String::new(),
                owns_file: false,
            },
        );
        let dialect_of = |_: &str| Some(topos_harness::mcp::McpDialect::CursorJson);
        let recovered = l.recover_pending(&fs, &dialect_of);
        restore();
        assert!(recovered);
        assert!(l.pending.is_empty(), "the intent drops");
        assert_eq!(
            l.entries[&lk].fingerprint, "fp-standing",
            "the standing entry stays authoritative on a read error"
        );
    }

    #[test]
    fn priors_are_scoped_to_the_recorded_file_and_stale_rows_are_named() {
        let mut l = McpLedger::default();
        l.keys.insert("s_1".into(), "topos-ws-a".into());
        l.entries.insert(
            placement_key("cursor", "topos-ws-a"),
            LedgerEntry {
                bundle_id: "s_1".into(),
                version_id: "v1".into(),
                file: "/old/home/.cursor/mcp.json".into(),
                fingerprint: "fp-old".into(),
                owns_file: false,
            },
        );
        let here = Path::new("/new/home/.cursor/mcp.json");
        assert!(
            l.prior_for("cursor", here).is_empty(),
            "a row recorded at another file is no prior for this surface"
        );
        assert_eq!(
            l.prior_for("cursor", Path::new("/old/home/.cursor/mcp.json"))
                .get("topos-ws-a")
                .map(String::as_str),
            Some("fp-old")
        );
        assert_eq!(
            l.stale_rows("cursor", here),
            vec![(
                "topos-ws-a".to_owned(),
                "/old/home/.cursor/mcp.json".to_owned()
            )]
        );
        assert!(
            l.stale_rows("cursor", Path::new("/old/home/.cursor/mcp.json"))
                .is_empty()
        );
    }
}
