//! The one-shot v1→v2 migration on the sweep: an old `[bundles]` file is rewritten WHOLE
//! before the plans load — so an upgraded machine heals on its first run instead of silently
//! delivering nothing — and `--frozen` refuses instead of writing.

use std::sync::{Arc, Mutex};

use super::rig::*;
use crate::ops;

#[test]
fn a_v1_global_file_is_rewritten_before_the_plans_load() {
    let rig = Rig::new("v1mig");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(None);
    rig.write_global(&format!(
        "[bundles]\n\
         \"{HOST}/{WS_NAME}\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));

    let out = sweep(&ctx, &plane, &dir);

    let path = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("schema = 1"), "{text}");
    assert!(text.contains("[workspaces]"), "{text}");
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}\" = \"latest\"")),
        "{text}"
    );
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/deploy\" = \"latest\"")),
        "{text}"
    );
    assert!(!text.contains("[bundles]"), "{text}");
    assert!(
        out.disclosures
            .iter()
            .any(|m| m.code.as_deref() == Some("MANIFEST_MIGRATED") && m.text.contains("migrated")),
        "{:?}",
        out.disclosures
    );

    // The second sweep finds a v2 file and says nothing about migration.
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.disclosures
            .iter()
            .any(|m| m.code.as_deref() == Some("MANIFEST_MIGRATED")),
        "{:?}",
        out.disclosures
    );
}

#[test]
fn frozen_refuses_a_v1_file_and_writes_nothing() {
    let rig = Rig::new("v1frozen");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(None);
    let v1 = format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n");
    rig.write_global(&v1);

    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            lock: ops::LockMode::Frozen,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap_err();
    let detail = err.detail();
    assert!(detail.contains("v1"), "{detail}");
    assert!(detail.contains("--frozen"), "{detail}");

    let path = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), v1, "unchanged");
}
