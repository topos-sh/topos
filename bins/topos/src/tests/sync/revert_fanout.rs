//! `revert` (the two-phase describe + the byte-level no-op) and the settled-draft fan-out
//! across a bundle's other placements.

use std::path::{Path, PathBuf};

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::identity::{self, Commit};
use topos_types::WireCurrentRecord;
use topos_types::results::PullAction;

use crate::{doc, ops};

use super::rig::*;

// ---------------------------------------------------------------------------------------------
// revert — the two-phase describe + the byte-level no-op. A forward revert mints a NEW commit id
// over IDENTICAL bytes, so repeated identical reverts must be caught by comparing TREE digests
// (not commit ids), else they mint generation after generation.
// ---------------------------------------------------------------------------------------------

/// The fixed forward-revert message. MIRRORS `ops::contribute::REVERT_MESSAGE` (that const is private
/// to the ops module); the forward commit id folds it in, so the served pointer names the same id.
const REVERT_MSG: &str = "topos: revert";

/// A contribute transport that COUNTS the writes it receives and answers a fixed receipt. The
/// two-phase describe and the no-op must never reach it (zero POSTs).
struct RecordingContribute {
    posts: std::rc::Rc<std::cell::Cell<usize>>,
    receipt: crate::plane::WriteReceipt,
}
impl crate::plane::ContributeSource for RecordingContribute {
    fn publish(
        &self,
        _b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        unreachable!("revert never publishes")
    }
    fn propose(
        &self,
        _b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        unreachable!("revert never proposes")
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        self.posts.set(self.posts.get() + 1);
        Ok(self.receipt.clone())
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        unreachable!("revert never reviews")
    }
}

/// The tree (bundle) digest of a version's files — the value the no-op compares.
fn tree_of(v: &Version) -> [u8; 32] {
    let entries: Vec<ManifestEntry> = v
        .fetched
        .files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    digest::bundle_digest(&entries).unwrap()
}

/// Write the enrolled `instance.json` `revert` reads (the follow-state comes from the [`FixtureFollow`]
/// the caller hands `ctx`, not from disk).
fn seed_instance(rig: &Rig) {
    crate::sessions::upsert_session(
        &rig.fs,
        &rig.layout(),
        crate::sessions::Session {
            host: "topos.example".to_owned(),
            base_url: "https://topos.example/api".to_owned(),
            workspace_id: WS.to_owned(),
            workspace_name: "acme".to_owned(),
            display_name: "Acme".to_owned(),
            session_id: "sn_1".to_owned(),
            credential: "cred-1".to_owned(),
            status: crate::sessions::SESSION_ACTIVE.to_owned(),
            logged_in_at: 1,
        },
    )
    .unwrap();
}

/// An OK revert receipt naming `record` as the moved-to pointer (the server echoes the forward id).
fn ok_revert_receipt(record: WireCurrentRecord) -> crate::plane::WriteReceipt {
    crate::plane::WriteReceipt {
        receipt: Some(topos_types::Receipt {
            schema_version: 1,
            op_id: "op-rv".to_owned(),
            command: "reverts".to_owned(),
            outcome: topos_types::TerminalOutcome::Ok,
            workspace_id: WS.to_owned(),
            skill_id: Some(record.scope.skill_id.clone()),
            version_id: Some(record.record.version_id.clone()),
            bundle_digest: None,
            expected_generation: None,
            current_generation: Some(record.record.generation),
            created_at: "2026-07-16T00:00:00Z".to_owned(),
            details: None,
        }),
        error: None,
        wire_record: Some(record),
    }
}

#[test]
fn revert_bare_describes_without_writing_then_yes_applies() {
    let rig = Rig::new("rv-2phase");
    let (id, name, _genesis) = rig.adopt(&[("SKILL.md", FileMode::Regular, b"base\n")]);
    seed_instance(&rig);
    let foll = follow(&id);

    // good (tree A) and current (tree B, DIFFERENT) — both served by the plane.
    let good = mk_version(
        &[],
        &[("SKILL.md", FileMode::Regular, b"good bytes\n")],
        "auth",
        "m-good",
    );
    let current = mk_version(
        &[],
        &[("SKILL.md", FileMode::Regular, b"current bytes\n")],
        "auth",
        "m-current",
    );
    let mut plane = FixturePlane::default();
    plane.set_current(&id, served(WS, &id, current.id, 5));
    plane.add_version(&id, &good);
    plane.add_version(&id, &current);

    // The forward commit id the client computes + the server would echo (I-COMMIT-PARITY).
    let forward = identity::commit_id(&Commit {
        parents: &[current.id],
        tree: tree_of(&good),
        author: DEVICE,
        message: REVERT_MSG,
    })
    .unwrap();
    let posts = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let receipt = ok_revert_receipt(served(WS, &id, forward, 6));
    let connect = {
        let posts = posts.clone();
        move |_b: &str, _c: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
            Box::new(RecordingContribute {
                posts: posts.clone(),
                receipt: receipt.clone(),
            })
        }
    };
    let good_hex = to_hex(&good.id);
    let ctx = rig.ctx(&plane, &foll);

    // Bare = DESCRIBE: nothing written — no POST, no op-WAL. The paste-ready apply carries no
    // `--workspace` when none was given.
    let described = ops::revert(&ctx, &connect, &name, &good_hex, false, None).unwrap();
    match &described {
        ops::RevertOutcome::Describe { yes_argv, .. } => {
            assert_eq!(
                yes_argv,
                &vec![
                    "topos".to_owned(),
                    "revert".to_owned(),
                    name.clone(),
                    "--to".to_owned(),
                    good_hex.clone(),
                    "--yes".to_owned(),
                ],
            );
        }
        other => panic!("bare revert describes, got {other:?}"),
    }
    assert_eq!(posts.get(), 0, "a describe POSTs nothing");

    // A `--workspace` disambiguation is PRESERVED on the paste-ready apply (as the canonical id),
    // so the suggested command re-resolves to exactly the skill described.
    let described_ws = ops::revert(&ctx, &connect, &name, &good_hex, false, Some(WS)).unwrap();
    match &described_ws {
        ops::RevertOutcome::Describe { yes_argv, .. } => {
            assert_eq!(
                yes_argv,
                &vec![
                    "topos".to_owned(),
                    "revert".to_owned(),
                    name.clone(),
                    "--to".to_owned(),
                    good_hex.clone(),
                    "--workspace".to_owned(),
                    WS.to_owned(),
                    "--yes".to_owned(),
                ],
            );
        }
        other => panic!("bare revert with --workspace describes, got {other:?}"),
    }
    assert_eq!(
        posts.get(),
        0,
        "the workspace-scoped describe POSTs nothing either"
    );
    assert!(
        crate::op_wal::find_pending_for_skill(
            &rig.fs,
            &rig.layout(),
            WS,
            &id,
            &[topos_types::persisted::OpKind::Revert],
        )
        .unwrap()
        .is_none(),
        "a describe writes no op-WAL"
    );

    // `--yes` = apply: exactly one POST; the forward move lands.
    let applied = ops::revert(&ctx, &connect, &name, &good_hex, true, None).unwrap();
    match applied {
        ops::RevertOutcome::Applied(data) => {
            assert_eq!(data.reverted_to, good_hex);
            assert_eq!(data.new_version_id, to_hex(&forward));
        }
        other => panic!("--yes applies, got {other:?}"),
    }
    assert_eq!(posts.get(), 1, "--yes POSTs exactly once");
}

#[test]
fn revert_over_identical_bytes_is_a_no_op_under_differing_commit_ids() {
    let rig = Rig::new("rv-noop");
    let (id, name, _genesis) = rig.adopt(&[("SKILL.md", FileMode::Regular, b"base\n")]);
    seed_instance(&rig);
    let foll = follow(&id);

    // good and current share the SAME tree but DIFFERENT commit ids — current is a forward revert over
    // good's bytes, exactly the state one revert leaves behind (the repeated-revert bug's trigger).
    let files: &[(&str, FileMode, &[u8])] = &[("SKILL.md", FileMode::Regular, b"shared bytes\n")];
    let good = mk_version(&[], files, "auth", "m-good");
    let current = mk_version(&[good.id], files, DEVICE, REVERT_MSG);
    assert_ne!(good.id, current.id, "the ids differ");
    assert_eq!(tree_of(&good), tree_of(&current), "the bytes are identical");

    let mut plane = FixturePlane::default();
    plane.set_current(&id, served(WS, &id, current.id, 6));
    plane.add_version(&id, &good);
    plane.add_version(&id, &current);

    let posts = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let receipt = ok_revert_receipt(served(WS, &id, current.id, 6));
    let connect = {
        let posts = posts.clone();
        move |_b: &str, _c: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
            Box::new(RecordingContribute {
                posts: posts.clone(),
                receipt: receipt.clone(),
            })
        }
    };
    let good_hex = to_hex(&good.id);
    let ctx = rig.ctx(&plane, &foll);

    // Both bare and `--yes` are a typed no-op that mints no forward commit and POSTs nothing — the
    // pre-fix id compare (good.id != current.id) would have minted one.
    let bare = ops::revert(&ctx, &connect, &name, &good_hex, false, None).unwrap();
    assert!(
        matches!(&bare, ops::RevertOutcome::NoOp(d) if d.is_noop),
        "bare: {bare:?}"
    );
    let yes = ops::revert(&ctx, &connect, &name, &good_hex, true, None).unwrap();
    assert!(
        matches!(yes, ops::RevertOutcome::NoOp(_)),
        "--yes acknowledges the no-op"
    );
    assert_eq!(posts.get(), 0, "a no-op POSTs nothing on either path");
}

// ---------------------------------------------------------------------------------------------
// The settled-draft fan-out: one bundle, one scope, several agent folders. A draft that is
// UNCHANGED across two runs is copied onto the bundle's other placements (their recorded
// baselines advance with it); a mid-edit file never spreads; true competitors still freeze.
// ---------------------------------------------------------------------------------------------

/// Append a STALE-CLEAN replica: the dir holds `files` and its recorded baseline names EXACTLY
/// those bytes, so it scans CLEAN (no local edit to protect) — but at an older version than the
/// lock's base. That is the crash-window residue a local converge REFRESHES.
fn add_stale_replica(rig: &Rig, id: &str, dir: &Path, files: &[(&str, FileMode, &[u8])]) {
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    write_tree(dir, files);
    let own = to_hex(&crate::scan::scan(dir).unwrap().bundle_digest);
    let sp = rig.layout().published(&sid(id));
    let mut map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(dir.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: None,
        materialized_sha: Some(own),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    doc::write_map(&rig.fs, &sp.map, &map).unwrap();
}

/// The standard fan-out rig: a followed skill fast-forwarded to v1 with a byte-identical replica
/// folder beside the primary placement.
fn fanout_rig(tag: &str) -> (Rig, String, FixturePlane, FixtureFollow, PathBuf) {
    let rig = Rig::new(tag);
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    {
        let ctx = rig.ctx(&plane, &foll);
        assert_eq!(
            only(&pull_data(&ctx, ops::PullScope::AllFollowed).unwrap()).action,
            PullAction::FastForwarded
        );
    }
    let replica = rig.work.0.join("replica");
    add_replica(&rig, &id, &replica, V1);
    (rig, id, plane, foll, replica)
}

#[test]
fn a_settled_draft_spreads_and_advances_the_sibling_baselines() {
    let (rig, id, plane, foll, replica) = fanout_rig("spread");
    let ctx = rig.ctx(&plane, &foll);

    // THE draft: edit the primary copy.
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();

    // Sweep 1: the observation only — a first sighting never spreads.
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::UpToDate);
    assert_eq!(
        snapshot(&replica),
        Some(expect(V1)),
        "no spread on the first sighting"
    );
    let d_hex = to_hex(&crate::scan::scan(&rig.placement()).unwrap().bundle_digest);
    assert_eq!(
        rig.read_sync(&id).draft_observed.as_deref(),
        Some(d_hex.as_str()),
        "the observation is durable"
    );

    // Sweep 2: SETTLED — the draft lands on the replica, disclosed on the DISCLOSURE channel (it
    // worked; the warning channel is what the receipt counts as failures).
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(row.action, PullAction::DraftSynced);
    assert_eq!(row.synced_placements, Some(1));
    // THE EDITS STILL STAND, and the row says so. The draft flag was attached below this arm's
    // early returns, so the two rows that leave early — a settled draft fanning out, and a folder
    // this run re-created — reported `draft: false` about a machine `list` was calling drafted.
    assert!(
        row.draft,
        "a fan-out moves the draft around; it does not end it: {row:?}"
    );
    assert_eq!(
        row.destinations,
        vec![replica.display().to_string()],
        "the landed folder is named — the destination convention"
    );
    assert!(
        crate::message::legacy_lines(&out.disclosures)
            .into_iter()
            .any(|w| w.starts_with("DRAFT_SYNCED") && w.contains("1 other folder")),
        "{:?}",
        out.disclosures
    );
    assert!(
        out.warnings.is_empty(),
        "a successful fan-out reports no failure: {:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# my draft\n"
    );
    // The draft copy is untouched; the LOCK still names the pristine v1 (the draft detector's
    // reference did not move); the replica's recorded baseline is the DRAFT digest now.
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"# my draft\n"
    );
    let sp = rig.layout().published(&sid(&id));
    let map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    assert_eq!(
        map.placement_state[1].materialized_sha.as_deref(),
        Some(d_hex.as_str()),
        "the synced folder's baseline advances to the draft"
    );
    // A changed sweep for the hook: the fan-out counts as moved bytes.
    assert!(ops::sweep_changed_bytes(&out.data));

    // Sweep 3: a stable fixpoint — everything already carries the draft.
    assert_eq!(
        only(&pull_data(&ctx, ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::UpToDate
    );

    // A LATER edit at the SYNCED replica is a fresh draft against its advanced baseline — the
    // primary (still holding the old draft) is stale behind it, never a competitor: no freeze.
    std::fs::write(replica.join("SKILL.md"), b"# my draft, refined\n").unwrap();
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(only(&out.data).action, PullAction::UpToDate);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"# my draft\n",
        "the stale copy is untouched until the refined draft settles"
    );
}

#[test]
fn a_converge_landing_survives_the_same_runs_settled_fanout() {
    // The corruption regression: ONE run both CONVERGES a reservation (a never-materialized
    // placement first-installs from the local store) and runs the settled-draft fan-out. The
    // fan-out's materialize must commit the POST-converge map — handing it the stale pre-converge
    // map would erase the reservation's just-recorded baseline, and the next scan would classify
    // the installed dir as FOREIGN (a managed placement lost to its own sweep).
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    let (rig, id, plane, foll, replica) = fanout_rig("converge-settle");
    let ctx = rig.ctx(&plane, &foll);

    // THE draft, observed once (sweep 1) — the next sweep's fan-out will see it settled.
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();

    // A RESERVATION row: recorded, never materialized, dir absent — exactly what the converge
    // first-installs (a newly added target).
    let extra = rig.work.0.join("extra");
    let sp = rig.layout().published(&sid(&id));
    let mut map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(extra.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: Some("extra".to_owned()),
        materialized_sha: None,
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    doc::write_map(&rig.fs, &sp.map, &map).unwrap();

    // Sweep 2: the converge lands `extra` at the pristine v1 AND the settled draft spreads onto
    // the replica — in ONE run.
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        only(&out.data).action,
        PullAction::DraftSynced,
        "{:?}",
        out.warnings
    );
    assert_eq!(
        snapshot(&extra),
        Some(expect(V1)),
        "the converge landed the reservation from the local store"
    );
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# my draft\n",
        "the settled draft spread onto the replica in the same run"
    );
    // THE regression assertion: the converge's recorded baseline SURVIVES the fan-out's commit.
    let map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    let extra_idx = map
        .placements
        .iter()
        .position(|p| Path::new(p) == extra)
        .expect("the reservation row is still recorded");
    let v1_digest = {
        let lock: topos_types::persisted::Lock = doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
        lock.bundle_digest
    };
    assert_eq!(
        map.placement_state[extra_idx].materialized_sha.as_deref(),
        Some(v1_digest.as_str()),
        "the just-converged placement stays MANAGED — the fan-out must not commit a stale map"
    );

    // And the placement keeps behaving managed: the next sweep spreads the settled draft onto it
    // (a clean copy stale behind the draft), never reading it as foreign.
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        std::fs::read(extra.join("SKILL.md")).unwrap(),
        b"# my draft\n",
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_refreshed_stale_replica_never_reads_all_up_to_date() {
    // The crash-window residue: a managed copy whose bytes AND recorded baseline sit at an OLDER
    // version than the one this machine holds. The converge rewrites it — bytes moved on disk, so
    // the run may not claim "all up to date". A refresh is not a first install, so the row says
    // `updated` and names WHERE THE BUNDLE NOW STANDS — every copy at the applied version, not
    // just the one the converge happened to rewrite — and the quiet hook's changed-bytes signal
    // fires.
    let (rig, id, plane, foll, replica) = fanout_rig("stale-refresh");
    let stale = rig.work.0.join("stale");
    add_stale_replica(&rig, &id, &stale, BASE);
    let ctx = rig.ctx(&plane, &foll);

    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(
        row.action,
        PullAction::Refreshed,
        "a rewritten stale copy is a refresh, never up-to-date: {row:?}"
    );
    // THE anti-regression: three folders hold this bundle and only one needed rewriting. A row
    // that named the rewritten one alone would read as if the other two had gone.
    let sp = rig.layout().published(&sid(&id));
    let placements = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap().placements;
    assert_eq!(placements.len(), 3, "{placements:?}");
    assert_eq!(
        row.destinations, placements,
        "every copy now holding the applied version is named, in map order"
    );
    for named in [&stale, &replica] {
        assert!(
            row.destinations.contains(&named.display().to_string()),
            "{:?} names {}",
            row.destinations,
            named.display()
        );
    }
    assert_eq!(row.note, None, "nothing else was written this run");
    assert_eq!(
        snapshot(&stale),
        Some(expect(V1)),
        "the stale copy now holds the version this machine applied"
    );
    // No version moved: the refresh is purely a local catch-up.
    assert_eq!(row.observed, row.applied);
    assert!(
        ops::sweep_changed_bytes(&out.data),
        "the quiet hook must hear about bytes that moved"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(tty.contains("updated (3 folders)"), "{tty}");
    // Counted rows spell their folders out — a number nobody can act on is not an answer.
    assert!(
        tty.contains(&format!("\n    {}\n", stale.display())),
        "{tty}"
    );
    assert!(
        !tty.contains("refreshed"),
        "the internal word never reaches a person: {tty}"
    );
    assert!(
        !tty.contains("all up to date"),
        "a run that rewrote a folder may not claim it: {tty}"
    );
    assert!(
        !tty.contains("installed"),
        "a refresh is not an install: {tty}"
    );

    // Refreshed and untouched again → the honest all-up-to-date summary is back.
    let again = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&again.data).action, PullAction::UpToDate);
    assert!(!ops::sweep_changed_bytes(&again.data));
    assert!(
        crate::render::pull_tty(
            &again.data,
            &again.decisions,
            &again.warnings,
            &again.advisories,
            &again.disclosures,
            0,
        )
        .contains("all up to date")
    );
}

#[test]
fn a_heal_riding_along_with_a_settled_fanout_is_named_on_the_row() {
    // ONE run both HEALS an absent placement (the converge first-installs it from the local
    // store) and spreads a settled draft onto a sibling. The fan-out chose its targets from the
    // pre-converge scan, so it SKIPS the just-healed dir — and the `synced` column cannot name it.
    // The row states it as its second fact: nothing this run wrote goes unnamed until the next
    // sweep.
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    let (rig, id, plane, foll, replica) = fanout_rig("heal-fanout");
    let ctx = rig.ctx(&plane, &foll);

    // THE draft, observed once (sweep 1) — the next sweep's fan-out sees it settled.
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();

    // A recorded placement whose dir is absent (a hand-deleted folder, a fresh target).
    let extra = rig.work.0.join("extra");
    let sp = rig.layout().published(&sid(&id));
    let mut map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(extra.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: Some("extra".to_owned()),
        materialized_sha: None,
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    doc::write_map(&rig.fs, &sp.map, &map).unwrap();

    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(row.action, PullAction::DraftSynced, "{:?}", out.warnings);
    assert_eq!(
        row.destinations,
        vec![replica.display().to_string()],
        "the fan-out's own column names only what took the draft"
    );
    assert_eq!(
        row.note.as_deref(),
        Some(format!("also installed {}", extra.display()).as_str()),
        "the healed folder is named on the row: {row:?}"
    );
    assert_eq!(
        snapshot(&extra),
        Some(expect(V1)),
        "the heal landed the pristine version from the local store"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(tty.contains("synced your edits to"), "{tty}");
    assert!(
        tty.contains(&format!("also installed {}", extra.display())),
        "{tty}"
    );
    assert!(!tty.contains("all up to date"), "{tty}");
    assert!(ops::sweep_changed_bytes(&out.data));
}

#[test]
fn an_unsettled_draft_never_spreads() {
    let (rig, id, plane, foll, replica) = fanout_rig("unsettled");
    let ctx = rig.ctx(&plane, &foll);

    std::fs::write(rig.placement().join("SKILL.md"), b"# edit one\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap(); // observes edit one

    // The file MOVES between sweeps: only the observation updates — nothing spreads.
    std::fs::write(rig.placement().join("SKILL.md"), b"# edit two\n").unwrap();
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::UpToDate);
    assert_eq!(
        snapshot(&replica),
        Some(expect(V1)),
        "a mid-edit file never spreads"
    );
    let d2 = to_hex(&crate::scan::scan(&rig.placement()).unwrap().bundle_digest);
    assert_eq!(
        rig.read_sync(&id).draft_observed.as_deref(),
        Some(d2.as_str())
    );

    // Once it settles, the NEXT sweep spreads exactly the settled bytes.
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::DraftSynced);
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# edit two\n"
    );
}

/// The OTHER early return out of the no-pending-update arm: a folder this run RE-CREATED. Healing
/// a hand-deleted copy is an install, and the row said so — while dropping the fact that the
/// person's edits are still sitting in the copy that survived. A receipt that reports an install
/// and denies the draft sends an agent to `update` when the answer is `publish`.
#[test]
fn a_healed_folder_still_reports_the_draft_standing_beside_it() {
    let (rig, id, plane, foll, replica) = fanout_rig("healed-draft");
    let ctx = rig.ctx(&plane, &foll);

    // The draft, unsettled (first sighting — the fan-out deliberately does not run).
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();
    // …and the sibling copy is hand-deleted, so the same sweep must re-create it.
    std::fs::remove_dir_all(&replica).unwrap();

    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(row.action, PullAction::Installed, "{row:?}");
    assert!(replica.is_dir(), "the hand-deleted folder is healed");
    assert!(
        row.draft,
        "the heal is not the whole news — the edits still stand: {row:?}"
    );
    let _ = id;
}

#[test]
fn true_competitors_freeze_and_the_fanout_never_runs() {
    let (rig, id, plane, foll, replica) = fanout_rig("compete");
    let ctx = rig.ctx(&plane, &foll);

    std::fs::write(rig.placement().join("SKILL.md"), b"# primary edit\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap(); // observes the primary draft

    // A DIFFERENT edit lands in the replica: two contents, neither at the other's baseline —
    // true competitors. The sweep freezes the skill (typed, isolated) and syncs NOTHING.
    std::fs::write(replica.join("SKILL.md"), b"# replica edit\n").unwrap();
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("PLACEMENTS_DIVERGED")),
        "{:?}",
        out.warnings
    );
    assert!(out.data.skills.is_empty(), "{:?}", out.data.skills);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"# primary edit\n"
    );
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# replica edit\n"
    );
    let _ = id;
}
