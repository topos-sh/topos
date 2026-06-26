//! The NEGATIVE half of the contract oracle: prove the generated JSON-Schemas actually REJECT
//! malformed instances (a wrong-length hash, `alg: "none"`, a bad `schema_version`, an unknown
//! field-shape) — not just that valid ones round-trip. Also proves serde output validates against the
//! schemars schema (they are generated independently from the same types; this catches divergence).
//!
//! Schemas are generated in-process via `schemars`; the `gen-schema --check` drift gate already
//! guarantees the committed files are byte-identical to these, so validating here covers both.

use jsonschema::Validator;
use schemars::schema_for;
use serde_json::{Value, json};
use topos_types::{
    CurrentRecord, Generation, PointerScope, Receipt, Signature, SignatureAlg, SignedCurrentRecord,
    TerminalOutcome,
};

fn validator_for<T: schemars::JsonSchema>() -> Validator {
    let schema = serde_json::to_value(schema_for!(T)).expect("schema serializes");
    jsonschema::validator_for(&schema).expect("generated schema compiles")
}

fn good_pointer() -> SignedCurrentRecord {
    SignedCurrentRecord {
        schema_version: 1,
        scope: PointerScope {
            workspace_id: "w_1".into(),
            skill_id: "s_1".into(),
        },
        record: CurrentRecord {
            version_id: "a".repeat(64),
            generation: Generation { epoch: 1, seq: 1 },
        },
        signature: Signature {
            alg: SignatureAlg::Ed25519,
            key_id: "pk_1".into(),
            value: "A".repeat(86),
        },
    }
}

#[test]
fn signed_pointer_accepts_valid_and_rejects_malformed() {
    let v = validator_for::<SignedCurrentRecord>();
    let good: Value = serde_json::to_value(good_pointer()).unwrap();
    assert!(
        v.is_valid(&good),
        "a serialized valid pointer must validate against its own schema"
    );

    // alg outside the closed set must fail closed.
    let mut bad = good.clone();
    bad["signature"]["alg"] = json!("none");
    assert!(
        !v.is_valid(&bad),
        "alg:\"none\" must be rejected (closed enum)"
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

    // signature value must be exactly 86 base64url chars.
    let mut bad = good.clone();
    bad["signature"]["value"] = json!("A".repeat(80));
    assert!(
        !v.is_valid(&bad),
        "a short signature value must be rejected (length)"
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
        key_id: Some("pk_1".into()),
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
