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
fn a_manifest_valued_file_is_not_addressable_as_the_version_or_digest_it_renders() {
    // The three sha256 id spaces (content id / bundle digest / version id) can collide numerically:
    // a file whose bytes are bundle A's rendered manifest has a content id equal to A's bundle_digest.
    // Pin that every store lookup stays space-scoped — the value resolves as a CONTENT id only inside
    // the version whose tree actually carries it, never as a version ref, and never inside A itself
    // (where the same 32 bytes are a digest, not a leaf).
    let scratch = Scratch::new("idspace");
    let store = Store::init(&scratch.0).expect("init");
    let a_files = [ImportFile {
        path: "SKILL.md",
        mode: FileMode::Regular,
        bytes: b"# the described bundle\n",
    }];
    let (vid_a, digest_a) = commit_genesis(&store, &a_files);

    let entries: Vec<digest::ManifestEntry> = a_files
        .iter()
        .map(|f| digest::ManifestEntry {
            path: f.path.to_string(),
            mode: f.mode,
            content_sha256: digest::sha256(f.bytes),
        })
        .collect();
    let manifest = digest::canonical_manifest(&entries).expect("manifest");
    assert_eq!(digest::sha256(manifest.as_bytes()), digest_a);

    // The carrier bundle holds A's manifest bytes as an ordinary file.
    let b_files = [ImportFile {
        path: "manifest.txt",
        mode: FileMode::Regular,
        bytes: manifest.as_bytes(),
    }];
    let (vid_b, _) = commit_genesis(&store, &b_files);

    // Content space: readable only within the carrier's tree; inside A it is a typed miss.
    assert_eq!(
        store
            .read_object_in_version(vid_b, digest_a)
            .expect("the carrier's leaf reads by its content id"),
        manifest.as_bytes()
    );
    assert!(matches!(
        store.read_object_in_version(vid_a, digest_a),
        Err(VerifyError::ObjectNotInVersion)
    ));
    // Version space: a bundle digest is never a version id — no ref, nothing renders.
    assert!(matches!(
        store.render_verified(digest_a, digest_a),
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
fn read_commit_meta_reads_exact_metadata_and_fails_on_an_unmapped_parent() {
    let scratch = Scratch::new("commit-meta");
    let store = Store::init(&scratch.0).expect("init");

    // genesis, then a child with a distinct author + message.
    let (vid1, _bd1) = commit_genesis(
        &store,
        &[ImportFile {
            path: "SKILL.md",
            mode: FileMode::Regular,
            bytes: b"v1\n",
        }],
    );
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
        author: "alice",
        message: "second",
    })
    .unwrap();
    store
        .commit(vid2, &[vid1], &th2, "alice", "second")
        .expect("commit2");

    // Exact metadata for the child: its id, the COMPLETE parent set, and the display author + message
    // (cross-checked against `log`, which decodes the same commit identically).
    let meta = store.read_commit_meta(vid2).expect("read_commit_meta");
    assert_eq!(meta.version_id, vid2);
    assert_eq!(meta.parents, vec![vid1]);
    assert_eq!(meta.author, "alice");
    let log0 = store.log(vid2).expect("log");
    assert_eq!(meta.author, log0[0].author);
    assert_eq!(meta.message, log0[0].message);

    // A genesis commit has no parents.
    let g = store
        .read_commit_meta(vid1)
        .expect("read_commit_meta genesis");
    assert!(g.parents.is_empty());

    // Delete vid1's version ref so the child's parent maps to no known version. read_commit_meta must FAIL
    // (never silently drop the parent, unlike `log`'s lenient first-parent walk). Re-open to read fresh refs.
    let ref_path = store
        .git_dir()
        .join("refs/topos/versions")
        .join(digest::to_hex(&vid1));
    std::fs::remove_file(&ref_path).expect("remove vid1 ref");
    let store = Store::open(store.git_dir()).expect("reopen");
    assert!(matches!(
        store.read_commit_meta(vid2),
        Err(VerifyError::UnmappedParent)
    ));
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

// ===== The local-filesystem large-object store (the size-routed offload backend) =====

use crate::error::GitstoreError;
use crate::largeobj::{LargeObjectStore, LocalLargeStore};

/// Reconstruct a blob's final shard path under a store root (the store keeps this private; the test rebuilds
/// it to assert layout + to inject at-rest corruption).
fn large_final_path(root: &std::path::Path, id: &[u8; 32]) -> PathBuf {
    let hex = digest::to_hex(id);
    root.join("objects")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(&hex)
}

#[test]
fn large_store_round_trips_every_byte_and_shards_by_hash() {
    let scratch = Scratch::new("large-rt");
    let store = LocalLargeStore::new(scratch.0.clone());
    // empty, small, and arbitrary-byte (incl. NUL/high) payloads all round-trip byte-exact.
    for bytes in [
        b"".to_vec(),
        b"hello".to_vec(),
        (0u16..512).map(|n| (n & 0xff) as u8).collect::<Vec<u8>>(),
    ] {
        let id = digest::sha256(&bytes);
        store.put(id, &bytes).expect("put");
        assert_eq!(
            store.get(id).expect("get"),
            bytes,
            "round-trip changed bytes"
        );
        assert!(store.exists(id).expect("exists"));
        // It landed at the sharded content-addressed path.
        assert!(
            large_final_path(&scratch.0, &id).is_file(),
            "blob must be installed at objects/aa/bb/<hex>"
        );
    }
}

#[test]
fn large_store_put_rejects_a_mislabeled_id_and_writes_nothing() {
    let scratch = Scratch::new("large-bad-put");
    let store = LocalLargeStore::new(scratch.0.clone());
    let bytes = b"the real bytes";
    let wrong_id = digest::sha256(b"a different content id");
    assert!(matches!(
        store.put(wrong_id, bytes),
        Err(GitstoreError::BlobIntegrity)
    ));
    // Nothing reaches the final path under the wrong id, and no temp lingers as a referenced object.
    assert!(!large_final_path(&scratch.0, &wrong_id).exists());
    assert!(!store.exists(wrong_id).expect("exists"));
}

#[test]
fn large_store_get_detects_at_rest_corruption() {
    let scratch = Scratch::new("large-rot");
    let store = LocalLargeStore::new(scratch.0.clone());
    let bytes = b"authentic bytes that will be tampered with on disk";
    let id = digest::sha256(bytes);
    store.put(id, bytes).expect("put");
    // Tamper the stored file in place (a bit-rot / swap), keeping the same path/id.
    std::fs::write(large_final_path(&scratch.0, &id), b"tampered").expect("corrupt");
    assert!(
        matches!(store.get(id), Err(GitstoreError::BlobIntegrity)),
        "verify-on-read must refuse bytes that no longer hash to the id"
    );
}

#[test]
fn large_store_delete_is_idempotent_and_put_self_heals() {
    let scratch = Scratch::new("large-del");
    let store = LocalLargeStore::new(scratch.0.clone());
    let bytes = b"deletable + re-installable";
    let id = digest::sha256(bytes);

    store.put(id, bytes).expect("put");
    assert!(store.exists(id).expect("exists"));
    store.delete(id).expect("delete");
    assert!(!store.exists(id).expect("exists after delete"));
    // Deleting an already-absent object is a no-op (the recovery sweep re-running).
    store.delete(id).expect("idempotent re-delete");
    assert!(matches!(store.get(id), Err(GitstoreError::Io(_))));

    // A re-put of byte-identical content re-installs (overwrite — the migrate re-materialize belt's path).
    store.put(id, bytes).expect("re-put");
    assert_eq!(store.get(id).expect("get"), bytes);
    // And a re-put over an EXISTING (here truncated) file overwrites it with verified bytes (self-heal).
    std::fs::write(large_final_path(&scratch.0, &id), b"truncated").expect("truncate");
    store.put(id, bytes).expect("re-put over a damaged final");
    assert_eq!(store.get(id).expect("get heals"), bytes);
}

#[test]
fn large_store_per_root_isolation_has_no_cross_root_dedup() {
    // Two workspaces are two separate roots: byte-identical content is two distinct physical objects, and
    // one root's handle never sees the other's bytes (the hard tenant boundary is the path, not just an ACL).
    let scratch = Scratch::new("large-tenant");
    let root_a = scratch.0.join("wsA");
    let root_b = scratch.0.join("wsB");
    let a = LocalLargeStore::new(root_a.clone());
    let b = LocalLargeStore::new(root_b.clone());
    let bytes = b"identical content uploaded by two different workspaces";
    let id = digest::sha256(bytes);

    a.put(id, bytes).expect("put A");
    assert!(a.exists(id).expect("A exists"));
    assert!(
        !b.exists(id).expect("B exists"),
        "a different per-workspace root must NOT see workspace A's object"
    );
    b.put(id, bytes).expect("put B");
    // Two distinct physical files under separate roots — no cross-workspace content dedup.
    let (pa, pb) = (
        large_final_path(&root_a, &id),
        large_final_path(&root_b, &id),
    );
    assert_ne!(pa, pb);
    assert!(pa.is_file() && pb.is_file(), "each root holds its own copy");
}

/// Build a version whose tree references an OFFLOADED blob (staged, but never installed into the main git
/// store) — the on-disk shape the size-routed migrate produces — and return (main store, staged bundle).
fn version_with_one_offloaded_blob(root: &std::path::Path) -> (Store, crate::StagedBundle) {
    let q_dir = root.join("q");
    let main_dir = root.join("main");
    let small: &[u8] = b"# small prose, stays in git\n";
    let big: &[u8] = b"PRETEND THIS IS A BIG OFFLOADED BLOB whose bytes never enter the git store";
    let nested: &[u8] = b"#!/bin/sh\necho nested git blob\n";
    let files = [
        ImportFile {
            path: "small.txt",
            mode: FileMode::Regular,
            bytes: small,
        },
        ImportFile {
            path: "big.bin",
            mode: FileMode::Regular,
            bytes: big,
        },
        ImportFile {
            path: "scripts/run.sh",
            mode: FileMode::Executable,
            bytes: nested,
        },
    ];
    let staged = Store::stage(&q_dir, &files).expect("stage");
    let quarantine = Store::open(&q_dir).expect("open quarantine");
    let main = Store::init(&main_dir).expect("init main");
    // Install every blob EXCEPT big.bin into the main store — big.bin is "offloaded" (absent from git).
    for e in &staged.entries {
        if e.path != "big.bin" {
            main.install_object_durable(&quarantine, e.git_oid)
                .expect("install git-resident blob");
        }
    }
    let entries: Vec<(&str, FileMode, [u8; 20])> = staged
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.mode, e.git_oid))
        .collect();
    let vid = sign::commit_id(&Commit {
        parents: &[],
        tree: staged.bundle_digest,
        author: AUTHOR,
        message: MESSAGE,
    })
    .expect("vid");
    main.commit_durable(vid, &[], &entries, staged.bundle_digest, AUTHOR, MESSAGE)
        .expect("commit_durable tolerates the absent offloaded blob");
    (main, staged)
}

#[test]
fn read_tree_structure_yields_every_leaf_including_an_offloaded_one() {
    let scratch = Scratch::new("tree-struct");
    std::fs::create_dir_all(&scratch.0).unwrap();
    let (main, staged) = version_with_one_offloaded_blob(&scratch.0);
    let vid = sign::commit_id(&Commit {
        parents: &[],
        tree: staged.bundle_digest,
        author: AUTHOR,
        message: MESSAGE,
    })
    .expect("vid");

    // The structure walk yields ALL files (paths/modes/git_oids) without reading any blob — so the offloaded
    // big.bin (absent from git) is yielded fine, with the git_oid the tree entry recorded.
    let leaves = main.read_tree_structure(vid).expect("read_tree_structure");
    let mut by_path: std::collections::HashMap<&str, &crate::TreeLeaf> =
        leaves.iter().map(|l| (l.path.as_str(), l)).collect();
    let mut paths: Vec<&str> = by_path.keys().copied().collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["big.bin", "scripts/run.sh", "small.txt"]);
    for e in &staged.entries {
        let leaf = by_path
            .remove(e.path.as_str())
            .expect("leaf for staged entry");
        assert_eq!(
            leaf.git_oid, e.git_oid,
            "leaf git_oid matches the staged entry"
        );
        assert_eq!(leaf.mode, e.mode, "leaf mode matches");
    }
}

#[test]
fn read_git_blob_verified_returns_bytes_or_a_typed_miss_for_an_offloaded_blob() {
    let scratch = Scratch::new("git-blob-verified");
    std::fs::create_dir_all(&scratch.0).unwrap();
    let (main, staged) = version_with_one_offloaded_blob(&scratch.0);

    let small = staged
        .entries
        .iter()
        .find(|e| e.path == "small.txt")
        .unwrap();
    let big = staged.entries.iter().find(|e| e.path == "big.bin").unwrap();

    // A git-resident blob comes back with its bytes + recomputed sha256 (== the staged object_id).
    let (bytes, sha) = main
        .read_git_blob_verified(small.git_oid)
        .expect("git-resident blob reads");
    assert_eq!(digest::sha256(&bytes), small.object_id);
    assert_eq!(sha, small.object_id);

    // The offloaded blob's git object is absent → the typed MissingObject (NEVER inferred as "offloaded";
    // the database's location is what decides offload — this dumb primitive only reports the git miss).
    assert!(matches!(
        main.read_git_blob_verified(big.git_oid),
        Err(VerifyError::MissingObject)
    ));
}

// ===== The unified-diff renderer: patch-apply round-trips (the diff must be machine-appliable) =====

use crate::{DiffFile, unified_diff};

/// Reconstruct `new` from `old` + a rendered single-file unified diff — the test-side patch applier.
/// It asserts what `patch`/`git apply` enforce: every context/`-` line must match `old` at the cursor
/// and each hunk must start at or after the previous hunk's end — so an overlapping or misaligned
/// hunk set fails the test rather than silently double-applying.
fn apply_unified(old: &str, diff: &str) -> String {
    let old_lines: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.split_inclusive('\n').collect()
    };
    let mut out = String::new();
    let mut cursor = 0usize; // the next unconsumed old line (0-based)
    let mut lines = diff.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ -") {
            let old_range = rest.split_once(' ').expect("hunk header").0;
            let (s, n) = old_range.split_once(',').expect("old range");
            let (start, len): (usize, usize) = (s.parse().expect("start"), n.parse().expect("len"));
            // Unified convention: an empty old range is rendered at line 0 (no -1 shift).
            let hunk_start = if len == 0 { start } else { start - 1 };
            assert!(
                hunk_start >= cursor,
                "hunk starts at old line {hunk_start} but line {cursor} was already consumed (overlap)"
            );
            while cursor < hunk_start {
                out.push_str(old_lines[cursor]);
                cursor += 1;
            }
            continue;
        }
        // A body line: the renderer always terminates it with '\n'; a following no-newline marker
        // means the content genuinely lacks one, otherwise the '\n' is part of the content.
        let (op, rest) = line.split_at(1);
        let mut content = String::from(rest.strip_suffix('\n').unwrap_or(rest));
        if lines.peek().is_some_and(|l| l.starts_with("\\ No newline")) {
            lines.next();
        } else {
            content.push('\n');
        }
        match op {
            " " => {
                assert_eq!(
                    old_lines[cursor], content,
                    "context line disagrees with old"
                );
                out.push_str(&content);
                cursor += 1;
            }
            "-" => {
                assert_eq!(old_lines[cursor], content, "deletion disagrees with old");
                cursor += 1;
            }
            "+" => out.push_str(&content),
            other => panic!("unexpected diff line prefix {other:?} in {line:?}"),
        }
    }
    while cursor < old_lines.len() {
        out.push_str(old_lines[cursor]);
        cursor += 1;
    }
    out
}

/// Render + apply one Regular-mode text file pair, asserting the rebuilt bytes equal `new`.
fn assert_diff_round_trips(old: &str, new: &str, label: &str) {
    let base = [DiffFile {
        path: "t.md",
        mode: FileMode::Regular,
        bytes: old.as_bytes(),
    }];
    let draft = [DiffFile {
        path: "t.md",
        mode: FileMode::Regular,
        bytes: new.as_bytes(),
    }];
    let out = unified_diff(&base, &draft);
    if old == new {
        assert!(
            out.is_empty(),
            "{label}: identical bytes must render nothing"
        );
        return;
    }
    let rebuilt = apply_unified(old, &out);
    assert_eq!(
        rebuilt, new,
        "{label}: applying the rendered hunks must reconstruct new\nold:\n{old}\nnew:\n{new}\ndiff:\n{out}"
    );
}

#[test]
fn unified_diff_round_trips_two_edits_across_every_gap() {
    // Two one-line edits at every separation 0..=12 equal lines — sweeping straight through the
    // hunk-merge boundary (gap <= 2*CONTEXT merges, >= 2*CONTEXT+1 splits): both shapes must apply.
    for gap in 0..=12 {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..4 {
            old.push_str(&format!("lead{i}\n"));
            new.push_str(&format!("lead{i}\n"));
        }
        old.push_str("first-old\n");
        new.push_str("first-new\n");
        for i in 0..gap {
            old.push_str(&format!("mid{i}\n"));
            new.push_str(&format!("mid{i}\n"));
        }
        old.push_str("second-old\n");
        new.push_str("second-new\n");
        for i in 0..4 {
            old.push_str(&format!("tail{i}\n"));
            new.push_str(&format!("tail{i}\n"));
        }
        assert_diff_round_trips(&old, &new, &format!("gap {gap}"));
    }
}

#[test]
fn unified_diff_round_trips_generated_line_edits() {
    // The same deterministic-xorshift fuzz shape as the put→render round-trip, aimed at the diff
    // renderer: random old/new line sequences (repeated lines, empty files, missing trailing
    // newlines, edits at every distance) → render → apply → must equal new, byte-exact.
    let pool: &[&str] = &[
        "alpha\n",
        "bravo\n",
        "charlie\n",
        "delta\n",
        "echo\n",
        "alpha\n",
        "bravo\n",
    ];
    let mut rng = Rng::new(0xD1FF_5EED_0BAD_CAFE);

    // A file of up to 30 pool lines; ~1 in 8 drops the trailing newline.
    fn gen_file(rng: &mut Rng, pool: &[&str]) -> String {
        let n = (rng.next() % 31) as usize;
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(pool[(rng.next() % pool.len() as u64) as usize]);
        }
        if !s.is_empty() && rng.next().is_multiple_of(8) {
            s.pop();
        }
        s
    }

    // Derive `new` from `old` by 1..=4 random splices (delete / insert / replace short runs) —
    // biased toward the nearby-change-runs region the hunk grouper has to get right.
    fn mutate(rng: &mut Rng, old: &str, pool: &[&str]) -> String {
        let mut lines: Vec<String> = old.split_inclusive('\n').map(String::from).collect();
        for _ in 0..=(rng.next() % 4) {
            let at = if lines.is_empty() {
                0
            } else {
                (rng.next() % (lines.len() as u64 + 1)) as usize
            };
            let k = (rng.next() % 3) as usize + 1;
            match rng.next() % 3 {
                0 => {
                    let end = (at + k).min(lines.len());
                    lines.drain(at..end);
                }
                1 => {
                    for i in 0..k {
                        let l = pool[(rng.next() % pool.len() as u64) as usize];
                        lines.insert(at + i, String::from(l));
                    }
                }
                _ => {
                    let end = (at + k).min(lines.len());
                    let repl = pool[(rng.next() % pool.len() as u64) as usize];
                    lines.splice(at..end, [String::from(repl)]);
                }
            }
        }
        let mut s: String = lines.concat();
        if !s.is_empty() && rng.next().is_multiple_of(8) && s.ends_with('\n') {
            s.pop();
        }
        s
    }

    for case in 0..300 {
        let old = gen_file(&mut rng, pool);
        let new = if rng.next().is_multiple_of(4) {
            gen_file(&mut rng, pool) // unrelated contents
        } else {
            mutate(&mut rng, &old, pool)
        };
        assert_diff_round_trips(&old, &new, &format!("case {case}"));
    }
}

#[test]
fn write_tree_rejects_a_dotgit_component_like_the_high_level_editor() {
    // The plumbing tree build must still reject the dangerous path components the high-level (client) editor
    // rejects — a `.git` directory and the like — even though it skips the child-existence check for offload.
    let scratch = Scratch::new("wt-validate");
    std::fs::create_dir_all(&scratch.0).unwrap();
    let store = Store::init(&scratch.0).unwrap();
    let oid = store.repo().write_blob(b"x").unwrap().detach();
    let git_oid: [u8; 20] = oid.as_slice().try_into().unwrap();

    for bad in [".git/config", "a/.git/b"] {
        assert!(
            matches!(
                store.write_tree(&[(bad, FileMode::Regular, git_oid)]),
                Err(GitstoreError::RejectPath(_))
            ),
            "a .git component ({bad}) must be rejected"
        );
    }
    // A normal nested path is accepted.
    assert!(
        store
            .write_tree(&[("scripts/run.sh", FileMode::Regular, git_oid)])
            .is_ok()
    );
}
