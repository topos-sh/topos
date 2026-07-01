//! The canonical bundle manifest + the byte-exact `bundle_digest` — the unit of consent.
//!
//! The digest is a plain sha256 over a canonical, line-oriented manifest. **Nothing is normalized**
//! (content, line-endings, frontmatter, encoding, whitespace are byte-exact) — different bytes are
//! never "the same." The digest is **placement-independent**: it depends only on each file's
//! content hash, mode, and path, never on where the bytes are stored.
//!
//! This module is pure: the caller does the filesystem walk (and rejects symlinks / devices /
//! non-regular files there), then hands the kernel `(path, mode, content_sha256)` per file. The
//! byte-pure path checks live here.
//!
//! ## Three id spaces, never conflated
//!
//! sha256 names three DISTINCT id spaces in this system: a file's **content id** (`sha256(raw
//! bytes)`), the **bundle digest** (`sha256(canonical manifest)`), and the **commit id**
//! (`sha256(canonical commit frame)` — [`crate::sign`]). All are 32 bytes, and a value can equal a
//! value of another space by construction — a file whose bytes ARE a rendered manifest has a content
//! id equal to that manifest's bundle digest — so the three spaces must never share a column, a
//! lookup, or a dedup key. A cross-kind by-hash lookup (or a combined cache) would let one space
//! answer for another; every resolution must be space-scoped (a content id only within an authorized
//! version's tree, a version id only under the version-ref namespace, a digest only as a pin).

use alloc::string::String;
use alloc::vec::Vec;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// A file's mode in the canonical manifest — git's two regular-file modes, the only ones allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// `100644` — a regular, non-executable file.
    Regular,
    /// `100755` — a regular, executable file.
    Executable,
}

impl FileMode {
    /// The literal octal mode string written into the manifest line.
    pub fn as_str(self) -> &'static str {
        match self {
            FileMode::Regular => "100644",
            FileMode::Executable => "100755",
        }
    }
}

/// One file's contribution to the manifest: its bundle-relative UTF-8 path, mode, and the sha256 of
/// its raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: String,
    pub mode: FileMode,
    pub content_sha256: [u8; 32],
}

/// Why a candidate path is rejected at publish — the **byte-pure** subset.
///
/// Filesystem-level rejects (symlinks, devices, other non-regular files) are the caller's job during
/// the walk; the kernel never sees the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// An empty path component or empty path.
    EmptyPath,
    /// An absolute path (leading `/`).
    AbsolutePath,
    /// A `..` parent-traversal component.
    ParentTraversal,
    /// A `.` current-directory component (`./a`, `a/./b`) — distinct manifest text that aliases the
    /// same file, so it is rejected to keep the path canonical.
    DotComponent,
    /// A control character (C0, DEL, or C1) anywhere in the path — would collide with the manifest
    /// line delimiters or fail to round-trip.
    ControlChar,
    /// Two entries share an identical path.
    DuplicatePath,
    /// Two entries collide under Unicode NFC normalization (e.g. precomposed `café` vs decomposed
    /// `cafe\u{301}`) — they would collapse to one file on a normalizing filesystem (macOS APFS/HFS+),
    /// so the digest could "cover" bytes a follower can't faithfully place. Rejected at publish.
    NfcCollision,
    /// Two entries collide under ASCII case-folding (can't both materialize on a case-insensitive FS).
    ///
    /// NOTE: this catches NFC + ASCII-case collisions. **Full Unicode case-fold** (e.g. Kelvin `K`
    /// U+212A folding to `k`) is genuinely version-sensitive — it needs a *pinned Unicode version*, a
    /// freeze decision the spec leaves open (tracked alongside the deferred signing encodings). The
    /// common collisions are caught now; the Unicode-case-fold tail is the documented residual.
    CaseFoldCollision,
}

/// sha256 of raw bytes — the one file-hash implementation; the client hashes each file through this,
/// so the manifest's `content_sha256` and the kernel agree by construction.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Lowercase-hex encoding of bytes (the manifest renders every sha256 as 64 lowercase hex chars).
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Validate one bundle-relative path against the byte-pure reject rules.
///
/// Paths are forward-slash, Unix-relative, UTF-8 — the client normalizes a host path to this form
/// (and rejects symlinks / devices) BEFORE the kernel sees it; a backslash here is just a legal
/// filename byte, so platform separator handling is the client's job, not the digest's.
pub fn check_path(path: &str) -> Result<(), RejectReason> {
    if path.is_empty() {
        return Err(RejectReason::EmptyPath);
    }
    if path.starts_with('/') {
        return Err(RejectReason::AbsolutePath);
    }
    // `char::is_control` covers C0 (U+0000–U+001F), DEL (U+007F) AND C1 (U+0080–U+009F) — the latter
    // are multi-byte in UTF-8, so a raw `b < 0x20` byte test would miss them.
    if path.chars().any(char::is_control) {
        return Err(RejectReason::ControlChar);
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(RejectReason::EmptyPath); // leading/trailing/double slash
        }
        if component == ".." {
            return Err(RejectReason::ParentTraversal);
        }
        if component == "." {
            return Err(RejectReason::DotComponent);
        }
    }
    Ok(())
}

/// Build the canonical manifest string for a set of entries.
///
/// Steps (all byte-exact): reject any invalid path → reject path collisions (exact + ASCII case-fold)
/// → sort entries by raw path bytes → emit one `"<sha256-hex> <mode> <path>\n"` line per entry →
/// concatenate. The caller has already excluded `.git/` / `.DS_Store` and filesystem non-regular
/// files; an empty entry set yields an empty manifest (and the digest of the empty string).
pub fn canonical_manifest(entries: &[ManifestEntry]) -> Result<String, RejectReason> {
    for entry in entries {
        check_path(&entry.path)?;
    }
    reject_collisions(entries)?;

    let mut sorted: Vec<&ManifestEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut manifest = String::new();
    for entry in sorted {
        manifest.push_str(&to_hex(&entry.content_sha256));
        manifest.push(' ');
        manifest.push_str(entry.mode.as_str());
        manifest.push(' ');
        manifest.push_str(&entry.path);
        manifest.push('\n');
    }
    Ok(manifest)
}

/// The bundle digest = `sha256(canonical_manifest(entries))` — the byte-exact unit of consent.
pub fn bundle_digest(entries: &[ManifestEntry]) -> Result<[u8; 32], RejectReason> {
    Ok(sha256(canonical_manifest(entries)?.as_bytes()))
}

/// Normalize a path to the form the collision rules compare on — NFC then ASCII case-fold. Two paths whose
/// normalized forms are EQUAL would collapse to one file on a normalizing / case-insensitive filesystem,
/// which is exactly what [`canonical_manifest`] rejects (as exact / NFC / case-fold collisions). A client
/// that derives a new path (e.g. a conflict `.topos-mine` sidecar) compares candidates under this so it
/// never generates a name the digest would later reject. (Full Unicode case-fold stays the documented
/// version-sensitive residual — see [`RejectReason::CaseFoldCollision`].)
#[must_use]
pub fn normalize_for_collision(path: &str) -> String {
    path.nfc().collect::<String>().to_ascii_lowercase()
}

/// Reject paths that would collapse to the same file on a real filesystem: exact duplicates, NFC
/// normalization collisions, and ASCII case-fold collisions. (Full Unicode case-fold is the documented
/// version-sensitive residual — see [`RejectReason::CaseFoldCollision`].)
fn reject_collisions(entries: &[ManifestEntry]) -> Result<(), RejectReason> {
    // Exact duplicates: sort by path bytes, check neighbours.
    let mut exact: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    exact.sort_unstable();
    for pair in exact.windows(2) {
        if pair[0] == pair[1] {
            return Err(RejectReason::DuplicatePath);
        }
    }
    // NFC collisions: precomposed vs decomposed paths normalize to the same file.
    let mut nfc: Vec<String> = entries.iter().map(|e| e.path.nfc().collect()).collect();
    nfc.sort_unstable();
    for pair in nfc.windows(2) {
        if pair[0] == pair[1] {
            return Err(RejectReason::NfcCollision);
        }
    }
    // ASCII case-fold collisions (over the NFC form): lowercase, sort, check neighbours.
    let mut folded: Vec<String> = nfc.iter().map(|p| p.to_ascii_lowercase()).collect();
    folded.sort_unstable();
    for pair in folded.windows(2) {
        if pair[0] == pair[1] {
            return Err(RejectReason::CaseFoldCollision);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn entry(path: &str, mode: FileMode, content: &[u8]) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            mode,
            content_sha256: sha256(content),
        }
    }

    #[test]
    fn sha256_known_answer_vectors() {
        // The canonical NIST/RFC sha256 KATs — proves we hash raw bytes with no normalization.
        assert_eq!(
            to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn manifest_is_byte_exact_and_sorted_by_path_bytes() {
        // Two files given out of order; expect them sorted by raw path bytes, each on its own line.
        let entries = vec![
            entry("b/run.sh", FileMode::Executable, b"#!/bin/sh\n"),
            entry("a.txt", FileMode::Regular, b"hello\n"),
        ];
        let manifest = canonical_manifest(&entries).unwrap();
        let expected = alloc::format!(
            "{} 100644 a.txt\n{} 100755 b/run.sh\n",
            to_hex(&sha256(b"hello\n")),
            to_hex(&sha256(b"#!/bin/sh\n")),
        );
        assert_eq!(manifest, expected);
    }

    #[test]
    fn digest_golden_vector_is_stable() {
        // A KNOWN-ANSWER vector: this exact input must always produce this exact digest. A change to
        // the canonical form (line format, ordering, mode rendering, hex case) breaks it loudly.
        let entries = vec![
            entry("a.txt", FileMode::Regular, b"hello\n"),
            entry("b/run.sh", FileMode::Executable, b"#!/bin/sh\n"),
        ];
        let digest = to_hex(&bundle_digest(&entries).unwrap());
        assert_eq!(
            digest, "b346bc5e46b56487225ca1975df4a89f3826678feed165f56e8151c366415ee7",
            "digest changed — update only if the canonical form INTENTIONALLY changed",
        );
    }

    #[test]
    fn order_independent_input_same_digest() {
        let a = vec![
            entry("x", FileMode::Regular, b"1"),
            entry("y", FileMode::Regular, b"2"),
        ];
        let b = vec![
            entry("y", FileMode::Regular, b"2"),
            entry("x", FileMode::Regular, b"1"),
        ];
        assert_eq!(bundle_digest(&a).unwrap(), bundle_digest(&b).unwrap());
    }

    #[test]
    fn one_byte_or_mode_change_alters_the_digest() {
        let base = vec![entry("s.sh", FileMode::Regular, b"a")];
        let content_changed = vec![entry("s.sh", FileMode::Regular, b"b")];
        let mode_changed = vec![entry("s.sh", FileMode::Executable, b"a")];
        let path_changed = vec![entry("S.sh", FileMode::Regular, b"a")];
        assert_ne!(
            bundle_digest(&base).unwrap(),
            bundle_digest(&content_changed).unwrap()
        );
        assert_ne!(
            bundle_digest(&base).unwrap(),
            bundle_digest(&mode_changed).unwrap()
        );
        assert_ne!(
            bundle_digest(&base).unwrap(),
            bundle_digest(&path_changed).unwrap()
        );
    }

    #[test]
    fn empty_bundle_is_the_digest_of_the_empty_manifest() {
        assert_eq!(bundle_digest(&[]).unwrap(), sha256(b""));
    }

    #[test]
    fn normalize_for_collision_folds_case_and_nfc() {
        // Two paths that the manifest would reject as collisions normalize to the SAME string, so a client
        // can dedup derived paths under the same rule.
        assert_eq!(
            normalize_for_collision("LOGO.BIN.TOPOS-MINE"),
            normalize_for_collision("logo.bin.topos-mine")
        );
        // Precomposed "café" vs decomposed "cafe\u{301}" normalize equal.
        assert_eq!(
            normalize_for_collision("caf\u{e9}"),
            normalize_for_collision("cafe\u{301}")
        );
        // Genuinely distinct paths stay distinct.
        assert_ne!(
            normalize_for_collision("a.topos-mine"),
            normalize_for_collision("a.topos-mine-1")
        );
    }

    #[test]
    fn rejects_forbidden_paths() {
        assert_eq!(check_path("/etc/passwd"), Err(RejectReason::AbsolutePath));
        assert_eq!(check_path("../escape"), Err(RejectReason::ParentTraversal));
        assert_eq!(check_path("a/../b"), Err(RejectReason::ParentTraversal));
        assert_eq!(check_path("./a"), Err(RejectReason::DotComponent));
        assert_eq!(check_path("a/./b"), Err(RejectReason::DotComponent));
        assert_eq!(check_path("with\0nul"), Err(RejectReason::ControlChar));
        assert_eq!(check_path("with\nnewline"), Err(RejectReason::ControlChar));
        // A C1 control (U+0085 NEL) is multi-byte UTF-8 — a raw byte test would miss it.
        assert_eq!(check_path("a\u{85}b"), Err(RejectReason::ControlChar));
        assert_eq!(check_path(""), Err(RejectReason::EmptyPath));
        assert_eq!(check_path("a//b"), Err(RejectReason::EmptyPath));
        assert_eq!(check_path("ok/path.md"), Ok(()));
    }

    #[test]
    fn manifest_as_file_content_does_not_alias_the_described_bundle() {
        // The cross-space numeric collision is real by construction: a file whose bytes are exactly a
        // rendered manifest has a CONTENT id equal to that manifest's BUNDLE digest. Pin that this
        // never aliases identities within the digest space itself — a bundle CARRYING the manifest
        // bytes as a file shares neither the described bundle's manifest nor its digest, so only a
        // space-blind lookup could conflate them (which the module docs forbid).
        let described = vec![
            entry("a.txt", FileMode::Regular, b"hello\n"),
            entry("b/run.sh", FileMode::Executable, b"#!/bin/sh\n"),
        ];
        let manifest = canonical_manifest(&described).unwrap();
        let described_digest = bundle_digest(&described).unwrap();
        assert_eq!(sha256(manifest.as_bytes()), described_digest);

        let carrier = vec![entry(
            "manifest.txt",
            FileMode::Regular,
            manifest.as_bytes(),
        )];
        assert_ne!(canonical_manifest(&carrier).unwrap(), manifest);
        assert_ne!(bundle_digest(&carrier).unwrap(), described_digest);
    }

    #[test]
    fn canonical_manifest_is_injective_over_generated_entry_sets() {
        // Generative: distinct entry SETS (as sets — input order is legitimately erased) must render
        // distinct manifests, over an alphabet that includes tricky-but-legal paths (spaces, non-ASCII,
        // a backslash byte, near-miss names, deep nesting). An injectivity break here would let two
        // different bundles share one digest — the consent unit would stop meaning "these exact bytes."
        use crate::testgen::Rng;
        use alloc::collections::BTreeMap;

        let paths: &[&str] = &[
            "a",
            "a.txt",
            "A.md",
            "dir/with spaces.md",
            "caf\u{e9}",
            "b\\c",
            "reference/deep/nested/notes.md",
            "x/y/z",
        ];
        let contents: &[&[u8]] = &[b"", b"1", b"22"];
        let mut rng = Rng::new(0x1D5E_7C0D_E5E7_0001);
        let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut distinct = 0usize;
        for _ in 0..500 {
            let mut entries = Vec::new();
            for &p in paths {
                if rng.next() & 1 == 0 {
                    let mode = if rng.next() & 1 == 0 {
                        FileMode::Regular
                    } else {
                        FileMode::Executable
                    };
                    let content = contents[(rng.next() % contents.len() as u64) as usize];
                    entries.push(entry(p, mode, content));
                }
            }
            // Collision-rejected sets have no manifest — they are outside the injectivity domain.
            let Ok(manifest) = canonical_manifest(&entries) else {
                continue;
            };
            // Determinism: the same set renders the same manifest.
            assert_eq!(canonical_manifest(&entries).unwrap(), manifest);
            let mut key: Vec<String> = entries
                .iter()
                .map(|e| {
                    alloc::format!(
                        "{}|{}|{}",
                        e.path,
                        e.mode.as_str(),
                        to_hex(&e.content_sha256)
                    )
                })
                .collect();
            key.sort_unstable();
            match seen.get(&manifest) {
                Some(prior) => assert_eq!(
                    prior, &key,
                    "two distinct entry sets rendered the SAME manifest:\n{manifest}"
                ),
                None => {
                    seen.insert(manifest, key);
                    distinct += 1;
                }
            }
        }
        assert!(distinct > 50, "generator degenerated: {distinct} sets");
    }

    #[test]
    fn collision_rejects_agree_with_normalize_for_collision() {
        // Exhaustive over pairs of legal paths (incl. NFC and ASCII-case near-misses, and the non-ASCII
        // case pair the documented residual deliberately does NOT fold): the manifest rejects the pair
        // as a collision IFF the two paths normalize equal under [`normalize_for_collision`] — the same
        // rule a client pre-checks derived paths with, so the two can never drift apart.
        let paths: &[&str] = &[
            "a",
            "A",
            "b",
            "caf\u{e9}",
            "cafe\u{301}",
            "CAF\u{c9}", // non-ASCII uppercase É: NOT ASCII-foldable — the documented residual
            "a.topos-mine",
            "A.TOPOS-MINE",
            "dir/x",
            "DIR/X",
        ];
        for &p1 in paths {
            for &p2 in paths {
                let entries = vec![
                    entry(p1, FileMode::Regular, b"1"),
                    entry(p2, FileMode::Regular, b"2"),
                ];
                let normalized_equal = normalize_for_collision(p1) == normalize_for_collision(p2);
                match canonical_manifest(&entries) {
                    Err(RejectReason::DuplicatePath) => assert_eq!(p1, p2),
                    Err(RejectReason::NfcCollision | RejectReason::CaseFoldCollision) => {
                        assert!(normalized_equal && p1 != p2, "{p1:?} vs {p2:?}");
                    }
                    Ok(_) => assert!(
                        !normalized_equal,
                        "{p1:?} vs {p2:?} normalize equal but passed"
                    ),
                    Err(other) => panic!("unexpected reject {other:?} for {p1:?} vs {p2:?}"),
                }
            }
        }
    }

    #[test]
    fn rejects_path_collisions() {
        let dup = vec![
            entry("README.md", FileMode::Regular, b"1"),
            entry("README.md", FileMode::Regular, b"2"),
        ];
        assert_eq!(bundle_digest(&dup), Err(RejectReason::DuplicatePath));

        let case = vec![
            entry("Readme.md", FileMode::Regular, b"1"),
            entry("readme.md", FileMode::Regular, b"2"),
        ];
        assert_eq!(bundle_digest(&case), Err(RejectReason::CaseFoldCollision));

        // Precomposed "café" vs decomposed "cafe\u{301}" — distinct bytes, same file on macOS.
        let nfc = vec![
            entry("caf\u{e9}.md", FileMode::Regular, b"1"),
            entry("cafe\u{301}.md", FileMode::Regular, b"2"),
        ];
        assert_eq!(bundle_digest(&nfc), Err(RejectReason::NfcCollision));
    }
}
