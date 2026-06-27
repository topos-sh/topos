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

/// Render a unified diff of two bundles, each sorted by raw path bytes. Files only in `base` are
/// deletions; files only in `draft` are additions; common files with differing bytes **or mode** are shown.
pub fn unified_diff(base: &[DiffFile<'_>], draft: &[DiffFile<'_>]) -> String {
    let mut out = String::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < base.len() || j < draft.len() {
        match (base.get(i), draft.get(j)) {
            (Some(b), Some(d)) if b.path == d.path => {
                if b.bytes != d.bytes || b.mode != d.mode {
                    out.push_str(&file_diff(b.path, Some(*b), Some(*d)));
                }
                i += 1;
                j += 1;
            }
            (Some(b), Some(d)) if b.path.as_bytes() < d.path.as_bytes() => {
                out.push_str(&file_diff(b.path, Some(*b), None));
                i += 1;
            }
            (Some(_), Some(d)) => {
                out.push_str(&file_diff(d.path, None, Some(*d)));
                j += 1;
            }
            (Some(b), None) => {
                out.push_str(&file_diff(b.path, Some(*b), None));
                i += 1;
            }
            (None, Some(d)) => {
                out.push_str(&file_diff(d.path, None, Some(*d)));
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
                // peek: keep extending if another change is within CONTEXT.
                let next = (end..(end + CONTEXT + 1).min(script.len())).find(|&k| changed[k]);
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
        let base = [f("SKILL.md", FileMode::Regular, b"a\nb\nc\nold\n")];
        let draft = [f("SKILL.md", FileMode::Regular, b"a\nb\nc\nnew\n")];
        let out = unified_diff(&base, &draft);
        assert_eq!(
            out, "--- a/SKILL.md\n+++ b/SKILL.md\n@@ -1,4 +1,4 @@\n a\n b\n c\n-old\n+new\n",
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
}
