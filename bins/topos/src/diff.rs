//! A vendored, deterministic line-oriented unified-diff renderer (LCS), owned in-repo so the committed
//! `diff` golden is byte-stable regardless of any external diff library's output across releases.
//!
//! Lines are compared **byte-exactly** (a line is split inclusive of its `\n`, so a trailing-newline
//! change is a real difference). A non-UTF-8 file renders as `Binary files … differ`; a missing trailing
//! newline renders the standard `\ No newline at end of file`.

/// One file's bytes under its bundle-relative path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileBytes<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
}

const CONTEXT: usize = 3;

/// Render a unified diff of two bundles, each sorted by raw path bytes. Files only in `base` are
/// deletions; files only in `draft` are additions; common files with differing bytes are hunked.
pub(crate) fn unified_bundle_diff(base: &[FileBytes<'_>], draft: &[FileBytes<'_>]) -> String {
    let mut out = String::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < base.len() || j < draft.len() {
        match (base.get(i), draft.get(j)) {
            (Some(b), Some(d)) if b.path == d.path => {
                if b.bytes != d.bytes {
                    out.push_str(&file_diff(b.path, Some(b.bytes), Some(d.bytes)));
                }
                i += 1;
                j += 1;
            }
            (Some(b), Some(d)) if b.path.as_bytes() < d.path.as_bytes() => {
                out.push_str(&file_diff(b.path, Some(b.bytes), None));
                i += 1;
            }
            (Some(_), Some(d)) => {
                out.push_str(&file_diff(d.path, None, Some(d.bytes)));
                j += 1;
            }
            (Some(b), None) => {
                out.push_str(&file_diff(b.path, Some(b.bytes), None));
                i += 1;
            }
            (None, Some(d)) => {
                out.push_str(&file_diff(d.path, None, Some(d.bytes)));
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

fn file_diff(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> String {
    let old_lines = old.map(split_lines);
    let new_lines = new.map(split_lines);
    // Binary on either side -> we don't render content.
    if matches!(old_lines, Some(None)) || matches!(new_lines, Some(None)) {
        return format!("Binary files a/{path} and b/{path} differ\n");
    }
    let old_lines = old_lines.flatten().unwrap_or_default();
    let new_lines = new_lines.flatten().unwrap_or_default();

    let mut out = String::new();
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
    let mut out = format!(
        "@@ -{},{} +{},{} @@\n",
        hunk.old_start + 1,
        hunk.old_len,
        hunk.new_start + 1,
        hunk.new_len
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
