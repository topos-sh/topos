//! The Codex `config.toml` patcher — [`McpDialect::CodexToml`] over `toml_edit` (the same
//! format-preserving editor the CLI's manifest edits use): comments, layout, and every
//! non-managed table survive byte-for-byte; the edit touches only `[mcp_servers.<topos-*>]`
//! tables.
//!
//! An entry table carries ONLY `url = "…"` and, when headers exist, `http_headers = { … }` —
//! never any other key (a wrong key hard-fails Codex's whole config load). The fingerprint is
//! computed over the table converted to a sorted structural value, so a whitespace or key-order
//! reflow of the file never reads as drift.
//!
//! Provable shapes only: the file must parse as TOML and `mcp_servers`, when present, must be a
//! STANDARD table (an inline-table or dotted spelling is somebody's deliberate style this driver
//! must not rewrite — [`EditPlan::Unprovable`], zero writes). Verification after every edit:
//! the output re-parses, the touched entries carry exactly the desired values, and scrubbing the
//! touched keys from BOTH documents yields byte-identical renderings — so everything outside our
//! tables is provably unchanged. Any surprise downgrades to `Unprovable`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use toml_edit::{DocumentMut, InlineTable, Item, Table};

use super::{
    ApplyOutcome, EditPlan, EntryState, FoundEntry, MANAGED_KEY_PREFIX, McpDialect, McpEntry,
    Observed, Reconcile, effectively_absent, entry_value, fingerprint_value, outcome, reconcile,
    unprovable, validate_desired,
};

/// The one table this driver may touch children of.
const SERVERS_KEY: &str = "mcp_servers";

/// Compute the placement plan for a Codex `config.toml`. See the module doc for the contract.
#[must_use]
pub fn apply(
    current: Option<&[u8]>,
    desired: &[McpEntry],
    prior: &BTreeMap<String, String>,
) -> ApplyOutcome {
    if let Err(reason) = validate_desired(desired) {
        return unprovable(reason);
    }
    let desired_fps: Vec<String> = desired
        .iter()
        .map(|e| fingerprint_value(&entry_value(McpDialect::CodexToml, e)))
        .collect();

    if effectively_absent(current) {
        if desired.is_empty() {
            return outcome(EditPlan::Leave, Reconcile::default(), false);
        }
        let mut doc = DocumentMut::new();
        if let Err(reason) =
            upsert_entries(&mut doc, desired, &(0..desired.len()).collect::<Vec<_>>())
        {
            return unprovable(reason);
        }
        let mut rec = Reconcile::default();
        for (i, e) in desired.iter().enumerate() {
            rec.states.push((e.key.clone(), EntryState::PlacedNew));
            rec.fingerprints
                .push((e.key.clone(), desired_fps[i].clone()));
        }
        return outcome(EditPlan::Write(doc.to_string().into_bytes()), rec, true);
    }
    let Ok(text) = std::str::from_utf8(current.unwrap_or_default()) else {
        return unprovable("the Codex config is not UTF-8");
    };
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return unprovable("the Codex config is not valid TOML");
    };
    let found = match found_entries(&doc) {
        Ok(found) => found,
        Err(reason) => return unprovable(reason),
    };
    let rec = reconcile(desired, &desired_fps, &found, prior);
    if rec.is_noop() {
        return outcome(EditPlan::Leave, rec, false);
    }

    // Mutate: removes first, then updates (position-preserving), then inserts.
    if let Some(servers) = doc.get_mut(SERVERS_KEY).and_then(Item::as_table_mut) {
        for key in &rec.removes {
            servers.remove(key);
        }
    }
    for &i in &rec.updates {
        let entry = &desired[i];
        let Some(servers) = doc.get_mut(SERVERS_KEY).and_then(Item::as_table_mut) else {
            return unprovable(
                "the config's mcp_servers section disappeared while topos was editing it",
            );
        };
        // Keep the replaced table's document position so the file doesn't reorder, and
        // TRANSPLANT its decor — a user comment attached above `[mcp_servers.<key>]` travels
        // with the table, so an update must carry it onto the replacement instead of dropping
        // it with the old table.
        let old = servers.get(&entry.key).and_then(Item::as_table);
        let position = old.and_then(Table::position);
        let decor = old.map(|t| t.decor().clone());
        let mut table = entry_table(entry);
        table.set_position(position);
        if let Some(decor) = decor {
            *table.decor_mut() = decor;
        }
        servers.insert(&entry.key, Item::Table(table));
    }
    if let Err(reason) = upsert_entries(&mut doc, desired, &rec.inserts) {
        return unprovable(reason);
    }
    let out_text = doc.to_string();
    if let Err(reason) = verify(text, &out_text, desired, &desired_fps, &rec) {
        return unprovable(reason);
    }
    outcome(EditPlan::Write(out_text.into_bytes()), rec, false)
}

/// Whether the input re-serializes byte-identical through `toml_edit` (the dispatcher's
/// byte-preservation precondition — see [`apply`](super::apply)). `toml_edit` normalizes what it
/// does not model — a BOM is dropped, CRLF line endings come back LF — so a file it cannot
/// reproduce byte-for-byte is one this driver must not rewrite.
#[must_use]
pub(crate) fn round_trips(text: &str) -> bool {
    text.parse::<DocumentMut>()
        .is_ok_and(|doc| doc.to_string() == text)
}

/// Read the surface without writing: `key → fingerprint` for every `topos-`-prefixed entry.
/// Reads accept any table-LIKE `mcp_servers` spelling (broader than the edit path — reading
/// cannot damage anything).
#[must_use]
pub fn observe(current: Option<&[u8]>) -> Observed {
    let unreadable = Observed {
        entries: BTreeMap::new(),
        parseable: false,
    };
    if effectively_absent(current) {
        return Observed {
            entries: BTreeMap::new(),
            parseable: true,
        };
    }
    let Ok(text) = std::str::from_utf8(current.unwrap_or_default()) else {
        return unreadable;
    };
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return unreadable;
    };
    let mut entries = BTreeMap::new();
    match doc.get(SERVERS_KEY) {
        None => {}
        Some(item) => match item.as_table_like() {
            None => return unreadable,
            Some(servers) => {
                for (key, child) in servers.iter() {
                    if key.starts_with(MANAGED_KEY_PREFIX) {
                        entries.insert(key.to_owned(), fingerprint_value(&item_to_value(child)));
                    }
                }
            }
        },
    }
    Observed {
        entries,
        parseable: true,
    }
}

/// Every entry under the servers table — foreign ones included (see
/// [`super::observe_entries`]). `selector` overrides `mcp_servers` with a `.`-separated table
/// path; a `*` component has no meaning in this dialect and answers UNREADABLE rather than
/// guessing at one.
#[must_use]
pub fn observe_entries(
    current: Option<&[u8]>,
    selector: Option<&str>,
) -> Option<Vec<super::SeenEntry>> {
    // The selector is judged FIRST: one this dialect cannot mean is unreadable whatever the file
    // holds — an empty answer would read as "nothing is there".
    let path: Vec<&str> = selector.map_or_else(|| vec![SERVERS_KEY], |s| s.split('.').collect());
    if path.iter().any(|c| *c == "*" || c.is_empty()) {
        return None;
    }
    if effectively_absent(current) {
        return Some(Vec::new());
    }
    let text = std::str::from_utf8(current.unwrap_or_default()).ok()?;
    let doc = text.parse::<DocumentMut>().ok()?;
    let mut table: &dyn toml_edit::TableLike = doc.as_table();
    for (i, key) in path.iter().enumerate() {
        let Some(item) = table.get(key) else {
            return Some(Vec::new()); // the path does not exist: nothing is there
        };
        let Some(child) = item.as_table_like() else {
            return None; // present but not a table: this file is not readable in this shape
        };
        if i + 1 == path.len() {
            return Some(
                child
                    .iter()
                    .map(|(name, value)| super::SeenEntry {
                        name: name.to_owned(),
                        address: super::entry_address(&item_to_value(value)),
                    })
                    .collect(),
            );
        }
        table = child;
    }
    Some(Vec::new())
}

/// Whether the file holds content beyond topos's own `[mcp_servers.topos-*]` tables. The check
/// is the driver's own scrub: remove every managed-looking child (and the emptied parent) and
/// ask whether anything still renders. A comment attached to one of OUR tables travels with it —
/// exactly as the driver's removal moves it — and everything else survives the scrub and
/// answers `true`. Fail-closed: unparseable answers `true` (the caller's whole-file-ownership
/// reasoning fails TOWARD keeping the file).
#[must_use]
pub fn holds_unmanaged(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return true;
    };
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return true;
    };
    match doc.get_mut(SERVERS_KEY) {
        None => {}
        Some(item) => {
            let Some(servers) = item.as_table_mut() else {
                return true; // a spelling topos never writes
            };
            let managed: Vec<String> = servers
                .iter()
                .filter(|(k, _)| k.starts_with(MANAGED_KEY_PREFIX))
                .map(|(k, _)| k.to_owned())
                .collect();
            for key in &managed {
                servers.remove(key);
            }
            if servers.is_empty() {
                doc.remove(SERVERS_KEY);
            }
        }
    }
    !doc.to_string().trim().is_empty()
}

// ---------------------------------------------------------------------------------------------
// Reading + rendering.
// ---------------------------------------------------------------------------------------------

/// Every managed-looking entry under a STANDARD `mcp_servers` table. `Err` when the key exists
/// in a spelling the edit path must not rewrite.
fn found_entries(doc: &DocumentMut) -> Result<BTreeMap<String, FoundEntry>, String> {
    let mut found = BTreeMap::new();
    match doc.get(SERVERS_KEY) {
        None => {}
        Some(item) => match item.as_table() {
            None => {
                return Err(
                    "mcp_servers exists but is not a standard TOML table; refusing to rewrite its spelling"
                        .to_owned(),
                );
            }
            Some(servers) => {
                for (key, child) in servers.iter() {
                    if key.starts_with(MANAGED_KEY_PREFIX) {
                        found.insert(
                            key.to_owned(),
                            FoundEntry::Value(fingerprint_value(&item_to_value(child))),
                        );
                    }
                }
            }
        },
    }
    Ok(found)
}

/// Insert the desired entries at `indices` as `[mcp_servers.<key>]` tables, creating an
/// IMPLICIT `mcp_servers` parent (no bare header of its own) when absent.
fn upsert_entries(
    doc: &mut DocumentMut,
    desired: &[McpEntry],
    indices: &[usize],
) -> Result<(), String> {
    if indices.is_empty() {
        return Ok(());
    }
    if doc.get(SERVERS_KEY).is_none() {
        let mut parent = Table::new();
        parent.set_implicit(true);
        doc.insert(SERVERS_KEY, Item::Table(parent));
    }
    let servers = doc
        .get_mut(SERVERS_KEY)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| "mcp_servers is not a standard TOML table".to_owned())?;
    for &i in indices {
        let entry = &desired[i];
        servers.insert(&entry.key, Item::Table(entry_table(entry)));
    }
    Ok(())
}

/// The canonical entry table: ONLY `url` and, when headers exist, an inline `http_headers`
/// (name-sorted) — never any other key.
fn entry_table(entry: &McpEntry) -> Table {
    let mut table = Table::new();
    table.insert("url", toml_edit::value(entry.url.as_str()));
    if !entry.headers.is_empty() {
        let sorted: BTreeMap<&str, &str> = entry
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut headers = InlineTable::new();
        for (k, v) in sorted {
            headers.insert(k, v.into());
        }
        table.insert("http_headers", toml_edit::value(headers));
    }
    table
}

/// A TOML item as a structural [`Value`] for fingerprinting. Total: every TOML shape converts
/// (datetimes render as their display string), so any drifted entry still fingerprints
/// deterministically.
fn item_to_value(item: &Item) -> Value {
    match item {
        Item::None => Value::Null,
        Item::Value(v) => toml_value_to_value(v),
        Item::Table(t) => {
            let mut m = Map::new();
            for (k, child) in t.iter() {
                m.insert(k.to_owned(), item_to_value(child));
            }
            Value::Object(m)
        }
        Item::ArrayOfTables(arr) => Value::Array(
            arr.iter()
                .map(|t| item_to_value(&Item::Table(t.clone())))
                .collect(),
        ),
    }
}

fn toml_value_to_value(v: &toml_edit::Value) -> Value {
    use toml_edit::Value as T;
    match v {
        T::String(s) => Value::String(s.value().clone()),
        T::Integer(n) => Value::Number((*n.value()).into()),
        T::Float(f) => serde_json::Number::from_f64(*f.value())
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(f.value().to_string())),
        T::Boolean(b) => Value::Bool(*b.value()),
        T::Datetime(d) => Value::String(d.value().to_string()),
        T::Array(arr) => Value::Array(arr.iter().map(toml_value_to_value).collect()),
        T::InlineTable(t) => {
            let mut m = Map::new();
            for (k, child) in t.iter() {
                m.insert(k.to_owned(), toml_value_to_value(child));
            }
            Value::Object(m)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Verification.
// ---------------------------------------------------------------------------------------------

/// Post-edit verification: the output re-parses; the touched entries carry exactly the desired
/// values (and the removed keys are gone); and scrubbing the touched keys from BOTH documents
/// yields byte-identical renderings — everything outside our tables provably unchanged.
fn verify(
    input: &str,
    output: &str,
    desired: &[McpEntry],
    desired_fps: &[String],
    rec: &Reconcile,
) -> Result<(), String> {
    let surprise =
        |what: &str| format!("post-edit verification failed ({what}); refusing to write");
    let mut in_doc = input
        .parse::<DocumentMut>()
        .map_err(|_| surprise("input re-parse"))?;
    let mut out_doc = output
        .parse::<DocumentMut>()
        .map_err(|_| surprise("output does not re-parse"))?;

    if let Some(servers) = out_doc.get(SERVERS_KEY) {
        let servers = servers
            .as_table()
            .ok_or_else(|| surprise("output mcp_servers shape"))?;
        for &i in rec.inserts.iter().chain(rec.updates.iter()) {
            let key = desired[i].key.as_str();
            let placed = servers.get(key).ok_or_else(|| surprise("entry missing"))?;
            if fingerprint_value(&item_to_value(placed)) != desired_fps[i] {
                return Err(surprise("entry bytes are not the desired value"));
            }
        }
        for key in &rec.removes {
            if servers.get(key).is_some() {
                return Err(surprise("a removed entry is still present"));
            }
        }
    } else if !rec.inserts.is_empty() || !rec.updates.is_empty() {
        return Err(surprise("entry missing"));
    }

    // Scrub the touched keys from both sides and compare renderings byte-for-byte.
    let touched: Vec<&str> = rec
        .inserts
        .iter()
        .chain(rec.updates.iter())
        .map(|&i| desired[i].key.as_str())
        .chain(rec.removes.iter().map(String::as_str))
        .collect();
    for doc in [&mut in_doc, &mut out_doc] {
        if let Some(servers) = doc.get_mut(SERVERS_KEY).and_then(Item::as_table_mut) {
            for key in &touched {
                servers.remove(key);
            }
            if servers.is_empty() {
                doc.remove(SERVERS_KEY);
            }
        }
    }
    if in_doc.to_string() != out_doc.to_string() {
        return Err(surprise("content outside the managed tables changed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{entry, entry_with_headers};
    use super::*;

    fn fp(e: &McpEntry) -> String {
        fingerprint_value(&entry_value(McpDialect::CodexToml, e))
    }

    fn write_of(out: &ApplyOutcome) -> String {
        match &out.plan {
            EditPlan::Write(bytes) => String::from_utf8(bytes.clone()).unwrap(),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn fresh_file_creation_golden_bytes() {
        let out = apply(
            None,
            &[
                entry("topos-acme-linear", "https://mcp.example/linear"),
                entry_with_headers("topos-h", "https://h.example", &[("X-T", "v")]),
            ],
            &BTreeMap::new(),
        );
        assert!(out.created_file);
        assert_eq!(
            write_of(&out),
            "[mcp_servers.topos-acme-linear]\nurl = \"https://mcp.example/linear\"\n\n[mcp_servers.topos-h]\nurl = \"https://h.example\"\nhttp_headers = { X-T = \"v\" }\n"
        );
    }

    #[test]
    fn user_config_with_comments_round_trips_outside_our_tables() {
        const USER: &str = "# my codex config\nmodel = \"o5\"\n\n[mcp_servers.mine]\nurl = \"https://user.example\"\n\n[profiles.fast]\nmodel = \"o5-mini\"\n";
        let v1 = entry("topos-x", "https://one.example");
        let v2 = entry("topos-x", "https://two.example");

        let out = apply(
            Some(USER.as_bytes()),
            std::slice::from_ref(&v1),
            &BTreeMap::new(),
        );
        let after_add = write_of(&out);
        assert!(after_add.starts_with("# my codex config\nmodel = \"o5\"\n"));
        assert!(after_add.contains("[mcp_servers.mine]\nurl = \"https://user.example\"\n"));
        assert!(after_add.contains("[profiles.fast]\nmodel = \"o5-mini\"\n"));
        assert!(after_add.contains("[mcp_servers.topos-x]\nurl = \"https://one.example\"\n"));
        let ledger1: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();

        // Idempotent re-apply.
        let again = apply(
            Some(after_add.as_bytes()),
            std::slice::from_ref(&v1),
            &ledger1,
        );
        assert_eq!(again.plan, EditPlan::Leave);
        assert_eq!(
            again.states,
            vec![("topos-x".to_owned(), EntryState::Current)]
        );

        // Update in place — user bytes byte-identical.
        let out = apply(
            Some(after_add.as_bytes()),
            std::slice::from_ref(&v2),
            &ledger1,
        );
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Updated)]
        );
        let after_update = write_of(&out);
        assert!(after_update.contains("url = \"https://two.example\""));
        assert!(after_update.starts_with("# my codex config\nmodel = \"o5\"\n"));
        assert!(after_update.contains("[mcp_servers.mine]\nurl = \"https://user.example\"\n"));
        let ledger2: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();

        // Remove — the file returns EXACTLY to the user-only original.
        let out = apply(Some(after_update.as_bytes()), &[], &ledger2);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Removed)]
        );
        assert_eq!(write_of(&out), USER);
    }

    #[test]
    fn drift_and_foreign_are_never_touched() {
        const FILE: &str = "[mcp_servers.topos-x]\nurl = \"https://hand-edited\"\n";
        let placed = entry("topos-x", "https://placed");
        let prior: BTreeMap<String, String> =
            [("topos-x".to_owned(), fp(&placed))].into_iter().collect();

        // Drifted: prior mismatched → Leave, ledger keeps the prior fingerprint.
        let out = apply(
            Some(FILE.as_bytes()),
            &[entry("topos-x", "https://newer")],
            &prior,
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
        assert_eq!(out.fingerprints, vec![("topos-x".to_owned(), fp(&placed))]);
        // Removal skips it too.
        let out = apply(Some(FILE.as_bytes()), &[], &prior);
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );

        // Foreign: no prior record at all → untouched, never in the ledger.
        let out = apply(
            Some(FILE.as_bytes()),
            &[entry("topos-x", "https://newer")],
            &BTreeMap::new(),
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Foreign)]
        );
        assert!(out.fingerprints.is_empty());
    }

    /// A user comment attached directly above a topos-managed table is the user's byte: an
    /// UPDATE transplants it onto the replacement table instead of destroying it. (Removal-side
    /// loss is inherent — the comment is attached to a table that is leaving.)
    #[test]
    fn an_update_transplants_the_users_comment_above_our_table() {
        const FILE: &str = "model = \"o5\"\n\n# team linear server — ask #tools before edits\n[mcp_servers.topos-x]\nurl = \"https://one.example\"\n";
        let v1 = entry("topos-x", "https://one.example");
        let v2 = entry("topos-x", "https://two.example");
        let prior: BTreeMap<String, String> =
            [("topos-x".to_owned(), fp(&v1))].into_iter().collect();
        let out = apply(Some(FILE.as_bytes()), std::slice::from_ref(&v2), &prior);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Updated)]
        );
        let text = write_of(&out);
        assert!(
            text.contains("# team linear server — ask #tools before edits\n[mcp_servers.topos-x]"),
            "the comment rides the update: {text}"
        );
        assert!(text.contains("url = \"https://two.example\""), "{text}");
        assert!(!text.contains("one.example"), "{text}");
    }

    #[test]
    fn fingerprint_survives_reflow_but_not_value_change() {
        let placed = entry_with_headers("topos-x", "https://u", &[("A", "1")]);
        let prior: BTreeMap<String, String> =
            [("topos-x".to_owned(), fp(&placed))].into_iter().collect();
        // The same entry hand-reflowed (key order flipped, spacing changed) → still Current.
        const REFLOWED: &str =
            "[mcp_servers.topos-x]\nhttp_headers = {A=\"1\"}\nurl =    \"https://u\"\n";
        let out = apply(Some(REFLOWED.as_bytes()), &[placed], &prior);
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Current)]
        );
    }

    #[test]
    fn unprovable_shapes_fail_closed() {
        let e = [entry("topos-a", "https://a")];
        let none = BTreeMap::new();
        // Not TOML at all.
        let out = apply(Some(b"= not toml ="), &e, &none);
        assert!(matches!(out.plan, EditPlan::Unprovable(_)));
        // mcp_servers as an inline table — a spelling we must not rewrite.
        let out = apply(Some(b"mcp_servers = { x = { url = \"u\" } }\n"), &e, &none);
        let EditPlan::Unprovable(reason) = &out.plan else {
            panic!("expected Unprovable")
        };
        assert!(reason.contains("standard TOML table"), "{reason}");
        // Non-UTF-8.
        let out = apply(Some(&[0xff]), &e, &none);
        assert!(matches!(out.plan, EditPlan::Unprovable(_)));
    }

    #[test]
    fn observe_reads_broadly_and_never_writes() {
        let o = observe(None);
        assert!(o.parseable && o.entries.is_empty());
        let placed = entry("topos-x", "https://u");
        let text = write_of(&apply(
            None,
            std::slice::from_ref(&placed),
            &BTreeMap::new(),
        ));
        let o = observe(Some(text.as_bytes()));
        assert!(o.parseable);
        assert_eq!(o.entries.get("topos-x"), Some(&fp(&placed)));
        // The inline spelling is READABLE (observe is broader than the edit path).
        let o = observe(Some(b"mcp_servers = { topos-y = { url = \"u\" } }\n"));
        assert!(o.parseable);
        assert!(o.entries.contains_key("topos-y"));
        // Garbage is not.
        let o = observe(Some(b"= nope"));
        assert!(!o.parseable);
    }
}
