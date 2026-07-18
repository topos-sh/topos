//! A deterministic, byte-stable line-oriented unified-diff renderer over two bundles.
//!
//! The diff **algorithm** is [`imara-diff`](imara_diff) (the histogram differ — the same family of engine
//! gix's own blob diff is built on); the unified-diff **formatting** is owned here so the committed `diff`
//! golden stays byte-stable regardless of imara-diff's output across its releases. We tokenize on our own
//! line split (so the lines we render are exactly the lines we diff), then render the resulting edit
//! script with our own hunk grouping and headers.
//!
//! Lines are compared **byte-exactly** (a line is split inclusive of its `\n`, so a trailing-newline
//! change is a real difference). A **mode** change (e.g. `chmod +x`) is surfaced even with identical
//! content — it changes the `bundle_digest`, so the diff must not hide it. A non-UTF-8 file renders as
//! `Binary files … differ`; a missing trailing newline renders the standard `\ No newline at end of file`.

use std::ops::Range;

use imara_diff::intern::{InternedInput, TokenSource};
use imara_diff::{Algorithm, Sink};

use topos_core::digest::FileMode;

/// One file's mode + bytes under its bundle-relative path — the borrowed view both the rendered base
/// bundle and a freshly scanned draft map into.
#[derive(Debug, Clone, Copy)]
pub struct DiffFile<'a> {
    pub path: &'a str,
    pub mode: FileMode,
    pub bytes: &'a [u8],
}

const CONTEXT: usize = 3;

/// One changed file's rendered section of a unified diff — its bundle-relative path plus the exact
/// bytes [`unified_diff`] would emit for it (headers + hunks; a mode-only change or a binary note
/// included). Concatenating every section in order IS the full unified diff, by construction —
/// which is what lets a byte-budgeted consumer truncate at file boundaries without a second
/// renderer.
#[derive(Debug, Clone)]
pub struct FileDiffSection {
    pub path: String,
    pub text: String,
}

/// Render a unified diff of two bundles, each sorted by raw path bytes. Files only in `base` are
/// deletions; files only in `draft` are additions; common files with differing bytes **or mode** are shown.
pub fn unified_diff(base: &[DiffFile<'_>], draft: &[DiffFile<'_>]) -> String {
    unified_diff_sections(base, draft)
        .into_iter()
        .map(|s| s.text)
        .collect()
}

/// [`unified_diff`], split per changed file — the same walk, the same bytes, one section per file
/// (files with no rendered output are skipped, exactly as the concatenated form omits them).
pub fn unified_diff_sections(
    base: &[DiffFile<'_>],
    draft: &[DiffFile<'_>],
) -> Vec<FileDiffSection> {
    let mut out = Vec::new();
    let mut push = |path: &str, text: String| {
        if !text.is_empty() {
            out.push(FileDiffSection {
                path: path.to_owned(),
                text,
            });
        }
    };
    let (mut i, mut j) = (0usize, 0usize);
    while i < base.len() || j < draft.len() {
        match (base.get(i), draft.get(j)) {
            (Some(b), Some(d)) if b.path == d.path => {
                if b.bytes != d.bytes || b.mode != d.mode {
                    push(b.path, file_diff(b.path, Some(*b), Some(*d)));
                }
                i += 1;
                j += 1;
            }
            (Some(b), Some(d)) if b.path.as_bytes() < d.path.as_bytes() => {
                push(b.path, file_diff(b.path, Some(*b), None));
                i += 1;
            }
            (Some(_), Some(d)) => {
                push(d.path, file_diff(d.path, None, Some(*d)));
                j += 1;
            }
            (Some(b), None) => {
                push(b.path, file_diff(b.path, Some(*b), None));
                i += 1;
            }
            (None, Some(d)) => {
                push(d.path, file_diff(d.path, None, Some(*d)));
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

fn file_diff(path: &str, old: Option<DiffFile<'_>>, new: Option<DiffFile<'_>>) -> String {
    let mut out = String::new();

    // A mode change on an existing file (content may be identical) — surface it git-style.
    if let (Some(o), Some(n)) = (old, new)
        && o.mode != n.mode
    {
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        out.push_str(&format!("old mode {}\n", o.mode.as_str()));
        out.push_str(&format!("new mode {}\n", n.mode.as_str()));
    }

    let old_bytes = old.map(|f| f.bytes);
    let new_bytes = new.map(|f| f.bytes);
    if old_bytes == new_bytes {
        // No content change (a pure mode change already rendered above, or nothing).
        return out;
    }

    let old_lines = old_bytes.map(split_lines);
    let new_lines = new_bytes.map(split_lines);
    if matches!(old_lines, Some(None)) || matches!(new_lines, Some(None)) {
        out.push_str(&format!("Binary files a/{path} and b/{path} differ\n"));
        return out;
    }
    let old_lines = old_lines.flatten().unwrap_or_default();
    let new_lines = new_lines.flatten().unwrap_or_default();

    out.push_str(&format!(
        "--- {}\n",
        if old.is_some() {
            format!("a/{path}")
        } else {
            "/dev/null".into()
        }
    ));
    out.push_str(&format!(
        "+++ {}\n",
        if new.is_some() {
            format!("b/{path}")
        } else {
            "/dev/null".into()
        }
    ));
    for hunk in hunks(&old_lines, &new_lines) {
        out.push_str(&render_hunk(&hunk, &old_lines, &new_lines));
    }
    out
}

/// Split into lines that each retain their trailing `\n` (the last may lack one). `None` if non-UTF-8.
fn split_lines(bytes: &[u8]) -> Option<Vec<&str>> {
    let s = std::str::from_utf8(bytes).ok()?;
    if s.is_empty() {
        return Some(Vec::new());
    }
    Some(s.split_inclusive('\n').collect())
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Keep,
    Del,
    Ins,
}

/// The pre-split lines as an imara-diff token source: one token per line, interned in order, so a
/// `process_change` range indexes straight back into the line slice.
struct Lines<'a>(&'a [&'a str]);

impl<'a> TokenSource for Lines<'a> {
    type Token = &'a str;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, &'a str>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }
}

/// Collects imara-diff's monotonic `process_change` calls into the flat `(Op, old_idx, new_idx)` script
/// the hunk grouper consumes. The runs between changes are equal lines (rendered as context).
struct ScriptSink {
    old_len: usize,
    a: usize,
    b: usize,
    script: Vec<(Op, usize, usize)>,
}

impl Sink for ScriptSink {
    type Out = Vec<(Op, usize, usize)>;

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        let before_start = before.start as usize;
        let before_end = before.end as usize;
        let after_end = after.end as usize;
        // Equal run up to this change: old[a..before_start] aligns 1:1 with new[b..after.start].
        while self.a < before_start {
            self.script.push((Op::Keep, self.a, self.b));
            self.a += 1;
            self.b += 1;
        }
        // Removed lines: old[before_start..before_end].
        while self.a < before_end {
            self.script.push((Op::Del, self.a, self.b));
            self.a += 1;
        }
        // Inserted lines: new[after.start..after_end] (b is now at after.start).
        while self.b < after_end {
            self.script.push((Op::Ins, self.a, self.b));
            self.b += 1;
        }
    }

    fn finish(mut self) -> Self::Out {
        // The trailing equal run after the last change.
        while self.a < self.old_len {
            self.script.push((Op::Keep, self.a, self.b));
            self.a += 1;
            self.b += 1;
        }
        self.script
    }
}

/// The edit script over lines via imara-diff's histogram algorithm: `(Op, old_idx, new_idx)`.
fn edit_script(old: &[&str], new: &[&str]) -> Vec<(Op, usize, usize)> {
    let input = InternedInput::new(Lines(old), Lines(new));
    imara_diff::diff(
        Algorithm::Histogram,
        &input,
        ScriptSink {
            old_len: old.len(),
            a: 0,
            b: 0,
            script: Vec::new(),
        },
    )
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
    ops: Vec<(Op, usize, usize)>,
}

/// Group the edit script into hunks with up to [`CONTEXT`] equal lines of surrounding context.
fn hunks(old: &[&str], new: &[&str]) -> Vec<Hunk> {
    let script = edit_script(old, new);
    let changed: Vec<bool> = script
        .iter()
        .map(|(op, ..)| !matches!(op, Op::Keep))
        .collect();
    if !changed.iter().any(|&c| c) {
        return Vec::new();
    }

    let mut hunks = Vec::new();
    let mut idx = 0;
    while idx < script.len() {
        if !changed[idx] {
            idx += 1;
            continue;
        }
        // Extend a window of changes, absorbing <= 2*CONTEXT equal lines between change runs.
        let start = idx.saturating_sub(CONTEXT);
        let mut end = idx;
        while end < script.len() {
            if changed[end] {
                end += 1;
            } else {
                // peek: keep extending if another change is within 2*CONTEXT — closer than that, this
                // hunk's trailing context would meet the next hunk's leading context (git merges those).
                // A split therefore means a gap >= 2*CONTEXT+1, which makes split hunks provably
                // disjoint: the next hunk starts at `change - CONTEXT` > this hunk's `stop`.
                let next = (end..(end + 2 * CONTEXT + 1).min(script.len())).find(|&k| changed[k]);
                match next {
                    Some(_) => end += 1,
                    None => break,
                }
            }
        }
        let stop = (end + CONTEXT).min(script.len());
        let slice: Vec<(Op, usize, usize)> = script[start..stop].to_vec();
        let (mut o0, mut n0) = (usize::MAX, usize::MAX);
        let (mut ol, mut nl) = (0, 0);
        for (op, a, b) in &slice {
            match op {
                Op::Keep => {
                    o0 = o0.min(*a);
                    n0 = n0.min(*b);
                    ol += 1;
                    nl += 1;
                }
                Op::Del => {
                    o0 = o0.min(*a);
                    ol += 1;
                }
                Op::Ins => {
                    n0 = n0.min(*b);
                    nl += 1;
                }
            }
        }
        if o0 == usize::MAX {
            o0 = 0;
        }
        if n0 == usize::MAX {
            n0 = 0;
        }
        hunks.push(Hunk {
            old_start: o0,
            old_len: ol,
            new_start: n0,
            new_len: nl,
            ops: slice,
        });
        idx = stop;
    }
    hunks
}

fn render_hunk(hunk: &Hunk, old: &[&str], new: &[&str]) -> String {
    // Unified-diff convention: an empty range starts at line 0 (e.g. a new file is `-0,0`).
    let old_start = if hunk.old_len == 0 {
        0
    } else {
        hunk.old_start + 1
    };
    let new_start = if hunk.new_len == 0 {
        0
    } else {
        hunk.new_start + 1
    };
    let mut out = format!(
        "@@ -{},{} +{},{} @@\n",
        old_start, hunk.old_len, new_start, hunk.new_len
    );
    for (op, a, b) in &hunk.ops {
        let (prefix, line) = match op {
            Op::Keep => (' ', old[*a]),
            Op::Del => ('-', old[*a]),
            Op::Ins => ('+', new[*b]),
        };
        out.push(prefix);
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push_str("\n\\ No newline at end of file\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f<'a>(path: &'a str, mode: FileMode, bytes: &'a [u8]) -> DiffFile<'a> {
        DiffFile { path, mode, bytes }
    }

    #[test]
    fn sections_concatenate_to_the_exact_unified_diff() {
        // The per-file split is the SAME renderer: concatenating the sections in order reproduces
        // `unified_diff` byte-for-byte, and each section names its file. Covers a content edit, a
        // deletion, an addition, a mode-only change, and a binary file in one walk.
        let base = [
            f("a.md", FileMode::Regular, b"one\n"),
            f("bin", FileMode::Regular, &[0xff, 0xfe]),
            f("gone.txt", FileMode::Regular, b"x\n"),
            f("run.sh", FileMode::Regular, b"#!/bin/sh\n"),
            f("same.txt", FileMode::Regular, b"kept\n"),
        ];
        let draft = [
            f("a.md", FileMode::Regular, b"two\n"),
            f("bin", FileMode::Regular, &[0x00, 0x01]),
            f("new.txt", FileMode::Regular, b"added\n"),
            f("run.sh", FileMode::Executable, b"#!/bin/sh\n"),
            f("same.txt", FileMode::Regular, b"kept\n"),
        ];
        let sections = unified_diff_sections(&base, &draft);
        let concat: String = sections.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(concat, unified_diff(&base, &draft));
        // One section per CHANGED file, in walk order; the unchanged file emits none.
        let paths: Vec<&str> = sections.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md", "bin", "gone.txt", "new.txt", "run.sh"]);
        assert!(sections.iter().all(|s| !s.text.is_empty()));
    }

    #[test]
    fn mode_only_change_is_surfaced() {
        // chmod +x with identical content must still produce a diff (it changes the bundle_digest).
        let base = [f("run.sh", FileMode::Regular, b"#!/bin/sh\n")];
        let draft = [f("run.sh", FileMode::Executable, b"#!/bin/sh\n")];
        let out = unified_diff(&base, &draft);
        assert!(
            out.contains("old mode 100644") && out.contains("new mode 100755"),
            "{out}"
        );
    }

    #[test]
    fn added_file_uses_zero_old_range() {
        let draft = [f("new.txt", FileMode::Regular, b"a\nb\n")];
        let out = unified_diff(&[], &draft);
        assert!(out.contains("@@ -0,0 +1,2 @@"), "{out}");
        assert!(out.contains("--- /dev/null") && out.contains("+++ b/new.txt"));
    }

    #[test]
    fn deleted_file_uses_zero_new_range() {
        let base = [f("gone.txt", FileMode::Regular, b"x\n")];
        let out = unified_diff(&base, &[]);
        assert!(out.contains("@@ -1,1 +0,0 @@"), "{out}");
    }

    #[test]
    fn no_newline_marker_is_rendered() {
        let base = [f("a", FileMode::Regular, b"x")];
        let draft = [f("a", FileMode::Regular, b"y")];
        let out = unified_diff(&base, &draft);
        assert!(out.contains("\\ No newline at end of file"), "{out}");
    }

    #[test]
    fn single_line_edit_renders_one_replacement_hunk() {
        // The shape the `diff` golden pins: a one-line change with leading context, deletions before
        // insertions. imara-diff and a naive LCS agree byte-for-byte on this.
        let base = [f("GUIDE.md", FileMode::Regular, b"a\nb\nc\nold\n")];
        let draft = [f("GUIDE.md", FileMode::Regular, b"a\nb\nc\nnew\n")];
        let out = unified_diff(&base, &draft);
        assert_eq!(
            out, "--- a/GUIDE.md\n+++ b/GUIDE.md\n@@ -1,4 +1,4 @@\n a\n b\n c\n-old\n+new\n",
            "{out}"
        );
    }

    #[test]
    fn insertion_groups_deletions_before_insertions() {
        // A pure insertion in the middle: only `+` lines, anchored by context, no spurious deletions.
        let base = [f("x", FileMode::Regular, b"a\nb\nc\n")];
        let draft = [f("x", FileMode::Regular, b"a\nINSERTED\nb\nc\n")];
        let out = unified_diff(&base, &draft);
        assert_eq!(
            out, "--- a/x\n+++ b/x\n@@ -1,3 +1,4 @@\n a\n+INSERTED\n b\n c\n",
            "{out}"
        );
    }

    /// Two one-line edits separated by `gap` unchanged lines, with 4 unchanged lines on each flank
    /// (one more than [`CONTEXT`], so the hunk boundary is exercised on both ends).
    fn two_edit_diff(gap: usize) -> String {
        let mut base_s = String::new();
        let mut draft_s = String::new();
        for i in 0..4 {
            base_s.push_str(&format!("lead{i}\n"));
            draft_s.push_str(&format!("lead{i}\n"));
        }
        base_s.push_str("first-old\n");
        draft_s.push_str("first-new\n");
        for i in 0..gap {
            base_s.push_str(&format!("mid{i}\n"));
            draft_s.push_str(&format!("mid{i}\n"));
        }
        base_s.push_str("second-old\n");
        draft_s.push_str("second-new\n");
        for i in 0..4 {
            base_s.push_str(&format!("tail{i}\n"));
            draft_s.push_str(&format!("tail{i}\n"));
        }
        let base = [f("t.md", FileMode::Regular, base_s.as_bytes())];
        let draft = [f("t.md", FileMode::Regular, draft_s.as_bytes())];
        unified_diff(&base, &draft)
    }

    /// The `(start, len)` pairs of every `@@ -<start>,<len> +<start>,<len> @@` header: (old, new).
    #[allow(clippy::type_complexity)]
    fn hunk_ranges(out: &str) -> Vec<((usize, usize), (usize, usize))> {
        out.lines()
            .filter_map(|l| l.strip_prefix("@@ -"))
            .map(|l| {
                let (old, rest) = l.split_once(" +").expect("header shape");
                let new = rest.strip_suffix(" @@").expect("header shape");
                let parse = |r: &str| {
                    let (s, n) = r.split_once(',').expect("range shape");
                    (s.parse().expect("start"), n.parse().expect("len"))
                };
                (parse(old), parse(new))
            })
            .collect()
    }

    #[test]
    fn edits_up_to_two_context_apart_merge_into_one_hunk() {
        // Gaps of 4..=2*CONTEXT equal lines between two change runs: trailing + leading context would
        // meet or overlap, so they must render as ONE hunk (git's behavior). Gaps <= CONTEXT merged
        // before the window fix too; 4..=6 are the previously-overlapping shapes.
        for gap in [4, 5, 6] {
            let out = two_edit_diff(gap);
            assert_eq!(out.matches("@@ -").count(), 1, "gap {gap}:\n{out}");
            // Both edits render exactly once, and every between-lines equal line exactly once (context).
            assert_eq!(out.matches("-first-old\n").count(), 1, "gap {gap}:\n{out}");
            assert_eq!(out.matches("-second-old\n").count(), 1, "gap {gap}:\n{out}");
            assert_eq!(out.matches(" mid").count(), gap, "gap {gap}:\n{out}");
        }
    }

    #[test]
    fn edits_beyond_two_context_apart_split_into_disjoint_hunks() {
        // A gap >= 2*CONTEXT+1 splits — and the split is provably non-overlapping: the second hunk's
        // leading context starts strictly after the first hunk's trailing context, with at least one
        // untouched line between them (no duplicated context, `git apply`-well-formed).
        for gap in [7, 8, 12] {
            let out = two_edit_diff(gap);
            let ranges = hunk_ranges(&out);
            assert_eq!(ranges.len(), 2, "gap {gap}:\n{out}");
            let ((o1, ol1), (n1, nl1)) = ranges[0];
            let ((o2, _), (n2, _)) = ranges[1];
            assert!(
                o2 > o1 + ol1,
                "old ranges must not touch — gap {gap}:\n{out}"
            );
            assert!(
                n2 > n1 + nl1,
                "new ranges must not touch — gap {gap}:\n{out}"
            );
            // Exactly 2*CONTEXT of the between lines render (3 trailing + 3 leading), each once.
            assert_eq!(
                out.matches(" mid").count(),
                2 * CONTEXT,
                "gap {gap}:\n{out}"
            );
        }
    }
}
