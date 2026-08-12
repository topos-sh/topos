//! `topos --schema` — the machine-readable contract for the `--json` surface, printed as ONE
//! document.
//!
//! The bytes are the COMMITTED `contracts/schemas/*.schema.json` artifacts, embedded with
//! `include_str!`. That is the whole trick: `cargo xtask gen-schema --check` already fails CI on
//! any drift between those files and the `topos-types` shapes this binary serializes, so embedding
//! the committed files makes the printed contract current BY CONSTRUCTION — no second generator,
//! no schema-of-schemas, and no runtime `schemars` in the client (which `check-arch` bans outright).
//!
//! The one deliberate hole: `self-update`'s outcome has no committed schema (an ad-hoc maintenance
//! payload — the decision is recorded where the type is declared), so it is absent here too rather
//! than invented.
//!
//! An agent that reads this document knows every `data` shape a verb can answer with, plus the
//! envelope, receipt, error, and next-action shapes that wrap them, without a network call.

/// Every committed schema, keyed by its file stem (`add-data` ⇒ `add-data.schema.json`) — the same
/// name `cargo xtask gen-schema` writes the file under. Sorted, so the printed document is
/// deterministic. A test proves this list IS the committed directory, so a new schema that nobody
/// embeds here fails the suite rather than quietly missing from the contract.
const SCHEMAS: &[(&str, &str)] = &[
    (
        "add-data",
        include_str!("../../../contracts/schemas/add-data.schema.json"),
    ),
    (
        "add-describe-data",
        include_str!("../../../contracts/schemas/add-describe-data.schema.json"),
    ),
    (
        "auth-status-data",
        include_str!("../../../contracts/schemas/auth-status-data.schema.json"),
    ),
    (
        "device-auth-poll-request",
        include_str!("../../../contracts/schemas/device-auth-poll-request.schema.json"),
    ),
    (
        "device-auth-poll-response",
        include_str!("../../../contracts/schemas/device-auth-poll-response.schema.json"),
    ),
    (
        "device-auth-start-request",
        include_str!("../../../contracts/schemas/device-auth-start-request.schema.json"),
    ),
    (
        "device-auth-start-response",
        include_str!("../../../contracts/schemas/device-auth-start-response.schema.json"),
    ),
    (
        "diff-data",
        include_str!("../../../contracts/schemas/diff-data.schema.json"),
    ),
    (
        "fmt-data",
        include_str!("../../../contracts/schemas/fmt-data.schema.json"),
    ),
    (
        "init-data",
        include_str!("../../../contracts/schemas/init-data.schema.json"),
    ),
    (
        "invitation-data",
        include_str!("../../../contracts/schemas/invitation-data.schema.json"),
    ),
    (
        "invite-describe-data",
        include_str!("../../../contracts/schemas/invite-describe-data.schema.json"),
    ),
    (
        "invite-read-data",
        include_str!("../../../contracts/schemas/invite-read-data.schema.json"),
    ),
    (
        "json-envelope",
        include_str!("../../../contracts/schemas/json-envelope.schema.json"),
    ),
    (
        "keep-as-yours-data",
        include_str!("../../../contracts/schemas/keep-as-yours-data.schema.json"),
    ),
    (
        "list-data",
        include_str!("../../../contracts/schemas/list-data.schema.json"),
    ),
    (
        "log-data",
        include_str!("../../../contracts/schemas/log-data.schema.json"),
    ),
    (
        "login-connect-request",
        include_str!("../../../contracts/schemas/login-connect-request.schema.json"),
    ),
    (
        "login-connect-response",
        include_str!("../../../contracts/schemas/login-connect-response.schema.json"),
    ),
    (
        "login-data",
        include_str!("../../../contracts/schemas/login-data.schema.json"),
    ),
    (
        "logout-data",
        include_str!("../../../contracts/schemas/logout-data.schema.json"),
    ),
    (
        "next-action",
        include_str!("../../../contracts/schemas/next-action.schema.json"),
    ),
    (
        "notice-ack-request",
        include_str!("../../../contracts/schemas/notice-ack-request.schema.json"),
    ),
    (
        "persisted-conflict",
        include_str!("../../../contracts/schemas/persisted-conflict.schema.json"),
    ),
    (
        "persisted-lock",
        include_str!("../../../contracts/schemas/persisted-lock.schema.json"),
    ),
    (
        "persisted-map",
        include_str!("../../../contracts/schemas/persisted-map.schema.json"),
    ),
    (
        "persisted-op",
        include_str!("../../../contracts/schemas/persisted-op.schema.json"),
    ),
    (
        "persisted-sync",
        include_str!("../../../contracts/schemas/persisted-sync.schema.json"),
    ),
    (
        "propose-data",
        include_str!("../../../contracts/schemas/propose-data.schema.json"),
    ),
    (
        "protect-data",
        include_str!("../../../contracts/schemas/protect-data.schema.json"),
    ),
    (
        "protection-set-request",
        include_str!("../../../contracts/schemas/protection-set-request.schema.json"),
    ),
    (
        "publish-data",
        include_str!("../../../contracts/schemas/publish-data.schema.json"),
    ),
    (
        "publish-describe-data",
        include_str!("../../../contracts/schemas/publish-describe-data.schema.json"),
    ),
    (
        "pull-data",
        include_str!("../../../contracts/schemas/pull-data.schema.json"),
    ),
    (
        "receipt",
        include_str!("../../../contracts/schemas/receipt.schema.json"),
    ),
    (
        "remove-data",
        include_str!("../../../contracts/schemas/remove-data.schema.json"),
    ),
    (
        "reset-data",
        include_str!("../../../contracts/schemas/reset-data.schema.json"),
    ),
    (
        "revert-data",
        include_str!("../../../contracts/schemas/revert-data.schema.json"),
    ),
    (
        "revert-describe-data",
        include_str!("../../../contracts/schemas/revert-describe-data.schema.json"),
    ),
    (
        "review-data",
        include_str!("../../../contracts/schemas/review-data.schema.json"),
    ),
    (
        "review-describe-data",
        include_str!("../../../contracts/schemas/review-describe-data.schema.json"),
    ),
    (
        "review-index-data",
        include_str!("../../../contracts/schemas/review-index-data.schema.json"),
    ),
    (
        "status-data",
        include_str!("../../../contracts/schemas/status-data.schema.json"),
    ),
    (
        "trigger-report",
        include_str!("../../../contracts/schemas/trigger-report.schema.json"),
    ),
    (
        "uninstall-data",
        include_str!("../../../contracts/schemas/uninstall-data.schema.json"),
    ),
    (
        "uninstall-describe-data",
        include_str!("../../../contracts/schemas/uninstall-describe-data.schema.json"),
    ),
    (
        "wire-applied-report",
        include_str!("../../../contracts/schemas/wire-applied-report.schema.json"),
    ),
    (
        "wire-channel-index",
        include_str!("../../../contracts/schemas/wire-channel-index.schema.json"),
    ),
    (
        "wire-current-record",
        include_str!("../../../contracts/schemas/wire-current-record.schema.json"),
    ),
    (
        "wire-delivery",
        include_str!("../../../contracts/schemas/wire-delivery.schema.json"),
    ),
    (
        "wire-error",
        include_str!("../../../contracts/schemas/wire-error.schema.json"),
    ),
    (
        "wire-me",
        include_str!("../../../contracts/schemas/wire-me.schema.json"),
    ),
    (
        "wire-proposal-index",
        include_str!("../../../contracts/schemas/wire-proposal-index.schema.json"),
    ),
    (
        "wire-protocol-card",
        include_str!("../../../contracts/schemas/wire-protocol-card.schema.json"),
    ),
    (
        "wire-skill-log",
        include_str!("../../../contracts/schemas/wire-skill-log.schema.json"),
    ),
];

/// The ONE document `topos --schema` prints: `{"schemas": {"<file-stem>": <schema>, …}}`, pretty,
/// newline-terminated.
///
/// # Panics
/// Only if a committed schema stopped being JSON — the files are generated by `xtask` and
/// drift-gated in CI, so a malformed one is a broken build, never a runtime condition.
#[must_use]
pub(crate) fn schema_doc() -> String {
    let mut schemas = serde_json::Map::with_capacity(SCHEMAS.len());
    for (name, text) in SCHEMAS {
        let value: serde_json::Value = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("committed schema {name}.schema.json is not JSON: {e}"));
        schemas.insert((*name).to_owned(), value);
    }
    let doc = serde_json::json!({ "schemas": serde_json::Value::Object(schemas) });
    let mut out = serde_json::to_string_pretty(&doc).expect("a JSON document always re-serializes");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded list IS the committed directory — a schema added to `xtask`'s list and
    /// committed, but never embedded here, would silently vanish from the printed contract.
    #[test]
    fn the_embedded_list_is_the_committed_schema_directory() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/schemas");
        let mut committed: Vec<String> = std::fs::read_dir(&dir)
            .expect("contracts/schemas is readable")
            .map(|e| e.expect("a readable dir entry").file_name())
            .filter_map(|n| n.to_str().map(str::to_owned))
            .filter_map(|n| n.strip_suffix(".schema.json").map(str::to_owned))
            .collect();
        committed.sort();
        let embedded: Vec<String> = SCHEMAS.iter().map(|(n, _)| (*n).to_owned()).collect();
        assert_eq!(
            embedded, committed,
            "`topos --schema` must embed exactly the committed schemas, sorted"
        );
    }

    #[test]
    fn every_embedded_schema_parses_and_the_document_carries_them_all() {
        let doc: serde_json::Value =
            serde_json::from_str(&schema_doc()).expect("the printed document is JSON");
        let schemas = doc["schemas"]
            .as_object()
            .expect("the document has one `schemas` object");
        assert_eq!(schemas.len(), SCHEMAS.len());
        for (name, _) in SCHEMAS {
            let schema = schemas
                .get(*name)
                .unwrap_or_else(|| panic!("{name} is in the document"));
            assert!(
                schema.is_object(),
                "{name} deserialized to a JSON-Schema object"
            );
        }
        // The three shapes this document gained when their types moved into the contract crate.
        for name in [
            "auth-status-data",
            "uninstall-describe-data",
            "uninstall-data",
        ] {
            assert!(schemas.contains_key(name), "{name} is contracted");
        }
        // The deliberate hole: `self-update`'s ad-hoc outcome has no committed schema, so no key
        // pretends to be one.
        assert!(!schemas.contains_key("self-update-data"));
        assert!(schema_doc().ends_with("}\n"), "pretty + newline-terminated");
    }
}
