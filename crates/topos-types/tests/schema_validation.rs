//! The NEGATIVE half of the contract oracle: prove the generated JSON-Schemas actually REJECT
//! malformed instances (a wrong-length hash, a bad `schema_version`, an unknown field-shape) — not
//! just that valid ones round-trip. Also proves serde output validates against the schemars schema
//! (they are generated independently from the same types; this catches divergence).
//!
//! Schemas are generated in-process via `schemars`; the `gen-schema --check` drift gate already
//! guarantees the committed files are byte-identical to these, so validating here covers both.

use jsonschema::Validator;
use schemars::schema_for;
use serde_json::{Value, json};
use topos_types::requests::{WireDelivery, WireDeliverySkill, WireNotice, WireVia};
use topos_types::{CurrentRecord, PointerScope, Receipt, TerminalOutcome, WireCurrentRecord};

fn validator_for<T: schemars::JsonSchema>() -> Validator {
    let schema = serde_json::to_value(schema_for!(T)).expect("schema serializes");
    jsonschema::validator_for(&schema).expect("generated schema compiles")
}

fn good_pointer() -> WireCurrentRecord {
    WireCurrentRecord {
        schema_version: 1,
        scope: PointerScope {
            workspace_id: "w_1".into(),
            skill_id: "s_1".into(),
        },
        record: CurrentRecord {
            version_id: "a".repeat(64),
            generation: 1,
        },
    }
}

#[test]
fn current_pointer_accepts_valid_and_rejects_malformed() {
    let v = validator_for::<WireCurrentRecord>();
    let good: Value = serde_json::to_value(good_pointer()).unwrap();
    assert!(
        v.is_valid(&good),
        "a serialized valid pointer must validate against its own schema"
    );

    // version_id must be 64 lowercase hex.
    let mut bad = good.clone();
    bad["record"]["version_id"] = json!("deadbeef");
    assert!(
        !v.is_valid(&bad),
        "a short version_id must be rejected (pattern)"
    );
    let mut bad = good.clone();
    bad["record"]["version_id"] = json!("A".repeat(64)); // uppercase
    assert!(
        !v.is_valid(&bad),
        "an uppercase version_id must be rejected (lowercase-hex pattern)"
    );

    // schema_version is pinned const 1.
    let mut bad = good.clone();
    bad["schema_version"] = json!(2);
    assert!(
        !v.is_valid(&bad),
        "schema_version != 1 must be rejected (const)"
    );
}

fn good_delivery() -> WireDelivery {
    WireDelivery {
        schema_version: 1,
        workspace_id: "w_1".into(),
        skills: vec![WireDeliverySkill {
            skill_id: "s_1".into(),
            name: "one".into(),
            kind: "skill".into(),
            display_name: None,
            protection: "open".into(),
            version_id: "a".repeat(64),
            bundle_digest: "b".repeat(64),
            generation: 1,
            updated_at: 1,
            via: WireVia {
                channels: vec!["everyone".into()],
                direct: false,
            },
        }],
        detached: vec![],
        excluded: Vec::new(),
        notices: vec![WireNotice {
            id: "n_1".into(),
            kind: "verdict".into(),
            skill_id: Some("s_1".into()),
            skill_name: Some("one".into()),
            version_id: None,
            actor: None,
            outcome: Some("approve".into()),
            reason: Some("ok".into()),
            message: None,
            created_at: "2026-06-25T00:00:00Z".into(),
        }],
        proposals_awaiting: 1,
        staleness_window_ms: 604_800_000,
        link_status: "active".into(),
    }
}

#[test]
fn delivery_accepts_valid_and_rejects_bad_version_and_schema_version() {
    let v = validator_for::<WireDelivery>();
    let good: Value = serde_json::to_value(good_delivery()).unwrap();
    assert!(
        v.is_valid(&good),
        "a serialized valid delivery must validate against its own schema"
    );

    // A skill's version_id must be 64 lowercase hex — a 63-char id is rejected (pattern).
    let mut bad = good.clone();
    bad["skills"][0]["version_id"] = json!("a".repeat(63));
    assert!(
        !v.is_valid(&bad),
        "a 63-char skill version_id must be rejected (pattern)"
    );

    // schema_version is pinned const 1.
    let mut bad = good.clone();
    bad["schema_version"] = json!(2);
    assert!(
        !v.is_valid(&bad),
        "schema_version != 1 must be rejected (const)"
    );

    // The device↔workspace link status is REQUIRED on every delivery (a clean wire break).
    let mut bad = good.clone();
    bad.as_object_mut().unwrap().remove("link_status");
    assert!(
        !v.is_valid(&bad),
        "a delivery without link_status must be rejected (required)"
    );

    // An unknown extra field is ACCEPTED — schemars sets no `additionalProperties: false`, so the wire
    // stays additively tolerant (a newer plane may add fields an older client ignores).
    let mut extra = good.clone();
    extra["unknown_future_field"] = json!("tolerated");
    assert!(
        v.is_valid(&extra),
        "an unknown extra field must be tolerated (additive)"
    );
}

#[test]
fn receipt_accepts_valid_and_rejects_malformed() {
    let v = validator_for::<Receipt>();
    let good = serde_json::to_value(Receipt {
        schema_version: 1,
        op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".into(),
        command: "publish".into(),
        outcome: TerminalOutcome::Ok,
        workspace_id: "w_1".into(),
        skill_id: Some("s_1".into()),
        version_id: Some("b".repeat(64)),
        bundle_digest: Some("c".repeat(64)),
        expected_generation: None,
        current_generation: None,
        created_at: "2026-06-25T00:00:00Z".into(),
        details: None,
    })
    .unwrap();
    assert!(
        v.is_valid(&good),
        "a serialized valid receipt must validate"
    );

    // An unknown outcome is not in the closed set.
    let mut bad = good.clone();
    bad["outcome"] = json!("WAT");
    assert!(
        !v.is_valid(&bad),
        "an unknown TerminalOutcome must be rejected"
    );

    // A malformed bundle_digest.
    let mut bad = good.clone();
    bad["bundle_digest"] = json!("nothex");
    assert!(
        !v.is_valid(&bad),
        "a non-hex bundle_digest must be rejected"
    );
}
