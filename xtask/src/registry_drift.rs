//! `check-registry-drift` — an OPT-IN, advisory check that the baked harness registry
//! ([`topos_harness::registry`]) still matches vercel-labs/skills' upstream `src/agents.ts`.
//!
//! This command FETCHES the current upstream file over HTTPS at runtime and diffs it against the baked
//! table. It is deliberately **not** part of `cargo xtask ci` and NEVER runs in CI: the baked table is a
//! committed artifact, and re-syncing it is a human decision (an upstream agent's dirs are load-bearing
//! for on-disk skill discovery, so a table change deserves a real look). Run it by hand —
//! `cargo xtask check-registry-drift` — when you want to know whether upstream moved; it prints a
//! human-readable report and exits **nonzero on any drift** so the drift is impossible to miss.
//!
//! ## What it parses, and its limits
//! The upstream source is TypeScript, so this is a LIGHTWEIGHT line parse of the `agents` object literal
//! — NOT a real TS parse. For each entry it reads the `name:` slug, the `skillsDir:` string literal (the
//! project/cwd dir), and the `globalSkillsDir:` expression. It reduces the common `join(<root>, '<a>'[,
//! '<b>'…])` form to a canonical `<root-tag>/<suffix>` string that lines up with
//! [`topos_harness::registry::KnownHarness::user_dir_specs`]; an expression it can't reduce (a helper
//! call like `getOpenClawGlobalSkillsDir()`, or `undefined`) is recorded as opaque/none, and the
//! global-dir comparison is skipped for that row (`openclaw`'s three-root helper is exactly why our port
//! carries three user dirs where upstream states one opaque call).
//!
//! It does **not** parse the `detectInstalled` function bodies at all — they are arbitrary TS (helper
//! calls, `process.cwd()`, boolean `||` logic) that a string scan can't model faithfully, so a
//! detect-only upstream change is a KNOWN blind spot this check won't catch. What it DOES catch reliably:
//! an added or removed agent (slug-set drift) and a changed `skillsDir` / `globalSkillsDir`. On a real
//! re-sync, still skim `agents.ts` by eye — this command points you at it, it doesn't replace the read.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::time::Duration;
use topos_harness::registry::KnownHarness;

/// The upstream table, raw, on the default branch. A branch rename or a file move surfaces as a fetch
/// failure — which is itself a signal to go look.
const UPSTREAM_URL: &str =
    "https://raw.githubusercontent.com/vercel-labs/skills/main/src/agents.ts";

/// The dir fields parsed out of one upstream `agents` entry.
struct UpstreamAgent {
    slug: String,
    /// `skillsDir` — the project/cwd dir literal (always present in a well-formed entry).
    project_dir: Option<String>,
    /// `globalSkillsDir`, classified.
    global_dir: GlobalDir,
}

/// How an entry's `globalSkillsDir` reduced.
enum GlobalDir {
    /// A `join(<root>, …)` reduced to a canonical `<tag>/<suffix>` string.
    Spec(String),
    /// `undefined` — the harness has no global skills dir (matches an empty `user_dir_specs`).
    None,
    /// An expression we can't reduce (a helper call) — not comparable, so skipped.
    Opaque,
}

/// Fetch the current upstream `agents.ts` over HTTPS. Network use is intentional and confined to this
/// opt-in command; the workspace's blocking `ureq` (rustls+ring) transport carries it.
fn fetch_upstream() -> Result<String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(20)))
            .build(),
    );
    let resp = agent
        .get(UPSTREAM_URL)
        .header("User-Agent", "topos-xtask-check-registry-drift")
        .call()
        .with_context(|| format!("fetching {UPSTREAM_URL}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        bail!("fetching {UPSTREAM_URL}: HTTP {status}");
    }
    // The default 10 MiB body cap is ample for a source file; raise it a little for headroom anyway.
    let bytes = resp
        .into_body()
        .into_with_config()
        .limit(16 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("reading {UPSTREAM_URL} body"))?;
    String::from_utf8(bytes).context("upstream agents.ts is not valid UTF-8")
}

/// Parse the `agents` object literal into a per-entry list. Bounded to that object so the helper
/// functions ABOVE it (which also contain `join(home, '.openclaw/skills')` etc.) and the exported
/// functions below it are ignored.
fn parse_upstream(src: &str) -> Result<Vec<UpstreamAgent>> {
    let start = src.find("export const agents").context(
        "upstream: no `export const agents` — the file shape changed; re-read agents.ts by hand",
    )?;
    // The `detectInstalledAgents` export directly follows the object literal — an unambiguous end fence.
    let end = src[start..]
        .find("export async function detectInstalledAgents")
        .map_or(src.len(), |i| start + i);
    let region = &src[start..end];

    let mut agents = Vec::new();
    let mut current: Option<UpstreamAgent> = None;
    for line in region.lines() {
        let t = line.trim();
        if let Some(slug) = single_quoted_after(t, "name:") {
            // Every entry opens with its `name:` field; that starts a new record.
            if let Some(done) = current.take() {
                agents.push(done);
            }
            current = Some(UpstreamAgent {
                slug,
                project_dir: None,
                global_dir: GlobalDir::Opaque,
            });
        } else if let Some(a) = current.as_mut() {
            if let Some(dir) = single_quoted_after(t, "skillsDir:") {
                a.project_dir = Some(dir);
            } else if let Some(rhs) = t.strip_prefix("globalSkillsDir:") {
                a.global_dir = classify_global(rhs.trim().trim_end_matches(','));
            }
        }
    }
    if let Some(done) = current.take() {
        agents.push(done);
    }
    if agents.is_empty() {
        bail!("upstream: parsed zero agents — the file shape changed; re-read agents.ts by hand");
    }
    Ok(agents)
}

/// If `line` begins with `key`, return the first single-quoted string literal after it. The trimmed-line
/// prefix match means `globalSkillsDir:` can never be mistaken for `skillsDir:`.
fn single_quoted_after(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    single_quoted(rest.split(',').next().unwrap_or(rest))
}

/// The content of the first single-quoted literal in `s` (`  '.foo/bar'  ` → `.foo/bar`), else `None`.
fn single_quoted(s: &str) -> Option<String> {
    let open = s.find('\'')? + 1;
    let close = s[open..].find('\'')? + open;
    Some(s[open..close].to_owned())
}

/// Classify a `globalSkillsDir` right-hand side (already comma-trimmed).
fn classify_global(rhs: &str) -> GlobalDir {
    if rhs == "undefined" {
        return GlobalDir::None;
    }
    // Only the `join(<root>, '<a>'[, '<b>'…])` form is reducible; a helper call is opaque.
    let Some(inner) = rhs.strip_prefix("join(").and_then(|s| s.strip_suffix(')')) else {
        return GlobalDir::Opaque;
    };
    let mut parts = inner.split(',').map(str::trim);
    let tag = match parts.next() {
        Some("home") => "home",
        Some("configHome") => "configHome",
        Some("codexHome") => "codexHome",
        Some("claudeHome") => "claudeHome",
        Some("vibeHome") => "vibeHome",
        Some("hermesHome") => "hermesHome",
        Some("autohandHome") => "autohandHome",
        _ => return GlobalDir::Opaque,
    };
    let mut segments = Vec::new();
    for part in parts {
        // Every remaining arg must be a plain single-quoted literal; anything else → opaque.
        let Some(lit) = single_quoted(part) else {
            return GlobalDir::Opaque;
        };
        segments.extend(lit.split('/').filter(|s| !s.is_empty()).map(str::to_owned));
    }
    if segments.is_empty() {
        GlobalDir::Spec(tag.to_owned())
    } else {
        GlobalDir::Spec(format!("{tag}/{}", segments.join("/")))
    }
}

/// Fetch, parse, diff, and report. Exits nonzero (via `bail`) on any drift.
pub(crate) fn run() -> Result<()> {
    println!("fetching {UPSTREAM_URL} …");
    let src = fetch_upstream()?;
    let upstream = parse_upstream(&src)?;
    let local = topos_harness::registry::known_harnesses();

    let upstream_slugs: BTreeMap<&str, ()> =
        upstream.iter().map(|a| (a.slug.as_str(), ())).collect();
    let local_by: BTreeMap<&str, &KnownHarness> = local.iter().map(|h| (h.slug, h)).collect();

    // Rows upstream added (missing here) / removed (still here). Reported in the reading order of each
    // source so the report is stable run-to-run.
    let missing_local: Vec<&str> = upstream
        .iter()
        .map(|a| a.slug.as_str())
        .filter(|s| !local_by.contains_key(s))
        .collect();
    let missing_upstream: Vec<&str> = local
        .iter()
        .map(|h| h.slug)
        .filter(|s| !upstream_slugs.contains_key(s))
        .collect();

    // Per-row dir mismatches (only for slugs present in BOTH tables).
    let mut mismatches: Vec<String> = Vec::new();
    for up in &upstream {
        let Some(loc) = local_by.get(up.slug.as_str()) else {
            continue;
        };
        let slug = &up.slug;
        if let Some(up_proj) = &up.project_dir
            && up_proj != loc.project_dir()
        {
            mismatches.push(format!(
                "{slug}: project dir — upstream `{up_proj}`, local `{}`",
                loc.project_dir()
            ));
        }
        match &up.global_dir {
            GlobalDir::Opaque => {} // an un-reducible upstream expression — not comparable
            GlobalDir::None => {
                let local_dirs = loc.user_dir_specs();
                if !local_dirs.is_empty() {
                    mismatches.push(format!(
                        "{slug}: global dir — upstream none, local {local_dirs:?}"
                    ));
                }
            }
            GlobalDir::Spec(up_dir) => {
                let local_first = loc.user_dir_specs().into_iter().next();
                if local_first.as_deref() != Some(up_dir.as_str()) {
                    let shown = local_first.map_or_else(|| "none".to_owned(), |d| format!("`{d}`"));
                    mismatches.push(format!(
                        "{slug}: global dir — upstream `{up_dir}`, local {shown}"
                    ));
                }
            }
        }
    }
    mismatches.sort();

    println!(
        "\nupstream agents: {}   |   baked harnesses: {}",
        upstream.len(),
        local.len()
    );

    if missing_local.is_empty() && missing_upstream.is_empty() && mismatches.is_empty() {
        println!(
            "\nno drift — the baked registry matches upstream agents.ts (slugs + project/global dirs)."
        );
        println!(
            "(note: detectInstalled bodies are not parsed — a detect-only upstream change is not \
             caught here; skim agents.ts on a real re-sync.)"
        );
        return Ok(());
    }

    println!(
        "\nDRIFT DETECTED — re-sync crates/topos-harness/src/registry.rs against upstream agents.ts:\n"
    );
    if !missing_local.is_empty() {
        println!("  rows upstream but MISSING locally (add them, in upstream file order):");
        for s in &missing_local {
            println!("    + {s}");
        }
    }
    if !missing_upstream.is_empty() {
        println!("  rows local but GONE upstream (remove them):");
        for s in &missing_upstream {
            println!("    - {s}");
        }
    }
    if !mismatches.is_empty() {
        println!("  per-row dir mismatches:");
        for m in &mismatches {
            println!("    ~ {m}");
        }
    }
    bail!("harness registry drift vs upstream agents.ts (see report above)");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed slice of the real upstream shape — enough to exercise every parse branch: a plain
    /// `join(home, …)`, a multi-arg `join(home, '.agents', 'skills')`, an env-rooted `join(codexHome, …)`,
    /// an opaque helper call, and `undefined`.
    const SAMPLE: &str = r#"
export function getOpenClawGlobalSkillsDir() { return join(home, '.openclaw/skills'); }
export const agents: Record<AgentType, AgentConfig> = {
  'aider-desk': {
    name: 'aider-desk',
    displayName: 'AiderDesk',
    skillsDir: '.aider-desk/skills',
    globalSkillsDir: join(home, '.aider-desk/skills'),
    detectInstalled: async () => { return existsSync(join(home, '.aider-desk')); },
  },
  cline: {
    name: 'cline',
    displayName: 'Cline',
    skillsDir: '.agents/skills',
    globalSkillsDir: join(home, '.agents', 'skills'),
    detectInstalled: async () => { return existsSync(join(home, '.cline')); },
  },
  codex: {
    name: 'codex',
    displayName: 'Codex',
    skillsDir: '.agents/skills',
    globalSkillsDir: join(codexHome, 'skills'),
    detectInstalled: async () => { return existsSync(codexHome); },
  },
  openclaw: {
    name: 'openclaw',
    displayName: 'OpenClaw',
    skillsDir: 'skills',
    globalSkillsDir: getOpenClawGlobalSkillsDir(),
    detectInstalled: async () => { return existsSync(join(home, '.openclaw')); },
  },
  eve: {
    name: 'eve',
    displayName: 'Eve',
    skillsDir: 'agent/skills',
    globalSkillsDir: undefined,
    detectInstalled: async () => { return false; },
  },
};
export async function detectInstalledAgents() { return []; }
"#;

    #[test]
    fn parses_slug_and_dirs_and_classifies_globals() {
        let agents = parse_upstream(SAMPLE).expect("parses");
        let by: BTreeMap<&str, &UpstreamAgent> =
            agents.iter().map(|a| (a.slug.as_str(), a)).collect();
        // Exactly the five entries — the helper above the object never trips the `name:` anchor.
        assert_eq!(agents.len(), 5, "one record per agent, helpers ignored");

        let aider = by["aider-desk"];
        assert_eq!(aider.project_dir.as_deref(), Some(".aider-desk/skills"));
        assert!(matches!(&aider.global_dir, GlobalDir::Spec(s) if s == "home/.aider-desk/skills"));

        // Multi-arg join reduces to one `/`-joined suffix.
        assert!(
            matches!(&by["cline"].global_dir, GlobalDir::Spec(s) if s == "home/.agents/skills")
        );
        // An env-rooted join keeps its root tag.
        assert!(matches!(&by["codex"].global_dir, GlobalDir::Spec(s) if s == "codexHome/skills"));
        // A helper call is opaque; `undefined` is none.
        assert!(matches!(by["openclaw"].global_dir, GlobalDir::Opaque));
        assert!(matches!(by["eve"].global_dir, GlobalDir::None));
    }

    #[test]
    fn canonical_forms_agree_with_the_baked_accessors() {
        // The parse's canonical strings must line up with the registry's own rendering for the same
        // rows — otherwise a clean re-sync would still report false drift.
        let agents = parse_upstream(SAMPLE).expect("parses");
        let by: BTreeMap<&str, &UpstreamAgent> =
            agents.iter().map(|a| (a.slug.as_str(), a)).collect();
        let local: BTreeMap<&str, &KnownHarness> = topos_harness::registry::known_harnesses()
            .iter()
            .map(|h| (h.slug, h))
            .collect();
        for slug in ["aider-desk", "cline", "codex"] {
            let GlobalDir::Spec(up) = &by[slug].global_dir else {
                panic!("{slug} should reduce to a spec");
            };
            assert_eq!(
                local[slug].user_dir_specs().first().map(String::as_str),
                Some(up.as_str()),
                "{slug}: parsed global dir must equal the baked accessor's first user dir",
            );
            assert_eq!(
                by[slug].project_dir.as_deref(),
                Some(local[slug].project_dir())
            );
        }
    }
}
