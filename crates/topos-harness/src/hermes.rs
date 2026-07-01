//! The `Hermes` [`HarnessAdapter`] — mixed-depth skill discovery, byte-exact placement targeting,
//! and the idempotent **per-turn currency trigger** edit of `~/.hermes/config.yaml`.
//!
//! Hermes's currency mechanism is its **injecting `pre_llm_call` hook**: a shell hook Hermes runs
//! before *every* LLM turn (its `on_session_start` event exists but is observer-only and is
//! deliberately NOT registered here). The registered command is `topos pull --quiet` — the same
//! byte-silent sweep the Claude Code adapter arms — so a followed skill's placement bytes are
//! refreshed before the turn and an update is current **on the first turn**.
//!
//! Hermes gates shell hooks behind a one-time `(event, command)` **consent allowlist** persisted at
//! `~/.hermes/shell-hooks-allowlist.json`: it solicits approval at an interactive TTY, silently
//! skips soliciting in a non-TTY session, and auto-accepts under its own `--accept-hooks` /
//! `HERMES_ACCEPT_HOOKS` / `hooks_auto_accept: true` escape hatches. The approval is string-keyed
//! on the exact command, so a topos binary update never re-prompts. This adapter **never writes
//! that allowlist** (consent is Hermes's own artifact — topos builds no second permission system);
//! it only *reads* it, plus the auto-accept signals, as the evidence for an honest report:
//! [`TriggerState::Active`] is claimed **only** on durable acceptance evidence, otherwise the entry
//! is still registered but the report degrades plainly to [`CurrencyKind::ExplicitPullOnly`] —
//! never a fake "it will appear on its own."
//!
//! Verified against a real local Hermes Agent v0.17.0 (2026.6.19) install: the `hooks:` block
//! schema and its shipped `hooks: {}` default, the argv (`shlex.split`, no shell) command form, the
//! allowlist file schema and exact-string keying, the auto-accept resolution set, the per-turn
//! `pre_llm_call` invocation, and the mixed-depth skills layout. MUST-VERIFY: the pilot team's
//! exact Hermes build may differ — every concrete filename / key / line shape below is a named
//! const so a correction is a one-line change, and a failed probe only flips reports onto the
//! explicit-pull degrade floor (never a rebuild).
//!
//! Content-blind: it reads skill *directories* only to confirm a `SKILL.md` exists (never the
//! bytes, never the frontmatter), and the only file it ever writes is the harness **config** —
//! never a skill file. The config is YAML, which no dependency here parses, so the edit is an
//! **anchored, line-surgical merge**: it handles exactly the shapes it can prove (the shipped
//! `hooks: {}` default, an absent/empty `hooks:` key, an absent file, its own previously-written
//! entry) byte-preservingly, and fails **closed** (`Degraded`, zero writes) on everything else —
//! it never re-styles, never guesses an insertion point, and never clobbers. The merge itself is
//! pure (bytes in → an [`EditPlan`] out); the crash-safe write is delegated to the injected
//! [`ConfigStore`], exactly like the Claude Code reference.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::{ConfigStore, DiscoveredPlacement, HarnessAdapter, PlacementTarget};

/// Hermes's user config file, under the resolved home. (Probed: v0.17.0 reads
/// `~/.hermes/config.yaml`; its `hooks:` block is the only registration surface — there is no
/// `hermes hooks add`.)
const CONFIG_FILENAME: &str = "config.yaml";

/// Hermes's persisted consent allowlist (probed schema:
/// `{"approvals": [{"event": <str>, "command": <str>}, …]}`). READ-ONLY here — Hermes records its
/// own approvals; topos never forges one.
const ALLOWLIST_FILENAME: &str = "shell-hooks-allowlist.json";

/// The injecting per-turn event this adapter registers. `on_session_start` is observer-only and
/// is never the currency mechanism.
const EVENT_NAME: &str = "pre_llm_call";

/// The exact argv command Hermes runs each turn (`shlex.split`, `shell=False` — no shell one-liner,
/// no `command -v` guard is possible). Byte-stable across topos updates, so the string-keyed
/// allowlist approval never re-prompts. `--quiet` keeps the sweep byte-silent on stdout.
const HOOK_COMMAND: &str = "topos pull --quiet";

/// The command-identity substring marking any `topos pull` hook (managed or hand-rolled).
const COMMAND_IDENTITY: &str = "topos pull";

/// The managed entry line as this adapter writes it (compared against `str::trim`ed config lines).
/// The trailing `# topos:currency` is a YAML comment *outside* the scalar — Hermes parses the
/// command as exactly [`HOOK_COMMAND`]; the comment is the ownership sentinel.
const ENTRY_LINE: &str = "- command: topos pull --quiet  # topos:currency";

/// The structured marker identity reported in [`TriggerReport::marker_id`].
const MARKER_ID: &str = "topos:hermes:currency:1";

/// The 3-line block registering the managed hook (also the whole fresh-file config). Verified to
/// parse: `hooks.pre_llm_call[0].command == "topos pull --quiet"`.
const HOOK_BLOCK: &str =
    "hooks:\n  pre_llm_call:\n  - command: topos pull --quiet  # topos:currency\n";

/// The shipped default form of an empty hooks block — the one line the surgical install replaces
/// (and a clean removal restores).
const HOOKS_EMPTY_LINE: &str = "hooks: {}";

/// The zero-indent prefix identifying the top-level `hooks` key (any form: bare, flow, commented).
const HOOKS_KEY_PREFIX: &str = "hooks:";

/// The `pre_llm_call` sub-key line inside the hooks block (trim-compared).
const EVENT_KEY_LINE: &str = "pre_llm_call:";

/// Hermes's config-file auto-accept key (zero-indent, top-level).
const AUTO_ACCEPT_KEY_PREFIX: &str = "hooks_auto_accept:";

/// The auto-accept value that counts as durable acceptance evidence.
const AUTO_ACCEPT_TRUE_LINE: &str = "hooks_auto_accept: true";

/// The default category a no-discovery placement lands under: `<home>/skills/general/<skill_id>`.
/// A category is the *user's* taxonomy — `general` is the neutral catch-all shelf.
const DEFAULT_CATEGORY: &str = "general";

/// Support directories Hermes prunes from the skill walk **under a dir that itself has a
/// `SKILL.md`** (they hold progressive-disclosure data, sometimes including archived `SKILL.md`
/// files that must not surface as skills). Probed from v0.17.0's own walk.
const SKILL_SUPPORT_DIRS: [&str; 4] = ["references", "templates", "assets", "scripts"];

/// The `Hermes` [`HarnessAdapter`]. Holds the resolved config home and the acceptance-evidence
/// flag (both injected, so tests never touch the real `~/.hermes` or the process environment) and
/// the [`ConfigStore`] port that performs the durable config write.
pub struct Hermes<'a> {
    /// `$HERMES_HOME` (Hermes's own override) else `$HOME/.hermes`.
    home: PathBuf,
    /// Whether `HERMES_ACCEPT_HOOKS` was set truthy in this environment — Hermes's own auto-accept
    /// escape hatch. Sampled once by production via [`Hermes::resolve_accept_hooks`]; an
    /// env set for topos but not for the later Hermes process over-reports until Hermes's next
    /// run records the approval — a named residual of Hermes's own env contract.
    accept_hooks: bool,
    cfg: &'a dyn ConfigStore,
}

impl std::fmt::Debug for Hermes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hermes")
            .field("home", &self.home)
            .field("accept_hooks", &self.accept_hooks)
            .finish_non_exhaustive()
    }
}

impl<'a> Hermes<'a> {
    /// Construct over an explicit config home + acceptance evidence + a config-store port.
    /// Production passes [`Hermes::resolve_home`] and [`Hermes::resolve_accept_hooks`]; tests pass
    /// a temp dir and a literal so the real `~/.hermes` and the env are never touched.
    #[must_use]
    pub fn new(home: PathBuf, accept_hooks: bool, cfg: &'a dyn ConfigStore) -> Self {
        Self {
            home,
            accept_hooks,
            cfg,
        }
    }

    /// Resolve Hermes's home exactly as Hermes does: `$HERMES_HOME` if set, else `$HOME/.hermes`
    /// (falling back to `./.hermes` if `$HOME` is unset).
    #[must_use]
    pub fn resolve_home() -> PathBuf {
        if let Some(dir) = std::env::var_os("HERMES_HOME") {
            return PathBuf::from(dir);
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".hermes")
    }

    /// Whether `HERMES_ACCEPT_HOOKS` is set truthy, using Hermes's own truthiness set
    /// (probed: lowercase value in `{"1", "true", "yes", "on"}`).
    #[must_use]
    pub fn resolve_accept_hooks() -> bool {
        std::env::var("HERMES_ACCEPT_HOOKS")
            .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    fn config_path(&self) -> PathBuf {
        self.home.join(CONFIG_FILENAME)
    }

    fn allowlist_path(&self) -> PathBuf {
        self.home.join(ALLOWLIST_FILENAME)
    }

    /// Read the current config, returning `None` if the file does not exist and `Err` only on a
    /// genuine I/O failure (a permission error, say) — distinct from absent.
    fn read_config(&self) -> std::io::Result<Option<Vec<u8>>> {
        self.cfg.read(&self.config_path())
    }

    /// Durable acceptance evidence: Hermes's persisted allowlist holds our exact
    /// `(event, command)` pair, or the config carries a top-level `hooks_auto_accept: true`, or
    /// this environment carries Hermes's own accept env. Every read fails **closed** — an
    /// unreadable or oddly-shaped source is never evidence.
    fn acceptance_evidence(&self, config: Option<&[u8]>) -> bool {
        if self.accept_hooks {
            return true;
        }
        if let Ok(Some(bytes)) = self.cfg.read(&self.allowlist_path())
            && allowlist_approves(&bytes)
        {
            return true;
        }
        config
            .and_then(|b| std::str::from_utf8(b).ok())
            .is_some_and(config_auto_accepts)
    }

    /// Apply a planned edit: write through the port (degrading honestly if the write fails) or
    /// leave the file untouched, reporting the planned state.
    fn apply(&self, plan: EditPlan) -> TriggerReport {
        match plan {
            EditPlan::Leave(state) => self.report(state, false),
            EditPlan::Write(bytes, state) => match self.cfg.replace(&self.config_path(), &bytes) {
                Ok(()) => self.report(state, true),
                Err(_) => self.report(TriggerState::Degraded, false),
            },
        }
    }

    /// Build the report. The currency kind rides the state honestly: only a confirmably-live
    /// trigger claims the per-turn `FirstTurn`; every other state degrades plainly to the
    /// explicit-pull floor.
    fn report(&self, state: TriggerState, touched: bool) -> TriggerReport {
        TriggerReport {
            harness: HarnessId::Hermes,
            currency_kind: if state == TriggerState::Active {
                CurrencyKind::FirstTurn
            } else {
                CurrencyKind::ExplicitPullOnly
            },
            touched_path: touched.then(|| self.config_path().to_string_lossy().into_owned()),
            marker_id: MARKER_ID.to_owned(),
            state,
        }
    }

    /// Whether the managed currency entry is currently present (drives `--footprint` disclosure).
    /// A missing/unreadable/unprovable config means "not present" — we never claim to own a path
    /// we cannot confirm.
    fn has_managed_entry(&self) -> bool {
        let Ok(Some(bytes)) = self.read_config() else {
            return false;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return false;
        };
        matches!(analyze(text), Analysis::Managed(_))
    }
}

/// The non-dot, UTF-8-named child directories of `dir` (following symlinks — a symlinked skill dir
/// is valid). Absent or unreadable → empty, never an error. Dot-dirs are never skills (incl. the
/// materializer's transient `.topos-staging-*` siblings during the sub-second swap window).
fn child_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            out.push((name, path));
        }
    }
    out
}

/// SKILL.md's *existence* confirms skill-ness — never the frontmatter (all-optional, never parsed
/// here), so a malformed SKILL.md cannot mislead.
fn is_skill_dir(path: &Path) -> bool {
    path.join("SKILL.md").is_file()
}

impl HarnessAdapter for Hermes<'_> {
    fn id(&self) -> HarnessId {
        HarnessId::Hermes
    }

    /// Walk `<home>/skills/` in Hermes's own mixed-depth shape (probed): a dir with a root
    /// `SKILL.md` is a skill wherever it sits — `skills/<name>/` (uncategorized, `layer: None`)
    /// or `skills/<category>/<name>/` (`layer: Some(category)`). A level-1 skill dir is still
    /// descended (minus its support dirs) so a nested skill under it is not invisible. Deeper
    /// nesting is out of this probe's shape and left undiscovered.
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        let mut out = Vec::new();
        for (name, path) in child_dirs(&self.skills_dir()) {
            let is_skill = is_skill_dir(&path);
            if is_skill {
                out.push(DiscoveredPlacement {
                    path: path.clone(),
                    layer: None,
                });
            }
            // Level 2: the children of a category dir — or of a level-1 skill dir, minus its
            // support dirs (which may hold archived SKILL.md files that must not surface).
            for (child_name, child_path) in child_dirs(&path) {
                if is_skill && SKILL_SUPPORT_DIRS.contains(&child_name.as_str()) {
                    continue;
                }
                if is_skill_dir(&child_path) {
                    out.push(DiscoveredPlacement {
                        path: child_path,
                        layer: Some(name.clone()),
                    });
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path)); // read_dir order is OS-dependent — pin it
        out
    }

    fn placement_for(
        &self,
        skill_id: &str,
        discovered: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        match discovered {
            // The discovered categorized (or root-level) dir is reused verbatim: placement keys by
            // the stable skill id + this concrete path, never a bare name — two same-name skills
            // in different categories stay distinct by path. (Name-level collisions are the
            // client's existing name-or-id UX, not the adapter's concern.)
            Some(d) => PlacementTarget {
                dir: d.path.clone(),
            },
            // No-discovery default: the categorized `<home>/skills/general/<skill_id>` — the shape
            // a pure follower's first receive lands in.
            None => PlacementTarget {
                dir: self.skills_dir().join(DEFAULT_CATEGORY).join(skill_id),
            },
        }
    }

    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::FirstTurn
    }

    fn install_currency_trigger(&self) -> TriggerReport {
        match self.read_config() {
            Ok(current) => {
                let live = self.acceptance_evidence(current.as_deref());
                self.apply(plan_install(current.as_deref(), live))
            }
            // Unreadable (e.g. a permission error) — degrade honestly, never blind-overwrite.
            Err(_) => self.report(TriggerState::Degraded, false),
        }
    }

    fn remove_currency_trigger(&self) -> TriggerReport {
        match self.read_config() {
            Ok(current) => self.apply(plan_remove(current.as_deref())),
            Err(_) => self.report(TriggerState::Degraded, false),
        }
    }

    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        // Disclose the config file ONLY when it actually holds our managed entry — and never as a
        // path `uninstall` will delete (it is scrubbed via `remove_currency_trigger`, the file
        // kept). The allowlist is Hermes's own consent record, never disclosed as topos-owned.
        if self.has_managed_entry() {
            vec![self.config_path()]
        } else {
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pure config merge — bytes in → an edit plan out. No I/O. The config is YAML, which nothing
// here parses generally: the merge is line-anchored and total over exactly the shapes it can
// prove, and fails closed (`Degraded`, zero writes) on everything else. Untouched lines are
// preserved byte-for-byte — this is a user's file; it is never re-styled.
// ---------------------------------------------------------------------------------------------

/// What a planned edit does: write the post-image bytes (and report the resulting state), or
/// leave the file untouched (a true no-op — an unchanged run never rewrites the user's file).
enum EditPlan {
    Write(Vec<u8>, TriggerState),
    Leave(TriggerState),
}

/// The managed entry's location, for the surgical remove.
struct ManagedEntry {
    /// Index of the single zero-indent `hooks:` line.
    hooks_idx: usize,
    /// Index of the `pre_llm_call:` key line inside the hooks region.
    event_idx: usize,
    /// End of the hooks region (exclusive line index).
    region_end: usize,
    /// End of the `pre_llm_call` block (exclusive line index).
    block_end: usize,
    /// Every line in the block that is exactly our managed entry.
    entry_lines: Vec<usize>,
}

/// How the existing config relates to the managed entry.
enum Analysis {
    /// No zero-indent `hooks:` key at all — a block can be appended as a new top-level key.
    NoHooksKey,
    /// A single bare/empty `hooks:` or `hooks: {}` with nothing under it — the shipped default;
    /// that one line can be replaced by the managed block.
    EmptyHooks { hooks_idx: usize },
    /// Our sentinel-marked entry is present under `hooks.pre_llm_call`.
    Managed(ManagedEntry),
    /// A `topos pull` command entry exists in the hooks region that is not provably ours
    /// (hand-rolled, or ours after a comment-stripping config rewrite) — adopt-or-leave.
    UnmanagedToposPull,
    /// A populated hooks block with no topos entry — never guess an insertion point.
    Populated,
    /// Duplicate `hooks:` keys, non-UTF-8 bytes, or any other unprovable shape — fail closed.
    Unprovable,
}

/// Split into lines preserving each line's bytes (terminators included), so untouched lines are
/// re-emitted verbatim.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A line's zero-indent key test: the raw line (not its trim) starts with the prefix, so an
/// indented occurrence nested inside some other mapping never matches.
fn is_zero_indent(line: &str, prefix: &str) -> bool {
    line.starts_with(prefix)
}

/// Whether a line is blank or a YAML comment (at any indent) — neutral for region/block scans.
fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// The exclusive end of the region belonging to the zero-indent key at `start`: lines up to the
/// next zero-indent content line (blank lines and comments — even at column 0 — stay in-region;
/// they cannot end a YAML block).
fn region_end(lines: &[&str], start: usize) -> usize {
    let mut end = start + 1;
    while end < lines.len() {
        let line = lines[end];
        let first = line.chars().next();
        let indented = matches!(first, Some(' ' | '\t'));
        if !indented && !is_blank_or_comment(line) {
            break;
        }
        end += 1;
    }
    end
}

/// The indentation width (leading spaces) of a raw line.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Whether a trimmed line is a command entry (a `pre_llm_call`-style list item or its bare form).
fn is_command_line(trimmed: &str) -> bool {
    trimmed.starts_with("- command:") || trimmed.starts_with("command:")
}

fn analyze(text: &str) -> Analysis {
    let lines = split_lines(text);
    let hooks_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_zero_indent(l, HOOKS_KEY_PREFIX))
        .map(|(i, _)| i)
        .collect();
    match hooks_lines.len() {
        0 => return Analysis::NoHooksKey,
        1 => {}
        _ => return Analysis::Unprovable, // duplicate top-level keys — YAML-ambiguous
    }
    let hooks_idx = hooks_lines[0];
    let hooks_trim = lines[hooks_idx].trim();
    let end = region_end(&lines, hooks_idx);
    let region_empty = lines[hooks_idx + 1..end]
        .iter()
        .all(|l| is_blank_or_comment(l));

    if hooks_trim == HOOKS_EMPTY_LINE || hooks_trim == HOOKS_KEY_PREFIX {
        if region_empty {
            return Analysis::EmptyHooks { hooks_idx };
        }
        if hooks_trim == HOOKS_EMPTY_LINE {
            // `hooks: {}` with indented content under it is not YAML we can reason about.
            return Analysis::Unprovable;
        }
    } else if region_empty {
        // A flow/inline form (`hooks: {…}`) or a trailing comment on the key — never provable.
        return Analysis::Unprovable;
    }

    // A populated block: locate the `pre_llm_call:` key and our entry inside its sub-block.
    let event_lines: Vec<usize> = (hooks_idx + 1..end)
        .filter(|&i| lines[i].trim() == EVENT_KEY_LINE)
        .collect();
    if event_lines.len() > 1 {
        return Analysis::Unprovable;
    }
    if let Some(&event_idx) = event_lines.first() {
        let event_indent = indent_of(lines[event_idx]);
        // The block: subsequent region lines that are blank/comments, list items at the key's
        // indent, or anything indented deeper.
        let mut block_end = event_idx + 1;
        while block_end < end {
            let line = lines[block_end];
            if is_blank_or_comment(line) {
                block_end += 1;
                continue;
            }
            let ind = indent_of(line);
            if ind > event_indent || (ind == event_indent && line.trim().starts_with("- ")) {
                block_end += 1;
                continue;
            }
            break;
        }
        let entry_lines: Vec<usize> = (event_idx + 1..block_end)
            .filter(|&i| lines[i].trim() == ENTRY_LINE)
            .collect();
        if !entry_lines.is_empty() {
            return Analysis::Managed(ManagedEntry {
                hooks_idx,
                event_idx,
                region_end: end,
                block_end,
                entry_lines,
            });
        }
    }
    // No managed entry: any topos-pull command entry anywhere in the hooks region (any event,
    // incl. our own entry after a comment-stripping rewrite, or moved under another event) is
    // adopt-or-leave — never blind-append a second one, never claim it.
    let unmanaged = (hooks_idx + 1..end).any(|i| {
        let t = lines[i].trim();
        is_command_line(t) && t.contains(COMMAND_IDENTITY)
    });
    if unmanaged {
        Analysis::UnmanagedToposPull
    } else {
        Analysis::Populated
    }
}

/// The state a successful install reports: live evidence claims the per-turn trigger; otherwise
/// the entry is registered but honestly not active until Hermes's own approval completes.
fn install_state(live: bool) -> TriggerState {
    if live {
        TriggerState::Active
    } else {
        TriggerState::Inactive
    }
}

fn plan_install(current: Option<&[u8]>, live: bool) -> EditPlan {
    let text = match current {
        None => return EditPlan::Write(HOOK_BLOCK.as_bytes().to_vec(), install_state(live)),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(t) if t.trim().is_empty() => {
                return EditPlan::Write(HOOK_BLOCK.as_bytes().to_vec(), install_state(live));
            }
            Ok(t) => t,
            Err(_) => return EditPlan::Leave(TriggerState::Degraded),
        },
    };
    match analyze(text) {
        Analysis::Managed(_) => EditPlan::Leave(install_state(live)), // ours → true no-op
        Analysis::UnmanagedToposPull => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged),
        Analysis::Populated | Analysis::Unprovable => EditPlan::Leave(TriggerState::Degraded),
        Analysis::NoHooksKey => {
            // Append as a new top-level key, separated by a newline if the file lacks one.
            let mut out = text.as_bytes().to_vec();
            if !out.ends_with(b"\n") {
                out.push(b'\n');
            }
            out.extend_from_slice(HOOK_BLOCK.as_bytes());
            EditPlan::Write(out, install_state(live))
        }
        Analysis::EmptyHooks { hooks_idx } => {
            // Replace the one empty-hooks line with the managed block; every other line verbatim.
            let lines = split_lines(text);
            let mut out = Vec::with_capacity(text.len() + HOOK_BLOCK.len());
            for (i, line) in lines.iter().enumerate() {
                if i == hooks_idx {
                    out.extend_from_slice(HOOK_BLOCK.as_bytes());
                } else {
                    out.extend_from_slice(line.as_bytes());
                }
            }
            EditPlan::Write(out, install_state(live))
        }
    }
}

fn plan_remove(current: Option<&[u8]>) -> EditPlan {
    let text = match current {
        None => return EditPlan::Leave(TriggerState::Inactive), // nothing to remove
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => return EditPlan::Leave(TriggerState::Degraded),
        },
    };
    match analyze(text) {
        Analysis::NoHooksKey | Analysis::EmptyHooks { .. } | Analysis::Populated => {
            EditPlan::Leave(TriggerState::Inactive)
        }
        Analysis::UnmanagedToposPull => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged),
        Analysis::Unprovable => EditPlan::Leave(TriggerState::Degraded),
        Analysis::Managed(m) => {
            let lines = split_lines(text);
            let mut drop: Vec<usize> = m.entry_lines.clone();
            // Prune the `pre_llm_call:` key ONLY when our removal is what emptied its block
            // (comments and blanks are kept — they are the user's, and YAML tolerates them).
            let block_emptied = (m.event_idx + 1..m.block_end)
                .all(|i| drop.contains(&i) || is_blank_or_comment(lines[i]));
            if block_emptied {
                drop.push(m.event_idx);
            }
            // Restore the shipped `hooks: {}` form ONLY when the whole region is then empty of
            // content — never disturbing any sibling event or user line.
            let region_emptied = block_emptied
                && (m.hooks_idx + 1..m.region_end)
                    .all(|i| drop.contains(&i) || is_blank_or_comment(lines[i]));
            let mut out = Vec::with_capacity(text.len());
            for (i, line) in lines.iter().enumerate() {
                if drop.contains(&i) {
                    continue;
                }
                if i == m.hooks_idx && region_emptied {
                    out.extend_from_slice(HOOKS_EMPTY_LINE.as_bytes());
                    out.push(b'\n');
                } else {
                    out.extend_from_slice(line.as_bytes());
                }
            }
            EditPlan::Write(out, TriggerState::Inactive)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Acceptance evidence — read-only views of Hermes's own consent state. Fail closed everywhere.
// ---------------------------------------------------------------------------------------------

/// Whether Hermes's persisted allowlist approves our exact `(event, command)` pair. The schema is
/// the probed `{"approvals": [{"event": …, "command": …}]}`; anything absent, malformed, or
/// wrong-typed is NOT evidence.
fn allowlist_approves(bytes: &[u8]) -> bool {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let Some(approvals) = root.get("approvals").and_then(serde_json::Value::as_array) else {
        return false;
    };
    approvals.iter().any(|e| {
        e.get("event").and_then(serde_json::Value::as_str) == Some(EVENT_NAME)
            && e.get("command").and_then(serde_json::Value::as_str) == Some(HOOK_COMMAND)
    })
}

/// Whether the config carries a top-level `hooks_auto_accept: true`. Only zero-indent lines count
/// (a nested occurrence inside some other mapping is not Hermes's key), and any conflicting
/// duplicate fails closed.
fn config_auto_accepts(text: &str) -> bool {
    let mut saw_true = false;
    for line in text.lines() {
        if is_zero_indent(line, AUTO_ACCEPT_KEY_PREFIX) {
            if line.trim() == AUTO_ACCEPT_TRUE_LINE {
                saw_true = true;
            } else {
                return false; // a non-true (or ambiguous) top-level value — not evidence
            }
        }
    }
    saw_true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// An in-memory [`ConfigStore`] keyed by path (the adapter reads two files: the config and the
    /// allowlist), for the pure-merge tests. The crash-safe write itself is exercised by the CLI's
    /// fault-injection sweep, where the real syscalls live.
    #[derive(Debug, Default)]
    struct MemConfig {
        files: RefCell<HashMap<PathBuf, Vec<u8>>>,
        writes: RefCell<u32>,
    }
    impl MemConfig {
        fn with_config(bytes: &str) -> Self {
            let store = Self::default();
            store
                .files
                .borrow_mut()
                .insert(PathBuf::from("/h/config.yaml"), bytes.as_bytes().to_vec());
            store
        }
        fn set_allowlist(&self, bytes: &str) {
            self.files.borrow_mut().insert(
                PathBuf::from("/h/shell-hooks-allowlist.json"),
                bytes.as_bytes().to_vec(),
            );
        }
        fn config_text(&self) -> Option<String> {
            self.files
                .borrow()
                .get(&PathBuf::from("/h/config.yaml"))
                .map(|b| String::from_utf8(b.clone()).unwrap())
        }
        fn writes(&self) -> u32 {
            *self.writes.borrow()
        }
    }
    impl ConfigStore for MemConfig {
        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.files.borrow().get(path).cloned())
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), bytes.to_vec());
            *self.writes.borrow_mut() += 1;
            Ok(())
        }
    }

    /// A self-cleaning temp dir for the `discover` tests (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-hermes-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        /// A skill dir at `skills/<rel...>` with a root `SKILL.md`.
        fn skill(&self, rel: &[&str]) {
            let mut d = self.0.join("skills");
            for part in rel {
                d = d.join(part);
            }
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), b"---\nname: x\n---\n# x\n").unwrap();
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn adapter<'a>(cfg: &'a MemConfig) -> Hermes<'a> {
        Hermes::new(PathBuf::from("/h"), false, cfg)
    }

    fn accepting_adapter<'a>(cfg: &'a MemConfig) -> Hermes<'a> {
        Hermes::new(PathBuf::from("/h"), true, cfg)
    }

    /// The exact fresh-install bytes — the byte-compared fixture. Registers ONLY the injecting
    /// per-turn `pre_llm_call` (never `on_session_start`), the command a plain argv with the
    /// sentinel as a YAML comment outside the scalar.
    const FRESH_INSTALL: &str = "\
hooks:
  pre_llm_call:
  - command: topos pull --quiet  # topos:currency
";

    /// A shipped-default-shaped config: sibling keys + the literal empty hooks line.
    const DEFAULT_CONFIG: &str = "\
model: gpt-9
approvals:
  mode: manual
hooks: {}
hooks_auto_accept: false
personalities: {}
";

    /// What installing into [`DEFAULT_CONFIG`] must produce: only the one line replaced, every
    /// sibling byte verbatim.
    const DEFAULT_CONFIG_INSTALLED: &str = "\
model: gpt-9
approvals:
  mode: manual
hooks:
  pre_llm_call:
  - command: topos pull --quiet  # topos:currency
hooks_auto_accept: false
personalities: {}
";

    #[test]
    fn install_into_absent_config_writes_the_exact_fresh_block() {
        let cfg = MemConfig::default(); // absent
        let report = adapter(&cfg).install_currency_trigger();

        assert_eq!(report.harness, HarnessId::Hermes);
        assert_eq!(report.marker_id, MARKER_ID);
        // No acceptance evidence: the entry is registered but honestly NOT active — the report
        // degrades to the explicit-pull floor, never a fake Active.
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert!(
            report.touched_path.is_some(),
            "a fresh write touches the file"
        );
        assert_eq!(
            cfg.config_text().as_deref(),
            Some(FRESH_INSTALL),
            "byte-exact fixture"
        );
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_registers_pre_llm_call_and_never_on_session_start() {
        let cfg = MemConfig::default();
        adapter(&cfg).install_currency_trigger();
        let text = cfg.config_text().unwrap();
        assert!(
            text.contains("pre_llm_call"),
            "the injecting per-turn event"
        );
        assert!(
            !text.contains("session_start"),
            "on_session_start is observer-only and never the currency mechanism"
        );
        // And the adapter never claims the session-start kind in any report.
        let report = adapter(&cfg).install_currency_trigger();
        assert_ne!(report.currency_kind, CurrencyKind::SessionStart);
    }

    #[test]
    fn install_replaces_the_shipped_default_empty_hooks_line() {
        let cfg = MemConfig::with_config(DEFAULT_CONFIG);
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(
            cfg.config_text().as_deref(),
            Some(DEFAULT_CONFIG_INSTALLED),
            "only the `hooks: {{}}` line is replaced; every sibling byte survives verbatim"
        );
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_appends_when_no_hooks_key_even_without_trailing_newline() {
        let cfg = MemConfig::with_config("model: gpt-9"); // no trailing newline
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(
            cfg.config_text().as_deref(),
            Some(
                "model: gpt-9\nhooks:\n  pre_llm_call:\n  - command: topos pull --quiet  # topos:currency\n"
            ),
            "a separator newline lands before the appended top-level block"
        );
    }

    #[test]
    fn install_ignores_a_nested_hooks_mapping_and_appends_a_top_level_one() {
        // A `hooks: {}` nested inside another mapping is NOT the top-level key (zero-indent
        // anchoring) — it is preserved verbatim and a real top-level block is appended.
        let before = "profiles:\n  default:\n    hooks: {}\n";
        let cfg = MemConfig::with_config(before);
        adapter(&cfg).install_currency_trigger();
        let after = cfg.config_text().unwrap();
        assert!(
            after.starts_with(before),
            "the nested user mapping is untouched"
        );
        assert!(after.ends_with(FRESH_INSTALL));
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        for start in [None, Some(DEFAULT_CONFIG)] {
            let cfg = match start {
                None => MemConfig::default(),
                Some(s) => MemConfig::with_config(s),
            };
            adapter(&cfg).install_currency_trigger();
            let after_first = cfg.config_text();
            let report = adapter(&cfg).install_currency_trigger();
            assert_eq!(report.state, TriggerState::Inactive);
            assert!(
                report.touched_path.is_none(),
                "idempotent re-run touches nothing"
            );
            assert_eq!(cfg.writes(), 1, "second install writes nothing");
            assert_eq!(cfg.config_text(), after_first, "bytes unchanged on re-run");
        }
    }

    #[test]
    fn install_leaves_a_hand_rolled_topos_pull_unmanaged() {
        // A hand-rolled variant…
        let cfg = MemConfig::with_config("hooks:\n  pre_llm_call:\n  - command: topos pull\n");
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(
            cfg.writes(),
            0,
            "never blind-append next to a user's own hook"
        );

        // …and the exact bare command WITHOUT our sentinel comment (a user's own line, or ours
        // after a comment-stripping Hermes config rewrite) is honestly not claimed either.
        let cfg =
            MemConfig::with_config("hooks:\n  pre_llm_call:\n  - command: topos pull --quiet\n");
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);

        // A topos-pull entry moved under a different event is also adopt-or-leave.
        let cfg = MemConfig::with_config(
            "hooks:\n  post_llm_call:\n  - command: topos pull --quiet  # topos:currency\n",
        );
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn install_fails_closed_on_unprovable_shapes() {
        for bad in [
            "hooks:\n  post_llm_call:\n  - command: echo hi\n", // populated, not ours
            "hooks: {pre_llm_call: []}\n",                      // flow form
            "hooks: {}  # none\n",                              // trailing comment on the key
            "hooks: {}\nmodel: a\nhooks: {}\n",                 // duplicate top-level keys
            "hooks: {}\n  stray: 1\n",                          // empty-flow with indented content
        ] {
            let cfg = MemConfig::with_config(bad);
            let report = adapter(&cfg).install_currency_trigger();
            assert_eq!(report.state, TriggerState::Degraded, "input: {bad:?}");
            assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
            assert_eq!(cfg.writes(), 0);
            assert_eq!(cfg.config_text().as_deref(), Some(bad), "untouched");
        }
        // Non-UTF-8 bytes degrade too.
        let cfg = MemConfig::default();
        cfg.files
            .borrow_mut()
            .insert(PathBuf::from("/h/config.yaml"), vec![0xff, 0xfe, b'x']);
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn an_entry_line_inside_foreign_content_is_never_claimed() {
        // Our exact line inside a block scalar (user notes) is content, not a hook: install sees
        // no top-level hooks key and appends a real one; remove scrubs only the real one.
        let before = "notes: |\n  - command: topos pull --quiet  # topos:currency\n";
        let cfg = MemConfig::with_config(before);
        adapter(&cfg).install_currency_trigger();
        let installed = cfg.config_text().unwrap();
        assert!(
            installed.starts_with(before),
            "the user's scalar is untouched"
        );

        let report = adapter(&cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        let after = cfg.config_text().unwrap();
        assert!(
            after.contains("notes: |\n  - command: topos pull --quiet  # topos:currency"),
            "remove never deletes a look-alike line outside the hooks block"
        );
        assert!(
            !after.contains("pre_llm_call"),
            "the real entry was scrubbed"
        );
    }

    #[test]
    fn approval_evidence_flips_active_honestly() {
        // env/ctor evidence → Active + FirstTurn.
        let cfg = MemConfig::default();
        let report = accepting_adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::FirstTurn);

        // Persisted allowlist evidence (Hermes's own record, exact string key) → Active.
        let cfg = MemConfig::default();
        cfg.set_allowlist(
            "{\"approvals\":[{\"event\":\"pre_llm_call\",\"command\":\"topos pull --quiet\"}]}",
        );
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::FirstTurn);

        // A different command in the allowlist is NOT our approval.
        let cfg = MemConfig::default();
        cfg.set_allowlist(
            "{\"approvals\":[{\"event\":\"pre_llm_call\",\"command\":\"topos pull\"}]}",
        );
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);

        // Malformed / wrong-typed allowlists are never evidence (fail closed).
        for bad in ["not json", "{\"approvals\": \"oops\"}", "[]"] {
            let cfg = MemConfig::default();
            cfg.set_allowlist(bad);
            let report = adapter(&cfg).install_currency_trigger();
            assert_eq!(report.state, TriggerState::Inactive, "allowlist: {bad:?}");
        }
    }

    #[test]
    fn config_auto_accept_counts_only_at_top_level() {
        // The shipped default (`hooks_auto_accept: false`) is not evidence.
        let cfg = MemConfig::with_config(DEFAULT_CONFIG);
        assert_eq!(
            adapter(&cfg).install_currency_trigger().state,
            TriggerState::Inactive
        );

        // Top-level true IS Hermes's own durable auto-accept.
        let cfg = MemConfig::with_config("hooks: {}\nhooks_auto_accept: true\n");
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::FirstTurn);

        // A nested occurrence is some other mapping's key — not evidence.
        let cfg =
            MemConfig::with_config("hooks: {}\nprofiles:\n  dev:\n    hooks_auto_accept: true\n");
        assert_eq!(
            adapter(&cfg).install_currency_trigger().state,
            TriggerState::Inactive
        );

        // Conflicting duplicates fail closed.
        let cfg = MemConfig::with_config(
            "hooks: {}\nhooks_auto_accept: true\nhooks_auto_accept: false\n",
        );
        assert_eq!(
            adapter(&cfg).install_currency_trigger().state,
            TriggerState::Inactive
        );
    }

    #[test]
    fn the_approval_key_is_the_stable_command_string() {
        // The allowlist keys on the exact command string; the managed entry's YAML comment is
        // outside the scalar, so what Hermes approves is exactly HOOK_COMMAND — stable across
        // topos updates (no version, no path, no wrapper content in the key).
        assert_eq!(
            ENTRY_LINE,
            format!("- command: {HOOK_COMMAND}  # topos:currency")
        );
        assert!(!HOOK_COMMAND.contains('#'));
        // Re-install with the approval present stays Active and writes nothing (no re-prompt
        // surface: the entry and its approval key are byte-identical run over run).
        let cfg = MemConfig::default();
        cfg.set_allowlist(
            "{\"approvals\":[{\"event\":\"pre_llm_call\",\"command\":\"topos pull --quiet\"}]}",
        );
        adapter(&cfg).install_currency_trigger();
        let report = adapter(&cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "the approved re-install is a true no-op");
    }

    #[test]
    fn remove_scrubs_only_ours_and_restores_the_default_shape() {
        let cfg = MemConfig::with_config(DEFAULT_CONFIG);
        adapter(&cfg).install_currency_trigger();
        let report = adapter(&cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(
            cfg.config_text().as_deref(),
            Some(DEFAULT_CONFIG),
            "a clean uninstall restores the shipped `hooks: {{}}` form byte-exactly"
        );

        // Idempotent: a second remove is a clean no-op.
        let writes_before = cfg.writes();
        let report = adapter(&cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes_before, "second remove writes nothing");
    }

    #[test]
    fn remove_keeps_user_items_and_comments_in_the_block() {
        let cfg = MemConfig::with_config(
            "hooks:\n  pre_llm_call:\n  - command: topos pull --quiet  # topos:currency\n  # keep me\n  - command: echo keep\n",
        );
        let report = adapter(&cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(
            cfg.config_text().as_deref(),
            Some("hooks:\n  pre_llm_call:\n  # keep me\n  - command: echo keep\n"),
            "only our entry line is removed; the user's comment and item survive, no pruning"
        );
    }

    #[test]
    fn remove_keeps_sibling_event_blocks() {
        let cfg = MemConfig::with_config(
            "hooks:\n  pre_llm_call:\n  - command: topos pull --quiet  # topos:currency\n  post_llm_call:\n  - command: echo bye\n",
        );
        let report = adapter(&cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(
            cfg.config_text().as_deref(),
            Some("hooks:\n  post_llm_call:\n  - command: echo bye\n"),
            "our entry and its emptied key are pruned; the sibling event survives"
        );
    }

    #[test]
    fn remove_leaves_hand_rolled_and_absent_alone() {
        // A user's own `topos pull` (no sentinel) is never blind-removed.
        let cfg =
            MemConfig::with_config("hooks:\n  pre_llm_call:\n  - command: topos pull --quiet\n");
        let report = adapter(&cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);

        // An absent config → a clean no-op, never created.
        let absent = MemConfig::default();
        let report = adapter(&absent).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            absent.config_text().is_none(),
            "remove never creates the file"
        );
    }

    #[test]
    fn remove_degrades_on_unprovable_without_clobbering() {
        let bad = "hooks: {}\nmodel: a\nhooks: {}\n"; // duplicate top-level keys
        let cfg = MemConfig::with_config(bad);
        let report = adapter(&cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cfg.config_text().as_deref(), Some(bad), "never clobbered");
    }

    #[test]
    fn discover_finds_both_root_level_and_categorized_skills() {
        let home = TempHome::new();
        home.skill(&["computer-use"]); // root-level skill (uncategorized)
        home.skill(&["devops", "deploy"]); // categorized
        home.skill(&["devops", "rollback"]);
        // A category dir named like a support dir still yields its children (the prune applies
        // only under a dir that itself has a SKILL.md).
        home.skill(&["scripts", "runner"]);
        // A root-level skill's own support dir must NOT surface, but its nested skill must.
        home.skill(&["computer-use", "sub-skill"]);
        std::fs::create_dir_all(home.0.join("skills/computer-use/references")).unwrap();
        std::fs::write(
            home.0.join("skills/computer-use/references/SKILL.md"),
            b"archived",
        )
        .unwrap();
        // Noise: dot-dirs at both levels, stray files, an empty category.
        std::fs::create_dir_all(home.0.join("skills/.topos-staging-x")).unwrap();
        std::fs::write(home.0.join("skills/.topos-staging-x/SKILL.md"), b"x").unwrap();
        std::fs::create_dir_all(home.0.join("skills/devops/.hidden")).unwrap();
        std::fs::write(home.0.join("skills/devops/.hidden/SKILL.md"), b"x").unwrap();
        std::fs::write(home.0.join("skills/loose.txt"), b"x").unwrap();
        std::fs::create_dir_all(home.0.join("skills/empty-category")).unwrap();

        let cfg = MemConfig::default();
        let found = Hermes::new(home.0.clone(), false, &cfg).discover();
        let summary: Vec<(String, Option<String>)> = found
            .iter()
            .map(|d| {
                (
                    d.path
                        .strip_prefix(home.0.join("skills"))
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    d.layer.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("computer-use".to_owned(), None),
                (
                    "computer-use/sub-skill".to_owned(),
                    Some("computer-use".to_owned())
                ),
                ("devops/deploy".to_owned(), Some("devops".to_owned())),
                ("devops/rollback".to_owned(), Some("devops".to_owned())),
                ("scripts/runner".to_owned(), Some("scripts".to_owned())),
            ],
            "sorted; root-level skills carry no layer; the category is the layer; noise skipped"
        );
    }

    #[test]
    fn discover_on_absent_home_is_empty_not_an_error() {
        let cfg = MemConfig::default();
        let found = Hermes::new(PathBuf::from("/no-such-hermes-home-xyz"), false, &cfg).discover();
        assert!(found.is_empty());
    }

    #[test]
    fn placement_reuses_a_discovered_dir_and_defaults_to_the_general_category() {
        let cfg = MemConfig::default();
        let a = adapter(&cfg);
        let disc = DiscoveredPlacement {
            path: PathBuf::from("/h/skills/devops/deploy"),
            layer: Some("devops".to_owned()),
        };
        assert_eq!(
            a.placement_for("topos_abc", Some(&disc)).dir,
            PathBuf::from("/h/skills/devops/deploy"),
            "a discovered categorized dir is reused verbatim"
        );
        assert_eq!(
            a.placement_for("topos_abc", None).dir,
            PathBuf::from("/h/skills/general/topos_abc"),
            "the no-discovery default is the categorized general shelf"
        );
    }

    #[test]
    fn footprint_is_disclosed_only_when_our_entry_is_present() {
        let cfg = MemConfig::default();
        assert!(
            adapter(&cfg).uninstall_footprint().is_empty(),
            "no entry → nothing disclosed"
        );
        adapter(&cfg).install_currency_trigger();
        assert_eq!(
            adapter(&cfg).uninstall_footprint(),
            vec![PathBuf::from("/h/config.yaml")],
            "our entry present → config.yaml disclosed (never deleted)"
        );
        adapter(&cfg).remove_currency_trigger();
        assert!(
            adapter(&cfg).uninstall_footprint().is_empty(),
            "after the scrub → nothing disclosed again"
        );
    }

    #[test]
    fn currency_kind_is_first_turn_and_the_id_is_hermes() {
        let cfg = MemConfig::default();
        let a = adapter(&cfg);
        assert_eq!(a.currency_kind(), CurrencyKind::FirstTurn);
        assert_eq!(a.id(), HarnessId::Hermes);
    }
}
