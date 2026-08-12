//! The durable kind marker: classification survives a lost custody, fails closed without evidence,
//! a targeted go-back converges configs, and the applied report never claims an empty-map skill.

use std::collections::BTreeSet;

use topos_types::results::TargetOutcome;

use crate::ctx::Ctx;
use crate::ops;
use crate::plane::{FetchedVersion, KnownCurrent, PlaneError, PlaneSource, PointerFetch};
use crate::sessions::{self, SESSION_ACTIVE, Session};

use super::rig::*;

/// Deliver ONE mcp bundle at machine scope over the hermetic slugs, returning the plane + dir the
/// follow-up sweeps and targeted verbs reuse.
fn deliver_linear(rig: &Rig, v: &Version) -> (FakePlane, FakeDirectory) {
    let plane = FakePlane::new().with_version("s_linear", v);
    plane.serves(vec![delivered_mcp("s_linear", "linear", v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
    ));
    (plane, dir)
}

/// ITEM PAIR (kind fails durable): the store + empty map stand, the LEDGER is gone — a targeted
/// go-back must still classify the record as config-placed and materialize NO skill dirs. Before
/// the fix, classification hung on the custody alone: its loss let the skill planner run and
/// `server.json` landed in skill dirs.
#[test]
fn a_lost_ledger_never_lets_a_targeted_go_back_materialize_skill_dirs() {
    let rig = Rig::new("lost-custody");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // The failure shape: the custody is gone, and so is the delivery cache — the MARKER alone must
    // answer.
    std::fs::remove_file(rig.layout().config_custody_path()).unwrap();
    std::fs::remove_file(rig.layout().sync_status_path()).unwrap();

    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the marker classifies the record; the go-back applies store-only");
    assert_eq!(out.data.skills.len(), 1);

    // NOTHING materialized into skill dirs, and the map still records zero placements.
    assert!(
        !rig.work.0.join("skills").exists(),
        "no harness skill dir was created"
    );
    assert!(!rig.home.0.join(".agents").exists(), "no shared-dir copy");
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert!(map.placements.is_empty(), "{map:?}");
}

/// ITEM PAIR (fail closed): with the map EMPTY and every kind source gone — marker, cache, custody
/// — the targeted verb REFUSES with the typed placement error instead of guessing "skill" and
/// materializing. Before the fix the guess ran the planner.
#[test]
fn an_empty_map_with_no_kind_evidence_fails_closed_on_targeted_verbs() {
    let rig = Rig::new("no-evidence");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    std::fs::remove_file(rig.layout().config_custody_path()).unwrap();
    std::fs::remove_file(rig.layout().sync_status_path()).unwrap();
    std::fs::remove_file(rig.layout().published(&sid).kind).unwrap();

    let Err(err) = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    ) else {
        panic!("kind indeterminate over an empty map must refuse");
    };
    assert_eq!(err.code(), "PLACEMENT_UNSUPPORTED");
    assert!(err.detail().contains("kind"), "{}", err.detail());
    assert!(
        !rig.work.0.join("skills").exists(),
        "nothing materialized on the refusal"
    );
}

/// THE TWO-DOMAIN RACE, made single-process. A bundle's DIR custody (`map.json`) and its CONFIG
/// custody (`entries.json`) are written under DIFFERENT locks — the per-skill flock and the scope's
/// mcp lock — and a targeted verb legitimately holds the skill lock across a converge, so the two
/// can never be made to serialize against each other without deadlocking. Safety therefore comes
/// from the FILES, not the locks: one writer each.
///
/// This drives the interleave that proves it, in one process because the locks no longer share a
/// document: a targeted go-back reads dir custody (t0), a hook-fired quiet sweep converges config
/// custody in the window (t1), the go-back commits its dir custody from the t0 snapshot (t2), and
/// only then converges (t3). With both halves in ONE document, t2 writes back a snapshot taken
/// before t1 and silently drops the rows t1 committed — leaving a live entry with no record, which
/// the go-back's converge reads as a hand edit and refuses to touch: a permanent drift standoff
/// that no later sweep clears. With the halves in sibling files, t2 cannot reach the rows: the
/// entry stays provably topos's, current at v2, and is REPLACED with v1.
#[test]
fn a_go_back_racing_a_sweep_never_lands_in_a_drift_standoff() {
    let rig = Rig::new("standoff");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v1 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v1").as_bytes(),
    )]);
    let v2 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v2").as_bytes(),
    )]);
    let plane = FakePlane::new()
        .with_version("s_linear", &v1)
        .with_version("s_linear", &v2);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v1)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v1)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // The team moves to v2 and the sweep converges onto it.
    let mut served2 = delivered_mcp("s_linear", "linear", &v2);
    served2.generation = 2;
    plane.serves(vec![served2]);
    let mut entry2 = mcp_catalog_entry("s_linear", "linear", &v2);
    entry2.generation = 2;
    let dir2 = FakeDirectory {
        skills: vec![entry2],
        channels: Vec::new(),
    };
    sweep(&ctx, &plane, &dir2);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v2")
    );

    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let map_path = rig.layout().published(&sid).map;

    // t0 — the snapshot a targeted verb reads before it converges.
    let stale_map = crate::doc::read_map(&rig.fs, &map_path).unwrap().unwrap();

    // t1 — the quiet sweep lands in the window. The person had deleted the entry by hand, so this
    // run REWRITES it and commits fresh config custody.
    std::fs::write(&cursor, "{\n  \"mcpServers\": {}\n}\n").unwrap();
    sweep(&ctx, &plane, &dir2);
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v2"),
        "the interleaved sweep re-placed the entry"
    );

    // t2 — the targeted verb commits DIR custody from its pre-sweep snapshot. This is the write
    // that used to take the config rows with it.
    crate::doc::write_map(&rig.fs, &map_path, &stale_map).unwrap();
    assert!(
        !crate::config_custody::entries_of(&rig.fs, &rig.layout(), "s_linear").is_empty(),
        "a dir-custody commit must not disturb config custody — the two have different writers"
    );

    // t3 — the go-back converges. The entry is topos's own, current at v2, so it is REPLACED.
    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v1.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back applies");
    let text = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        text.contains("https://mcp.example/v1") && !text.contains("https://mcp.example/v2"),
        "the restored document must reach the config, not stand off against it: {text}"
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .expect("the go-back row");
    let cursor_state = row
        .harnesses
        .iter()
        .find(|h| h.agent == "cursor")
        .expect("a cursor state");
    assert_ne!(
        cursor_state.state,
        TargetOutcome::Drifted,
        "an entry topos itself wrote must never read as a hand edit: {cursor_state:?}"
    );
    assert!(cursor_state.state.wrote(), "{cursor_state:?}");
}

/// ITEM PAIR (go-back converges configs): a targeted `update <mcp>@<version>` must leave the
/// agent configs carrying the RESTORED document before it returns success. Before the fix only
/// the store/lock moved — the configs kept the newer URL until the next sweep.
#[test]
fn a_targeted_go_back_converges_the_configs_to_the_restored_document() {
    let rig = Rig::new("goback-converge");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v1 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v1").as_bytes(),
    )]);
    let v2 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v2").as_bytes(),
    )]);
    let plane = FakePlane::new()
        .with_version("s_linear", &v1)
        .with_version("s_linear", &v2);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v1)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v1)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v1")
    );

    // The team moves to v2 (generation 2 — a publish moved the pointer); the sweep converges the
    // configs onto it.
    let mut served2 = delivered_mcp("s_linear", "linear", &v2);
    served2.generation = 2;
    plane.serves(vec![served2]);
    let mut entry2 = mcp_catalog_entry("s_linear", "linear", &v2);
    entry2.generation = 2;
    let dir2 = FakeDirectory {
        skills: vec![entry2],
        channels: Vec::new(),
    };
    let out2 = sweep(&ctx, &plane, &dir2);
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v2"),
        "warnings: {:?} rows: {:?}",
        out2.warnings,
        out2.data
            .skills
            .iter()
            .map(|s| (&s.skill, &s.action))
            .collect::<Vec<_>>()
    );

    // The deliberate go-back: the configs carry v1's document again BEFORE the verb returns.
    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v1.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back applies");
    let text = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        text.contains("https://mcp.example/v1") && !text.contains("https://mcp.example/v2"),
        "the configs carry the restored document immediately: {text}"
    );
    // The row reports the per-agent outcomes of the converge it just ran.
    let row = &out.data.skills[0];
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into());
}

/// ITEM PAIR (targeted go-back honors narrowing): a bundle narrowed to ONE harness must stay in
/// that one harness through a targeted `update <mcp>@<version>` — the go-back's converge reaches
/// only the harnesses that already hold a custody entry here, so a harness the narrowing excluded
/// never gains one. Before the fix the targeted demand carried an EMPTY narrowing, read as ALL
/// harnesses: openclaw (detected, engaged, excluded by the row) gained an entry the sweep then had
/// to claw back.
#[test]
fn a_targeted_go_back_never_reaches_a_narrowing_excluded_harness() {
    let rig = Rig::new("goback-narrow");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0); // BOTH hermetic slugs detect; the narrowing admits one.
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let openclaw = rig.home.0.join(".openclaw/openclaw.json");
    assert!(
        rig.home.0.join(".cursor/mcp.json").exists() && !openclaw.exists(),
        "the sweep placed cursor alone"
    );

    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back applies");

    // The excluded harness stayed excluded — no config was born for it.
    assert!(
        !openclaw.exists(),
        "a narrowing-excluded harness must not gain an entry on a targeted go-back"
    );
    let text = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap();
    assert!(text.contains("https://mcp.example/linear"), "{text}");
    let row = &out.data.skills[0];
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor"].into(), "{row:?}");
}

/// A plane that serves ONE bundle's `current` pointer (the targeted-accept read) over the fake
/// version store — what `topos update <mcp-name>` dials when the pointer has advanced.
struct ServesCurrent {
    inner: FakePlane,
    record: topos_types::WireCurrentRecord,
}
impl PlaneSource for ServesCurrent {
    fn get_current(
        &self,
        skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        if skill_id == self.record.scope.skill_id {
            Ok(PointerFetch::Record(self.record.clone()))
        } else {
            Err(PlaneError::NotFound)
        }
    }
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        self.inner.fetch_version(skill_id, version_id)
    }
}

/// ITEM PAIR (targeted accept converges): `topos update <mcp-name>` must not report success while
/// every agent config still carries the previous document. The accept advances the store AND
/// converges this scope's configs before returning, threading the per-agent states onto the row —
/// the same converge the go-back runs. Before the fix the targeted path gave an mcp record an
/// empty placement plan and left the configs stale until the next sweep.
#[test]
fn a_targeted_accept_updates_the_configs_before_reporting_success() {
    let rig = Rig::new("accept-converge");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v1 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v1").as_bytes(),
    )]);
    let v2 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v2").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v1);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v1"),
        "the sweep placed v1"
    );

    // The pointer advanced to v2; the person runs the targeted accept.
    let serves = ServesCurrent {
        inner: FakePlane::new().with_version("s_linear", &v2),
        record: topos_types::WireCurrentRecord {
            schema_version: topos_types::WIRE_SCHEMA_VERSION,
            scope: topos_types::PointerScope {
                workspace_id: WS.to_owned(),
                skill_id: "s_linear".to_owned(),
            },
            record: topos_types::CurrentRecord {
                version_id: topos_core::digest::to_hex(&v2.id),
                generation: 2,
            },
        },
    };
    let follow = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let ctx2 = Ctx {
        progress: crate::progress::silent(),
        fs: &rig.fs,
        ids: &rig.ids,
        clock: &rig.clock,
        device_id: "d_test".into(),
        layout: rig.layout(),
        harness: &rig.harness,
        triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
        plane: &serves,
        follow: &follow,
        roots: Some(crate::ctx::AgentRoots {
            home: rig.home.0.clone(),
            cwd: Some(rig.work.0.clone()),
        }),
    };
    let out = ops::pull(
        &ctx2,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
            store: ops::StoreScope::Here,
        },
    )
    .expect("the accept applies");

    // The configs carry v2 BEFORE the verb returned — and the row reports the per-agent states.
    let text = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        text.contains("https://mcp.example/v2") && !text.contains("https://mcp.example/v1"),
        "the accept converged the configs: {text}"
    );
    let row = &out.data.skills[0];
    assert_eq!(row.action, topos_types::results::PullAction::FastForwarded);
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into(), "{row:?}");
}

/// ITEM PAIR (keyless targeted converge skips): with the LEDGER deleted, a targeted go-back has
/// no ownership record to reuse — it must write NOTHING and say so, leaving the heal to the next
/// sweep. Before the fix it minted a fresh `topos-local-linear` key and placed a DUPLICATE entry
/// beside the original `topos-eng-linear` one (now foreign and unremovable).
#[test]
fn a_deleted_ledger_makes_the_targeted_go_back_skip_with_a_warning() {
    let rig = Rig::new("goback-keyless");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let openclaw = rig.home.0.join(".openclaw/openclaw.json");
    let before = (
        std::fs::read(&cursor).unwrap(),
        std::fs::read(&openclaw).unwrap(),
    );

    // The failure shape: the ownership record is GONE (the kind marker still classifies).
    std::fs::remove_file(rig.layout().config_custody_path()).unwrap();

    // The converge the go-back hand-runs: zero writes, one honest warning.
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let (states, warnings) = crate::mcp_engine::converge_bundle_now(&ctx, &sid, "linear");
    assert!(states.is_empty(), "{states:?}");
    assert!(
        crate::message::legacy_lines(&warnings).iter().any(|w| {
            w.contains("MCP_OWNERSHIP_MISSING") && w.contains("The next 'topos update' restores it")
        }),
        "{warnings:?}"
    );

    // And through the whole verb: the configs are byte-identical — no duplicate key appeared.
    ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back still applies store-side");
    let after = (
        std::fs::read(&cursor).unwrap(),
        std::fs::read(&openclaw).unwrap(),
    );
    assert_eq!(
        before, after,
        "no config byte moved without an ownership record"
    );
    assert!(
        !String::from_utf8_lossy(&after.0).contains("topos-local"),
        "no duplicate locally-minted entry"
    );
}

/// ITEM PAIR (empty map ≠ mcp): a SKILL delivered at project scope with NO detected agent records
/// an empty placement map — it must NOT be reported to the fleet as held (there are no bytes
/// anywhere an agent reads). Before the fix the empty map rode the config-placed exemption and
/// the report claimed it current.
#[test]
fn a_scoped_out_skill_with_an_empty_map_is_not_reported_held() {
    let rig = Rig::new("scoped-out");
    rig.seed_session();
    // Deliberately NO detect dirs anywhere: the project plan has no agent to place for.
    let v = mk_version(&[("SKILL.md", b"# alpha\n")]);
    let plane = FakePlane::new().with_version("s_alpha", &v);
    let mut delivered = delivered_mcp("s_alpha", "alpha", &v);
    delivered.kind = "skill".into();
    plane.serves(vec![delivered]);
    let mut entry = mcp_catalog_entry("s_alpha", "alpha", &v);
    entry.kind = "skill".into();
    let dir = FakeDirectory {
        skills: vec![entry],
        channels: Vec::new(),
    };
    // Person scope demands nothing; the PROJECT manifest carries a feed row — illegal in a
    // project file, so that scope is FROZEN. The run drives the MACHINE scope (driving the
    // frozen project would refuse the run whole now), which syncs nothing: the delivered skill
    // has NO placement map anywhere, and the report must not claim it held.
    rig.write_global("[bundles]\n");
    std::fs::write(
        rig.work.0.join("topos.toml"),
        format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Machine,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();

    let reported = plane.reported.lock().unwrap().clone();
    assert!(
        !reported.iter().any(|(id, ..)| id == "s_alpha"),
        "an empty-map skill placed nowhere must not be reported held: {reported:?}"
    );
}

/// A delivered, store-only mcp bundle's bare `diff` REFUSES, naming what `diff` acts on.
///
/// It used to answer an empty diff whose endpoint was the held current. That was honest about the
/// working tree (there is none) and dishonest about everything a person reads it for: "No changes"
/// over a bundle the verb never looked at, printed identically whether or not the entry standing in
/// their agent's config had been hand-edited. The refusal names the kind and points at the verb
/// that does answer. (It also still never trips the "placement map has no placement" corruption
/// error a store-only record would otherwise reach.)
#[test]
fn a_bare_diff_of_a_config_placed_bundle_refuses_naming_the_kind() {
    let rig = Rig::new("bare-diff");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    let err = ops::diff(
        &ctx,
        "linear",
        None,
        ops::DiffBudget::resolve(None, true),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect_err("a config-placed bundle has no file diff");
    let detail = err.detail();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{detail}");
    assert!(
        detail.contains("'linear' is an MCP server bundle"),
        "{detail}"
    );
    assert!(detail.contains("`diff` compares"), "{detail}");
    assert!(detail.contains("`topos list linear`"), "{detail}");
    assert!(
        !detail.contains("placement map"),
        "never the corruption error: {detail}"
    );
}

/// **An offline sweep never WIDENS a dest-narrowed bundle.** A row's `dest` freezes which config
/// files it reaches; a run that cannot dial still heals those files from the store, and the
/// narrowing must survive the round-trip through the offline path — otherwise one lost network
/// call quietly fans a server out to every MCP-capable agent on the machine, with the person's
/// stated destinations overruled. Held with a feed row present too: the cached-delivery arm the
/// feed falls back to must not pick this bundle up behind its own row's back.
#[test]
fn an_offline_sweep_keeps_a_dest_narrowed_bundle_narrow() {
    let rig = Rig::new("offline-narrow");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/a").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_a", &v);
    plane.serves(vec![delivered_mcp("s_a", "alpha", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_a", "alpha", &v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let elsewhere = [
        rig.home.0.join(".openclaw/openclaw.json"),
        rig.home.0.join(".codex/config.toml"),
        rig.home.0.join(".hermes/config.yaml"),
    ];
    assert!(cursor.exists(), "the named file got the entry");
    for p in &elsewhere {
        assert!(!p.exists(), "a dest row reaches only what it names: {p:?}");
    }

    // The network dies, and the entry is lost locally: the sweep heals it from the store — into
    // the SAME one file.
    std::fs::remove_file(&cursor).unwrap();
    plane.serve_unreachable();
    sweep(&ctx, &plane, &dir);
    assert!(
        std::fs::read_to_string(&cursor).is_ok_and(|t| t.contains("https://mcp.example/a")),
        "healed offline"
    );
    for p in &elsewhere {
        assert!(!p.exists(), "offline widened the row into {p:?}");
    }
}

/// THE ADD DOOR CLOSES ITS OWN WINDOW. Kind classification for a WORKSPACE bundle reads the store's
/// durable marker — the row itself never spells a kind — so `add` must not leave a row standing
/// with no marker for a later verb to guess about. It does not: the reference add DELIVERS in the
/// same invocation, and the sync that lands the bytes lays the marker before `add` returns.
#[test]
fn a_workspace_mcp_add_leaves_the_marker_behind_before_it_returns() {
    let rig = Rig::new("add-marker");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let proj = Scratch::new("add-marker-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));

    ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/linear"),
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap();

    // The PROJECT store the add delivered into carries the kind, durably, right now.
    let layout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the add created the project store");
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let marker = std::fs::read_to_string(layout.published(&sid).kind).expect("kind.json");
    assert!(marker.contains("\"mcp\""), "{marker}");

    // Which is the fact every later arm resolves against: the classifier answers mcp with no
    // delivery cache and no custody consulted.
    let sctx = crate::ops::ctx_with_layout(&ctx, &layout);
    assert!(
        crate::bundle_kind::classify(&sctx, "s_linear", &[]).is_mcp(),
        "the marker is what a later `remove -a <agent>` resolves the dest vocabulary from"
    );
}

/// AND THE WINDOW CANNOT BE OPENED BY A DARK PLANE EITHER. The pair that closes it structurally:
/// a workspace row's kind comes from the CATALOG, and the catalog is what `add` needs before it
/// writes anything — so there is no ordering in which a row exists with no marker beside it.
///
/// With the DELIVERY dark the explicit row still syncs (it resolves against the catalog, not the
/// feed snapshot), so the marker lands anyway; with the catalog answering NO the add refuses and
/// writes no row at all. A never-swept mcp row with no marker is therefore unreachable, and no arm
/// can fall back to the skills vocabulary for one.
#[test]
fn an_mcp_row_never_stands_without_its_marker_even_when_the_feed_is_dark() {
    let rig = Rig::new("dark-feed-marker");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    // The FEED is unreachable for the whole invocation.
    plane.serve_unreachable();
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/linear"),
        true,
        true,
        &Default::default(),
        None,
    )
    .expect("an explicit row resolves against the catalog, not the feed");

    let manifest =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("linear"), "the row landed: {manifest}");
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let marker = std::fs::read_to_string(rig.layout().published(&sid).kind)
        .expect("and the marker landed with it");
    assert!(marker.contains("\"mcp\""), "{marker}");

    // The other half: with no catalog answer there is no row either, so "row without marker" has
    // no way to come about.
    let empty = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    ops::add_reference(
        &ctx,
        &connect(&plane, &empty),
        None,
        &format!("{HOST}/{WS_NAME}/absent"),
        true,
        true,
        &Default::default(),
        None,
    )
    .expect_err("no catalog answer, no row");
    let manifest =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!manifest.contains("absent"), "{manifest}");
}

/// A BARE NAME IS NOT AN IDENTITY. Two records in one store can answer to `linear` — here a
/// workspace MCP bundle and a local skill folder of the same name. Asking the store for "linear"
/// answers AMBIGUOUS, and a row that falls back from that ambiguity resolves its `-a` selector
/// against the SKILLS-folder vocabulary: the wrong dest table for a config-placed bundle. The row
/// is qualified, so the lookup must be too.
#[test]
fn a_qualified_row_resolves_its_own_record_when_the_name_is_shared() {
    let rig = Rig::new("shared-name");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);

    // Record ONE: a local skill folder that happens to be called `linear`.
    let local = rig.work.0.join("linear");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("SKILL.md"), b"# linear\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    crate::ops::add(&ctx, &local).expect("the local skill adopts");

    // Record TWO: the workspace's MCP bundle, same name, delivered by its qualified row.
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_mcp", &v);
    plane.serves(vec![delivered_mcp("s_mcp", "linear", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_mcp", "linear", &v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/linear\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    assert!(
        rig.home.0.join(".cursor/mcp.json").exists(),
        "the mcp bundle landed in a config file"
    );

    // The premise: TWO records in this one store answer to the name, so a bare-name lookup
    // resolves to neither and only the row's workspace can pick between them.
    let named = std::fs::read_dir(rig.layout().skills_dir())
        .unwrap()
        .filter_map(|e| {
            let sid = crate::id::SkillId::parse(&e.ok()?.file_name().to_string_lossy()).ok()?;
            crate::doc::read_doc::<topos_types::persisted::Lock>(
                &rig.fs,
                &rig.layout().published(&sid).lock,
            )
            .ok()
            .flatten()
            .filter(|l| l.name == "linear")
        })
        .count();
    assert_eq!(
        named, 2,
        "the test is meaningless unless both records answer to the name"
    );

    // Narrowing the QUALIFIED mcp row by `-a cursor` must resolve cursor's MCP CONFIG FILE. It is
    // the row's only destination, so the subtraction takes the whole row — and the receipt speaks
    // in config files, which is only reachable through the mcp vocabulary.
    let outcome = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/linear")],
        None,
        true,
        &ops::Selection::new(&["cursor".to_owned()], &[]),
    )
    .expect("the qualified row resolves its own record");
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("applies under --yes");
    };
    let shown = format!("{:?}", data.uninstalled);
    assert!(
        shown.contains("mcp.json"),
        "the mcp vocabulary resolved cursor to its config file: {shown}"
    );
}

// =================================================================================================
// The failure tally's key: `(scope label, bundle identity)`.
//
// Two bundles of one name are two bundles. Keyed by DISPLAY NAME the tally counted them once, and
// the converge fold's row stand-down — which matched name + scope — took a healthy twin's receipt
// row down beside the failed one's. Both fixtures below give every bundle an id UNLIKE its name on
// purpose: a fixture whose id IS its name passes with the bug still in.
// =================================================================================================

/// A SECOND connected workspace, on its own server — what makes two same-named bundles two
/// bundles rather than one name said twice.
fn seed_ops_session(rig: &Rig) {
    sessions::upsert_session(
        &rig.fs,
        &rig.layout(),
        Session {
            host: "beta.test".into(),
            base_url: "https://beta.test/api".into(),
            workspace_id: "w_ops".into(),
            workspace_name: "ops".into(),
            display_name: "Operations".into(),
            session_id: "sn_2".into(),
            credential: "cred-2".into(),
            status: SESSION_ACTIVE.into(),
            logged_in_at: 1,
        },
    )
    .unwrap();
}

/// Each workspace answers from its OWN catalog and delivery. The single-lane [`connect`] would
/// hand both sessions one catalog holding two entries of one name, which is an ambiguous catalog
/// — not two workspaces.
fn connect_lanes<'a>(
    lanes: &'a [(&'a str, FakePlane, FakeDirectory)],
) -> impl Fn(&Session) -> ops::SessionTransports + 'a {
    move |s: &Session| {
        let (_, plane, dir) = lanes
            .iter()
            .find(|(ws, ..)| *ws == s.workspace_id)
            .unwrap_or_else(|| panic!("no lane for {}", s.workspace_id));
        ops::SessionTransports {
            plane: Box::new(plane.clone()),
            directory: Box::new(dir.clone()),
            contribute: Box::new(NoContribute),
            governance: Box::new(NoGovernance),
        }
    }
}

fn sweep_lanes(ctx: &Ctx<'_>, lanes: &[(&str, FakePlane, FakeDirectory)]) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect_lanes(lanes),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap()
}

/// TWO WORKSPACES, ONE NAME, TWO FAILURES. Both teams publish an MCP server called `linear`, and
/// this run can place neither: each document names an http endpoint the gate refuses. The tally
/// counts what actually failed — two bundles — because it is keyed by the bundle identity the
/// sweep already joins on. Under the display name the second failure disappeared into the first
/// and the summary was short by one on every same-named pair.
#[test]
fn two_same_named_bundles_from_two_workspaces_both_failing_count_as_two() {
    let rig = Rig::new("tally-two-ws");
    rig.seed_session();
    seed_ops_session(&rig);
    seed_harness_dirs(&rig.home.0);
    let eng = mk_version(&[(
        "server.json",
        server_json("http://eng.example/linear").as_bytes(),
    )]);
    let ops_v = mk_version(&[(
        "server.json",
        server_json("http://ops.example/linear").as_bytes(),
    )]);
    let lanes = [
        (
            WS,
            FakePlane::new().with_version("s_lin_eng", &eng),
            FakeDirectory {
                skills: vec![mcp_catalog_entry("s_lin_eng", "linear", &eng)],
                channels: Vec::new(),
            },
        ),
        (
            "w_ops",
            FakePlane::new().with_version("s_lin_ops", &ops_v),
            FakeDirectory {
                skills: vec![mcp_catalog_entry("s_lin_ops", "linear", &ops_v)],
                channels: Vec::new(),
            },
        ),
    ];
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n\
         \"beta.test/ops/linear\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep_lanes(&ctx, &lanes);

    // BOTH refusals are on the record…
    assert_eq!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .filter(|w| w.contains("MCP_INSECURE_URL"))
            .count(),
        2,
        "each bundle's own refusal: {:?}",
        out.warnings
    );
    // …and the tally holds two entries, one per IDENTITY, in the one scope both rows stand in.
    let failed: Vec<(&str, &str)> = out
        .failed_bundles
        .iter()
        .map(|(label, id)| (label.as_str(), id.as_str()))
        .collect();
    assert_eq!(
        failed,
        vec![("person", "s_lin_eng"), ("person", "s_lin_ops")],
        "two bundles failed, so the summary counts two"
    );
    // Nothing was placed for either, and neither stood-down row is left claiming otherwise.
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "linear"),
        "a bundle nothing was placed for keeps no row: {:?}",
        out.data.skills
    );
    let cursor = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap_or_default();
    assert!(!cursor.contains("linear"), "{cursor}");
}

/// A FAILURE STANDS DOWN ITS OWN ROW AND NOBODY ELSE'S. One scope holds two bundles called
/// `linear`: the workspace's, which places cleanly, and a local folder whose `server.json` the
/// gate refuses. The stand-down finds the failed bundle's row through the identity index, so the
/// healthy twin keeps its own — a name + scope match took both rows down and left the run
/// wordless about a bundle it had just placed.
#[test]
fn a_failed_bundle_stands_down_its_own_row_and_the_healthy_twin_keeps_its_row() {
    let rig = Rig::new("tally-twin");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    // The healthy one: the workspace's `linear`, over https.
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_lin_ws", &v);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_lin_ws", "linear", &v)],
        channels: Vec::new(),
    };
    // The failing one: a LOCAL folder of the same name whose document names an http endpoint.
    let local = rig.home.0.join("linear");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("server.json"),
        server_json("http://local.example/linear"),
    )
    .unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n\
         \"{}\" = {{ kind = \"mcp\", {SAFE} }}\n",
        local.display()
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // ONE failure, filed under the LOCAL bundle's identity — never the name the two share.
    let failed: Vec<(&str, &str)> = out
        .failed_bundles
        .iter()
        .map(|(label, id)| (label.as_str(), id.as_str()))
        .collect();
    assert_eq!(
        failed,
        vec![(
            "person",
            crate::config_custody::local_identity(&local.display().to_string()).as_str()
        )],
        "the refused bundle is the one that is counted — keyed by ITS OWN LINE, never the name \
         it shares with the workspace's: {:?}",
        out.warnings
    );
    // The healthy twin keeps its receipt row, with the agents its entries reached.
    let rows: Vec<_> = out
        .data
        .skills
        .iter()
        .filter(|s| s.skill == "linear")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "one row stands down, the other stands: {:?}",
        out.data.skills
    );
    assert_eq!(
        rows[0].workspace_id.as_deref(),
        Some(WS),
        "the surviving row is the workspace bundle's: {:?}",
        rows[0]
    );
    let agents: BTreeSet<&str> = rows[0].harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into(), "{:?}", rows[0]);
    // …and the disk agrees: the healthy entry landed, the refused one never did.
    let cursor = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap();
    assert!(
        cursor.contains("topos-eng-linear") && cursor.contains("https://mcp.example/linear"),
        "{cursor}"
    );
    assert!(!cursor.contains("http://local.example"), "{cursor}");
}
