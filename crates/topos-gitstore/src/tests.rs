//! Gitstore invariants: the put→render round-trip fuzz (the tree renderer never loses or mangles a
//! byte), first-parent lineage, and the adversarial verify-on-read defenses (a swapped tree, a
//! corrupted loose object, a forged non-UTF-8 / non-blob entry, and a lying `version_id`).

use std::path::PathBuf;

use gix::objs::tree::EntryKind;

use topos_core::digest::{self, FileMode};
use topos_core::sign::{self, Commit};

use crate::error::VerifyError;
use crate::store::{ImportFile, Store};

const AUTHOR: &str = "d_test";
const MESSAGE: &str = "topos add";

/// A temp dir that cleans itself up (RAII, so a failing test still tidies).
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-gs-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Compute the kernel `version_id` for a genesis bundle (parents = none).
fn genesis_version_id(bundle_digest: [u8; 32]) -> [u8; 32] {
    sign::commit_id(&Commit {
        parents: &[],
        tree: bundle_digest,
        author: AUTHOR,
        message: MESSAGE,
    })
    .expect("commit_id")
}

/// `add`-shaped helper: write a bundle + commit it as a genesis version, returning (version_id, digest).
fn commit_genesis(store: &Store, files: &[ImportFile<'_>]) -> ([u8; 32], [u8; 32]) {
    let th = store.write_bundle(files).expect("write_bundle");
    let vid = genesis_version_id(th.bundle_digest);
    store
        .commit(vid, &[], &th, AUTHOR, MESSAGE)
        .expect("commit");
    (vid, th.bundle_digest)
}

#[test]
fn round_trip_preserves_every_byte_for_nested_and_empty_and_binary() {
    // A deterministic fuzz over many bundles: varied file counts, nested paths, both modes, and content
    // that includes empty files and arbitrary (incl. high/NUL) bytes. put -> render must be byte-identical.
    let path_pool: &[&str] = &[
        "SKILL.md",
        "a.txt",
        "scripts/run.sh",
        "scripts/lib/util.sh",
        "reference/guide.md",
        "reference/deep/nested/notes.md",
        "x",
        "dir/with spaces.md",
        "u/ni\u{e9}code.md",
    ];
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    for _ in 0..200 {
        let scratch = Scratch::new("rt");
        let store = Store::init(&scratch.0).expect("init");

        // Pick a unique subset of paths (uniqueness keeps us off the kernel's collision rejects).
        let count = (rng.next() % (path_pool.len() as u64 + 1)) as usize;
        let mut chosen: Vec<&str> = Vec::new();
        for &p in path_pool {
            if chosen.len() >= count {
                break;
            }
            if rng.next() & 1 == 0 {
                chosen.push(p);
            }
        }
        let mut owned: Vec<(String, FileMode, Vec<u8>)> = Vec::new();
        for p in &chosen {
            let mode = if rng.next() & 1 == 0 {
                FileMode::Regular
            } else {
                FileMode::Executable
            };
            let len = (rng.next() % 40) as usize; // includes 0 -> empty file
            let bytes: Vec<u8> = (0..len).map(|_| (rng.next() & 0xff) as u8).collect();
            owned.push((p.to_string(), mode, bytes));
        }
        let files: Vec<ImportFile<'_>> = owned
            .iter()
            .map(|(p, m, b)| ImportFile {
                path: p,
                mode: *m,
                bytes: b,
            })
            .collect();

        let (vid, bd) = commit_genesis(&store, &files);
        let rendered = store.render_verified(vid, bd).expect("render");

        // Compare as sorted (path, mode, bytes) sets.
        let mut want: Vec<(String, FileMode, Vec<u8>)> = owned.clone();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        let got: Vec<(String, FileMode, Vec<u8>)> = rendered
            .files
            .iter()
            .map(|f| (f.path.clone(), f.mode, f.bytes.clone()))
            .collect();
        assert_eq!(got, want, "round-trip changed bytes/paths/modes");
        assert_eq!(rendered.bundle_digest, bd);
    }
}

#[test]
fn read_object_in_version_returns_verified_bytes_and_typed_misses() {
    let scratch = Scratch::new("readobj");
    let store = Store::init(&scratch.0).expect("init");
    let files = [
        ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"# skill\n",
        },
        ImportFile {
            path: "scripts/run.sh",
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho hi\n",
        },
        ImportFile {
            path: "reference/deep/notes.md",
            mode: FileMode::Regular,
            bytes: b"notes",
        },
    ];
    let (vid, _bd) = commit_genesis(&store, &files);

    // Every file's bytes are fetchable by their content id, byte-exact (incl. the nested paths).
    for f in &files {
        let oid = digest::sha256(f.bytes);
        let got = store
            .read_object_in_version(vid, oid)
            .expect("read object by id");
        assert_eq!(got, f.bytes, "object at {} round-trips byte-exact", f.path);
    }

    // An object id not present in this version -> the typed gitstore miss `ObjectNotInVersion`. (The
    // plane's access port only reaches this with an ALREADY-AUTHORIZED witness — provenance said the
    // commit reaches the object — so it treats this miss as a database/store divergence, i.e. an
    // integrity fault, never a not-found.)
    let absent = digest::sha256(b"these bytes are in no bundle");
    assert!(matches!(
        store.read_object_in_version(vid, absent),
        Err(VerifyError::ObjectNotInVersion)
    ));

    // An unknown version -> MissingVersion (no ref).
    let bogus_vid = [0x11u8; 32];
    let some_oid = digest::sha256(files[0].bytes);
    assert!(matches!(
        store.read_object_in_version(bogus_vid, some_oid),
        Err(VerifyError::MissingVersion)
    ));
}

#[test]
fn empty_bundle_round_trips_at_the_store_layer() {
    // The CLIENT rejects an empty bundle as a policy; the dumb store must still handle a zero-entry tree
    // (digest = sha256 of the empty manifest) without panicking.
    let scratch = Scratch::new("empty");
    let store = Store::init(&scratch.0).expect("init");
    let (vid, bd) = commit_genesis(&store, &[]);
    assert_eq!(bd, digest::sha256(b""));
    let rendered = store.render_verified(vid, bd).expect("render empty");
    assert!(rendered.files.is_empty());
}

#[test]
fn first_parent_lineage_and_missing_parent() {
    let scratch = Scratch::new("lineage");
    let store = Store::init(&scratch.0).expect("init");

    // genesis
    let f1 = [ImportFile {
        path: "SKILL.md",
        mode: FileMode::Regular,
        bytes: b"v1\n",
    }];
    let (vid1, _bd1) = commit_genesis(&store, &f1);

    // child of genesis (a new version with the genesis as parents[0])
    let th2 = store
        .write_bundle(&[ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"v2\n",
        }])
        .expect("write2");
    let vid2 = sign::commit_id(&Commit {
        parents: &[vid1],
        tree: th2.bundle_digest,
        author: AUTHOR,
        message: "edit",
    })
    .unwrap();
    store
        .commit(vid2, &[vid1], &th2, AUTHOR, "edit")
        .expect("commit2");

    // log from the tip walks first-parent back to genesis.
    let log = store.log(vid2).expect("log");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].version_id, vid2);
    assert_eq!(log[0].parents, vec![vid1]);
    assert_eq!(log[1].version_id, vid1);
    assert!(log[1].parents.is_empty());

    // list_versions sees both.
    let mut versions = store.list_versions().expect("list");
    versions.sort();
    let mut want = vec![vid1, vid2];
    want.sort();
    assert_eq!(versions, want);

    // committing against an unknown parent fails typed (never silently orphaned).
    let th3 = store
        .write_bundle(&[ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"v3\n",
        }])
        .expect("write3");
    let bogus_parent = [0x99u8; 32];
    let vid3 = sign::commit_id(&Commit {
        parents: &[bogus_parent],
        tree: th3.bundle_digest,
        author: AUTHOR,
        message: "edit3",
    })
    .unwrap();
    let err = store
        .commit(vid3, &[bogus_parent], &th3, AUTHOR, "edit3")
        .unwrap_err();
    assert!(matches!(err, crate::GitstoreError::MissingParent));
}

#[test]
fn commit_refuses_a_lying_version_id() {
    let scratch = Scratch::new("lying");
    let store = Store::init(&scratch.0).expect("init");
    let th = store
        .write_bundle(&[ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"hi\n",
        }])
        .expect("write");
    // A version_id that is NOT commit_id(args) must be refused before any ref is written.
    let lie = [0x42u8; 32];
    let err = store.commit(lie, &[], &th, AUTHOR, MESSAGE).unwrap_err();
    assert!(matches!(err, crate::GitstoreError::VersionMismatch));
    assert!(store.list_versions().expect("list").is_empty());
}

#[test]
fn verify_rejects_a_swapped_tree_under_a_versions_ref() {
    // The integrity proof: re-point a version ref at a DIFFERENT (valid) tree. gix reads it happily
    // (sha-1 checks out), but the recomputed bundle_digest no longer matches the pin -> typed mismatch.
    let scratch = Scratch::new("swap");
    let store = Store::init(&scratch.0).expect("init");
    let (vid, bd) = commit_genesis(
        &store,
        &[ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"original\n",
        }],
    );
    // It verifies before tampering.
    assert!(store.render_verified(vid, bd).is_ok());

    // Forge a different-content tree + commit, then aim vid's ref at it.
    let forged_tree = forge_tree(&store, b"SKILL.md", EntryKind::Blob, b"TAMPERED\n");
    let forged_commit = forge_commit(&store, forged_tree);
    force_version_ref(&store, vid, forged_commit);

    let err = store.render_verified(vid, bd).unwrap_err();
    assert!(
        matches!(err, VerifyError::BundleDigestMismatch),
        "expected BundleDigestMismatch, got {err:?}"
    );
}

#[test]
fn verify_rejects_a_corrupted_loose_object() {
    // Flip a byte inside a stored loose object: render must fail typed (no bad bytes ever returned),
    // whether gix's own check or our sha256 recompute catches it.
    let scratch = Scratch::new("corrupt");
    let store = Store::init(&scratch.0).expect("init");
    let (vid, bd) = commit_genesis(
        &store,
        &[ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"some content to corrupt\n",
        }],
    );
    assert!(store.render_verified(vid, bd).is_ok());

    // Find a loose object file and flip its last byte.
    let objects = store.git_dir().join("objects");
    let mut corrupted = false;
    for shard in std::fs::read_dir(&objects).unwrap() {
        let shard = shard.unwrap().path();
        let name = shard.file_name().unwrap().to_string_lossy().into_owned();
        if name.len() != 2 || !shard.is_dir() {
            continue;
        }
        if let Some(obj) = std::fs::read_dir(&shard).unwrap().next() {
            let obj = obj.unwrap().path();
            let mut bytes = std::fs::read(&obj).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
            // git writes loose objects read-only (0444) — make it writable before overwriting.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&obj, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(&obj, &bytes).unwrap();
            corrupted = true;
            break;
        }
    }
    assert!(corrupted, "expected at least one loose object to corrupt");
    // Re-open from disk so gix re-reads the corrupted bytes rather than serving a cached object.
    let fresh = Store::open(&scratch.0).expect("reopen");
    assert!(
        fresh.render_verified(vid, bd).is_err(),
        "a corrupted loose object must fail typed on read"
    );
}

#[test]
fn verify_rejects_a_non_utf8_tree_name() {
    let scratch = Scratch::new("nonutf8");
    let store = Store::init(&scratch.0).expect("init");
    let tree = forge_tree(&store, b"bad\xff\xfename.md", EntryKind::Blob, b"x");
    let commit = forge_commit(&store, tree);
    let vid = [0x11u8; 32];
    force_version_ref(&store, vid, commit);
    let err = store.render_verified(vid, [0u8; 32]).unwrap_err();
    assert!(matches!(err, VerifyError::NonUtf8Name), "got {err:?}");
}

#[test]
fn verify_rejects_a_non_blob_entry() {
    // A symlink (gitlink-style) entry the scanner would never have written must be refused on read.
    let scratch = Scratch::new("nonblob");
    let store = Store::init(&scratch.0).expect("init");
    let tree = forge_tree(&store, b"link", EntryKind::Link, b"/etc/passwd");
    let commit = forge_commit(&store, tree);
    let vid = [0x22u8; 32];
    force_version_ref(&store, vid, commit);
    let err = store.render_verified(vid, [0u8; 32]).unwrap_err();
    assert!(matches!(err, VerifyError::NonBlobEntry), "got {err:?}");
}

#[test]
fn durability_set_names_the_whole_store_not_just_objects() {
    // A crash-safe add must fsync everything needed to OPEN the store, not only the loose objects:
    // a doc that names a commit must never become durable while the store can't be opened.
    let scratch = Scratch::new("durset");
    let store = Store::init(&scratch.0).expect("init");
    commit_genesis(
        &store,
        &[ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"x\n",
        }],
    );
    let batch = store.durability_set().expect("durability set");
    let files: Vec<String> = batch
        .files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let dirs: Vec<String> = batch
        .dirs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // The repo metadata gix wrote outside any fs seam.
    assert!(
        files.iter().any(|f| f.ends_with("/HEAD")),
        "HEAD missing: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.ends_with("/config")),
        "config missing"
    );
    // The repo root + the parent dirs of the loose objects + the version ref.
    assert!(dirs.iter().any(|d| d.ends_with("/objects")), "objects dir");
    assert!(dirs.iter().any(|d| d.ends_with("/refs")), "refs dir");
    assert!(
        files.iter().any(|f| f.contains("/refs/topos/versions/")),
        "the version ref must be named"
    );
    assert!(
        files.iter().filter(|f| f.contains("/objects/")).count() >= 1,
        "at least one loose object"
    );
}

#[test]
fn fence_stage_install_commit_render_and_delete() {
    // The fence primitives in isolation: stage into a quarantine, install into main durably, record the
    // version from the installed ids (no blob re-write), render byte-exact, then physically unlink.
    let main_scratch = Scratch::new("fence-main");
    let q_scratch = Scratch::new("fence-q");
    let main = Store::init(&main_scratch.0).expect("init main");
    let files = [
        ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"# skill\n",
        },
        ImportFile {
            path: "scripts/run.sh",
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho hi\n",
        },
    ];
    let staged = Store::stage(&q_scratch.0, &files).expect("stage");
    assert_eq!(staged.entries.len(), 2);

    // Staged bytes are in the quarantine, NOT yet in main.
    for e in &staged.entries {
        assert!(!main.object_exists(e.git_oid).expect("exists"));
    }

    // Install each object into main durably (the op names what it fsynced).
    let quarantine = Store::open(&q_scratch.0).expect("open quarantine");
    for e in &staged.entries {
        let batch = main
            .install_object_durable(&quarantine, e.git_oid)
            .expect("install");
        assert!(
            !batch.files.is_empty(),
            "install must name a synced object file"
        );
        assert!(main.object_exists(e.git_oid).expect("exists after install"));
    }

    // Record the version from the installed ids (write_tree builds the tree WITHOUT writing blobs).
    let entries: Vec<(&str, FileMode, [u8; 20])> = staged
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.mode, e.git_oid))
        .collect();
    let vid = genesis_version_id(staged.bundle_digest);
    main.commit_durable(vid, &[], &entries, staged.bundle_digest, AUTHOR, MESSAGE)
        .expect("commit_durable");

    // It renders byte-exact from its final path (placement-independent identity holds).
    let rendered = main
        .render_verified(vid, staged.bundle_digest)
        .expect("render");
    assert_eq!(rendered.files.len(), 2);

    // Physically unlink one object → a fresh store can no longer render the version.
    main.delete_loose_object(staged.entries[0].git_oid)
        .expect("delete");
    let fresh = Store::open(&main_scratch.0).expect("reopen");
    assert!(
        fresh.render_verified(vid, staged.bundle_digest).is_err(),
        "the unlinked object must be physically gone"
    );
    // Re-delete is idempotent (the recovery sweep re-running).
    main.delete_loose_object(staged.entries[0].git_oid)
        .expect("idempotent re-delete");
}

#[test]
fn install_refuses_a_corrupted_staged_object() {
    // A quarantine object corrupted after staging must NOT be installed-as-present: install fails (whether
    // gix rejects the unreadable object or our written-id check catches mismatched bytes), so the authority
    // never marks an object present whose bytes are not actually at its locator.
    let main_scratch = Scratch::new("fence-corrupt-main");
    let q_scratch = Scratch::new("fence-corrupt-q");
    let main = Store::init(&main_scratch.0).expect("init main");
    let files = [ImportFile {
        path: "SKILL.md",
        mode: FileMode::Regular,
        bytes: b"content to corrupt in quarantine\n",
    }];
    let staged = Store::stage(&q_scratch.0, &files).expect("stage");

    // Flip a byte inside the quarantine's loose object.
    let objects = q_scratch.0.join("objects");
    let mut corrupted = false;
    for shard in std::fs::read_dir(&objects).unwrap() {
        let shard = shard.unwrap().path();
        if !shard.is_dir() || shard.file_name().unwrap().to_string_lossy().len() != 2 {
            continue;
        }
        if let Some(obj) = std::fs::read_dir(&shard).unwrap().next() {
            let obj = obj.unwrap().path();
            let mut bytes = std::fs::read(&obj).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&obj, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(&obj, &bytes).unwrap();
            corrupted = true;
            break;
        }
    }
    assert!(corrupted, "expected a loose object to corrupt");

    // Re-open the quarantine so gix re-reads the corrupted bytes, then install must refuse.
    let quarantine = Store::open(&q_scratch.0).expect("reopen quarantine");
    assert!(
        main.install_object_durable(&quarantine, staged.entries[0].git_oid)
            .is_err(),
        "installing a corrupted staged object must fail, never silently mark it present"
    );
}

#[test]
fn render_rejects_missing_version() {
    let scratch = Scratch::new("missing");
    let store = Store::init(&scratch.0).expect("init");
    let err = store.render_verified([0x7u8; 32], [0u8; 32]).unwrap_err();
    assert!(matches!(err, VerifyError::MissingVersion));
}

// --- forging helpers (adversarial; bypass the safe write path on purpose) ---

fn forge_tree(store: &Store, filename: &[u8], kind: EntryKind, content: &[u8]) -> gix::ObjectId {
    let blob = store.repo().write_blob(content).unwrap().detach();
    let tree = gix::objs::Tree {
        entries: vec![gix::objs::tree::Entry {
            mode: kind.into(),
            filename: gix::bstr::BString::from(filename.to_vec()),
            oid: blob,
        }],
    };
    store.repo().write_object(&tree).unwrap().detach()
}

fn forge_commit(store: &Store, tree_oid: gix::ObjectId) -> gix::ObjectId {
    let time = gix::date::Time::new(0, 0);
    let who = gix::actor::Signature {
        name: "x".into(),
        email: "x@x".into(),
        time,
    };
    let mut buf_a = gix::date::parse::TimeBuf::default();
    let mut buf_c = gix::date::parse::TimeBuf::default();
    store
        .repo()
        .new_commit_as(
            who.to_ref(&mut buf_c),
            who.to_ref(&mut buf_a),
            "forge",
            tree_oid,
            gix::commit::NO_PARENT_IDS,
        )
        .unwrap()
        .id
}

fn force_version_ref(store: &Store, version_id: [u8; 32], commit_oid: gix::ObjectId) {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    let name = crate::store::version_ref_name(&version_id);
    store
        .repo()
        .edit_reference(RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "forge".into(),
                },
                expected: PreviousValue::Any,
                new: gix::refs::Target::Object(commit_oid),
            },
            name: name.try_into().unwrap(),
            deref: false,
        })
        .unwrap();
}

/// A tiny deterministic xorshift64* — varies the fuzz inputs without an RNG dependency or ambient state.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn stage_into_an_existing_quarantine_dir_re_stages_fresh() {
    // A pre-merge review finding: `ingest` may re-stage under a reused op id (the authority's quarantine row is
    // an upsert, so reuse is a supported retry path), so `stage` must tolerate a pre-existing quarantine dir —
    // `init_bare` alone rejects a non-empty one. A re-stage yields a FRESH quarantine holding exactly the new
    // candidate, never the stale prior one.
    let q = Scratch::new("fence-restage");
    let first = [ImportFile {
        path: "a.md",
        mode: FileMode::Regular,
        bytes: b"first",
    }];
    let s1 = Store::stage(&q.0, &first).expect("first stage");
    assert_eq!(s1.entries.len(), 1);

    // Re-stage into the SAME dir with different content — must succeed (not fail on the leftover repo).
    let second = [
        ImportFile {
            path: "a.md",
            mode: FileMode::Regular,
            bytes: b"second",
        },
        ImportFile {
            path: "b.md",
            mode: FileMode::Regular,
            bytes: b"more",
        },
    ];
    let s2 = Store::stage(&q.0, &second).expect("re-stage into the existing quarantine dir");
    assert_eq!(s2.entries.len(), 2);

    // The re-staged quarantine holds the NEW objects and not the stale first one (cleared, not merged).
    let q_store = Store::open(&q.0).expect("open re-staged quarantine");
    for e in &s2.entries {
        assert!(
            q_store
                .object_exists(e.git_oid)
                .expect("new object present"),
            "the re-staged candidate's objects must be present"
        );
    }
    assert!(
        !q_store
            .object_exists(s1.entries[0].git_oid)
            .expect("stale check"),
        "the prior candidate's object must be cleared, not preserved"
    );
}
