//! A vendored, deterministic line-oriented unified-diff renderer (LCS), owned in-repo so the committed
//! `diff` golden is byte-stable regardless of any external diff library's output across releases.
//!
//! Lines are compared **byte-exactly** (a line is split inclusive of its `\n`, so a trailing-newline
//! change is a real difference). A **mode** change (e.g. `chmod +x`) is surfaced even with identical
//! content — it changes the `bundle_digest`, so the diff must not hide it. A non-UTF-8 file renders as
//! `Binary files … differ`; a missing trailing newline renders the standard `\ No newline at end of file`.

use topos_core::digest::FileMode;

/// One file's mode + bytes under its bundle-relative path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileBytes<'a> {
    pub path: &'a str,
    pub mode: FileMode,
    pub bytes: &'a [u8],
}

const CONTEXT: usize = 3;

/// Render a unified diff of two bundles, each sorted by raw path bytes. Files only in `base` are
/// deletions; files only in `draft` are additions; common files with differing bytes **or mode** are shown.
pub(crate) fn unified_bundle_diff(base: &[FileBytes<'_>], draft: &[FileBytes<'_>]) -> String {
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

fn file_diff(path: &str, old: Option<FileBytes<'_>>, new: Option<FileBytes<'_>>) -> String {
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

/// The LCS edit script over lines: `(Op, old_idx, new_idx)`.
fn edit_script(old: &[&str], new: &[&str]) -> Vec<(Op, usize, usize)> {
    let (n, m) = (old.len(), new.len());
    // lcs[a][b] = LCS length of old[a..] and new[b..].
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for a in (0..n).rev() {
        for b in (0..m).rev() {
            lcs[a][b] = if old[a] == new[b] {
                lcs[a + 1][b + 1] + 1
            } else {
                lcs[a + 1][b].max(lcs[a][b + 1])
            };
        }
    }
    let mut script = Vec::new();
    let (mut a, mut b) = (0, 0);
    while a < n && b < m {
        if old[a] == new[b] {
            script.push((Op::Keep, a, b));
            a += 1;
            b += 1;
        } else if lcs[a + 1][b] >= lcs[a][b + 1] {
            script.push((Op::Del, a, b));
            a += 1;
        } else {
            script.push((Op::Ins, a, b));
            b += 1;
        }
    }
    while a < n {
        script.push((Op::Del, a, b));
        a += 1;
    }
    while b < m {
        script.push((Op::Ins, a, b));
        b += 1;
    }
    script
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

    fn f<'a>(path: &'a str, mode: FileMode, bytes: &'a [u8]) -> FileBytes<'a> {
        FileBytes { path, mode, bytes }
    }

    #[test]
    fn mode_only_change_is_surfaced() {
        // chmod +x with identical content must still produce a diff (it changes the bundle_digest).
        let base = [f("run.sh", FileMode::Regular, b"#!/bin/sh\n")];
        let draft = [f("run.sh", FileMode::Executable, b"#!/bin/sh\n")];
        let out = unified_bundle_diff(&base, &draft);
        assert!(
            out.contains("old mode 100644") && out.contains("new mode 100755"),
            "{out}"
        );
    }

    #[test]
    fn added_file_uses_zero_old_range() {
        let draft = [f("new.txt", FileMode::Regular, b"a\nb\n")];
        let out = unified_bundle_diff(&[], &draft);
        assert!(out.contains("@@ -0,0 +1,2 @@"), "{out}");
        assert!(out.contains("--- /dev/null") && out.contains("+++ b/new.txt"));
    }

    #[test]
    fn deleted_file_uses_zero_new_range() {
        let base = [f("gone.txt", FileMode::Regular, b"x\n")];
        let out = unified_bundle_diff(&base, &[]);
        assert!(out.contains("@@ -1,1 +0,0 @@"), "{out}");
    }

    #[test]
    fn no_newline_marker_is_rendered() {
        let base = [f("a", FileMode::Regular, b"x")];
        let draft = [f("a", FileMode::Regular, b"y")];
        let out = unified_bundle_diff(&base, &draft);
        assert!(out.contains("\\ No newline at end of file"), "{out}");
    }
}
