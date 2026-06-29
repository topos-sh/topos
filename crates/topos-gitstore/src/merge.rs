//! The per-file three-way (diff3) content merge — the byte execution behind the kernel's merge policy.
//!
//! The kernel ([`topos_core::merge`]) decides *which* paths need a content merge (all three sides differ);
//! this runs the actual byte-level reconciliation on those, via [`diffy`]. The split keeps the kernel
//! library-free and pure while the engine + real bytes live here, next to the two-way [`crate::diff`].
//!
//! ## Determinism is consent (own the bytes, pin the engine)
//!
//! A merged or conflict-marked file becomes a content-addressed, human-approved artifact, so its bytes
//! must be **byte-deterministic across OS/arch and across our own releases**. We get that two ways, the
//! same discipline [`crate::diff`] uses for the unified diff (see `diff.rs:3`):
//! 1. `diffy` is pinned to an **exact** version; its conflict output is locked by a golden vector in this
//!    module's tests, so an upgrade that changes the consent bytes fails loudly rather than silently.
//! 2. We fix the conflict style to [`ConflictStyle::Diff3`] (base section present) and **lengthen the
//!    conflict markers until they are unique** versus the content — so a file whose own lines begin with
//!    `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` cannot forge or hide a resolution boundary.
//!
//! This is *Topos-deterministic for this version*, not "git-compatible": we make no promise that our
//! merged bytes match git's, only that they are stable for a given Topos release.
//!
//! ## Bytes, never normalized
//!
//! The merge is a pure byte transform over the raw inputs — CRLF/LF and a missing final newline survive
//! byte-for-byte (the digest rail). A **non-UTF-8** side is never line-merged: it yields [`MergeFileResult::Binary`]
//! so the caller resolves it as a take/sidecar conflict, never a corrupt interleave of binary chunks.
//!
//! ## Caps before allocation (the client has no server ingest cap)
//!
//! Each side is bounded by [`MERGE_INPUT_CAP`] before anything runs. Peak allocation is then bounded
//! **before** `diffy` builds the output: the conflict-marker length is capped ([`MAX_MARKER_LEN`]; a longer
//! run in the content is pathological → refuse + keep both), and a safe upper bound on the output is
//! computed (`estimated_output`) and checked against [`MERGE_OUTPUT_CAP`] *before* `merge_bytes` is called —
//! so `diffy` is never handed inputs whose merge could exceed the cap. A typed [`MergeError`] rejects
//! either way; the merge never makes an unbounded allocation. (The plane's 1 MiB/100 MiB ingest caps live
//! server-side; they do not exist on the client.)

use diffy::{ConflictStyle, MergeOptions};

/// The largest a single side may be before a content merge is attempted (checked before `diffy` runs).
/// The plane's per-file/-bundle ingest caps are server-side; this is the client's own bound.
pub const MERGE_INPUT_CAP: usize = 16 * 1024 * 1024;
/// The largest a merged file (clean or marker-bearing) may be before it is rejected.
pub const MERGE_OUTPUT_CAP: usize = 64 * 1024 * 1024;

/// The floor on conflict-marker length — `diffy`'s default (`<<<<<<<` is seven chars). The actual length
/// is raised above any marker-like run in the content (see [`marker_length`]).
const MIN_MARKER_LEN: usize = 7;
/// The ceiling on conflict-marker length. `diffy` emits the full marker length × 4 per conflict hunk, so an
/// unbounded marker length (driven by a marker-char run in the attacker-controlled `theirs`) is a
/// super-linear allocation blowup. Beyond this, the content is pathological — refuse to line-merge it
/// (the caller keeps both sides) rather than emit a huge or forgeable marker. No real skill file has a
/// 256-char run of `<`/`|`/`=`/`>` at a line start.
const MAX_MARKER_LEN: usize = 256;

/// The four line-leading characters `diffy` builds conflict markers from.
const MARKER_CHARS: [u8; 4] = [b'<', b'|', b'=', b'>'];

/// The result of a per-file three-way content merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeFileResult {
    /// A clean merge — the resolved bytes (byte-exact; no normalization).
    Clean(Vec<u8>),
    /// A textual conflict — the bytes WITH diff3 markers (base section present), to write at the path.
    Conflict(Vec<u8>),
    /// A non-UTF-8 side with a genuine three-way divergence — never line-merged. The caller keeps both
    /// sides (theirs at the path + mine in a sidecar) rather than interleaving binary chunks.
    Binary,
}

/// Why a content merge could not be produced within the client's memory bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    /// A side exceeded [`MERGE_INPUT_CAP`] (the offending length) — checked before `diffy` allocates.
    #[error("merge input exceeds the size cap ({0} bytes)")]
    InputTooLarge(usize),
    /// The merged output exceeded [`MERGE_OUTPUT_CAP`] (the produced length).
    #[error("merged output exceeds the size cap ({0} bytes)")]
    OutputTooLarge(usize),
}

/// Three-way merge `base`/`mine`/`theirs` into clean bytes or diff3-marked conflict bytes.
///
/// Only paths the kernel classified as a genuine three-way content divergence reach here, so `base` is
/// the real common ancestor. A non-UTF-8 side returns [`MergeFileResult::Binary`] (never a line merge).
///
/// # Errors
/// [`MergeError::InputTooLarge`] if any side exceeds [`MERGE_INPUT_CAP`] (before any allocation);
/// [`MergeError::OutputTooLarge`] if the merged bytes exceed [`MERGE_OUTPUT_CAP`].
pub fn merge_file(base: &[u8], mine: &[u8], theirs: &[u8]) -> Result<MergeFileResult, MergeError> {
    merge_file_capped(base, mine, theirs, MERGE_INPUT_CAP, MERGE_OUTPUT_CAP)
}

/// [`merge_file`] with explicit caps — the production entry point passes the module constants; tests pass
/// small caps to exercise the reject paths without multi-megabyte allocations.
fn merge_file_capped(
    base: &[u8],
    mine: &[u8],
    theirs: &[u8],
    input_cap: usize,
    output_cap: usize,
) -> Result<MergeFileResult, MergeError> {
    // Caps FIRST — before any UTF-8 scan, marker scan, or merge allocation.
    for side in [base, mine, theirs] {
        if side.len() > input_cap {
            return Err(MergeError::InputTooLarge(side.len()));
        }
    }
    // A non-UTF-8 side is never line-merged.
    if !is_text(base) || !is_text(mine) || !is_text(theirs) {
        return Ok(MergeFileResult::Binary);
    }

    // The marker length must exceed any marker-like run already in the content (uniqueness), but a
    // *pathological* run (a multi-MiB line of one marker char in the attacker-controlled `theirs`) would
    // make `diffy` emit that length × 4 per conflict hunk — a super-linear blowup `MERGE_OUTPUT_CAP` could
    // only catch AFTER allocation. So cap the marker length: if the content needs one longer than
    // [`MAX_MARKER_LEN`], the input is pathological — refuse to line-merge (`OutputTooLarge`; the caller
    // keeps both sides) rather than emit a forgeable short marker or an unbounded one.
    let marker_len = marker_length(base, mine, theirs);
    if marker_len > MAX_MARKER_LEN {
        return Err(MergeError::OutputTooLarge(marker_len));
    }
    // Bound the PEAK allocation BEFORE `diffy` builds the output: a safe upper bound is the three sections
    // (each a subset of an input) plus the worst case of every line being its own conflict hunk. If even
    // that bound exceeds the cap, refuse here — `diffy` is never handed inputs that could OOM the process.
    if estimated_output(base, mine, theirs, marker_len) > output_cap {
        return Err(MergeError::OutputTooLarge(output_cap));
    }

    let mut opts = MergeOptions::new();
    opts.set_conflict_style(ConflictStyle::Diff3);
    opts.set_conflict_marker_length(marker_len);
    let (bytes, clean) = match opts.merge_bytes(base, mine, theirs) {
        Ok(merged) => (merged, true),
        Err(conflicted) => (conflicted, false),
    };
    // A belt: the pre-estimate above already bounds this, so the actual output never exceeds the cap.
    if bytes.len() > output_cap {
        return Err(MergeError::OutputTooLarge(bytes.len()));
    }
    Ok(if clean {
        MergeFileResult::Clean(bytes)
    } else {
        MergeFileResult::Conflict(bytes)
    })
}

/// A safe upper bound on `diffy`'s Diff3 output for these inputs: the three conflict sections never exceed
/// the inputs' combined size, and the markers cost at most `4 × (marker_len + label/newline overhead)` per
/// conflict hunk, with at most one hunk per source line. Computed with saturating arithmetic so it never
/// overflows (it just saturates high → reject). Used to refuse a merge whose output COULD exceed the cap
/// before `diffy` allocates it.
fn estimated_output(base: &[u8], mine: &[u8], theirs: &[u8], marker_len: usize) -> usize {
    let lines = |b: &[u8]| b.iter().filter(|&&c| c == b'\n').count();
    // One conflict hunk needs a change on a side, so the hunk count is bounded by mine+theirs line counts.
    let hunks_ub = lines(mine).saturating_add(lines(theirs)).saturating_add(2);
    // 4 marker lines per hunk; `+ 16` covers the ` ours` / ` original` / ` theirs` labels + the newline.
    let per_hunk = marker_len.saturating_add(16).saturating_mul(4);
    base.len()
        .saturating_add(mine.len())
        .saturating_add(theirs.len())
        .saturating_add(hunks_ub.saturating_mul(per_hunk))
}

/// Whether the bytes are valid UTF-8 — the same gate the two-way renderer uses (`diff.rs`), so binary
/// files are detected identically on both paths.
fn is_text(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

/// The conflict-marker length to use: one longer than the longest run of a single marker character at the
/// start of any line across all three inputs, floored at [`MIN_MARKER_LEN`]. This guarantees the emitted
/// markers are strictly longer than any marker-like run already in the content, so embedded `<<<<<<<` (or
/// `=======`, `|||||||`, `>>>>>>>`) lines cannot be mistaken for — or forge — a real conflict boundary.
fn marker_length(base: &[u8], mine: &[u8], theirs: &[u8]) -> usize {
    let longest = [base, mine, theirs]
        .iter()
        .map(|s| longest_leading_marker_run(s))
        .max()
        .unwrap_or(0);
    longest.saturating_add(1).max(MIN_MARKER_LEN)
}

/// The longest run of a single [`MARKER_CHARS`] character at the start of any line in `bytes`.
fn longest_leading_marker_run(bytes: &[u8]) -> usize {
    let mut max_run = 0usize;
    for line in bytes.split(|&b| b == b'\n') {
        let Some(&first) = line.first() else { continue };
        if MARKER_CHARS.contains(&first) {
            let run = line.iter().take_while(|&&c| c == first).count();
            max_run = max_run.max(run);
        }
    }
    max_run
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-overlapping edits on either side of an unchanged middle merge cleanly, byte-exactly.
    #[test]
    fn clean_three_way_merge_combines_disjoint_edits() {
        let base = b"line1\nline2\nline3\n";
        let mine = b"MINE\nline2\nline3\n"; // edited the first line
        let theirs = b"line1\nline2\nTHEIRS\n"; // edited the last line
        let out = merge_file(base, mine, theirs).unwrap();
        assert_eq!(
            out,
            MergeFileResult::Clean(b"MINE\nline2\nTHEIRS\n".to_vec())
        );
    }

    /// Overlapping edits on the same line conflict, with a diff3 base section, and produce NO clean bytes.
    #[test]
    fn overlapping_edits_conflict_with_base_section() {
        let base = b"hello\n";
        let mine = b"hello mine\n";
        let theirs = b"hello theirs\n";
        let out = merge_file(base, mine, theirs).unwrap();
        let MergeFileResult::Conflict(bytes) = out else {
            panic!("expected a conflict, got {out:?}");
        };
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("<<<<<<<"), "no ours marker:\n{text}");
        assert!(text.contains("|||||||"), "no base (diff3) marker:\n{text}");
        assert!(text.contains("======="), "no separator:\n{text}");
        assert!(text.contains(">>>>>>>"), "no theirs marker:\n{text}");
        assert!(text.contains("hello mine") && text.contains("hello theirs"));
    }

    /// The exact conflict bytes are pinned — a `diffy` upgrade that changes them fails THIS test loudly,
    /// because those bytes are a consent-path artifact (re-digested + human-approved).
    #[test]
    fn conflict_bytes_are_a_stable_golden() {
        let base = b"a\nb\nc\n";
        let mine = b"a\nB-mine\nc\n";
        let theirs = b"a\nB-theirs\nc\n";
        let MergeFileResult::Conflict(bytes) = merge_file(base, mine, theirs).unwrap() else {
            panic!("expected conflict");
        };
        let expected = "a\n\
<<<<<<< ours\n\
B-mine\n\
||||||| original\n\
b\n\
=======\n\
B-theirs\n\
>>>>>>> theirs\n\
c\n";
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    /// A non-UTF-8 side is never line-merged.
    #[test]
    fn non_utf8_side_is_binary_never_merged() {
        let base = b"text\n";
        let mine = &[0xff, 0xfe, 0x00, 0x01][..]; // invalid UTF-8
        let theirs = b"other\n";
        assert_eq!(
            merge_file(base, mine, theirs).unwrap(),
            MergeFileResult::Binary
        );
    }

    /// Markers are lengthened beyond any marker-like run already in the content, so an embedded
    /// `<<<<<<<` line cannot forge a resolution boundary.
    #[test]
    fn markers_lengthen_past_embedded_marker_lines() {
        // mine contains a literal 7-char `<<<<<<<` line; the real markers must be >= 8 long.
        let base = b"x\n";
        let mine = b"<<<<<<<\nmine\n";
        let theirs = b"theirs\n";
        let MergeFileResult::Conflict(bytes) = merge_file(base, mine, theirs).unwrap() else {
            panic!("expected conflict");
        };
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.contains("<<<<<<<<"),
            "markers were not lengthened past the embedded run:\n{text}"
        );
        // The marker length helper agrees: 7-char run -> 8.
        assert_eq!(marker_length(base, mine, theirs), 8);
    }

    /// No marker-like content -> the default minimum length.
    #[test]
    fn marker_length_floor_is_seven() {
        assert_eq!(marker_length(b"a\n", b"b\n", b"c\n"), MIN_MARKER_LEN);
    }

    /// A pathological marker run (longer than the cap) is REFUSED rather than line-merged with a huge or
    /// forgeable marker — the caller keeps both sides. This bounds peak allocation (the run would otherwise
    /// drive `diffy` to emit `run+1` marker bytes × 4 per conflict hunk).
    #[test]
    fn pathological_marker_run_is_refused_not_blown_up() {
        let base = b"a\n";
        let mine = b"b\n";
        // A single line of `MAX_MARKER_LEN + 1` `<` chars in the (attacker-controlled) theirs side.
        let mut theirs = vec![b'<'; MAX_MARKER_LEN + 1];
        theirs.push(b'\n');
        assert!(matches!(
            merge_file(base, mine, &theirs),
            Err(MergeError::OutputTooLarge(_))
        ));
    }

    /// A merge whose worst-case output exceeds the cap is rejected BEFORE `diffy` allocates it (the
    /// pre-estimate), not after.
    #[test]
    fn output_cap_is_enforced_before_allocation() {
        // Many short conflicting lines: the estimate (hunks × markers) blows the tiny cap pre-merge.
        let base = b"1\n2\n3\n4\n5\n6\n7\n8\n";
        let mine = b"a\nb\nc\nd\ne\nf\ng\nh\n";
        let theirs = b"A\nB\nC\nD\nE\nF\nG\nH\n";
        assert!(matches!(
            merge_file_capped(base, mine, theirs, 1_000_000, 64),
            Err(MergeError::OutputTooLarge(_))
        ));
    }

    /// CRLF and a missing final newline survive a clean merge byte-for-byte (no normalization).
    #[test]
    fn clean_merge_preserves_crlf_and_missing_final_newline() {
        let base = b"a\r\nb\r\nc"; // CRLF, no trailing newline
        let mine = b"A\r\nb\r\nc"; // edited first line
        let theirs = b"a\r\nb\r\nC"; // edited last line (still no trailing newline)
        assert_eq!(
            merge_file(base, mine, theirs).unwrap(),
            MergeFileResult::Clean(b"A\r\nb\r\nC".to_vec())
        );
    }

    /// The input cap is enforced before any allocation; the output cap on the produced bytes.
    #[test]
    fn caps_reject_oversize_input_and_output() {
        // Input cap: one side over the (tiny) cap.
        let big = vec![b'x'; 11];
        assert_eq!(
            merge_file_capped(b"x", &big, b"x", 10, 1_000),
            Err(MergeError::InputTooLarge(11))
        );
        // Output cap: a clean merge whose result exceeds a tiny output cap.
        let base = b"a\nb\nc\n";
        let mine = b"AAAA\nb\nc\n";
        let theirs = b"a\nb\nCCCC\n";
        let err = merge_file_capped(base, mine, theirs, 1_000, 4).unwrap_err();
        assert!(matches!(err, MergeError::OutputTooLarge(_)));
    }

    /// Identical inputs -> identical output: determinism across repeated runs.
    #[test]
    fn identical_inputs_yield_identical_bytes() {
        let base = b"1\n2\n3\n";
        let mine = b"1\nM\n3\n";
        let theirs = b"X\n2\n3\n";
        assert_eq!(
            merge_file(base, mine, theirs).unwrap(),
            merge_file(base, mine, theirs).unwrap()
        );
    }
}
