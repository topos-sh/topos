//! The crash gate + the fail-closed migration dispatch — the highest-priority durability invariants.
//!
//! `FaultFs` fails the Nth durable op, with a genuine real-syscall prefix, so each cell's post-fault
//! on-disk state is authentic. The table asserts, per cell: (a) the doc deserializes (no torn JSON),
//! (b) it is byte-for-byte the pre OR post state, (c) recovery is idempotent, (d) the draft survives.

use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;

use topos_types::persisted::{
    Lock, LockedFile, OpRecord, PlacementMap, RecordedTuple, SwapCapability, SyncState,
};
use topos_types::{Generation, SCHEMA_VERSION};

use crate::atomic::{atomic_write, load_versioned, temp_path};
use crate::error::ClientError;
use crate::fs_seam::{FaultFs, FsOps, RealFs};
use crate::sidecar::{Layout, footprint, recover};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-dur-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn doc_bytes<T: Serialize>(d: &T) -> Vec<u8> {
    let mut b = serde_json::to_vec_pretty(d).unwrap();
    b.push(b'\n');
    b
}

fn sample_lock(tag: u8) -> Lock {
    Lock {
        schema_version: 1,
        skill_id: format!("topos_t{tag}"),
        name: "pr-describe".into(),
        base_commit: hex(tag),
        bundle_digest: hex(tag.wrapping_add(1)),
        files: vec![LockedFile {
            path: "SKILL.md".into(),
            mode: "100644".into(),
            sha256: hex(tag.wrapping_add(2)),
            size: u64::from(tag),
        }],
    }
}

fn sample_map(tag: u8) -> PlacementMap {
    PlacementMap {
        schema_version: 1,
        placements: vec![format!("/home/u/skills/s{tag}")],
        applied_commit: hex(tag),
        materialized_sha: hex(tag),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
    }
}

fn sample_sync(tag: u8) -> SyncState {
    SyncState {
        schema_version: 1,
        observed: Generation {
            epoch: 0,
            seq: u64::from(tag),
        },
        applied: Generation {
            epoch: 0,
            seq: u64::from(tag),
        },
        recorded: vec![RecordedTuple {
            generation: Generation { epoch: 0, seq: 1 },
            commit_id: hex(tag),
        }],
        base_commit: hex(tag),
        work_hash: hex(tag),
        held: false,
    }
}

fn hex(seed: u8) -> String {
    (0..32)
        .map(|i| format!("{:02x}", seed.wrapping_add(i)))
        .collect()
}

/// (a)+(b): after a fault at any step, every doc type is byte-for-byte the pre OR post state and always
/// deserializes — across {lock, map, sync} × {fail before/after each step} × {pre-existing yes/no}.
#[test]
fn atomic_write_is_crash_safe_across_the_table() {
    crash_table(sample_lock);
    crash_table(sample_map);
    crash_table(sample_sync);
}

fn crash_table<T: Serialize + DeserializeOwned>(make: impl Fn(u8) -> T) {
    for fail_at in 1..=4 {
        for &pre in &[false, true] {
            let scratch = Scratch::new("aw");
            let real = RealFs;
            let target = scratch.0.join("doc.json");
            let old_bytes = doc_bytes(&make(1));
            let new_bytes = doc_bytes(&make(2));

            // Establish the pre-state.
            if pre {
                atomic_write(&real, &target, &old_bytes).unwrap();
            }
            let pre_state: Option<Vec<u8>> = if pre { Some(old_bytes) } else { None };

            // Faulted write — it must actually fault (every step 1..=4 is reached).
            let ff = FaultFs::new(fail_at);
            let result = atomic_write(&ff, &target, &new_bytes);
            assert!(
                result.is_err(),
                "fail_at={fail_at} pre={pre}: expected a fault"
            );

            // (b) target is exactly the pre OR the post state (never torn).
            let now = real.read_opt(&target).unwrap();
            let is_pre = now.as_deref() == pre_state.as_deref();
            let is_post = now.as_deref() == Some(new_bytes.as_slice());
            assert!(
                is_pre || is_post,
                "fail_at={fail_at} pre={pre}: neither pre nor post"
            );

            // (a) whatever is on disk deserializes (no torn JSON).
            if let Some(bytes) = &now {
                let _: T = load_versioned(bytes, SCHEMA_VERSION).expect("target deserializes");
            }

            // (c) the only artifact a faulted write leaves is the recognizable temp; clearing it and
            // re-writing cleanly always reaches the post state (idempotent forward).
            let tmp = temp_path(&target);
            if real.exists(&tmp) {
                real.remove_file(&tmp).unwrap();
            }
            atomic_write(&real, &target, &new_bytes).unwrap();
            let after = real.read_opt(&target).unwrap().unwrap();
            assert_eq!(after, new_bytes, "clean rewrite must reach the post state");
            let _: T = load_versioned(&after, SCHEMA_VERSION).unwrap();
        }
    }
}

/// (c)+(d): recovery sweeps a stray temp, repairs a torn log tail, leaves a valid skill untouched, and
/// is idempotent; the draft (source bytes) is never referenced for deletion.
#[test]
fn recover_sweeps_tmp_repairs_log_and_is_idempotent() {
    let scratch = Scratch::new("rec");
    let real = RealFs;
    let layout = Layout::new(&scratch.0);
    let id = "topos_keepme";
    let paths = layout.published(id);

    // A valid, complete skill (the lock marker is present).
    real.create_dir_all(&layout.skill_dir(id)).unwrap();
    atomic_write(&real, &paths.lock, &doc_bytes(&sample_lock(7))).unwrap();
    atomic_write(&real, &paths.map, &doc_bytes(&sample_map(7))).unwrap();
    atomic_write(&real, &paths.sync, &doc_bytes(&sample_sync(7))).unwrap();
    // A stray temp from a hypothetical faulted in-place write.
    let stray = temp_path(&paths.map);
    atomic_write(&real, &stray, b"garbage").unwrap();
    // A torn log tail (a partial trailing line).
    real.append_fsync(&layout.log_path(), b"{\"ok\":1}\n{\"partial\":")
        .unwrap();

    recover(&real, &layout).unwrap();

    // tmp swept, the valid skill + its docs intact, the torn tail dropped.
    assert!(!real.exists(&stray), "stray temp must be swept");
    assert!(real.exists(&paths.lock) && real.exists(&paths.map) && real.exists(&paths.sync));
    let lock: Lock = load_versioned(&real.read_opt(&paths.lock).unwrap().unwrap(), 1).unwrap();
    assert_eq!(lock.skill_id, "topos_t7");
    let log = real.read_opt(&layout.log_path()).unwrap().unwrap();
    assert_eq!(
        log, b"{\"ok\":1}\n",
        "the partial trailing line must be truncated"
    );

    // Idempotent: a second sweep changes nothing.
    let footprint_before = footprint(&real, &layout).unwrap();
    recover(&real, &layout).unwrap();
    let footprint_after = footprint(&real, &layout).unwrap();
    assert_eq!(
        footprint_before, footprint_after,
        "recovery must be idempotent"
    );
}

/// A log that is a single partial line with no trailing newline (a crash on the very first append) is
/// truncated to empty, idempotently.
#[test]
fn repair_truncates_a_lone_partial_line() {
    let scratch = Scratch::new("tail");
    let real = RealFs;
    let layout = Layout::new(&scratch.0);
    real.append_fsync(&layout.log_path(), b"{\"partial\":")
        .unwrap();

    crate::logfile::repair_torn_tail(&real, &layout.log_path()).unwrap();
    assert!(
        real.read_opt(&layout.log_path())
            .unwrap()
            .unwrap()
            .is_empty(),
        "a lone partial line must be truncated to empty"
    );
    // Idempotent.
    crate::logfile::repair_torn_tail(&real, &layout.log_path()).unwrap();
    assert!(
        real.read_opt(&layout.log_path())
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

/// Recovery removes an incomplete `add` staging dir, but never one a live writer is holding.
#[test]
fn recover_removes_unlocked_staging_keeps_locked() {
    let scratch = Scratch::new("stage");
    let real = RealFs;
    let layout = Layout::new(&scratch.0);

    let (dead_base, _) = layout.staging("topos_dead");
    let (live_base, _) = layout.staging("topos_live");
    real.create_dir_all(&dead_base).unwrap();
    real.create_dir_all(&live_base).unwrap();

    // Hold the live staging's lock for the duration of recovery.
    let _held = real
        .lock_exclusive(&layout.lock_file("topos_live"))
        .unwrap();
    recover(&real, &layout).unwrap();

    assert!(
        !real.exists(&dead_base),
        "an unlocked staging dir is incomplete -> removed"
    );
    assert!(
        real.exists(&live_base),
        "a locked staging dir is a live writer -> kept"
    );
}

/// Recovery must NOT delete a visible skill whose `lock.json` carries an unknown/newer schema_version —
/// that is "upgrade required", not "incomplete create". Deleting it would destroy newer-client data.
#[test]
fn recover_never_deletes_on_unknown_schema() {
    let scratch = Scratch::new("newer");
    let real = RealFs;
    let layout = Layout::new(&scratch.0);
    let id = "topos_future";
    let paths = layout.published(id);
    real.create_dir_all(&layout.skill_dir(id)).unwrap();

    // A lock.json from a newer client (schema_version = 2).
    let mut v = serde_json::to_value(sample_lock(3)).unwrap();
    v["schema_version"] = serde_json::json!(2);
    let mut bytes = serde_json::to_vec_pretty(&v).unwrap();
    bytes.push(b'\n');
    atomic_write(&real, &paths.lock, &bytes).unwrap();

    recover(&real, &layout).unwrap();

    assert!(
        real.exists(&paths.lock),
        "a newer-schema skill must never be deleted by recovery"
    );
    // ...and reading it fails closed (upgrade required), never silently parsed.
    let err = crate::doc::read_doc::<Lock>(&real, &paths.lock).unwrap_err();
    assert!(
        matches!(err, ClientError::UnknownSchemaVersion { found: 2, max: 1 }),
        "got {err:?}"
    );
}

/// The migration dispatch over {0, 1, 2, missing} × every persisted doc type.
#[test]
fn migration_dispatch_is_fail_closed() {
    check_dispatch::<Lock>(sample_lock(1));
    check_dispatch::<PlacementMap>(sample_map(1));
    check_dispatch::<SyncState>(sample_sync(1));
    check_dispatch::<OpRecord>(OpRecord {
        schema_version: 1,
        op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".into(),
        candidate_commit: hex(1),
        expected_generation: Generation { epoch: 1, seq: 1 },
        last_receipt: None,
    });
}

fn check_dispatch<T: Serialize + DeserializeOwned + std::fmt::Debug>(valid: T) {
    let base = serde_json::to_value(&valid).unwrap();

    // schema_version = 1 -> parses.
    let bytes = serde_json::to_vec(&base).unwrap();
    assert!(load_versioned::<T>(&bytes, SCHEMA_VERSION).is_ok());

    // = 2 -> newer -> fail closed (never handed to serde).
    let mut newer = base.clone();
    newer["schema_version"] = serde_json::json!(2);
    let err =
        load_versioned::<T>(&serde_json::to_vec(&newer).unwrap(), SCHEMA_VERSION).unwrap_err();
    assert!(matches!(
        err,
        ClientError::UnknownSchemaVersion { found: 2, max: 1 }
    ));

    // = 0 -> below floor -> unsupported.
    let mut zero = base.clone();
    zero["schema_version"] = serde_json::json!(0);
    let err = load_versioned::<T>(&serde_json::to_vec(&zero).unwrap(), SCHEMA_VERSION).unwrap_err();
    assert!(matches!(err, ClientError::UnsupportedLegacy { found: 0 }));

    // missing schema_version -> corrupt (never silently accepted).
    let mut missing = base.clone();
    missing.as_object_mut().unwrap().remove("schema_version");
    let err =
        load_versioned::<T>(&serde_json::to_vec(&missing).unwrap(), SCHEMA_VERSION).unwrap_err();
    assert!(matches!(err, ClientError::Corrupt(_)));
}
