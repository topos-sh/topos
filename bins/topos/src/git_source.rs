//! The remote-source seam for `add <owner/repo>` — fetch a repo as a `.tar.gz`, extract it in memory,
//! and select the skill to adopt. Kept behind a trait (mirroring [`crate::release::ReleaseSource`]) so the
//! whole import flow is unit-tested with a fake and no HTTP; the real `ureq` fetcher
//! ([`crate::plane_http::UreqGitSource`]) lives beside the other network transports.
//!
//! Extraction never trusts the archive: it strips the single top-level dir, rejects `..`/absolute paths,
//! skips non-regular entries (symlinks/devices), and caps total/ per-file bytes + the file count. What
//! lands is exactly the repo's regular files — the byte-exact bundle the kernel digest then commits to.

use std::collections::BTreeMap;
use std::io::Read;

use crate::error::ClientError;
use crate::source::RemoteSpec;

/// What a cheap [`GitTarballSource::probe`] learned about a repo — everything needed to decide
/// whether the tarball is worth downloading at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoHead {
    /// The full commit the repo's default branch points at right now.
    pub commit: String,
    /// The `<owner>/<repo>` the request actually landed on, when the forge redirected the one
    /// asked for to a new name. `None` = the reference is still canonical.
    pub renamed_to: Option<(String, String)>,
    /// The earliest instant (epoch millis) the host asked to be called back at — a rate-limit or
    /// backoff signal. Only ever DELAYS the next check; it can never pull one earlier.
    pub retry_after_ms: Option<i64>,
}

/// The upstream repo source — GitHub in production, a fake in tests.
pub(crate) trait GitTarballSource {
    /// Fetch `spec.owner/spec.repo` at `spec.git_ref` (or the default branch) as a `.tar.gz`.
    ///
    /// # Errors
    /// [`ClientError::RemoteFetch`], classified by [`crate::error::FetchFault`].
    fn fetch(&self, spec: &RemoteSpec) -> Result<Vec<u8>, ClientError>;

    /// Ask what commit `spec`'s default branch points at, WITHOUT downloading the repo.
    ///
    /// This is what makes an automatic cadence affordable: a repo that has not moved costs one
    /// small request instead of a whole archive, so a machine tracking several sources can check
    /// them on a clock rather than only when a person asks. Only FLOATING rows are probed — a
    /// pinned row either already satisfies its pin (and never dials) or must fetch the pinned
    /// bytes anyway, so the probe deliberately answers for the default branch alone.
    ///
    /// # Errors
    /// [`ClientError::RemoteFetch`], classified by [`crate::error::FetchFault`] — the caller's
    /// breaker trips only on the never-reached one.
    fn probe(&self, spec: &RemoteSpec) -> Result<RepoHead, ClientError>;
}

/// A generous ceiling on a fetched repo (defense against a decompression bomb; skill repos are small).
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// A per-file ceiling.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// A ceiling on the file count.
const MAX_FILES: usize = 20_000;
/// The LICENSE filenames recorded as provenance (repo-root or skill-root), in preference order.
const LICENSE_NAMES: &[&str] = &[
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "LICENCE",
    "COPYING",
    "COPYING.md",
];

/// One file selected for a skill bundle — its path relative to the SKILL ROOT (forward slash), its unix
/// mode (masked to the permission bits), and raw bytes.
#[derive(Debug, Clone)]
pub(crate) struct RepoFile {
    pub path: String,
    pub mode: u32,
    pub bytes: Vec<u8>,
}

/// The skill chosen out of a fetched repo.
#[derive(Debug, Clone)]
pub(crate) struct SelectedSkill {
    /// The adopt name — the skill root's directory basename, or the repo name for a repo-root skill.
    pub name: String,
    /// The skill root's path WITHIN the repo (`None` for a repo-root skill) — recorded as provenance.
    pub subdir: Option<String>,
    /// A LICENSE filename found at the skill root or the repo root, recorded as provenance (never injected
    /// into the bundle — the bundle stays byte-exact to the repo).
    pub license: Option<String>,
    /// The skill's files, relative to its root.
    pub files: Vec<RepoFile>,
}

/// A repo fetched + extracted into memory: the resolved commit (best-effort, parsed from the archive's
/// top-level dir) and every regular file keyed by its repo-relative path.
#[derive(Debug, Clone)]
pub(crate) struct ExtractedRepo {
    pub commit: Option<String>,
    files: BTreeMap<String, (u32, Vec<u8>)>,
}

/// Decode a gzip'd tar into an [`ExtractedRepo`], stripping the single top-level dir and applying the
/// safety caps.
///
/// # Errors
/// [`ClientError::WireInvalid`] on an unreadable / path-unsafe / over-limit archive.
pub(crate) fn extract_tree(targz: &[u8]) -> Result<ExtractedRepo, ClientError> {
    let gz = flate2::read::GzDecoder::new(targz);
    let mut ar = tar::Archive::new(gz);
    let entries = ar
        .entries()
        .map_err(|e| ClientError::WireInvalid(format!("repo archive unreadable: {e}")))?;

    let mut files: BTreeMap<String, (u32, Vec<u8>)> = BTreeMap::new();
    let mut total: u64 = 0;
    let mut commit: Option<String> = None;

    for entry in entries {
        let e =
            entry.map_err(|err| ClientError::WireInvalid(format!("repo archive entry: {err}")))?;
        let path = e
            .path()
            .map_err(|err| ClientError::WireInvalid(format!("repo archive path: {err}")))?;
        let rel = path
            .to_str()
            .ok_or_else(|| ClientError::WireInvalid("non-UTF-8 path in repo archive".into()))?
            .to_owned();

        // Strip the single top-level dir GitHub wraps every archive in (`<owner>-<repo>-<sha>/…`). The
        // first segment is the top dir; the remainder is the repo-relative path.
        let mut split = rel.splitn(2, '/');
        let first = split.next().unwrap_or("");
        if commit.is_none() && !first.is_empty() {
            commit = parse_commit_suffix(first);
        }
        let Some(inner) = split.next() else {
            continue; // the top dir itself (or a stray bare entry) — nothing to adopt
        };
        if inner.is_empty() {
            continue;
        }
        // Only regular files land — a directory, symlink, device, or fifo is skipped (skills are plain
        // files; this also keeps the write side from ever materializing a link). Done BEFORE the path
        // check so a legitimate directory entry (`skills/` — a trailing slash yields an empty segment) is
        // never mistaken for a crafted path.
        if !e.header().entry_type().is_file() {
            continue;
        }
        // Path safety on a file that WILL land: never a `..`, an absolute, or an empty segment (defense
        // against a crafted archive; a real file path has none of these).
        if inner.starts_with('/')
            || inner
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(ClientError::WireInvalid(
                "repo archive contains an unsafe path".into(),
            ));
        }
        if files.len() >= MAX_FILES {
            return Err(ClientError::WireInvalid(format!(
                "repo archive has more than {MAX_FILES} files — refusing to extract"
            )));
        }
        let declared = e.header().size().unwrap_or(0);
        if declared > MAX_FILE_BYTES {
            return Err(ClientError::WireInvalid(format!(
                "a file in the repo archive is implausibly large ({declared} bytes)"
            )));
        }
        let mode = e.header().mode().unwrap_or(0o644) & 0o777;
        let mut buf = Vec::new();
        e.take(MAX_FILE_BYTES)
            .read_to_end(&mut buf)
            .map_err(|err| ClientError::WireInvalid(format!("reading {inner}: {err}")))?;
        total = total.saturating_add(buf.len() as u64);
        if total > MAX_TOTAL_BYTES {
            return Err(ClientError::WireInvalid(
                "repo archive expands past the size ceiling — refusing to extract".into(),
            ));
        }
        files.insert(inner.to_owned(), (mode, buf));
    }

    Ok(ExtractedRepo { commit, files })
}

impl ExtractedRepo {
    /// Select the skill to adopt: narrow to `subdir` (from a `/tree/<ref>/<subdir>` URL) if any, discover
    /// the skill root(s), then pick by `skill` (or the sole skill). `source_label`/`repo_name` shape the
    /// typed errors + the repo-root skill's name.
    ///
    /// # Errors
    /// [`ClientError::NoSkillInSource`] / [`ClientError::AmbiguousSkillInRepo`] /
    /// [`ClientError::SkillNotInRepo`] — each machine-branchable so the agent re-picks.
    pub(crate) fn select(
        &self,
        subdir: Option<&str>,
        skill: Option<&str>,
        repo_name: &str,
        source_label: &str,
    ) -> Result<SelectedSkill, ClientError> {
        let search_root = subdir.map(trim_slashes).unwrap_or("");
        let roots = self.discover_roots(search_root);
        if roots.is_empty() {
            return Err(ClientError::NoSkillInSource {
                src: source_label.to_owned(),
            });
        }
        let named: Vec<(String, String)> = roots
            .iter()
            .map(|r| (skill_name(r, repo_name), r.clone()))
            .collect();

        let root = match skill {
            Some(want) => {
                // ALL roots of that name — a repo can carry two dirs of one basename (e.g.
                // `skills/.curated/foo` + `skills/.experimental/foo`). Never silently pick one.
                let matches: Vec<&String> = named
                    .iter()
                    .filter(|(n, _)| n == want)
                    .map(|(_, r)| r)
                    .collect();
                match matches.as_slice() {
                    [] => {
                        return Err(ClientError::SkillNotInRepo {
                            skill: want.to_owned(),
                            src: source_label.to_owned(),
                            available: sorted_names(&named),
                        });
                    }
                    [one] => (*one).clone(),
                    many => {
                        return Err(ClientError::DuplicateSkillName {
                            src: source_label.to_owned(),
                            name: want.to_owned(),
                            paths: many.iter().map(|r| (*r).clone()).collect(),
                        });
                    }
                }
            }
            None => match named.as_slice() {
                [(_, r)] => r.clone(),
                _ => {
                    return Err(ClientError::AmbiguousSkillInRepo {
                        src: source_label.to_owned(),
                        skills: sorted_names(&named),
                    });
                }
            },
        };

        let name = skill_name(&root, repo_name);
        let files = self.files_under(&root);
        let license = self.find_license(&root);
        Ok(SelectedSkill {
            name,
            subdir: (!root.is_empty()).then(|| root.clone()),
            license,
            files,
        })
    }

    /// Every discoverable skill NAME under `subdir` (sorted, deduped) — the expansion of `add -s '*'`
    /// (import every skill of a multi-skill repo). Empty when the repo carries no `SKILL.md`. The caller
    /// loops the single-select [`select`](Self::select) path per name, so a repo-root skill returns the
    /// repo name and a `skills/<x>` layout returns each `<x>`.
    pub(crate) fn skill_names(&self, subdir: Option<&str>, repo_name: &str) -> Vec<String> {
        let search_root = subdir.map(trim_slashes).unwrap_or("");
        let mut names: Vec<String> = self
            .discover_roots(search_root)
            .iter()
            .map(|r| skill_name(r, repo_name))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The skill root paths (repo-relative) reachable under `search_root`. A `SKILL.md` AT the search root
    /// shadows any nested ones (that root is the single skill); otherwise the depth-bounded nested roots.
    fn discover_roots(&self, search_root: &str) -> Vec<String> {
        let prefix = if search_root.is_empty() {
            String::new()
        } else {
            format!("{search_root}/")
        };
        // A SKILL.md at the search root itself → the whole (sub)tree is one skill.
        if self.files.contains_key(&format!("{prefix}SKILL.md")) {
            return vec![search_root.to_owned()];
        }
        let mut roots: Vec<String> = Vec::new();
        for key in self.files.keys() {
            let Some(rel) = key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(dir) = rel.strip_suffix("/SKILL.md") else {
                continue;
            };
            if is_allowed_skill_dir(dir) {
                roots.push(format!("{prefix}{dir}"));
            }
        }
        roots.sort();
        roots.dedup();
        roots
    }

    /// The files under a skill root, relative to that root (the root itself → the whole repo).
    fn files_under(&self, root: &str) -> Vec<RepoFile> {
        let prefix = if root.is_empty() {
            String::new()
        } else {
            format!("{root}/")
        };
        self.files
            .iter()
            .filter_map(|(key, (mode, bytes))| {
                let path = key.strip_prefix(&prefix)?;
                (!path.is_empty()).then(|| RepoFile {
                    path: path.to_owned(),
                    mode: *mode,
                    bytes: bytes.clone(),
                })
            })
            .collect()
    }

    /// The LICENSE at the skill root, else at the repo root — recorded as provenance only. The
    /// recorded value is the license's NAME classified from the file's text ("MIT", "Apache-2.0"),
    /// falling back to the filename when the text matches nothing known — a receipt that says
    /// "LICENSE.txt" answers no question anyone asked.
    fn find_license(&self, root: &str) -> Option<String> {
        let root_prefix = if root.is_empty() {
            String::new()
        } else {
            format!("{root}/")
        };
        let hit = |name: &str, path: &str| -> Option<String> {
            self.files
                .get(path)
                .map(|(_, bytes)| classify_license(bytes).unwrap_or_else(|| name.to_owned()))
        };
        for name in LICENSE_NAMES {
            if let Some(found) = hit(name, &format!("{root_prefix}{name}")) {
                return Some(found);
            }
        }
        if !root.is_empty() {
            for name in LICENSE_NAMES {
                if let Some(found) = hit(name, name) {
                    return Some(found);
                }
            }
        }
        None
    }
}

/// Classify a license's TEXT into its common name (SPDX-ish spelling) — a small
/// signature match over the well-known headers, never a legal judgment. `None` = unrecognized
/// (the caller falls back to the filename).
fn classify_license(bytes: &[u8]) -> Option<String> {
    let head: String = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)])
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let name = if head.contains("mit license")
        || head.contains("permission is hereby granted, free of charge")
    {
        "MIT"
    } else if head.contains("apache license") && head.contains("version 2.0") {
        "Apache-2.0"
    } else if head.contains("gnu affero general public license") {
        "AGPL-3.0"
    } else if head.contains("gnu lesser general public license") {
        "LGPL-3.0"
    } else if head.contains("gnu general public license") && head.contains("version 3") {
        "GPL-3.0"
    } else if head.contains("gnu general public license") && head.contains("version 2") {
        "GPL-2.0"
    } else if head.contains("mozilla public license") && head.contains("2.0") {
        "MPL-2.0"
    } else if head.contains("redistribution and use in source and binary forms") {
        if head.contains("neither the name") {
            "BSD-3-Clause"
        } else {
            "BSD-2-Clause"
        }
    } else if head.contains("isc license") {
        "ISC"
    } else if head.contains("this is free and unencumbered software") {
        "Unlicense"
    } else if head.contains("cc0 1.0") || head.contains("creative commons zero") {
        "CC0-1.0"
    } else {
        return None;
    };
    Some(name.to_owned())
}

/// Whether a discovered dir (relative to the search root) is an allowed skill location — the search root's
/// direct child `<X>`, `skills/<X>`, or `skills/{.curated,.experimental,.system}/<X>` (the depth-2 walk
/// `vercel-labs/skills` uses). Anything deeper is ignored.
fn is_allowed_skill_dir(dir: &str) -> bool {
    let segs: Vec<&str> = dir.split('/').collect();
    match segs.as_slice() {
        [_one] => true,
        ["skills", _x] => true,
        ["skills", cat, _x] => matches!(*cat, ".curated" | ".experimental" | ".system"),
        _ => false,
    }
}

/// The adopt name for a skill root — its directory basename, or `repo_name` for the repo-root skill.
fn skill_name(root: &str, repo_name: &str) -> String {
    if root.is_empty() {
        repo_name.to_owned()
    } else {
        root.rsplit('/').next().unwrap_or(root).to_owned()
    }
}

fn sorted_names(named: &[(String, String)]) -> Vec<String> {
    let mut v: Vec<String> = named.iter().map(|(n, _)| n.clone()).collect();
    v.sort();
    v.dedup();
    v
}

fn trim_slashes(s: &str) -> &str {
    s.trim_matches('/')
}

/// The commit `HEAD` points at in a git smart-HTTP ref advertisement, or `None` when the bytes are
/// not one (or the HEAD line is not inside them).
///
/// The advertisement is pkt-line framed: each record is a 4-hex-digit length COVERING those four
/// digits, `0000` is a flush, and the payload of a ref record is `<sha> <name>` optionally followed
/// by `\0<capabilities>`. HEAD is advertised FIRST whenever it exists, which is what lets the
/// caller stop reading after a few hundred bytes — a busy repo's full advertisement runs to
/// megabytes of refs nobody here needs, so `input` is expected to be a bounded PREFIX and a record
/// running past its end simply ends the walk.
pub(crate) fn parse_head_advertisement(input: &[u8]) -> Option<String> {
    let mut rest = input;
    loop {
        if rest.len() < 4 {
            return None;
        }
        let (len_hex, tail) = rest.split_at(4);
        let len = usize::from_str_radix(std::str::from_utf8(len_hex).ok()?, 16).ok()?;
        // `0000` (flush) and `0001` (delimiter) carry no payload; anything under the 4-byte header
        // is malformed. Both just advance the walk.
        if len == 0 || len == 1 {
            rest = tail;
            continue;
        }
        let payload_len = len.checked_sub(4)?;
        if payload_len > tail.len() {
            return None; // the record runs past the prefix we read — stop, do not guess
        }
        let (payload, tail) = tail.split_at(payload_len);
        if let Some(sha) = head_ref_sha(payload) {
            return Some(sha);
        }
        rest = tail;
    }
}

/// The sha of a ref-advertisement record IF it advertises `HEAD` — `<sha> HEAD` with the optional
/// NUL-separated capability list after it. Anything else (the `# service=` banner, another ref)
/// answers `None`.
fn head_ref_sha(payload: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(payload).ok()?;
    let (sha, rest) = text.trim_end_matches('\n').split_once(' ')?;
    if sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let name = rest.split(['\0', '\n']).next().unwrap_or(rest);
    (name == "HEAD").then(|| sha.to_ascii_lowercase())
}

/// Parse a commit-ish suffix out of GitHub's archive top dir (`<owner>-<repo>-<sha>`): the trailing
/// hex-looking segment after the last `-`. `None` if it does not look like a sha (then the origin records
/// no commit — honest rather than wrong).
fn parse_commit_suffix(top: &str) -> Option<String> {
    let suffix = top.rsplit('-').next()?;
    let looks_hex = suffix.len() >= 7 && suffix.chars().all(|c| c.is_ascii_hexdigit());
    looks_hex.then(|| suffix.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{GitHost, RemoteSpec};

    /// Build a real `.tar.gz` with a `TOP/` prefix over `(repo-relative path, bytes, mode)` entries.
    fn build_repo_targz(top: &str, entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for (name, bytes, mode) in entries {
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(bytes.len() as u64);
            h.set_mode(*mode);
            h.set_mtime(0);
            tar.append_data(&mut h, format!("{top}/{name}"), *bytes)
                .unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn spec() -> RemoteSpec {
        RemoteSpec {
            host: GitHost::GitHub,
            owner: "vercel-labs".into(),
            repo: "agent-skills".into(),
            git_ref: None,
            subdir: None,
        }
    }

    #[test]
    fn strips_top_dir_and_parses_the_commit() {
        let targz = build_repo_targz(
            "vercel-labs-agent-skills-3f9a2c1",
            &[("SKILL.md", b"---\nname: x\n---\n", 0o644)],
        );
        let repo = extract_tree(&targz).unwrap();
        assert_eq!(repo.commit.as_deref(), Some("3f9a2c1"));
        assert!(repo.files.contains_key("SKILL.md"));
    }

    #[test]
    fn a_repo_root_skill_is_the_whole_repo() {
        let targz = build_repo_targz(
            "o-r-abc1234",
            &[
                ("SKILL.md", b"body", 0o644),
                ("scripts/run.sh", b"#!/bin/sh\n", 0o755),
                (
                    "LICENSE",
                    b"MIT License\n\nPermission is hereby granted, free of charge" as &[u8],
                    0o644,
                ),
            ],
        );
        let repo = extract_tree(&targz).unwrap();
        let sel = repo
            .select(None, None, "agent-skills", "github.com/o/r")
            .unwrap();
        assert_eq!(sel.name, "agent-skills"); // repo-root skill takes the repo name
        assert_eq!(sel.subdir, None);
        // The license NAME (classified from the text), never the filename, when recognizable.
        assert_eq!(sel.license.as_deref(), Some("MIT"));
        // The executable bit is preserved so the digest matches upstream.
        let run = sel
            .files
            .iter()
            .find(|f| f.path == "scripts/run.sh")
            .unwrap();
        assert_eq!(run.mode & 0o111, 0o111);
        assert_eq!(sel.files.len(), 3);
    }

    #[test]
    fn discovers_skills_under_a_skills_dir_and_disambiguates() {
        let targz = build_repo_targz(
            "o-r-abc1234",
            &[
                ("README.md", b"top-level readme, not a skill", 0o644),
                ("skills/alpha/SKILL.md", b"a", 0o644),
                ("skills/alpha/ref.md", b"aa", 0o644),
                ("skills/beta/SKILL.md", b"b", 0o644),
            ],
        );
        let repo = extract_tree(&targz).unwrap();
        // No `--skill` and two skills → ambiguous, listing both.
        let err = repo.select(None, None, "r", "github.com/o/r").unwrap_err();
        match err {
            ClientError::AmbiguousSkillInRepo { skills, .. } => {
                assert_eq!(skills, vec!["alpha".to_owned(), "beta".to_owned()]);
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
        // `--skill alpha` picks it, bundle relative to its root.
        let sel = repo
            .select(None, Some("alpha"), "r", "github.com/o/r")
            .unwrap();
        assert_eq!(sel.name, "alpha");
        assert_eq!(sel.subdir.as_deref(), Some("skills/alpha"));
        let paths: Vec<&str> = sel.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["SKILL.md", "ref.md"]);
        // A missing `--skill` names what's available.
        let err = repo
            .select(None, Some("gamma"), "r", "github.com/o/r")
            .unwrap_err();
        assert!(matches!(err, ClientError::SkillNotInRepo { .. }), "{err:?}");
    }

    #[test]
    fn skill_names_enumerates_every_skill_for_the_star_import() {
        // A multi-skill repo → every skill name (sorted, deduped) — the `-s '*'` fan-out set.
        let targz = build_repo_targz(
            "o-r-abc1234",
            &[
                ("README.md", b"not a skill", 0o644),
                ("skills/beta/SKILL.md", b"b", 0o644),
                ("skills/alpha/SKILL.md", b"a", 0o644),
            ],
        );
        let repo = extract_tree(&targz).unwrap();
        assert_eq!(
            repo.skill_names(None, "r"),
            vec!["alpha".to_owned(), "beta".to_owned()]
        );
        // A repo-root skill → the repo name (the single skill).
        let root = build_repo_targz("o-r-abc1234", &[("SKILL.md", b"x", 0o644)]);
        let root = extract_tree(&root).unwrap();
        assert_eq!(root.skill_names(None, "agent-skills"), vec!["agent-skills"]);
        // A repo with no SKILL.md → empty (the caller turns this into NO_SKILL_IN_SOURCE).
        let none = build_repo_targz("o-r-abc1234", &[("README.md", b"y", 0o644)]);
        let none = extract_tree(&none).unwrap();
        assert!(none.skill_names(None, "r").is_empty());
    }

    #[test]
    fn a_duplicate_skill_name_is_ambiguous_never_a_silent_pick() {
        // Two allowed roots share the basename `foo` — `--skill foo` must refuse, not pick one.
        let targz = build_repo_targz(
            "o-r-abc1234",
            &[
                ("skills/.curated/foo/SKILL.md", b"curated", 0o644),
                ("skills/.experimental/foo/SKILL.md", b"experimental", 0o644),
            ],
        );
        let repo = extract_tree(&targz).unwrap();
        let err = repo
            .select(None, Some("foo"), "r", "github.com/o/r")
            .unwrap_err();
        match err {
            ClientError::DuplicateSkillName { paths, .. } => {
                assert_eq!(paths.len(), 2, "{paths:?}");
            }
            other => panic!("expected DuplicateSkillName, got {other:?}"),
        }
        // A subdir-exact selection resolves it unambiguously.
        let sel = repo
            .select(Some("skills/.curated/foo"), None, "r", "github.com/o/r")
            .unwrap();
        assert_eq!(sel.name, "foo");
        assert_eq!(sel.subdir.as_deref(), Some("skills/.curated/foo"));
    }

    #[test]
    fn subdir_narrowing_targets_one_skill() {
        let targz = build_repo_targz(
            "o-r-abc1234",
            &[
                ("skills/alpha/SKILL.md", b"a", 0o644),
                ("skills/beta/SKILL.md", b"b", 0o644),
            ],
        );
        let repo = extract_tree(&targz).unwrap();
        let sel = repo
            .select(
                Some("skills/beta"),
                None,
                "r",
                "github.com/o/r/tree/main/skills/beta",
            )
            .unwrap();
        assert_eq!(sel.name, "beta");
        assert_eq!(sel.subdir.as_deref(), Some("skills/beta"));
    }

    #[test]
    fn a_repo_with_no_skill_md_is_no_skill_in_source() {
        let targz = build_repo_targz("o-r-abc1234", &[("README.md", b"nothing here", 0o644)]);
        let repo = extract_tree(&targz).unwrap();
        let err = repo.select(None, None, "r", "github.com/o/r").unwrap_err();
        assert!(
            matches!(err, ClientError::NoSkillInSource { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn directory_entries_are_skipped_not_rejected() {
        // GitHub tarballs include explicit directory entries (a trailing slash → an empty path segment).
        // These must be SKIPPED, never mistaken for a crafted/unsafe path (the bug the live test caught).
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for d in [
            "o-r-abc1234/",
            "o-r-abc1234/skills/",
            "o-r-abc1234/skills/alpha/",
        ] {
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_mtime(0);
            tar.append_data(&mut h, d, &b""[..]).unwrap();
        }
        let mut f = tar::Header::new_ustar();
        f.set_entry_type(tar::EntryType::Regular);
        f.set_size(1);
        f.set_mode(0o644);
        f.set_mtime(0);
        tar.append_data(&mut f, "o-r-abc1234/skills/alpha/SKILL.md", &b"x"[..])
            .unwrap();
        let targz = tar.into_inner().unwrap().finish().unwrap();
        let repo = extract_tree(&targz).expect("dir entries do not fail extraction");
        assert!(repo.files.contains_key("skills/alpha/SKILL.md"));
        let sel = repo.select(None, None, "r", "github.com/o/r").unwrap();
        assert_eq!(sel.name, "alpha");
    }

    #[test]
    fn skips_a_symlink_entry() {
        // A symlink alongside a regular file is dropped — only regular files land, so a crafted link can
        // never be materialized on disk. (`..`/absolute paths are refused too, but the safe `tar::Builder`
        // won't even construct one, so that guard is defense-in-depth exercised only by a raw archive.)
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut reg = tar::Header::new_ustar();
        reg.set_entry_type(tar::EntryType::Regular);
        reg.set_size(1);
        reg.set_mode(0o644);
        reg.set_mtime(0);
        tar.append_data(&mut reg, "o-r-abc1234/SKILL.md", &b"x"[..])
            .unwrap();
        let mut link = tar::Header::new_ustar();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_mtime(0);
        tar.append_link(&mut link, "o-r-abc1234/evil-link", "/etc/passwd")
            .unwrap();
        let targz = tar.into_inner().unwrap().finish().unwrap();
        let repo = extract_tree(&targz).unwrap();
        assert!(repo.files.contains_key("SKILL.md"));
        assert!(!repo.files.contains_key("evil-link"));
    }

    /// Frame payloads as pkt-lines the way a git server does (4 hex digits COVERING themselves).
    fn pkt(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in payloads {
            if p.is_empty() {
                out.extend_from_slice(b"0000"); // flush
                continue;
            }
            out.extend_from_slice(format!("{:04x}", p.len() + 4).as_bytes());
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn the_head_advertisement_yields_the_commit_the_default_branch_points_at() {
        // The real shape, verbatim: the service banner, a flush, then HEAD first with its
        // NUL-separated capability list.
        let adv = pkt(&[
            b"# service=git-upload-pack\n",
            b"",
            b"fd2a861ab0406a4ac536a55274d14ea6fd1ca9c9 HEAD\0multi_ack thin-pack side-band               symref=HEAD:refs/heads/master object-format=sha1\n",
            b"fd2a861ab0406a4ac536a55274d14ea6fd1ca9c9 refs/heads/master\n",
            b"",
        ]);
        assert_eq!(
            parse_head_advertisement(&adv).as_deref(),
            Some("fd2a861ab0406a4ac536a55274d14ea6fd1ca9c9")
        );
    }

    #[test]
    fn a_truncated_advertisement_answers_nothing_rather_than_guessing() {
        // The caller reads a bounded PREFIX (a busy repo advertises megabytes of refs), so a
        // record running past the end is the ordinary case at the cut — and must never be read as
        // a partial commit. HEAD is advertised first, so a prefix that cut before it means the
        // answer genuinely is not there.
        let adv = pkt(&[
            b"# service=git-upload-pack\n",
            b"",
            b"fd2a861ab0406a4ac536a55274d14ea6fd1ca9c9 HEAD\n",
        ]);
        let cut = &adv[..adv.len() - 10];
        assert_eq!(parse_head_advertisement(cut), None);
        assert_eq!(parse_head_advertisement(b""), None);
        assert_eq!(parse_head_advertisement(b"not pkt-line at all"), None);
    }

    #[test]
    fn only_the_head_record_answers_and_the_banner_never_does() {
        // A repo whose advertisement carries no HEAD (an empty repo) has no answer to give — and
        // neither the `# service=` banner nor another ref may be mistaken for one.
        let no_head = pkt(&[
            b"# service=git-upload-pack\n",
            b"",
            b"1111111111111111111111111111111111111111 refs/heads/main\n",
            b"",
        ]);
        assert_eq!(parse_head_advertisement(&no_head), None);

        // A ref whose NAME merely starts with HEAD is a different ref.
        let lookalike = pkt(&[
            b"2222222222222222222222222222222222222222 HEADSTONE\n",
            b"3333333333333333333333333333333333333333 HEAD\n",
        ]);
        assert_eq!(
            parse_head_advertisement(&lookalike).as_deref(),
            Some("3333333333333333333333333333333333333333")
        );

        // A capability line without a sha is not a commit.
        let junk = pkt(&[
            b"zzzz HEAD\n",
            b"4444444444444444444444444444444444444444 HEAD\n",
        ]);
        assert_eq!(
            parse_head_advertisement(&junk).as_deref(),
            Some("4444444444444444444444444444444444444444")
        );
    }

    #[test]
    fn spec_is_unused_but_type_is_wired() {
        // Guard the RemoteSpec plumbing compiles against this module (the real fetcher takes &RemoteSpec).
        let _ = spec().origin();
    }
}
