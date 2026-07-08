//! Classify an `add <source>` positional into WHAT to adopt: a local path (adopt in place), a local
//! discovered skill name (today's default), or a remote git source to fetch. Pure + shape-based — no
//! filesystem, no network, no env — so the whole dispatch is unit-tested; the fs/network work happens in
//! the ops layer over the classified spec.
//!
//! The shape rules (first match wins), matching the surface `vercel-labs/skills add` established so the
//! muscle memory carries over:
//!   1. `https://github.com/owner/repo[/tree/<ref>/<subdir>]`      → a remote GitHub source (a full URL).
//!   2. an ssh/git URL (`git@…`, `ssh://…`, `git://…`)            → recognized, not yet supported.
//!   3. a PATH-shaped token (`./` `../` `~/` `/` or a backslash)  → a local path (adopt in place).
//!   4. `owner/repo[#<ref>]` (exactly one `/`, repo-name chars)   → a remote GitHub shorthand.
//!   5. any other `/`-containing token                            → a local relative path.
//!   6. a bare word (`deploy`, `deploy@claude-code`)              → a local discovered skill NAME.
//!
//! `@` is NOT overloaded: it stays the local `<skill>@<harness>` disambiguator (a bare-word form resolved
//! downstream). A remote ref is `#<ref>`; a skill within a multi-skill repo is `--skill`.

use std::path::PathBuf;

/// A git host a remote source resolves against. GitHub is the v0 shorthand host; other hosts arrive by
/// full URL later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitHost {
    GitHub,
}

impl GitHost {
    pub(crate) fn domain(&self) -> &'static str {
        match self {
            GitHost::GitHub => "github.com",
        }
    }
}

/// A remote git source to fetch + import: `owner/repo`, optionally pinned to a `#<ref>` and narrowed to a
/// `/tree/<ref>/<subdir>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSpec {
    pub host: GitHost,
    pub owner: String,
    pub repo: String,
    /// A branch / tag / commit to pin (from `#<ref>` or a `/tree/<ref>/…` URL); `None` = the default branch.
    pub git_ref: Option<String>,
    /// A path WITHIN the repo to narrow to (from a `/tree/<ref>/<subdir>` URL); `None` = the whole repo.
    pub subdir: Option<String>,
}

impl RemoteSpec {
    /// The `<host>/<owner>/<repo>` origin recorded in `origin.json` (host + repo, sans ref/subdir). All
    /// public — safe to persist and to show verbatim.
    pub(crate) fn origin(&self) -> String {
        format!("{}/{}/{}", self.host.domain(), self.owner, self.repo)
    }

    /// The human/agent-facing source label for messages — the origin plus any `#<ref>` and `(subdir)`.
    pub(crate) fn label(&self) -> String {
        let mut s = self.origin();
        if let Some(r) = &self.git_ref {
            s.push('#');
            s.push_str(r);
        }
        if let Some(sub) = &self.subdir {
            s.push_str(" (");
            s.push_str(sub);
            s.push(')');
        }
        s
    }
}

/// What an `add <source>` positional resolves to (by shape alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceSpec {
    /// A local filesystem path to adopt in place.
    LocalPath(PathBuf),
    /// A bare skill NAME resolved against `list`'s untracked discovery. Carries the raw token verbatim
    /// (incl. any `@harness`), which `resolve_add_target` splits.
    LocalName(String),
    /// A remote git source to fetch + import.
    Remote(RemoteSpec),
    /// A source form we recognize but do not support yet (an ssh/git URL, a non-GitHub host). The `String`
    /// is verbatim usage guidance.
    Unsupported(String),
}

/// Classify a raw `add` positional (pure — no fs/network/env).
pub(crate) fn classify(raw: &str) -> SourceSpec {
    let s = raw.trim();

    // 1. A full URL.
    if let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    {
        return classify_url(raw, rest);
    }
    // 2. An ssh/git URL — recognized, not yet supported (deferred; needs auth/git).
    if s.starts_with("git@") || s.starts_with("ssh://") || s.starts_with("git://") {
        return SourceSpec::Unsupported(format!(
            "'{raw}' is an SSH/git URL — not supported yet; use `owner/repo` or an https://github.com URL"
        ));
    }
    // 3. A path-shaped token → adopt in place.
    if is_path_shaped(s) {
        return SourceSpec::LocalPath(PathBuf::from(s));
    }
    // 4. `owner/repo[#<ref>]` GitHub shorthand.
    let (before_ref, git_ref) = split_ref(s);
    if let Some((owner, repo)) = owner_repo(before_ref) {
        return SourceSpec::Remote(RemoteSpec {
            host: GitHost::GitHub,
            owner,
            repo,
            git_ref,
            subdir: None,
        });
    }
    // 5. Any other `/`-containing token is a local relative path (not valid `owner/repo` shorthand).
    if s.contains('/') {
        return SourceSpec::LocalPath(PathBuf::from(s));
    }
    // 6. A bare word → a local discovered skill name (`@harness` handled downstream).
    SourceSpec::LocalName(s.to_owned())
}

/// Parse the path AFTER an `https://`/`http://` scheme. `raw` is the original token (for the message).
fn classify_url(raw: &str, rest: &str) -> SourceSpec {
    let rest = rest.trim_end_matches('/');
    let Some((host, path)) = rest.split_once('/') else {
        return SourceSpec::Unsupported(format!(
            "'{raw}' is not a repo URL — use https://github.com/<owner>/<repo>"
        ));
    };
    let host = host.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return SourceSpec::Unsupported(format!(
            "'{host}' is not supported yet — use a github.com URL, or `owner/repo`"
        ));
    }
    // Drop any query/fragment, then split the path into segments.
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return SourceSpec::Unsupported(format!(
            "'{raw}' needs at least an owner and repo — https://github.com/<owner>/<repo>"
        ));
    }
    let owner = segs[0].to_owned();
    let repo = strip_git(segs[1]).to_owned();
    if !is_repo_token(&owner) || !is_repo_token(&repo) {
        return SourceSpec::Unsupported(format!("'{raw}' has an invalid owner/repo"));
    }
    // A `/tree/<ref>/<subdir…>` (or GitHub's `/blob/…`) narrows the ref + subdir.
    let (git_ref, subdir) = match segs.get(2) {
        Some(&("tree" | "blob")) => {
            let r = segs.get(3).map(|s| (*s).to_owned());
            let sub = if segs.len() > 4 {
                Some(segs[4..].join("/"))
            } else {
                None
            };
            (r, sub)
        }
        _ => (None, None),
    };
    SourceSpec::Remote(RemoteSpec {
        host: GitHost::GitHub,
        owner,
        repo,
        git_ref,
        subdir,
    })
}

/// Split a shorthand token on its FIRST `#` into `(before, Some(ref))`; a degenerate `foo#` folds back to
/// no ref.
fn split_ref(s: &str) -> (&str, Option<String>) {
    match s.split_once('#') {
        Some((before, r)) if !r.is_empty() => (before, Some(r.to_owned())),
        // A degenerate trailing `#` (`owner/repo#`) is stripped — no ref, not part of the name.
        Some((before, _)) => (before, None),
        None => (s, None),
    }
}

/// `owner/repo` (exactly one `/`, both sides repo-name chars, a trailing `.git` stripped) → the pair.
fn owner_repo(s: &str) -> Option<(String, String)> {
    let (owner, repo) = s.split_once('/')?;
    if repo.contains('/') {
        return None; // more than one slash — not shorthand (a deeper path or URL residue)
    }
    let repo = strip_git(repo);
    if !is_repo_token(owner) || !is_repo_token(repo) {
        return None;
    }
    Some((owner.to_owned(), repo.to_owned()))
}

/// Whether a token is SYNTACTICALLY a path (a `./ ../ ~/ /` prefix, a bare `.`/`..`/`~`, or a Windows
/// backslash/drive) — the one thing that overrides `owner/repo` shorthand toward a local adopt-in-place.
fn is_path_shaped(s: &str) -> bool {
    s == "."
        || s == ".."
        || s == "~"
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with("~/")
        || s.starts_with('/')
        || s.starts_with('\\')
        || s.contains(":\\")
}

/// A GitHub owner/repo token: ASCII alphanumerics plus `.`, `_`, `-`, non-empty. (Permissive but enough to
/// reject a token with a space, a scheme colon, or an `@`.)
fn is_repo_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn strip_git(s: &str) -> &str {
    s.strip_suffix(".git").unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(s: &str) -> RemoteSpec {
        match classify(s) {
            SourceSpec::Remote(r) => r,
            other => panic!("expected Remote for {s:?}, got {other:?}"),
        }
    }

    #[test]
    fn owner_repo_shorthand_is_a_github_remote() {
        let r = remote("vercel-labs/agent-skills");
        assert_eq!(r.host, GitHost::GitHub);
        assert_eq!(r.owner, "vercel-labs");
        assert_eq!(r.repo, "agent-skills");
        assert_eq!(r.git_ref, None);
        assert_eq!(r.subdir, None);
        assert_eq!(r.origin(), "github.com/vercel-labs/agent-skills");
    }

    #[test]
    fn shorthand_ref_rides_a_hash() {
        let r = remote("vercel-labs/agent-skills#v2");
        assert_eq!(r.repo, "agent-skills");
        assert_eq!(r.git_ref.as_deref(), Some("v2"));
        // A degenerate trailing '#' is no ref.
        assert_eq!(remote("owner/repo#").git_ref, None);
    }

    #[test]
    fn a_trailing_dot_git_is_stripped() {
        assert_eq!(remote("owner/repo.git").repo, "repo");
    }

    #[test]
    fn full_github_url_is_a_remote() {
        let r = remote("https://github.com/vercel-labs/agent-skills");
        assert_eq!(r.owner, "vercel-labs");
        assert_eq!(r.repo, "agent-skills");
        // A trailing slash and a www. host are both fine.
        assert_eq!(remote("https://www.github.com/o/r/").repo, "r");
    }

    #[test]
    fn tree_url_pins_ref_and_subdir() {
        let r = remote(
            "https://github.com/vercel-labs/agent-skills/tree/main/skills/web-design-guidelines",
        );
        assert_eq!(r.git_ref.as_deref(), Some("main"));
        assert_eq!(r.subdir.as_deref(), Some("skills/web-design-guidelines"));
        // A tree URL naming just a ref (no subdir).
        let r = remote("https://github.com/o/r/tree/v1.2.3");
        assert_eq!(r.git_ref.as_deref(), Some("v1.2.3"));
        assert_eq!(r.subdir, None);
    }

    #[test]
    fn path_shaped_tokens_are_local_paths() {
        for p in [
            "./skills/deploy",
            "../deploy",
            "~/skills/x",
            "/abs/deploy",
            ".",
            "..",
        ] {
            assert!(
                matches!(classify(p), SourceSpec::LocalPath(_)),
                "{p} should be a local path"
            );
        }
        // A `/`-prefixed absolute even without a `.`/`~`.
        assert!(matches!(classify("/tmp/deploy"), SourceSpec::LocalPath(_)));
    }

    #[test]
    fn a_bare_multi_segment_relative_path_is_local_not_shorthand() {
        // Two slashes is not `owner/repo` shorthand — it is a relative path.
        assert!(matches!(classify("a/b/c"), SourceSpec::LocalPath(_)));
    }

    #[test]
    fn a_bare_word_is_a_local_discovered_name() {
        assert_eq!(
            classify("deploy"),
            SourceSpec::LocalName("deploy".to_owned())
        );
        // `@harness` stays a bare-word local form (resolved downstream), NOT a remote.
        assert_eq!(
            classify("deploy@claude-code"),
            SourceSpec::LocalName("deploy@claude-code".to_owned())
        );
    }

    #[test]
    fn ssh_and_non_github_hosts_are_unsupported_not_misrouted() {
        assert!(matches!(
            classify("git@github.com:o/r.git"),
            SourceSpec::Unsupported(_)
        ));
        assert!(matches!(
            classify("https://gitlab.com/o/r"),
            SourceSpec::Unsupported(_)
        ));
        assert!(matches!(classify("ssh://x/y"), SourceSpec::Unsupported(_)));
    }

    #[test]
    fn label_reads_origin_ref_and_subdir() {
        let r = remote("https://github.com/o/r/tree/main/skills/foo");
        assert_eq!(r.label(), "github.com/o/r#main (skills/foo)");
        assert_eq!(remote("o/r").label(), "github.com/o/r");
    }
}
