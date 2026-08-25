//! Per-verb `--json` `data` payloads — the agent's primary signal.
//!
//! The envelope keeps a generic `data: Value` (one stable wrapper); a consumer reads `command` and
//! deserializes `data` into the matching type here. Each type gets a committed JSON-Schema.
//!
//! **Pinned vs inferred.** Only `pull`, `list`, and `diff` have their `data` fields named by the
//! spec — those are authoritative. The other nine (`add` `follow` `unfollow` `log` `publish`
//! `revert` `review` `invite` + `publish --propose`) are marked **INFERRED**: derived from the
//! documented mechanics, additive-only, and liable to tighten as each verb is built. The envelope,
//! receipt, error, outcome, and action-code shapes are all fully pinned (see the crate root).

use serde::{Deserialize, Serialize};

// =================================================================================================
// The one shape every verb names a workspace with.
// =================================================================================================

/// THE WORKSPACE A VERB'S ACT REACHED — one field, one spelling, in every payload below.
///
/// `<host>/<name>` is the address a person types and the one `topos workspace list` prints, so a
/// consumer can hand what it read here straight back to `--workspace`. The two halves ride apart
/// because a slash inside a name would make parsing one string ambiguous, and the opaque
/// `workspace_id` stays exactly what it was — the wire key — and is never spelled here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WorkspaceRef {
    /// The server host the workspace lives on (`topos.sh`, `topos.example.com:3000`).
    pub host: String,
    /// The workspace's ADDRESS name — the slug, never the display name and never the opaque id.
    pub name: String,
}

impl WorkspaceRef {
    /// The `<host>/<name>` address, joined — what a person reads and types. Written once here so
    /// no surface invents a second spelling of the same two fields.
    #[must_use]
    pub fn address(&self) -> String {
        format!("{}/{}", self.host, self.name)
    }
}

// =================================================================================================
// PINNED — `pull` (the four-state sync machine, per skill).
// =================================================================================================

/// `pull` result — per-skill update status plus the reviewer-queue count. **PINNED** (the original
/// fields); `notices` + `sync` are ADDITIVE (the delivery-driven sweep's feed + freshness — absent
/// on a targeted pull and from an older producer).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PullData {
    pub skills: Vec<PullSkill>,
    /// Open proposals on your followed skills (v0 is single-approver — any rostered member may review, so
    /// the count is all open-non-stale proposals across what you follow, not a reviewer-assignment queue).
    pub proposals_awaiting: u32,
    /// The unacked, person-scoped notices the delivery answered (verdicts first) — narrated by an
    /// interactive `update`, which then acks exactly these ids; the quiet hook fetches without
    /// acking. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<crate::requests::WireNotice>,
    /// Per-workspace delivery/report freshness after the sweep — the staleness clock the hook
    /// warning and `auth status` read. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync: Vec<WorkspaceSyncReport>,
    /// The scope(s) this update reconciled: `"project <dir>"`, `"machine"`, or `"both"` (the
    /// background sweep). Absent from producers predating the scoped update, and from the
    /// single-skill go-back, which acts on local bytes rather than a scope. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Bundles this installation holds at an OLDER version in a scope this run did not reconcile
    /// — the cross-scope staleness the ONE applied row per bundle cannot carry. Empty (and
    /// omitted) whenever every other scope is current, and whenever the difference is DELIBERATE
    /// (a pinned or `"off"` row, a local go-back): a copy that is meant to differ is not behind.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub behind_elsewhere: Vec<BehindElsewhere>,
    /// The auto-update triggers this run registered. Always empty since 0.1.52; kept for schema
    /// stability (the sweep registers no agent — hooks follow the agents pick). **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::TriggerReport>,
}

/// One bundle whose copy in ANOTHER scope stands behind the workspace's current. **INFERRED**
/// (additive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct BehindElsewhere {
    /// The bundle's catalog name.
    pub bundle: String,
    /// The project directory whose copy is behind (display path, `~`-abbreviated). Absent ⇒ the
    /// machine-wide copy, which `topos update -g` brings current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
}

/// One workspace's sync freshness in a [`PullData`]. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct WorkspaceSyncReport {
    pub workspace_id: String,
    /// The workspace this row is about, as a person addresses it. Absent only when this
    /// installation holds no session naming it. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// When the last successful delivery answered (epoch millis; absent if never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_at: Option<i64>,
    /// When the last successful applied-state report landed (epoch millis; absent if never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report_at: Option<i64>,
    /// The workspace's staleness window (ms).
    pub staleness_window_ms: u64,
}

/// One followed skill's pull state. `observed`/`applied`/`action` are PINNED by name; the *value
/// enum* (`PullAction`) is INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PullSkill {
    pub skill: String,
    /// The workspace this followed skill lives in, or `None` for a targeted go-back / local-only pull that
    /// has no follow entry. A pulled skill is normally followed (so `Some`), but `pull <skill>@<hash>` on an
    /// unfollowed copy has none — `Option` keeps that honest and stays symmetric with [`SkillEntry`]. Names
    /// the workspace so a session-start sweep does not show two same-named skills indistinguishably.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The generation the plane most recently served — the sync target.
    pub observed: u64,
    /// Highest generation actually materialized to disk.
    pub applied: u64,
    pub action: PullAction,
    /// Present for the author-merge outcomes (`merged` / `conflicted`) — the resolution disclosure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeReport>,
    /// How many OTHER agent folders a settled local draft was copied onto this run (a
    /// `draft_synced` row's count). Absent when the run synced nothing. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_placements: Option<u32>,
    /// The SCOPE this row reconciled in — `"person"` for the home dirs, or the project
    /// directory's path for an in-checkout delivery. The receipt sections rows by it. Absent on
    /// rows predating the field. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The DESTINATIONS this row's action touched: the skill folders an `installed` row landed in
    /// (or a `removed` row uninstalled), or the config files a config-placed (`mcp`) bundle's
    /// entries live in. Display paths — abbreviated to `~` under the user's home. Empty (and
    /// omitted) for actions that moved no destination. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<String>,
    /// Locally-EDITED copies a `removed` row kept in place instead of uninstalling (the person's
    /// own work is never deleted by ending delivery) — their display paths, `~`-abbreviated.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    /// The workspace-QUALIFIED display name a receipt leads with (`@<ws>/<name>` when the host is
    /// this machine's one connected host, else `<host>/<ws>/<name>`). Absent for non-workspace
    /// sources, which read by their plain `skill` name. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// The fact a row states BESIDE its action: the whole thing a `released` row prints (why the
    /// record resolved and where its files stand now — two lines, the second the folders, which a
    /// human renderer indents under the first), or — on any other action — the one line naming
    /// the folders this run also wrote that the action's own column does not name (a healed folder
    /// that rode along with a settled-draft fan-out, a stale copy refreshed beside a fresh
    /// install). Absent when the action says everything. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Per-agent applied states for a config-placed (`mcp`) bundle: which detected agents hold
    /// the server entry and how. Empty (and omitted) for file-bundle skills. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<McpAgentState>,
    /// The catalog bundle kind (`"mcp"` for a config-placed MCP-server bundle). Absent ⇒ an
    /// ordinary skill. Carried so a receipt can name what it reconciled instead of calling every
    /// row a skill — a state a row with no engaged agent could not otherwise report. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whether this bundle carries LOCAL EDITS that are not shared — the same fact `list` prints
    /// as `(draft)` and `status` counts as `drafts ahead`.
    ///
    /// Delivery had nothing to do for such a row, so its action is `up_to_date` — and a summary
    /// built from actions alone then announced "all up to date" about the very bundles `list` and
    /// `status` were calling drafts. The three surfaces disagreed about one machine. Carried here
    /// so the receipt can say both true things at once: nothing was owed, and something of yours
    /// is still unshared. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
    /// The NARROWING a `removed` row is the loss half of (see [`Narrowing`]) — present only where
    /// the run proved the bundle still delivers somewhere in this scope, which is what tells a
    /// deliberate narrowing apart from delivery ending. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrowed: Option<Narrowing>,
}

/// A row edit that stopped demanding some of a bundle's surfaces while the bundle itself keeps
/// delivering here — the loss an `update` receipt leads with, so a deliberate narrowing is never
/// quieter than the install that preceded it. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct Narrowing {
    /// Where the bundle STILL delivers, as the receipt spells it. Never empty: a row reaching
    /// nothing is not a narrowing, and the rows already say that in their own words.
    pub still: Vec<String>,
    /// The surfaces this run retired — one line each under the lead.
    pub from: Vec<Surface>,
}

/// **THE ONE OUTCOME VOCABULARY** — what a single managed TARGET looks like after a run.
///
/// A bundle's bytes reach a machine as one of two target shapes: a placement DIRECTORY the bundle
/// owns, or an ENTRY it owns inside a config file. They are the same question asked twice ("what
/// does the thing topos owns look like now?"), and they used to answer in two unrelated word sets
/// — dirs through the row's action, entries through a free-form per-agent string that a reader
/// had to know by heart. Both now project onto THIS set, and both render from
/// [`TargetOutcome::word`], so one outcome cannot be called two things.
///
/// Every variant is DERIVED, never chosen at a render site: from the drift vocabulary the
/// ownership record is scanned into (absent / clean / modified / foreign / unscannable) plus the
/// one bit the record cannot hold — whether this run WROTE the target. **INFERRED**, and the value
/// set is CLOSED: every consumer matches it exhaustively and there is no unknown arm, so a new
/// outcome is a deliberate change that ships with a release — never a value a running client is
/// expected to tolerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "kebab-case")]
pub enum TargetOutcome {
    /// Nothing was there and this run wrote it — the target's FIRST materialization here.
    Created,
    /// The target stood at what the record says topos wrote, and this run rewrote it: a version
    /// caught up, or a hand-deleted target put back. Never `created` — the distinction is the
    /// whole reason a person can tell a new install from a repair.
    Refreshed,
    /// Found byte-for-byte as recorded; nothing was written.
    Current,
    /// Changed since topos wrote it — a local edit, left exactly as it is.
    Drifted,
    /// Content topos holds no record of writing, standing where this bundle's target belongs.
    /// Never overwritten.
    Conflicting,
    /// The target could not be read safely, so nothing was decided about it — fail closed.
    Unprovable,
    /// The target left this run.
    Removed,
    /// The surface exists but this bundle was deliberately not placed there (no config of that
    /// kind at this scope, a `dest` that excludes it).
    #[serde(rename = "not-supported")]
    Withheld,
}

impl TargetOutcome {
    /// The ONE word a person reads for this outcome, on EITHER target shape. Every receipt line
    /// about a dir and every line about a config entry comes through here, so `add` and `update`
    /// — and a folder and a config file — can never name one outcome two different ways.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Refreshed => "refreshed",
            Self::Current => "current",
            Self::Drifted => "hand-edited",
            Self::Conflicting => "held by another entry",
            Self::Unprovable => "could not be read",
            Self::Removed => "removed",
            Self::Withheld => "not placed",
        }
    }

    /// Whether this run WROTE the target. The two writing outcomes are exactly the two that make
    /// a sweep a change rather than a look, so the rule lives here rather than in each caller's
    /// idea of which words count.
    #[must_use]
    pub const fn wrote(self) -> bool {
        matches!(self, Self::Created | Self::Refreshed)
    }

    /// Whether the target now HOLDS this bundle — the only question "did it arrive" is asking. A
    /// hand-edited target counts: something of this bundle is there, and it is the person's. The
    /// four that do not are the four where nothing of it stands (never placed, held by somebody
    /// else's entry, unreadable, gone), and a receipt counting those alongside the rest claimed
    /// `installed (16 config files)` over thirteen that hold an entry and three that do not.
    #[must_use]
    pub const fn stands(self) -> bool {
        matches!(
            self,
            Self::Created | Self::Refreshed | Self::Current | Self::Drifted
        )
    }
}

/// One agent's (harness's) state for a config-placed (`mcp`) bundle on this installation — its
/// config ENTRY, projected onto [the one outcome vocabulary](TargetOutcome). **INFERRED**
/// (additive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct McpAgentState {
    /// The harness registry slug (e.g. `claude-code`, `cursor`).
    pub agent: String,
    /// What this bundle's entry in that agent's config looks like after the run.
    pub state: TargetOutcome,
    /// A short human-readable qualifier (why not-supported / how a change goes live), when one
    /// exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The config file the entry lives in (the `list <name>` deep dive shows it; receipts omit
    /// it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// ONE SURFACE a run answered for, in the vocabulary both target shapes share: the agent that
/// reads it, where this bundle's copy lives there, and what the run left. `target` is a config
/// file for a config-placed (`mcp`) bundle and a folder for a skill. **INFERRED** (additive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct Surface {
    /// The harness registry slug that reads it (e.g. `opencode`). EMPTY where the destination
    /// names no single harness — a folder several agents share.
    pub agent: String,
    /// The config file or folder the copy lives in, as a receipt spells it. Absent where nothing
    /// stands and there is no target to name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub state: TargetOutcome,
    /// The short qualifier the outcome carries (why a surface was not placed), when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The predicted verdict of a three-way merge that has NOT been run against the placements — a pure
/// in-memory dry run of the same kernel plan + per-file diff3 the real resolution executes. Never a
/// promise: the authoritative outcome is the resolution's own [`MergeReport`]. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct MergePreview {
    pub verdict: MergePreviewVerdict,
    /// The paths that would conflict when `verdict` is `conflicted` (structural conflicts + failed
    /// content merges). Empty for a clean preview — and for the rare conflicted-with-no-path case
    /// (a merge that would empty the bundle).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

/// The two predicted merge verdicts. **INFERRED** (additive value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MergePreviewVerdict {
    /// Every path would resolve cleanly — the merge would land a publishable draft-on-current.
    Clean,
    /// At least one path would conflict — the merge would write markers and block publish.
    Conflicted,
}

/// What `pull` did for a skill. **INFERRED value set** — the four-state machine pins the
/// semantics (CURRENT / BEHIND / DRAFT / DIVERGED) but not these exact tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum PullAction {
    /// State ① — already current; nothing to do.
    UpToDate,
    /// State ② clean — auto fast-forwarded to the new bytes.
    FastForwarded,
    /// The bundle's FIRST materialization in this scope: the never-received baseline received its
    /// first bytes and they landed (skill folders, or — for a config-placed bundle — config
    /// entries). `destinations` names where. **Additive.**
    Installed,
    /// A managed copy that stood BEHIND the version this machine already holds was rewritten to it
    /// — no version moved (`observed`/`applied` are unchanged), only bytes on disk caught up (a
    /// crash-window residue, a copy left at an older version, a config-placed bundle's entry
    /// deleted or edited by hand and put back). `destinations` names the folders (config files,
    /// for a config-placed bundle). A catch-up is not a first materialization, so it never reads
    /// `installed`. **Additive.**
    ///
    /// Serialized as `updated` — the word every human surface prints for it. A wire token and a
    /// terminal line naming one outcome two ways is one vocabulary too many.
    #[serde(rename = "updated")]
    Refreshed,
    /// This machine's OWN recipe choice ended delivery here (a dropped feed line, an `"off"`
    /// switch, a dropped row) and the placed copies were uninstalled now — `destinations` names
    /// what left, `kept` any edited copy left in place. Distinct from `withdrawn`, which is
    /// upstream's act. **Additive.**
    Removed,
    /// State ④ resolved cleanly — a three-way merge (or the escape) landed a draft-on-current.
    Merged,
    /// State ④ resolved with conflicts — a complete conflict tree was materialized and publish is blocked
    /// until the author resolves (or escapes).
    Conflicted,
    /// State ③ — a SETTLED local draft (unchanged across two runs) was copied onto this bundle's
    /// other agent folders in its scope; their recorded baselines advance to the draft, and the
    /// draft copy itself is untouched.
    DraftSynced,
    /// A transient local hold (e.g. a local go-back is pinned).
    Held,
    /// UPSTREAM withdrew the skill (archived, or its last delivering channel dropped it): the agent
    /// dirs were cleaned; the sidecar keeps the bytes + any draft delta ("keep it as yours" is a
    /// narration away).
    Withdrawn,
    /// A store record no row claims and nothing delivers RESOLVED, once: the record retires from
    /// every surface; nothing on disk was deleted — placed files belong to the person now, and
    /// `note` carries the one-line fact. **Additive.**
    Released,
}

/// The author-merge disclosure (the `merged` / `conflicted` outcomes of a diverged draft). **INFERRED
/// fields** — the spec pins the merge semantics (deterministic, author-only, conflict-blocks-publish),
/// not this exact shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct MergeReport {
    /// The three-way base (the draft's fork point).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    /// `current` (theirs) the draft was merged onto.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub theirs_version_id: String,
    /// The forward 1-parent commit carrying the merged (or conflict-marked) tree.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub result_version_id: String,
    /// The merged/conflict tree's `bundle_digest`.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub result_digest: String,
    /// Whether the merge was clean (`true` → draft-on-current, publishable) or blocked (`false`).
    pub clean: bool,
    /// The conflicting paths when `clean` is `false` — the agent's resolution checklist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictPathReport>,
    /// For the escape / no-base 2-way fallback: a unified diff of what the chosen side drops vs the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_diff: Option<String>,
    /// Why the merge could not resolve itself — `three_way` (a real fork point; the workbench holds
    /// both sides marked up) or `no_base` (unrelated histories; the workbench holds this person's
    /// files with the team's beside them). Absent on a clean merge and on a resolution. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<crate::persisted::ConflictReason>,
    /// How a STOPPED merge was finished, when this row is a `--keep-mine` resolution. Absent on a
    /// clean merge and on a conflict. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<MergeResolution>,
    /// The paths a `keep_mine` resolution TOOK from the team — every file whose committed content
    /// or mode differs from this person's own draft, including ones the team deleted. Sorted by raw
    /// path bytes. Empty when the resolution changed nothing of theirs, and always empty for a
    /// `by_hand` resolution (that tree is the person's own, wholesale — nothing in it is claimable
    /// as taken from the team). **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub took: Vec<String>,
    /// Every folder this bundle sits in while the merge stands, each with WHAT IS IN IT — read
    /// from the folder itself, never assumed. A conflict writes to no folder, so the whole set
    /// reads `yours` the moment it stops; a copy taken by a narrowed reset, or edited on after the
    /// stop, says so instead. Folders that are gone (or unreadable) are absent: an unverifiable
    /// folder must never become a claim. Empty for a clean merge (which rewrites its placements).
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placements: Vec<ConflictPlacement>,
    /// Where the marked-up copy of BOTH versions was written: the scope's own conflict workbench,
    /// never a folder an agent reads. A hand resolution left there is what `update <skill>
    /// --keep-mine` commits. Display path (`~`-abbreviated). Absent for a clean merge.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_dir: Option<String>,
}

/// How `--keep-mine` finished a stopped merge — the two paths say different things about whose
/// wording landed, so a receipt must never speak for both at once. **INFERRED** (additive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub enum MergeResolution {
    /// The workbench was left alone: the three-way merge, settled on this person's side wherever
    /// the two sides collided, with everything else the team changed taken (`git merge -X ours`).
    KeepMine,
    /// The workbench was EDITED: those files are the person's own reconciliation, committed
    /// unexamined (`git commit` after resolving by hand). Nothing here is topos's choice.
    ByHand,
}

/// One folder a stopped merge's bundle sits in, and what that folder holds RIGHT NOW. The pair is
/// what lets a re-disclosure tell per-folder truth: a merge can stand for days while a narrowed
/// reset takes one copy and the person keeps working in another. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ConflictPlacement {
    /// The folder, as a receipt spells it (`~`-abbreviated display path).
    pub dir: String,
    /// What is in it.
    pub holds: ConflictHolds,
}

/// What a folder holds while a merge stands (see [`ConflictPlacement`]). **INFERRED** (additive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub enum ConflictHolds {
    /// This person's own version — the draft the merge stopped on. Every folder reads this the
    /// moment a merge stops, because a conflict writes to none of them.
    Yours,
    /// The team's version: a `--dest`-narrowed reset took this one copy while the merge stayed
    /// stopped over the others.
    Theirs,
    /// Both versions, marked up — topos's OWN comparison tree, which a receipt must never read as
    /// work of the person's. A merge writes it to the workbench, so a placement holds it only
    /// where one was copied over it (or an install upgraded mid-merge); either way the bytes are
    /// not a version anybody chose, and both ways out replace them.
    MarkedUp,
    /// None of the three — the person kept working in this folder after the merge stopped.
    NewerEdits,
}

/// One conflicting path in a [`MergeReport`]. **INFERRED** — `kind` reuses the persisted vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ConflictPathReport {
    pub path: String,
    pub kind: crate::persisted::ConflictPathKind,
}

// =================================================================================================
// `list` (the per-scope inventory + the deep dive + the agent-eye and remote views).
//
// The payload was REDESIGNED WHOLESALE pre-1.0 when `list` became the scoped inventory (and
// `status` the health panel): the old follow-shaped buckets are gone, not deprecated — a reader
// written against them does not degrade, it breaks. THIS shape is the stable one from here: the
// per-scope sections plus the summaries and the optional views, additive-only from now on.
// =================================================================================================

/// `list` result — the inventory, per scope. The scopes shown in full ride `scopes`; whatever the
/// invocation does not show rides ONE summary (`machine_summary` / `untracked_summary`) so nothing
/// is invisible. The optional views (`untracked` / `remote` / `detail` / `agent_view`) fill only
/// under their flag. **The stable shape** (see the section note: it replaced the pre-1.0 bucket
/// payload outright); additive-only from here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ListData {
    /// The scope sections shown in full, render order (here-scope first).
    pub scopes: Vec<ListScope>,
    /// The machine scope's one-line summary, present only when the machine scope is NOT shown in
    /// full (the default view inside a project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_summary: Option<ListScopeSummary>,
    /// The untracked discoveries' one-line summary (absent when nothing untracked was found, or
    /// when the untracked listing itself is shown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untracked_summary: Option<UntrackedSummary>,
    /// Signed-in state — the TTY renders the static remote pointer from it.
    pub signed_in: bool,
    /// Full untracked discoveries (only under `--untracked`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked: Vec<UntrackedEntry>,
    /// Per-workspace catalog view (only under `--remote`). ONE workspace — the one the
    /// invocation acts on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote: Vec<RemoteWorkspace>,
    /// The OTHER workspaces this machine is signed into, present only under `--remote`. The
    /// catalog view acts on one workspace; this names the rest so none of them is invisible, and
    /// the TTY closes the view with the spelling that shows theirs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_signed_in: Vec<WorkspaceRef>,
    /// The one-skill deep dive (`list <name>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ListDetail>,
    /// The agent-eye view (`list -a <slug>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_view: Option<AgentView>,
    /// Only present under `--footprint`: topos-owned paths outside skill dirs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint: Option<Vec<String>>,
    /// The buckets that were row-capped (`--limit`/`--offset`, or the `--json` default page), one
    /// marker per capped bucket. The page applies PER BUCKET; the `NEXT_PAGE` next action's argv
    /// fetches the next page. Empty (and omitted) on an uncapped list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated: Vec<BucketTruncation>,
    /// The last auto-update check of every external source the shown scopes name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forge: Vec<ForgeSource>,
    /// The agents picked where the panel stands (the project's effective pick, or the machine's
    /// under `-g`), as the pick file spells them. Empty = no pick yet. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Where `agents` comes from: `"project"`, `"machine"`, or `"legacy"` (the hooks an earlier
    /// build registered, not yet recorded as a pick). Absent with no pick. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_source: Option<String>,
    /// The pick file `agents` was read from, as a person reads it. Absent with no pick or a
    /// legacy record. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_path: Option<String>,
}

/// One scope section of a [`ListData`] — the rows one manifest delivers. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ListScope {
    /// `"project"` (the nearest `topos.toml` covering the working directory) or `"machine"`.
    pub scope: String,
    /// The governing manifest file, spelled as a person standing here reads it: a project manifest
    /// relative to the working directory (`./topos.toml`, `../topos.toml`), the machine file
    /// `~`-abbreviated. `None` = no machine-wide file — nothing is demanded machine-wide (`topos
    /// login` writes a feed row on this machine's first connection to a workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The scope's inventory rows.
    pub rows: Vec<SkillEntry>,
    /// Records this scope's store still holds — with skill folders or config entries still
    /// standing — that NO row in this manifest, and no feed, demands any more.
    ///
    /// An inventory built from manifest rows alone could not show these, and the sweep's orphan
    /// resolution deliberately passes over a record whose config entries still stand (they are
    /// placed, not abandoned). Between the two, an MCP server could sit live in an agent's config
    /// with no surface naming it and no command offered for it. One line each, never a section:
    /// the thing standing, and the way out. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<OrphanRecord>,
}

/// One record nothing demands whose bytes or entries still stand (see [`ListScope::orphans`]).
/// **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct OrphanRecord {
    /// The bundle's name — what a person calls it, never the store id.
    pub name: String,
    /// Where it still stands: the config files its entries sit in, or the folders holding its
    /// copies. Display paths, `~`-abbreviated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standing: Vec<String>,
    /// The catalog bundle kind (`"mcp"` for a config-placed bundle). Absent ⇒ an ordinary skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// The one-line summary of a scope the invocation does not show in full. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ListScopeSummary {
    /// Skills the scope delivers.
    pub skills: u64,
    /// How many of them sit behind their last-known served target.
    pub updates_pending: u64,
    /// The exact command that expands the summary into the full section.
    pub command: String,
}

/// The one-line summary of the untracked discoveries a bare `list` does not itemize. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct UntrackedSummary {
    /// Untracked skills discovered in the known agents' skill folders.
    pub skills: u64,
    /// The distinct folders holding them.
    pub folders: u64,
    /// The exact command that shows the full listing.
    pub command: String,
}

/// One workspace's catalog view under `list --remote` — its channels and skills, each annotated
/// with this machine's adoption state. Metadata only — the catalog grants no bytes. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoteWorkspace {
    /// The workspace this section is about, as a person addresses it.
    pub workspace: WorkspaceRef,
    /// The workspace's opaque id.
    pub workspace_id: String,
    /// The workspace's channels.
    pub channels: Vec<RemoteChannel>,
    /// The workspace's catalog skills.
    pub skills: Vec<RemoteSkill>,
}

/// One channel in a [`RemoteWorkspace`]. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoteChannel {
    /// The channel's name.
    pub name: String,
    /// How many skills it references.
    pub skills: u64,
    /// The manifest file whose row adopts this channel here, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_in: Option<String>,
}

/// One catalog skill in a [`RemoteWorkspace`]. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoteSkill {
    /// The skill's catalog name (the reference leaf).
    pub name: String,
    /// The catalog's bundle kind (`"skill"` · `"mcp"`) — an OPEN string, a CLOSED vocabulary at
    /// the server that mints it. A listing renders it; the rows that DELIVER a bundle branch on
    /// it to choose the delivery mechanics.
    pub kind: String,
    /// What the workspace shares this bundle AT: a file bundle's `current` commit (64-char
    /// lowercase hex), or a connected server's catalog revision (`mcpr_` + 32 lowercase hex).
    #[cfg_attr(
        feature = "contract-derives",
        schemars(extend("pattern" = "^([0-9a-f]{64}|mcpr_[0-9a-f]{32})$"))
    )]
    pub version_id: String,
    /// Open, non-stale proposal count on the skill. Always zero for a connected server: what it
    /// holds is published in the catalog, never proposed to this workspace.
    pub open_proposals: u64,
    /// This machine's adoption state for the skill.
    pub state: RemoteAdoption,
}

/// This machine's adoption state for a `--remote` catalog skill. **PINNED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum RemoteAdoption {
    /// A row in the here-scope delivers it.
    AdoptedHere,
    /// Only the machine scope delivers it (seen from inside a project).
    AdoptedOnMachine,
    /// No scope covering the working directory delivers it — `topos add` records the demand.
    NotAdopted,
    /// Adopted, but the catalog `current` is newer than what is applied here — `topos update`
    /// advances it.
    UpdateAvailable,
}

/// The agent-eye view (`list -a <slug>`): for one harness from this folder, each skills dir it
/// reads and what sits in it. Deliberately spans both scopes. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AgentView {
    /// The harness's registry slug.
    pub agent: String,
    /// The harness's human-readable name.
    pub agent_name: String,
    /// The skills dirs the harness reads from this folder, each with its entries.
    pub dirs: Vec<AgentViewDir>,
}

/// One skills dir in an [`AgentView`]. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AgentViewDir {
    /// The dir's path.
    pub path: String,
    /// `"user"` (a home dir) or `"project"` (under this folder).
    pub scope: String,
    /// The skill dirs sitting in it.
    pub entries: Vec<AgentViewEntry>,
}

/// One entry of an [`AgentViewDir`]. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AgentViewEntry {
    /// The skill dir's name.
    pub name: String,
    /// What manages it: `"<file>:<row-key>"` for a manifest row, `"feed <host>/<ws>"` for a
    /// workspace feed, `"built-in"` for the CLI's own placed meta-skill; `None` = untracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<String>,
}

/// The deep answer for ONE skill (`topos list <name>`): which file and line-key (or which feed)
/// delivers it, the version and any pin, where its bytes are, and its state. **PINNED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ListDetail {
    /// The bundle's name.
    pub name: String,
    /// The scope section that answered (`"project"` / `"machine"`) — the spelling every suggested
    /// command must honor: a machine-scope answer spells `topos update -g`. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The manifest FILE whose row delivers it, when a row does (a path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    /// The row's spelled KEY in that file (the joined reference).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// The feed origin, when no row names it (`<host>/<workspace>` — the workspace whose feed
    /// delivers it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    /// The delivery attribution (`assigned by <name>` / `picked by you`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    /// The applied version, when applied locally: a file bundle's commit (64-hex), or a connected
    /// server's catalog revision (`mcpr_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A row's version pin, when one is spelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// The placement directories this machine holds for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placements: Vec<String>,
    /// The line's state (the resolution's own vocabulary).
    pub state: StatusItemState,
    /// The catalog bundle kind (`"mcp"` for a config-placed MCP-server bundle). Absent ⇒
    /// `"skill"`. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// For an `mcp` bundle: the per-agent config entries this machine records (the placed file +
    /// state), shown instead of placement dirs. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<McpAgentState>,
    /// For an `mcp` bundle whose row freezes a `dest` that names no config file topos can edit:
    /// why the bundle reaches NO agent. Absent whenever it reaches at least one — a row with
    /// somewhere to land says nothing here. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_unreachable: Option<String>,
    /// `false` = NO row in any visible scope manages this name (and it is not the built-in): the
    /// answer is the not-managed headline plus `folders`, and every managed-only field above is
    /// absent/at its baseline. Omits when `true` (the pre-existing managed shape). **INFERRED**
    /// (additive).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub managed: bool,
    /// For a NOT-managed answer: the folders of unmanaged copies discovered on disk (the known
    /// agents' skill dirs, matched by name) — `topos add <folder>` manages one. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    /// The copies whose edits DISAGREE (always ≥ 2 when present) — the state the inventory row
    /// reports as `draft in N folders that disagree` and sends the reader here to resolve. Each
    /// carries the folder as a person reads it and the `--dest` value that names it back, so the
    /// answer can offer the per-copy acts. Absent whenever at most one copy is edited.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diverged: Vec<DivergedCopy>,
    /// For a BLOCKED bundle: the workbench folder holding the stopped merge, spelled the way the
    /// deep dive prints it. The receipt that stopped the merge named this folder once; this is
    /// where a reader gets it back after that receipt has scrolled away — or after a silent
    /// session-start sweep raised the block with no receipt to read at all. Present only when the
    /// conflict record itself names a folder (never derived from the bundle's name, which two
    /// bundles can share). **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_copy: Option<String>,
    /// Why that merge stopped — the recorded reason, so the folder above is described the one way
    /// the receipt already described it (`three_way`: both versions marked up in it; `no_base`:
    /// this person's files with the team's beside them). Absent exactly when `conflict_copy` is.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_reason: Option<crate::persisted::ConflictReason>,
    /// THIS checkout carries a draft — bytes ahead of the version it stands at, in any of its
    /// folders. The one draft rule every surface reads (bytes against the lock), so a settled
    /// draft nobody has touched since the last sweep still counts as unshared work.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drafted: bool,
    /// The OTHER reachable scope's checkout of the SAME bundle, when it holds one — a separate
    /// copy with its own draft state, which the answer names so a reader never mistakes one
    /// checkout for both. Absent whenever only this scope tracks it. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twin: Option<ScopeTwin>,
}

/// The OTHER scope's checkout of one bundle: which scope holds it, and whether it carries edits.
/// No folder — the answer sends a reader to that scope's own `list`, which names its folders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ScopeTwin {
    /// `true` = the MACHINE's copy (`-g`), `false` = this project's.
    pub machine: bool,
    /// Whether that copy carries edits of its own.
    pub drafted: bool,
}

/// ONE copy of a bundle whose edits disagree with another copy's, in the two spellings every
/// surface that names such a copy uses: the folder as a person reads it, and the `--dest` value
/// that names it back on a command line. The same pair the placement freeze's refusal prints —
/// one vocabulary, so the deep dive and the refusal never describe one state two ways.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct DivergedCopy {
    /// `project/.agents/skills/coolify-deploy` · `~/.claude/skills/coolify-deploy`.
    pub display: String,
    /// `.agents/skills` · `~/.claude/skills` — the `--dest` value naming this copy.
    pub dest: String,
}

/// serde helper — [`ListDetail::managed`] defaults `true` and omits when `true`, so every managed
/// answer keeps its pre-existing byte shape.
fn default_true() -> bool {
    true
}

/// serde helper — see [`default_true`].
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(b: &bool) -> bool {
    *b
}

/// One external source's last auto-update check — the answer to "is this even working?", computed
/// entirely from local state. Every check is recorded whatever its outcome, so a source that has
/// gone quiet is visible here long before the silent sweep is willing to interrupt a session over
/// it. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ForgeSource {
    /// The source a manifest row names (`<host>/<owner>/<repo>`).
    pub source: String,
    /// When it was last checked (epoch millis), whatever came of that check.
    pub checked_at: i64,
    /// When it last ANSWERED (epoch millis). Absent = it never has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<i64>,
    /// The commit the last answering check saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Why the last check failed. Absent = it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure is FINAL — the forge says this source is gone, so it is not retried until the
    /// row that names it changes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gone: bool,
}

/// One row-capped bucket in a paged [`ListData`]. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct BucketTruncation {
    /// The capped bucket's name (a scope name — `project` / `machine` — or `untracked` /
    /// `remote` / `agent`). `remote` sums the drop across every workspace's skills, `agent`
    /// across every dir's entries: one bucket per view, whatever it nests.
    pub bucket: String,
    /// Rows emitted on this page.
    pub shown: u64,
    /// Total rows in the bucket before paging.
    pub total: u64,
}

/// A discovered-but-unmanaged skill — known only by where it lives, not by any topos version yet.
/// A skill dir belongs to a FOLDER, and a folder is read by zero or more installed agents; there is
/// no single owning harness, so the entry carries the folder and its readers rather than one slug.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct UntrackedEntry {
    pub name: String,
    /// The skill dir itself.
    pub path: String,
    /// The skills folder holding it — the parent of [`Self::path`].
    pub folder: String,
    /// The registry slugs of every INSTALLED agent that reads [`Self::folder`], sorted. These are the
    /// `<name>@<slug>` / `-a <slug>` tokens. Every entry of one folder carries the same list.
    pub readers: Vec<String>,
    /// Where the skill dir was found: `user` (a global agent home) or `project` (the current repo).
    pub scope: String,
    /// The folder this entry's bytes really live in, when [`Self::path`] is a LINK SHELL onto
    /// another folder (a symlinked dir, or a dir whose `SKILL.md` links out) — the folder an `add`
    /// of this entry takes. Absent when the entry IS its own origin, which is the ordinary case.
    /// Absolute here; the terminal abbreviates it under the home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<String>,
}

/// A skill row. `<skill>@<version_id>` identity + `draft` are PINNED; the other field names INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SkillEntry {
    pub skill: String,
    /// The workspace this skill is followed in (its pointer scope), or `None` for a purely local,
    /// never-followed `add`'d skill. Provenance so two same-named skills from different workspaces are
    /// distinguishable; `--json` carries it flat, the TTY groups by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// What this row is ON: a file bundle's approvable `@` token (the commit SHA-256), or a
    /// connected server's catalog revision (`mcpr_` + 32 lowercase hex). ONE field carries both,
    /// because what a bundle's version IS belongs to its kind — and only the first is a token any
    /// verb takes.
    #[cfg_attr(
        feature = "contract-derives",
        schemars(extend("pattern" = "^([0-9a-f]{64}|mcpr_[0-9a-f]{32})$"))
    )]
    pub version_id: String,
    /// The byte-exact consent hash, shown alongside as evidence. All-zero for a bundle that has no
    /// files to hash.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// Local edits ahead of the version this entry is on.
    pub draft: bool,
    /// Open proposals, each as `<skill>@<version_id>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_proposals: Vec<String>,
    /// Where the bytes come from — the ORIGIN, never the manifest that asked for it: the workspace
    /// address (`topos.sh/acme`) for anything a workspace delivers, the repository
    /// (`github.com/owner/repo`) for a forge row, the folder as the manifest spells it for a local
    /// one, `built-in` for the CLI's own meta-skill. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The update status of the local copy: `current` / `behind` / `draft` / `off`. Absent when
    /// no status is honestly claimable (never applied, pending, no delivery yet). **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SkillStatus>,
    /// The catalog bundle kind (`"mcp"` for a config-placed MCP-server bundle). Absent ⇒
    /// `"skill"`. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The health of the source named in [`Self::source`], when that source has STOPPED answering.
    /// Absent = nothing is wrong (or the origin is not one that gets probed). **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_health: Option<SourceHealth>,
    /// The FOLDER holding this row's edited copy — the draft — as the row prints it: written
    /// against the folder its manifest governs (`project/.claude/skills/<name>`) for a project
    /// row, `~`-abbreviated for a machine-wide one. Present only when exactly one copy is edited;
    /// absent when several disagree (`draft_diverged` says so instead) and on every non-draft row.
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_dir: Option<String>,
    /// How many folders hold edits that DISAGREE — copies none of which explains another, so no
    /// single one is the draft. Always ≥ 2 when present, and present only on a draft row whose
    /// copies compete; `draft_dir` is the one-copy case. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_diverged: Option<u32>,
    /// The row's SOURCE FOLDER is gone: a local-path row whose folder no longer exists. Nothing
    /// can apply it, so the row's only honest next step is dropping the line — never the update
    /// every other never-applied row offers. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_missing: bool,
}

/// A row's origin has STOPPED ANSWERING — the one fact about a bundle that no other column can
/// carry. Its presence is the failure; the copy on disk keeps working, and would keep reading
/// `current` forever, while nothing is keeping it current any more.
///
/// Why the last ANSWER and not the last check: the sweep retries on its own cadence and its clock
/// advances on failure too, so "last checked" is always recent and reads like a blip to ignore.
/// The question a stopped source raises is how stale this copy is, and only the last answer
/// measures that. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SourceHealth {
    /// When the source last ANSWERED (epoch millis). Absent = it never has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<i64>,
}

/// A tracked skill's update status in [`SkillEntry`]. **INFERRED** (additive value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum SkillStatus {
    /// On the followed `current` (or a local skill at its own head), no local edits.
    Current,
    /// The followed workspace serves a newer `current` than this copy holds — `update` to advance.
    Behind,
    /// Local edits ahead of the version this copy is on.
    Draft,
    /// A machine-local `"off"` row withholds this bundle here — a standing statement of the file,
    /// never a delivery. **Additive.**
    Off,
    /// A merge is undecided here (see [`StatusItemState::Blocked`]): this copy is neither current
    /// nor an ordinary draft, and publishing it is refused until one of the two exits is taken.
    /// **Additive.**
    Blocked,
}

// =================================================================================================
// PINNED — `diff` (source + version_id; body is a plain unified diff).
// =================================================================================================

/// `diff` result. `source` + `version_id` (+ the emitted digest) are **PINNED**; the diff *body*
/// representation ("a plain unified diff") is the only INFERRED part. The byte-budget fields
/// (`truncated` / `files`) are ADDITIVE and omit entirely on an uncapped diff, so the pinned shape
/// is byte-identical there.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct DiffData {
    pub source: DiffSource,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// A plain unified diff. Under a byte budget (`--max-bytes`, or the `--json` default cap) it
    /// carries only the LEADING whole-file sections that fit — truncation is always at a file
    /// boundary, never mid-hunk — and `truncated`/`files` disclose the rest.
    pub diff: String,
    /// `true` when the emitted `diff` was capped by a byte budget (some file sections were dropped).
    /// Omits when `false` — an uncapped diff keeps the exact prior shape. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Present ONLY when `truncated`: one row per file in the FULL diff, in diff order, with
    /// `patch_omitted: true` on the files whose section was dropped from `diff`. The
    /// accompanying `FETCH_FULL_DIFF` next action re-runs the diff uncapped. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<DiffPatchInfo>,
    /// The folder the LOCAL side was read from, as a person reads it
    /// (`project/.agents/skills/coolify-deploy`). Populated only when the bundle sits in more than
    /// one folder — with a single copy there is nothing to disambiguate and the shape stays exactly
    /// as it was. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    /// The bundle's name, carried alongside `dest` so the answer can say WHICH bundle's which copy
    /// it read. Populated on the same condition. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
}

/// One file's row in a byte-capped [`DiffData`]. **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct DiffPatchInfo {
    /// The bundle-relative path of the changed file.
    pub path: String,
    /// `true` when this file's patch section was dropped from `diff` to fit the byte budget.
    pub patch_omitted: bool,
    /// The full section's size in bytes (headers + hunks), so an agent can budget a refetch.
    pub patch_bytes: u64,
}

/// Where the compared bytes came from: the local sidecar, or a plane-held proposal. **PINNED.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DiffSource {
    Local,
    Plane,
}

// =================================================================================================
// INFERRED — the nine verbs whose `data` field list the spec does not enumerate. Shapes derived
// from the documented mechanics; additive-only; will tighten as each verb is built.
// =================================================================================================

/// Where an adopted skill was imported FROM, when `add` fetched it from a remote source (a GitHub repo).
/// All fields are public provenance — never a secret — and travel with the adopted skill so the agent (and
/// a later re-sync) can see the upstream it came from. `None` on `AddData` for a locally-adopted skill.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SkillOrigin {
    /// The `<host>/<owner>/<repo>` the skill was imported from (e.g. `github.com/vercel-labs/agent-skills`).
    pub source: String,
    /// The branch / tag / commit requested (`#<ref>` or a `/tree/<ref>/…` URL), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The resolved commit the bytes came from (best-effort — parsed from the fetched archive), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The skill's path within the repo (a monorepo subdir), if it was not the repo root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// A LICENSE file found at the skill root or repo root, recorded as provenance (never injected into the
    /// bundle — the adopted bytes stay byte-exact to the repo). `None` if the source carried no license.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

/// Which of the two unblended manifests an add recorded into. Same vocabulary as
/// [`ListScope::scope`]. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ReceiptScope {
    /// The nearest `topos.toml` covering the working directory.
    Project,
    /// This machine's own `~/.topos/topos.toml` (`-g`).
    Machine,
}

/// `add` (local, offline — no plane op, `receipt: null`). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AddData {
    /// The local sidecar record this add minted, when it minted one. ABSENT for a row that has no
    /// bytes of its own here — a feed, a channel, a whole repo: a row-only add mints no version
    /// history, and a zeroed id would claim one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    pub name: String,
    /// What the local record starts from: a file bundle's base commit (64-char lowercase hex), or
    /// a connected server's catalog revision (`mcpr_` + 32 lowercase hex). Absent wherever
    /// `skill_id` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "contract-derives",
        schemars(extend("pattern" = "^([0-9a-f]{64}|mcpr_[0-9a-f]{32})$"))
    )]
    pub version_id: Option<String>,
    /// The kernel digest of the bytes this add landed. Present whenever bytes landed — including
    /// an adopted MCP folder, which IS the bundle — and absent for a row that points at somebody
    /// else's bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: Option<String>,
    pub tracked: bool,
    /// The auto-update-trigger outcome of an add into a recognized harness. Always empty since
    /// 0.1.52; kept for schema stability (an add writes no hook — hooks follow the agents pick).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<crate::TriggerReport>,
    /// The other agents' trigger outcomes of the same add. Always empty since 0.1.52; kept for
    /// schema stability. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::TriggerReport>,
    /// Where the skill was imported FROM, when `add` fetched it from a remote source. `None` for a
    /// locally-adopted skill (a path or a discovered name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<SkillOrigin>,
    /// WHERE THIS BUNDLE COMES FROM, in the spelling that identifies it forever: the full
    /// reference where a workspace or a forge governs it (`topos.sh/acme/code-review`,
    /// `github.com/owner/repo`), and the canonical folder on this machine where the bytes are the
    /// person's own. One derivation for every arm — the receipt's `source:` line is this value
    /// verbatim. Absent only for an internal adopt that recorded no row. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The MANIFEST this add edited — the trust rail's first half: a `topos.toml` path. Absent when
    /// no manifest line was written (an internal adopt). **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// WHICH file it was, as a receipt names it: this folder's `./topos.toml` or the machine-wide
    /// `~/.topos/topos.toml`. Written together with `manifest` and absent wherever that is — the
    /// path is the machine-readable fact, this is the one a person reads. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ReceiptScope>,
    /// The reference the manifest line stores (canonical where resolvable). **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The paste-ready inverse (`topos remove <ref>`). Empty when nothing is undoable. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undo: Vec<String>,
    /// A GOVERNED COPY of the same upstream source already living in a connected workspace — the
    /// dedup suggestion on a remote import ("acme already has this as `@acme/deploy`"). Disclosed,
    /// never blocking: the import proceeded as asked; the reference is the governed alternative.
    /// **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governed_copy: Option<GovernedCopy>,
    /// The connected workspace that publishes the same NAME an `add <name>` just adopted from a
    /// local directory — the team-managed spelling, disclosed beside the local copy that landed.
    /// Never blocking (the adopt happened as asked); absent when no workspace, or more than one,
    /// carries the name. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_match: Option<PublishedMatch>,
    /// A disclosure the receipt leads with when the edit was not the plain row write: a
    /// redundant row NOT written (the feed already delivers it), an `"off"` switch deleted
    /// instead of a row added, or a standing web decline this machine's row now overrides.
    /// **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The MCP SERVER this add landed (`add --kind mcp`, or a subscribe to a workspace `mcp`
    /// bundle): what the gated document says it is, the endpoint every agent will call, and the
    /// agents the converge reached — typed, so a JSON consumer never parses the prose note.
    /// Absent for every other add, and for a workspace subscribe whose delivery has not landed
    /// the document yet. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpServerSummary>,
    /// The destinations the row was frozen to (`-a`/`--dest`): skills folders as the manifest
    /// spells them (`~/`-abbreviated in the machine file, project-relative in a project file),
    /// or a config-placed bundle's config files. Empty when the add named no destination.
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dest: Vec<String>,
    /// The workspace-qualified display name the receipt leads with (`@<ws>/<name>` at the
    /// machine's one connected host, else `<host>/<ws>/<name>`). Absent for a non-workspace
    /// source. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// The WORKSPACE this add's source names, when one does — a workspace reference, a bare name
    /// that resolved to one, a channel or a feed row. Absent for a path, a forge import and the
    /// built-in. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// Where those destinations RESOLVED to on this machine, when that differs from how the
    /// manifest spells them.
    ///
    /// `dest` is the row's own spelling and must stay it — a manifest line is portable, and
    /// `~/.codex/config.toml` is what belongs in the file. But an env override (`$CODEX_HOME`,
    /// `$XDG_CONFIG_HOME`, `$CLAUDE_CONFIG_DIR`) moves where that spelling actually lands, and a
    /// receipt naming the default pointed at a file that does not exist on the machine it was
    /// printed on. Empty ⇒ nothing moved and the row's own spelling IS the resolved one.
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dest_resolved: Vec<String>,
    /// What this add did to the destinations a row ALREADY recorded. Absent when the row was born
    /// by this add — that receipt leads with what landed — and absent when the add named no
    /// destination at all. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_change: Option<DestChange>,
    /// The IDENTITY CLAIM this add recorded (`--as`): a folder that already held a copy of a
    /// bundle this scope manages is now one of its places. Nothing in the folder changed and no
    /// version was minted. Absent for every other add. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimReceipt>,
    /// This add changed NOTHING — the file already spelled exactly this row, or the folder was
    /// already one of the bundle's places. The receipt then leads with that fact instead of an
    /// installed/added headline above a note retracting it: an act that did not happen may not
    /// head the answer. `note` carries the sentence. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
    /// The MACHINE folder this add's own copy landed in, disclosed when a checkout at or above
    /// this folder ALREADY delivers the same bundle — a machine-wide add then gives you a second
    /// checkout beside the project's, which is a surprise worth stating at the moment it is
    /// created. Absolute (the wire's form; the receipt abbreviates it under the home). Absent for
    /// every add that creates no second copy. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_copy: Option<String>,
    /// The SET-DELIVERED add (see [`SetDelivery`]): the scope already delivers this bundle through
    /// a channel or feed row, so no row was written and the invocation converged its placements
    /// instead. Absent for every other add. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_delivery: Option<SetDelivery>,
}

/// `add -a`/`--dest` on a bundle a SET already delivers in the invoked scope — the one add that
/// records NOTHING: the demand stands already, and an explicit row could only narrow what the set
/// reaches. The invocation converges the bundle's placements at full reach instead, so the asked
/// surface gets the copy it was missing. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SetDelivery {
    /// The set that delivers it, as this scope names it: `channels/<name>` for a channel row, the
    /// workspace reference (`<host>/<workspace>`) for the feed row.
    pub set: String,
    /// WHICH FILE'S set this is — the receipt's word for the reach ("here" / "on this machine").
    /// Carried here rather than on the envelope's `scope`, which belongs to a manifest this add
    /// deliberately did not write.
    pub scope: ReceiptScope,
    /// Every surface the converge answered for, plus any asked surface it could not reach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surfaces: Vec<Surface>,
    /// The harness slugs the invocation ASKED for, in the order they were named — what the
    /// nothing-was-missing answer speaks about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asked: Vec<String>,
    /// The converge's own FAILURE for this bundle, in the sweep's words — present only when the
    /// converge ran and could not carry the bundle forward. It is what the asked agent's line says
    /// instead of a reason it does not have, and it is why the answer cannot close on
    /// `nothing changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// `add <path> --as <bundle>` — the folder is recorded as one of the bundle's places, byte for
/// byte as it stands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ClaimReceipt {
    /// The claimed folder, as a printed line spells it (`~/`-abbreviated under the machine home).
    pub folder: String,
    /// What the folder's bytes turned out to be, against the bundle's own history.
    pub state: ClaimState,
    /// A DUPLICATE of the same bundle beside the claimed folder — the copy the engine had placed
    /// under its collision-suffixed name. Absent when there was none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twin: Option<ClaimTwin>,
}

/// What a claimed folder holds, against the bundle's version history. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimState {
    /// Exactly the version the bundle is on — the folder is already current.
    Current,
    /// Exactly an OLDER version in the bundle's history — the next update brings it current.
    Older,
    /// Bytes no version explains: they become this bundle's draft (snapshotted first).
    Edited,
    /// Edited, AND another folder holds different edits — the placement freeze. Nothing syncs
    /// until one copy is chosen.
    Frozen,
}

/// The duplicate copy beside a claimed folder: the engine's own placement under its
/// collision-suffixed name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ClaimTwin {
    /// The duplicate's folder, as a printed line spells it.
    pub folder: String,
    /// `true` when it was retired (it held no edits); `false` when it was kept because it did.
    pub removed: bool,
}

/// The destination set of a STANDING row that already named destinations, changed by an add.
/// Those EXTEND: what the row recorded stays, and what this add named joins it. A row that named
/// none is replaced instead of extended, and carries no change here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DestChange {
    /// The destinations this add ADDED, in the order the row now spells them.
    pub added: Vec<String>,
}

/// The describe a bare `add` of a git source returns: the source, what was discovered in it, and
/// exactly what would be written where — nothing has landed. `--yes` applies. **INFERRED**
/// (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AddDescribeData {
    /// The source the add would take bytes from (e.g. `github.com/<owner>/<repo>`).
    pub source: String,
    /// The skills discovered in it (leaf directory names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// The manifest FILE the row would land in.
    pub manifest: String,
    /// The row key that would be written.
    pub reference: String,
    /// The row value that would be written (`"*"` or a commit pin).
    pub value: String,
    /// An honesty line the describe leads with when the manifest ALREADY spells this row (a
    /// cloned file, a prior add whose installs failed): the row is demand, not consent — trust
    /// in a forge origin is a store fact, so the describe still gates, and applying is what
    /// fetches and installs the skills the row names. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What an MCP `server.json` says, after the gate accepted it — the facts an applied `add`
/// receipt carries beside the undo, whether the document was adopted from a folder here or
/// delivered by a workspace subscribe. DERIVED from the document; the document itself is never
/// echoed whole.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct McpServerSummary {
    /// The registry name (`io.github.<owner>/<server>`) — the server's own identity.
    pub server: String,
    /// The publisher's one-line summary (the registry caps it at 100 characters).
    pub description: String,
    /// The version the document declares, or EMPTY for a server that names none — an editorial or
    /// self-maintained one. Never a fabricated stand-in: a receipt renders the empty as no version
    /// at all (`what_it_is` beside it), rather than printing a bare `v`.
    pub version: String,
    /// The endpoint an agent will call — always `https`, never a template. EMPTY when the bundle
    /// offers no address at all: a server may be a package every machine runs instead (see
    /// `packages`).
    pub url: String,
    /// The transport of that endpoint, always `streamable-http` today: the one a shared bundle can
    /// promise, since the same URL has to work from every machine. Empty exactly when `url` is.
    pub transport: String,
    /// The packages the bundle offers, for the machines that run the server instead of dialing it
    /// — and for the agents that cannot dial one at all. Empty when the bundle is an address and
    /// nothing else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<McpPackageSummary>,
    /// The publisher's `_meta` auth word (`oauth` / `none` / `manual`) when the document declared
    /// one. Absent means it said nothing — which is NOT a claim that no credential is needed.
    /// `manual` is the one that costs a person a step: this server takes only a token they create
    /// or an app an administrator registers, so no agent signs in by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// The names of the LITERAL headers every call will carry. A credential-shaped header never
    /// reaches here — the gate refuses the whole document instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    /// The folder holding the document (the bundle IS its directory). Absent for a
    /// workspace-subscribed server, whose bytes live in the scope's store rather than in a
    /// folder of their own here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    /// The agents set up here that the row reaches, after any `dest` narrowing — the honest
    /// breadth line, so nobody has to reverse-engineer which config files changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
}

/// One package a `kind = "mcp"` bundle offers — what a machine that RUNS the server installs, as
/// opposed to the address a machine dials. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct McpPackageSummary {
    /// The registry it comes from (`npm`, `pypi`, `oci`, …) — an open vocabulary: a bundle may
    /// name any registry the official server format expresses, and a given topos sets up the ones
    /// it has a runtime for.
    pub registry: String,
    /// The package's name in that registry.
    pub identifier: String,
    /// The exact version every machine installs. Absent only where the registry type carries the
    /// version inside the identifier (an OCI digest).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
}

/// `verify <name>` — the LIVE check of one MCP server bundle, printed and never stored. The verdict
/// is about this moment and this machine: it is not a delivery state, and nothing in the sidecar
/// remembers it. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct VerifyData {
    /// The bundle's name.
    pub name: String,
    /// What was checked: `address` (an endpoint this machine dialed) or `program` (a package this
    /// machine ran and spoke to over its pipes).
    pub target: String,
    /// The endpoint dialed, when the check dialed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The program run, as the command and its arguments, when the check ran one. No environment
    /// value ever appears here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The verdict.
    pub state: VerifyState,
    /// WHICH of the two sign-in worlds the server lives in, on `sign_in_required` alone. Absent
    /// wherever the check could not read the server's own OAuth discovery documents end to end —
    /// a chain nobody could read is never reported as the worse of the two, and it is absent on
    /// every other state because they demand no sign-in at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<SignInPath>,
    /// How many tools the server listed. Present only on `responding`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<u32>,
    /// The plain-words reason behind anything other than `responding` — the same words the printed
    /// line carries after its colon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The MCP protocol revision the server answered on (`2026-07-28` for the current revision,
    /// `2025-11-25` for the handshake era). Absent when nothing answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// The process exit code this verdict produces: `0` responding · `3` sign-in required ·
    /// `4` not reachable · `5` not answering as an MCP server.
    pub exit_code: u8,
    /// The one sentence the terminal prints — relay it verbatim.
    pub line: String,
}

/// The four things a live check can conclude. **INFERRED value set** (additive-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VerifyState {
    /// It answered as an MCP server and listed its tools.
    Responding,
    /// It refused without a sign-in — a HEALTHY server; topos holds no credential and never will.
    /// WHO can complete that sign-in is a second question, and [`VerifyData::sign_in`] is where it
    /// is answered.
    SignInRequired,
    /// Nothing answered: the name did not resolve, the connection failed or timed out, or the
    /// server is having trouble right now. Never a statement about the protocol.
    NotReachable,
    /// Something answered, but not as an MCP server.
    NotAnMcpServer,
}

/// Who can complete the sign-in a server demands — the distinction the same `401` hides.
/// **INFERRED value set** (additive-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SignInPath {
    /// Its authorization server registers clients on demand, so the agent app signs in on first
    /// use with nothing arranged beforehand.
    SelfService,
    /// It accepts only clients or tokens registered in advance — a personal token, or an app an
    /// administrator created. A person does that once, per machine or per organization; an agent
    /// cannot do it for them.
    Manual,
}

/// `fmt [-g]` — rewrite a manifest into the normal form. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct FmtData {
    /// The manifest's path.
    pub manifest: String,
    /// Whether the bytes changed (`false` = already normal).
    pub changed: bool,
}

/// The dedup suggestion an [`AddData`] carries when a remote import's source is already governed
/// in a connected workspace. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct GovernedCopy {
    /// The workspace holding the governed copy, as a person addresses it.
    pub workspace: WorkspaceRef,
    /// The skill's catalog name there.
    pub name: String,
    /// The paste-ready reference, in the canonical host-qualified form
    /// (`<host>/<workspace>/<name>` — unambiguous however many servers this installation is
    /// logged into).
    pub reference: String,
    /// Whether the governed copy was imported from the exact same subdirectory (`false` = same
    /// repository, different path).
    pub same_path: bool,
}

/// The workspace spelling an [`AddData`] carries when the bare NAME a local adopt resolved is also
/// published in exactly one connected workspace. Distinct from [`GovernedCopy`], which matches an
/// import's UPSTREAM source; this one matches nothing but the name the user typed.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PublishedMatch {
    /// The workspace publishing the same name, as a person addresses it.
    pub workspace: WorkspaceRef,
    /// The bundle's catalog name there (the name that matched).
    pub name: String,
    /// The paste-ready reference, in the canonical host-qualified form
    /// (`<host>/<workspace>/<name>` — unambiguous however many servers this installation is
    /// logged into).
    pub reference: String,
    /// Whether the adopted bytes are IDENTICAL to the version that workspace currently serves —
    /// `false` also covers "not knowable here" (an offline cache match carries no digest).
    pub identical: bool,
}

/// `init` — create this folder's `topos.toml` (the project manifest `add`/`remove` edit and
/// `update`/`status` resolve). **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct InitData {
    /// The manifest's path.
    pub manifest: String,
    /// `false` when one already existed (the no-op receipt).
    pub created: bool,
    /// A placement note (outside a git repo the file will not travel — stated honestly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The agents pick this run applied at the manifest's scope, and what landed for it. Absent
    /// when the run only created (or found) the file and applied no pick. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<PickReceipt>,
}

/// One picked agent's auto-update hook, as an `init` / `agents add` receipt names it.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PickHook {
    /// The registry slug.
    pub agent: String,
    /// The hook file, as a person reads it (project-relative, or `~`-abbreviated).
    pub file: String,
    /// The step the harness still owes before the hook fires (Codex: trusting the repo), when
    /// there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What an `init -a` / `agents add` placed for the pick at one scope. **INFERRED**
/// (additive-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PickReceipt {
    /// `"project"` or `"machine"`.
    pub scope: String,
    /// The picked agents' registry slugs, in harness-table order.
    pub agents: Vec<String>,
    /// The pick file, as a person reads it.
    pub pick_path: String,
    /// How many skill bundles have a copy in a picked agent's skills folder at this scope.
    pub skills: u64,
    /// The skills folders those copies sit in (project-relative, or `~`-abbreviated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills_dirs: Vec<String>,
    /// How many MCP server bundles have an entry in a picked agent's config at this scope.
    pub mcp_servers: u64,
    /// The MCP config files those entries sit in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_files: Vec<String>,
    /// The auto-update hooks registered for the picked agents at this scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_files: Vec<PickHook>,
    /// One line per picked agent that reads no hook at this scope (a project pick of an agent
    /// with no per-project hook).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_notes: Vec<String>,
    /// The agents installed on this machine that the pick leaves untouched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untouched: Vec<String>,
    /// The command that adds the picked agents' folders to `.gitignore`, offered when the
    /// project is in a git repository and they are not ignored yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitignore_hint: Option<String>,
    /// The `.gitignore` entries `--gitignore` appended this run (empty when nothing was missing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gitignored: Vec<String>,
    /// The bundle rows the scope's reconcile removed this run (a row the manifest no longer
    /// carries), with the folders or config files that left. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<PickRemoved>,
    /// The lines the reconcile and the built-in placement raised: a bundle that could not be
    /// carried forward, a project root the containment rail refused, a `.gitignore` that could
    /// not be edited. The same channel `update` prints; a `failure` line makes the run exit
    /// non-zero exactly as `update` does. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::Message>,
    /// How many bundles the reconcile could not carry forward this run. **Additive.**
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failed_bundles: u64,
}

/// One bundle row a pick run's reconcile removed, and where its copies or entries left.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PickRemoved {
    /// The bundle, as a person reads it (workspace-qualified where it came from one).
    pub bundle: String,
    /// The folders or config files that left.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// `topos agents` — the pick where you stand and what is installed on this machine.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AgentsData {
    /// `"project"` or `"machine"`.
    pub scope: String,
    /// The pick file in force, as a person reads it; absent when no pick stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick_path: Option<String>,
    /// Where the pick comes from: `"project"` (the project's own file) or `"machine"` (the
    /// machine file, inherited inside a project). Absent when no pick stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The picked agents' registry slugs, as the file spells them (`"*"` = every installed agent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// The picked slugs this binary's harness table does not know (a row a newer table dropped,
    /// or a hand-edited file). Kept in the pick as written; they pick nothing until a table
    /// knows them again, and `agents remove` takes them out. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_in_table: Vec<String>,
    /// Every agent installed on this machine (its detect dir exists), in harness-table order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed: Vec<String>,
    /// The auto-update hook state of each picked agent at this scope, probed read-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<StatusTrigger>,
    /// The `.gitignore` entries `--gitignore` appended this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gitignored: Vec<String>,
    /// The lines `--gitignore` raised (a `.gitignore` that is a symlink out of the checkout is
    /// not edited, and says so here). **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::Message>,
}

/// `topos agents remove` — the describe (`applied: false`) and the applied receipt.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AgentsChanged {
    /// `"project"` or `"machine"`.
    pub scope: String,
    /// The agents leaving the pick.
    pub removed: Vec<String>,
    /// The pick after the change (as the file would spell it).
    pub agents: Vec<String>,
    /// The pick file, as a person reads it.
    pub pick_path: String,
    /// `false` = the describe: nothing has changed yet.
    pub applied: bool,
    /// The skill copies that leave (describe) or left (applied), one folder each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_dirs: Vec<String>,
    /// The MCP config files the leaving agents' entries leave (describe) or left (applied).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_files: Vec<String>,
    /// The auto-update hook files that leave (describe) or left (applied).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<String>,
    /// Folders the leaving agents read that STAY, because another picked agent reads them too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untouched: Vec<String>,
    /// Copies with local edits kept in place (an edited copy is the person's own work).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
}

/// The `keep it as yours` describe — an `add <name>` that re-forks a RETAINED withdrawn/detached copy
/// into a NEW local skill with no upstream. Bare `add <name>` returns this preview; `--yes` re-adopts the
/// bytes and returns an ordinary [`AddData`]. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct KeepAsYoursData {
    /// The skill name to re-fork.
    pub name: String,
    /// The workspace the retained copy was followed in (its former upstream), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// That same workspace as a person addresses it. Absent alongside `workspace_id`, and
    /// whenever no session on this installation names it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// Why the local copy is retained (and no longer delivering here).
    pub reason: KeepReason,
    /// Whether a local draft rides along into the fork (a snapshotted or on-disk edit ahead of the base).
    pub has_draft: bool,
}

/// Why a `keep-as-yours` copy is retained but no longer live. **INFERRED** (additive value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum KeepReason {
    /// Upstream withdrew the skill (archived, or its last delivering channel dropped it) — the agent dirs
    /// were cleaned; the sidecar kept the bytes + any draft.
    WithdrawnUpstream,
}

/// A pending device-authorization a `follow` surfaced — the human opens `verification_uri` and
/// enters `user_code` there (or the invitation page the URI names weaves them through); the client
/// re-polls. The code never rides a URL. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct EnrollmentPending {
    /// The page the human opens to approve the session — the bare approval address (or, for an
    /// invitation enrollment, the invitation page that weaves accept + approval). Never embeds the
    /// code.
    pub verification_uri: String,
    /// The short human-facing code the approval page asks for and displays back (the glance-check
    /// against this terminal — never typed as a secret, never part of a URL).
    pub user_code: String,
    /// The session expiry as an RFC-3339 string, if it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The minimum poll interval, in seconds — a headless agent re-invokes no faster than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
}

/// `login <workspace-address>` — the SESSION mint + the acceptance disclosure (what connecting
/// delivers). A session = user × workspace × installation, carrying ONE workspace-scoped bearer
/// credential; further workspaces are further logins. Login IS the acceptance: from here delivery
/// is silent, npm-style. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct LoginData {
    /// The logged-into workspace's id (empty while the login is still pending approval).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
    /// The workspace this login connected to, as a person addresses it — the slug the browser
    /// approval recorded, on the host the session lives on. Absent while a bare login still
    /// awaits the approval that names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The API base this installation dials (from the protocol card).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The minted session's id (`sn_…`) — the handle the web sessions pages show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `"active"`, `"pending"` while the workspace's session-approval knob holds it (no data flows
    /// until an owner approves), or `"awaiting-approval"` while the browser approval is pending.
    pub session_status: String,
    /// How many bundles the workspace ASSIGNS this person right now (the acceptance disclosure;
    /// best-effort — absent when the count could not be read). Assigned, not delivered: login
    /// mints the session and reads the feed, and the first exchange that puts bytes on this
    /// machine is `topos update` — which is why `status` right after a login still says there has
    /// been no delivery yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned: Option<u64>,
    /// The assigned bundles' NAMES (the same best-effort read as `assigned`) — the receipt names
    /// what the acceptance brings, not just a number. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assigned_names: Vec<String>,
    /// Present while the login awaits the browser approval (re-run `topos login` to resume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<EnrollmentPending>,
    /// The auto-update-trigger outcome of the login. Always empty since 0.1.52; kept for schema
    /// stability (a login writes no hook — hooks follow the agents pick).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<crate::TriggerReport>,
    /// The other agents' trigger outcomes of the same login. Always empty since 0.1.52; kept for
    /// schema stability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::TriggerReport>,
    /// The honest reason this machine's own `topos.toml` was left alone when the login could not
    /// record the workspace's feed row there (an unreadable or unwritable file). Absent on every
    /// clean login. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_note: Option<String>,
    /// The signed-in person's display identity (the workspace membership describe's `principal`),
    /// read best-effort once the session stands — absent when the read failed or the session is
    /// still pending. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Whether THIS login wrote the workspace's feed row into the machine's own `topos.toml` —
    /// true only on this machine's first connection to the workspace (login never re-adds a row
    /// someone deleted). **Additive.**
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub feed_row_added: bool,
    /// The paste-ready inverse of the feed row this login wrote (argv tokens, `topos`-less) —
    /// present iff `feed_row_added`. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undo: Vec<String>,
    /// Whether an agents pick already stands for this machine. What the workspace delivers reaches
    /// the agents that pick names, so with no pick yet nothing installs anywhere and the receipt
    /// says how to pick instead of promising an install. **Additive.**
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub machine_pick: bool,
}

/// `logout [<workspace>|--all]` — end this installation's session(s). **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct LogoutData {
    /// The sessions ended, one entry per workspace, each as a person addresses it.
    pub ended: Vec<WorkspaceRef>,
    /// Whether the server-side revoke landed for EVERY ended session (`false` = at least one was
    /// already gone server-side, or unreachable — the local sign-out proceeded regardless).
    pub server_revoked: bool,
}

/// `log` — local action events (and, with `--team`, partial plane records). The individual event
/// fields are **not pinned by the spec**, so events stay open JSON. **INFERRED.**
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct LogData {
    /// Local action-event envelopes from `log.jsonl` (field set intentionally open).
    pub events: Vec<serde_json::Value>,
    /// Plane-side records under `--team` (op-receipts ⋈ approvals ⋈ lineage) — honestly partial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<Vec<serde_json::Value>>,
    /// When the skill was resolved by a FREED base name (it has since been archived under a new name),
    /// the archived-successor hint: "`<base>` is archived as `<archived>`". **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_successor: Option<String>,
    /// `true` when `events` was row-capped (`--limit`/`--offset`, or the `--json` default page):
    /// more events exist past this page — the `NEXT_PAGE` next action's argv fetches them. Omits
    /// when `false` (an uncapped log keeps the exact prior shape). **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The TOTAL event count before paging, present only when a page was applied. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// The delivering workspace's LAST exchange with this machine, when it did not land — this
    /// history was read from state nothing has refreshed since. Absent when that exchange landed,
    /// and for a copy no workspace delivers. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_fault: Option<SyncFault>,
    /// Present when this history came from the WORKSPACE because no copy is applied in the scope
    /// the command ran in: a recipe row here demands the bundle and no update has landed it yet.
    /// A history is a LOOKUP — the versions live in the workspace — so the read still answers,
    /// and this says which state it answered from. Absent for every applied copy. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_applied: Option<NotApplied>,
}

/// A bundle a recipe row in this scope DEMANDS with no applied copy behind it. **INFERRED**
/// (additive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct NotApplied {
    /// The bundle, as the workspace catalogs it.
    pub bundle: String,
    /// `true` when the row lives in the MACHINE recipe — the update that lands it is spelled
    /// `-g`. The refusal-side twin of this fact carries the `NOT_APPLIED` code.
    pub global: bool,
}

/// A workspace's last exchange with this machine, recorded because it did not land. **INFERRED**
/// (additive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SyncFault {
    /// The workspace as a person NAMES it — the recorded address name, and the opaque id only
    /// where nothing on this machine still names it. Deliberately NOT the uniform
    /// [`WorkspaceRef`]: a workspace whose session is gone has no knowable host, and half an
    /// address is worse than the name a person would recognize.
    pub workspace: String,
    /// What went wrong.
    pub kind: ExchangeFault,
}

/// Why an exchange with a workspace did not land. All three keep local state and retry later, but
/// they are DIFFERENT things to the person reading them, and only the first is a failure to reach
/// the server at all. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum ExchangeFault {
    /// Connect-level: the server could not be dialed at all (dial / TLS / timeout).
    Unreachable,
    /// The server was reachable but the exchange did not land — a failure status, or an answer
    /// that never fully arrived.
    Unavailable,
    /// A COMPLETE answer arrived and its structure was wrong.
    Malformed,
}

/// `publish` (a direct publish that moves `current`). Under a `reviewed` bundle a direct publish is
/// DOWNGRADED to a proposal (see [`ProposeData`]); a publish with no session is refused typed (sign in
/// with `topos login <workspace-address>` first). **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PublishData {
    pub skill_id: String,
    /// The cwd project's `topos.lock` beside this publish: its entry for the bundle ADVANCED to
    /// the version just shipped (the checkout runs it at once, the way a commit moves `HEAD`), or
    /// was HELD at the version the manifest pins. Absent when no project lock records the bundle.
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_lock: Option<ProjectLock>,
    /// Present when the shipped copy EQUALED an older version than the workspace's live `current`
    /// (a revert had moved `current` past this machine's own publish, say): the publish carried
    /// that version's content forward as the new version, and the receipt names both. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub republish: Option<Republish>,
    /// The skill's NAME — the handle humans speak and the TTY success line leads with
    /// (`Published <name>@…`); the opaque `skill_id` above stays the machine key.
    pub name: String,
    /// The new commit (the shipped `version_id`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The byte-exact digest of the shipped bytes — computed over the draft before any network call;
    /// an optional `<skill>@<digest>` pin gates it.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The pointer's new generation after the move.
    pub current_generation: u64,
    /// Present ONLY when THIS `publish` invocation ADDED the skill to topos first (the auto-add
    /// convenience: `publish <name>|<name>@<harness>|<dir>` adopts an untracked LOCAL skill, then ships it
    /// in one command). Discloses the one local `add` the publish folded in; `None` when the skill was
    /// already tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<AddedNote>,
    /// The channel whose placement was WITHHELD by its curated mode (the receipt's
    /// `details.placement = curated_role_required`): the publish itself landed — catalog + moved
    /// pointer — but the skill's reference was NOT placed into this channel (the `--to` target, or
    /// the default `everyone` for a brand-new skill). A reviewer or owner places it
    /// (`topos channel add <channel> <skill>`). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_withheld: Option<String>,
    /// The channel named by `--to` that no longer EXISTED at the write (deleted between the
    /// client's existence check and the transaction — the in-transaction refusal, never a silent
    /// mint): the publish landed catalog-only; re-run `--to` once the channel exists. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_missing: Option<String>,
    /// The GOVERNANCE-TRANSFER receipt half: the manifest whose local-path line this publish
    /// rewrote to the governed workspace reference. Absent when no manifest referenced the bundle
    /// by path (an already-governed republish). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The canonical workspace reference the manifest now stores. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The local-path spelling the manifest carried BEFORE the transfer (the inverse is
    /// `topos add <converted_from>` after a `topos remove <reference>`). **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<String>,
    /// The paste-able teammate handoff line (`Ask your agent: …`) — the join instruction that
    /// brings a teammate's machine into the workspace, composed from the workspace's server
    /// origin + address. Absent when the address is not known (a best-effort read — the publish
    /// itself is unaffected). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_line: Option<String>,
    /// The workspace this publish reached, as a person addresses it — how the receipt names
    /// WHERE these bytes landed. Absent when neither this installation's session nor the
    /// best-effort `me` read could name it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// The paste-able share line (`<address>/skills/<name>`) — a members' deep link to the skill
    /// just published. Absent when the workspace address is not known. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_line: Option<String>,
    /// The undo — the whole `topos revert <name> --to <version>` command that puts the team back
    /// on the version `current` held before this publish. WITHHELD unless it verifiably restores
    /// that state: a bundle this machine does not follow cannot be reverted from here, and a
    /// genesis publish left no prior version to restore. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<String>,
    /// The folder the published bytes were read from, as a person reads it
    /// (`project/.agents/skills/coolify-deploy`), present when it was a CHOICE — one of several
    /// edited copies (`--dest`), or a copy in a scope other than the one the command stood in. A
    /// single edited copy in the standing scope needs no such line: it is the draft, and naming its
    /// folder would say nothing. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_placement: Option<String>,
    /// The other EDITED copies IN THIS SCOPE, untouched by this publish — each keeps its bytes and
    /// becomes an ordinary draft ahead of the version just published. Populated by a `--dest`
    /// selection among several edited copies (a cross-scope ship discloses the other scope's copy
    /// through `other_scope_draft` instead). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_edited: Vec<String>,
    /// An honesty note about the bundle's GitHub origin, when one is recorded: publishing does
    /// NOT rewrite a manifest's origin-pin line (`github.com/…`), so when one still references
    /// the origin this says the project keeps tracking the pin until the line is swapped for the
    /// governed reference. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_note: Option<String>,
    /// The publish LANDED but the local governance-transfer rewrite did NOT (a manifest
    /// read/write fault): the truthful receipt half — the manifest still spells the local-path
    /// line, and the next `update` (or a re-run of this publish, which resolves no-change)
    /// converges the rewrite idempotently. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_pending: Option<String>,
    /// The manifest's local-path line was REMOVED while this publish ran (a concurrent
    /// `topos remove` completed between the describe and the locked rewrite): NO workspace row
    /// was written — a completed removal is never silently undone. The publish stands
    /// catalog-side; `topos add <reference>` records the demand deliberately. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_skipped: Option<String>,
    /// The BUNDLE KIND this publish shipped, when it is not the ordinary skill: `"mcp"` for a
    /// server document. It rides the write to the catalog, so the workspace records what the
    /// bundle IS and every receiving machine places it the right way. Absent = a skill.
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whether the shipped bytes came from the MACHINE copy while the command stood in a project
    /// checkout — the cross-scope ship, which `from_placement` names the folder of. False for the
    /// ordinary same-scope publish. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_machine: bool,
    /// The OTHER scope's copy, when it carries edits this publish did not ship. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_scope_draft: Option<ScopeDraft>,
}

/// A bundle's copy in the scope this command did NOT act in — disclosed beside a publish that left
/// it alone, so the edits it holds are never silently invisible. One command shares them, and which
/// one it is follows from `machine`. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ScopeDraft {
    /// The folder that copy stands in, as a person reads it
    /// (`~/.claude/skills/coolify-deploy` · `project/.agents/skills/coolify-deploy`).
    pub folder: String,
    /// Whether it is the MACHINE copy (`false` = a project checkout's) — which decides both the
    /// word the line uses and whether the command that shares it carries `-g`.
    pub machine: bool,
}

/// The disclosure a `publish` attaches when it ADDED the skill to topos before shipping — the auto-add
/// convenience (`publish` accepts an untracked local source and adopts it first). Public disclosure only
/// (the same facts an explicit `topos add` would return); never a secret. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct AddedNote {
    /// The name the skill was added under (what `list` / `diff` / `publish` now resolve it by).
    pub name: String,
    /// The folder the added directory sits in, or `None` when it has no parent. A folder, never an
    /// agent: a skills folder may be read by several agents, and which ones is a separate query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

/// `publish --propose` (opens a PR; uploads a full candidate **without moving `current`**). Returns
/// `NEEDS_REVIEW`. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ProposeData {
    /// `<skill>@<version_id>` of the candidate.
    pub proposal: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Present ONLY when THIS `publish --propose` invocation ADDED the skill to topos first (the auto-add
    /// convenience — a proposal of an untracked local source adopts it before opening the PR). Discloses the
    /// one local `add` the propose folded in; `None` when the skill was already tracked. **INFERRED.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<AddedNote>,
    /// The channel whose placement was WITHHELD by its curated mode on the proposal arm (the
    /// `--to` placement applies when the proposal opens — reach is curation-gated there too).
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_withheld: Option<String>,
    /// The channel named by `--to` that no longer existed at the proposal's write (the
    /// in-transaction refusal — never a silent mint). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_missing: Option<String>,
    /// The GOVERNANCE-TRANSFER receipt half on the PROPOSAL arm: the manifest whose local-path
    /// line this publish rewrote to the governed workspace reference (delivery follows once the
    /// proposal is approved). Absent when no manifest referenced the bundle by path. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The canonical workspace reference the manifest now stores. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The local-path spelling the manifest carried BEFORE the transfer. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<String>,
    /// The proposal OPENED but the local governance-transfer rewrite did NOT (a manifest
    /// read/write fault): the manifest still spells the local-path line; the next `update` (or a
    /// re-run) converges the rewrite idempotently. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_pending: Option<String>,
    /// The manifest's local-path line was REMOVED while this propose ran (a concurrent
    /// `topos remove`): NO workspace row was written — a completed removal is never silently
    /// undone. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_skipped: Option<String>,
    /// The workspace this proposal was opened in, as a person addresses it. Absent when neither
    /// this installation's session nor the best-effort `me` read could name it. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// The paste-able share line (`<address>/skills/<name>`) — a members' deep link to the skill
    /// under proposal. Absent when the workspace address is not known. **INFERRED**
    /// (additive-only). No `undo` rides this shape: a proposal never moved `current`, so there is
    /// no prior state to restore — the author's escape is `review <handle> --withdraw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_line: Option<String>,
    /// The folder these bytes were read from, present when it was a CHOICE — one of several edited
    /// copies (`--dest`), or a copy in a scope other than the one the command stood in. The same
    /// field the direct publish carries: a proposal ships bytes too, so it owes the same answer.
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_placement: Option<String>,
    /// Whether the proposed bytes came from the MACHINE copy while the command stood in a project
    /// checkout — the cross-scope ship, which `from_placement` names the folder of. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_machine: bool,
    /// The OTHER scope's copy, when it carries edits this proposal did not ship. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_scope_draft: Option<ScopeDraft>,
    /// The edited copies in this scope the proposal left alone (populated beside
    /// `from_placement`). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_edited: Vec<String>,
}

/// `revert` (a **forward** git-revert restoring older bytes as a new, higher-generation version —
/// never a pointer rollback, never a delete). `--to` names the GOOD version. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RevertData {
    pub skill_id: String,
    /// The workspace this revert reached, as a person addresses it — the receipt names where the
    /// pointer moved. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// The skill's NAME — the handle humans speak and the TTY success line leads with
    /// (`Reverted <name> …`); the opaque `skill_id` above stays the machine key.
    pub name: String,
    /// The good version named by `--to` (the bytes being restored).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub reverted_to: String,
    /// The new forward-revert commit that carries those bytes.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub new_version_id: String,
    pub current_generation: u64,
    /// The cwd project's `topos.lock` beside this revert: its entry for the bundle ADVANCED to the
    /// forward commit (the checkout runs the restored version at its next update), or was HELD at
    /// the version the manifest pins. Absent when no project lock records the bundle. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_lock: Option<ProjectLock>,
    /// The copy in the scope the command stood in, AFTER the revert: the same row `topos update
    /// <skill>` prints — the converge a landed revert runs on that one bundle, so the folder the
    /// command stood beside holds the restored content (a copy with unpublished edits gets the
    /// update's own draft/conflict handling, said on the row). Absent when the converge could
    /// not run (`copy_fault` says why). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<PullSkill>,
    /// Why the standing scope's copy was NOT converged after the revert landed — the remote half
    /// stands; this names the local fault and the update that finishes the job. The project lock
    /// is not advanced past a copy that does not hold the version. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_fault: Option<String>,
}

/// The cwd project's `topos.lock` beside a pointer move this machine made (a landed publish, a
/// landed revert): which file, which version its entry now records, and whether that entry was
/// HELD rather than moved — the manifest pins the bundle, so the lock keeps the pinned version
/// and the move changes nothing about what the project runs. A reader branches on `held`.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ProjectLock {
    /// The lock file, as a path.
    pub file: String,
    /// The version the entry records now: the moved-to version, or the pinned one it kept.
    pub version: String,
    /// Whether the entry was HELD by a manifest pin instead of moved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
}

/// `revert <skill> --to <good>` (bare, no `--yes`) — the two-phase DESCRIBE of the forward move: what
/// moves, the generation, and whether good's bytes already equal current's (a byte-level no-op). Nothing
/// is written on the describe. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RevertDescribeData {
    pub skill: String,
    pub skill_id: String,
    /// The version `current` holds now (what the forward move restores away from).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub current_version_id: String,
    /// The good version named by `--to` — the bytes the forward move restores.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub reverted_to: String,
    /// The live `current` generation the forward move would advance from.
    pub current_generation: u64,
    /// Whether good's bytes ALREADY equal current's (compared by verified bundle digest, not commit id):
    /// a repeated identical revert is a byte-level no-op that moves nothing.
    pub is_noop: bool,
}

/// `review` (`--approve` / `--reject` a proposal). Approve is a compare-and-set on the base; a stale
/// base returns `CONFLICT`. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ReviewData {
    /// `<skill>@<version_id>` of the reviewed proposal.
    pub proposal: String,
    /// The workspace this verdict reached, as a person addresses it. Absent when no session on
    /// this installation names it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    pub decision: ReviewDecision,
    /// The pointer's new generation when an approval moved `current`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<u64>,
}

/// A review verdict — `approve` promotes, `reject` carries a reason back, `withdraw` is the author
/// retracting their own open proposal. **INFERRED.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
    Withdraw,
}

// =================================================================================================
// INFERRED — the adopted describe/apply payloads (`remove` / `channel` / `protect` / the review
// inbox+describe / `invite`'s read+describe / `update --reset` / `publish`'s describe). Each rides the
// two-phase envelope: a bare mutating verb returns the payload under `data.describe` (nothing changed),
// `--yes` returns it as `data` with `applied: true`. Additive-only.
// =================================================================================================

/// `remove` — take skills off THIS device. A followed skill becomes a per-device exclusion (other
/// devices keep receiving it); an untracked local copy (or a never-published tracked one) is deleted
/// permanently. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoveData {
    pub items: Vec<RemoveItem>,
    /// `true` on an apply (immediate for a followed clean skill, or `--yes`), `false` on the
    /// describe (nothing changed yet).
    pub applied: bool,
    /// APPLY receipts: the literal inverse command (paste-ready argv) — the `topos add <reference>`
    /// that re-adds the manifest row this removal edited away. Empty when nothing is undoable (a
    /// permanent delete, or a row no single command re-spells) or on a describe. **INFERRED**
    /// (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undo: Vec<String>,
    /// APPLY receipts of a feed-line drop: the bundles whose placed copies left this machine IN
    /// THIS INVOCATION (the row edit uninstalls eagerly), one entry per bundle — with any edited
    /// copy kept in place. Empty everywhere else. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uninstalled: Vec<UninstalledBundle>,
}

/// One bundle an APPLIED removal uninstalled (or would have, had its copies not been edited).
/// **INFERRED** (additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct UninstalledBundle {
    /// The workspace-qualified display name (`@<ws>/<name>` at the machine's one connected host,
    /// else `<host>/<ws>/<name>`); a non-workspace bundle's plain name.
    pub name: String,
    /// The destinations the copies left — skill folders, or a config-placed bundle's config
    /// files. Display paths, `~`-abbreviated. Empty when every copy was edited and kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<String>,
    /// The catalog bundle kind (`"mcp"` for a config-placed bundle). Absent ⇒ an ordinary skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Locally-edited copies kept in place instead of uninstalled — display paths,
    /// `~`-abbreviated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    /// A NARROWED removal (`-a`/`--dest`): how many destinations the row still names after the
    /// subtraction — the receipt's `— N folders remain` clause (config files for an MCP
    /// bundle). Absent on a whole-row removal, and on a narrow that leaves the row standing for
    /// its DEFAULT reach (there is no count to state). **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
}

/// One skill in a [`RemoveData`]. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RemoveItem {
    pub name: String,
    /// How the removal behaves for this skill.
    pub kind: RemoveKind,
    /// The MANIFEST the removal edited (a `topos.toml` path). Absent when no manifest line was
    /// touched. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The workspace the exclusion is recorded in (a followed skill); absent for a local copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The destination directories cleaned (or, on the describe, that would be cleaned).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dest_dirs: Vec<String>,
    /// The directories the removal LEAVES STANDING — a folder the person adopted in place, which
    /// topos never created and therefore never deletes. Disjoint from [`Self::dest_dirs`], which
    /// names only what is emptied. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept_dirs: Vec<String>,
    /// Whether the sidecar bytes are kept (a manifest edit keeps the tracked bytes as a frozen
    /// copy; an untracked-local delete removes the only copy there is).
    pub bytes_kept: bool,
    /// A removal-specific disclosure (the built-in skill's durable opt-out + its way back). Absent
    /// for ordinary removals. **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How `remove` treats one skill. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum RemoveKind {
    /// The manifest's own include line was deleted — delivery to this scope just ends. **Additive.**
    ManifestRemoved,
    /// The machine-wide FEED line was deleted: the workspace's feed stops delivering here, and the
    /// copies it delivered were uninstalled in the same invocation ([`RemoveData::uninstalled`] —
    /// edited copies kept in place). **Additive.**
    FeedRemoved,
    /// A broader layer still provides the item, so an EXCLUDE line was recorded in the nearest
    /// manifest (the one negative state). **Additive.**
    ManifestExcluded,
    /// A NARROWED removal (`-a`/`--dest`): the named destination(s) were subtracted from the
    /// row's `dest` and that copy uninstalled; the row survives with the rest. **Additive.**
    ManifestNarrowed,
    /// A DETACHED claim (`-a`/`--dest` naming a folder brought in with `add … --as`): the record
    /// stops managing that folder and the bytes stay exactly where they are. The row is touched
    /// only to take back the destination the claim itself put on it. **Additive.**
    ClaimDetached,
    /// An untracked local copy in an agent dir → permanent delete (no other copy exists).
    UntrackedLocal,
    /// A tracked, never-published local skill → permanent delete (the sidecar entry drops too).
    TrackedLocalPermanent,
    /// A tracked local bundle whose bytes live in a folder the person ADOPTED IN PLACE. The topos
    /// record and any config entries it placed retire; the folder itself stays, because topos
    /// never created it ([`RemoveItem::kept_dirs`] names it, and `bytes_kept` is `true`).
    /// **Additive.**
    TrackedLocalRetired,
    /// The built-in `topos` bundle → the durable device opt-out. Not a permanent delete: the bytes
    /// ship inside the binary and `topos add topos` places them again. **Additive.**
    BuiltinOptOut,
}

/// `protect <target> [<level>]` — set a skill's or channel's protection level. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ProtectData {
    pub target: String,
    /// `skill` or `channel`.
    pub kind: String,
    pub workspace_id: String,
    /// The workspace this act reached, as a person addresses it — so a receipt always names it.
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// The level being set (`reviewed` / `curated` / `open`).
    pub level: String,
    /// `true` when the level LOOSENS protection (`open`) — the owner-gated direction.
    pub loosening: bool,
    /// A standing note the describe carries (e.g. "pending proposals survive a loosening").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `true` on the `--yes` apply, `false` on the describe.
    pub applied: bool,
}

/// `review` (bare) — the review inbox/outbox across every enrolled workspace, author-message first.
/// **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ReviewIndexData {
    /// Proposals others opened that you can review (inbox).
    pub inbox: Vec<ReviewIndexEntry>,
    /// Your own open proposals (outbox).
    pub outbox: Vec<ReviewIndexEntry>,
}

/// One proposal in the review inbox/outbox. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ReviewIndexEntry {
    pub workspace_id: String,
    /// The workspace this proposal lives in, as a person addresses it (the inbox groups by it).
    pub workspace: WorkspaceRef,
    pub skill: String,
    /// The review target handle, `<skill>@<version_id>`.
    pub proposal: String,
    pub proposer: String,
    /// The author's message — rendered FIRST.
    pub message: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    pub created_at: String,
    /// Whether `current` has moved past the proposal's base (a stale proposal needs a re-propose).
    pub stale: bool,
}

/// `review <target>` (bare, no verdict) — the target describe: who, what, base, staleness, and the
/// diff against current. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ReviewDescribeData {
    /// The review target handle, `<skill>@<version_id>`.
    pub proposal: String,
    /// The workspace the proposal lives in, as a person addresses it. Absent when no session on
    /// this installation names it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    pub skill: String,
    pub proposer: String,
    pub message: String,
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    pub stale: bool,
    /// Whether the CALLER authored this proposal — when `true` the describe offers `--withdraw` (a
    /// four-eyes author cannot approve their own version) and the renderer says "your proposal".
    pub yours: bool,
    /// The unified diff of the proposal against current (`current..<proposal>`). Under a byte
    /// budget (`--max-bytes`, or the `--json` default cap) it carries only the leading whole-file
    /// sections that fit; `diff_truncated` says so and the `FETCH_FULL_DIFF` next action's argv
    /// re-runs the same diff uncapped through `topos diff`.
    pub diff: String,
    /// `true` when `diff` was byte-capped. Omits when `false` (the prior shape is unchanged).
    /// **INFERRED** (additive).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub diff_truncated: bool,
}

/// `invite` (bare, no emails) — the no-mutation read of the workspace address. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct InviteReadData {
    /// The workspace's JOIN LINK — the URL teammates paste, as the server spells it (a scheme and,
    /// in a multi-workspace deployment, the name segment). Not the `<host>/<name>` address below:
    /// this one is what a person opens.
    pub address: String,
    /// The workspace they would join, as a person addresses it. Absent when this installation
    /// holds no session naming it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// Always `false` — a bare read sends nothing and changes nothing.
    pub changed: bool,
}

/// `invite <email>...` (bare, no `--yes`) — the describe: who gets invited, the optional
/// first-destination hint, and the mailed-link note. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct InviteDescribeData {
    /// The workspace's JOIN LINK, as the server spells it (see [`InviteReadData::address`]).
    pub address: String,
    /// The workspace the invitations would seat people in, as a person addresses it. Absent when
    /// this installation holds no session naming it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// The emails that would be seated (canonical form).
    pub seat: Vec<String>,
    /// The first-destination SKILL hint the invitation would carry (at most one of skill/channel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// The first-destination CHANNEL hint the invitation would carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// `update --reset <skill>` — discard a local draft back to the followed `current` (or an imported
/// skill's last-fetched origin snapshot). The describe LEADS with what is lost. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ResetData {
    pub skill: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// That same workspace as a person addresses it. Absent alongside `workspace_id`, and
    /// whenever no session on this installation names it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// The version the reset lands on (the followed current / the origin snapshot).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub to_version: String,
    /// The unified diff of the draft that would be (describe) / was (apply) discarded.
    pub drop_diff: String,
    /// `true` on the `--yes` apply, `false` on the describe.
    pub applied: bool,
    /// The ONE copy a `-a`/`--dest` selection narrowed this reset to, as a receipt spells the
    /// folder (`project/.agents/skills/coolify-deploy`). Absent = the whole bundle: every copy's
    /// edits. The loss surface must never overstate its reach, so the copy is carried rather than
    /// inferred. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    /// The bundle's OTHER edited copies — the ones this reset leaves untouched, still holding
    /// their edits. Always empty for a whole-bundle reset (it takes them all). **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others_kept: Vec<String>,
    /// The scope this reset ran in — `true` for the machine (`-g`). Every command the receipt
    /// offers is spelled for it. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub global: bool,
    /// The folder holding a by-hand merge this reset did NOT delete, as a receipt spells it. A
    /// reset never reads that folder, so it removes it only while it still holds what topos wrote
    /// there; anything else is left where the person can see it. Absent on the describe and
    /// whenever nothing survived. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hand_merge: Option<String>,
    /// Where this reset leaves a merge that had STOPPED on the bundle. Absent when none had —
    /// the ordinary reset says nothing about merges — and absent from every DESCRIBE: where the
    /// merge ends up is settled by the apply, so the preview would be predicting it. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<ResetMergeOutcome>,
}

/// What a `--reset` did to a stopped merge (see [`ResetData::merge`]). A narrowed reset takes ONE
/// copy, and a merge ends only when every copy is settled — so the two are genuinely different
/// answers and a receipt must not print one for the other. **INFERRED** (additive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub enum ResetMergeOutcome {
    /// The merge is STILL stopped: another copy still holds it, and both ways out remain live.
    StillStopped,
    /// The merge is over — this was the last copy holding it, resolved the team's way.
    Concluded,
}

/// `publish` (bare, no `--yes`) — the describe: where it lands, the gate outcome, the share line,
/// and the undo path. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PublishDescribeData {
    pub skill: String,
    pub skill_id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_display_name: Option<String>,
    /// The workspace these bytes would land in, as a person addresses it — how the describe names
    /// the destination. Absent when neither this installation's session nor the server-supplied
    /// address could name it (the copy then falls back to the display name rather than printing a
    /// broken address). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    /// The byte-exact digest of the draft being published.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The channels the reference lands in (`--to`, or `everyone` for a brand-new skill).
    pub placements: Vec<String>,
    /// The gate outcome: an OPEN bundle lands directly; a REVIEWED one becomes a proposal.
    pub gate: PublishGate,
    /// Whether this publish restores an ancestor's bytes (a revert-shaped publish, same gate).
    pub is_revert: bool,
    /// The folder the bytes WOULD be read from, as a person reads it
    /// (`project/.agents/skills/coolify-deploy`), present when it was a CHOICE — one of several
    /// edited copies (`--dest`), or a copy in a scope other than the one the command stood in. A
    /// single edited copy in the standing scope needs no such line: it is the draft, and naming its
    /// folder would say nothing. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_placement: Option<String>,
    /// The other EDITED copies IN THIS SCOPE this publish would leave alone — each keeps its bytes
    /// and becomes an ordinary draft ahead of the version published. Populated by a `--dest`
    /// selection among several edited copies (a cross-scope ship discloses the other scope's copy
    /// through `other_scope_draft` instead). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_edited: Vec<String>,
    /// The paste-able share line (`<address>/skills/<name>`), when the workspace address is known.
    /// A members' deep link — it answers only for people already in the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_line: Option<String>,
    /// The paste-able teammate handoff line (`Ask your agent: …`) — the join instruction that
    /// brings a teammate's machine into the workspace, composed from the workspace's server
    /// origin + address (the same read as `share_line`). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_line: Option<String>,
    /// The undo — the whole `topos revert <name> --to <version>` command that would put the team
    /// back on the version `current` holds now. WITHHELD unless it verifiably restores that state:
    /// a bundle this machine does not follow cannot be reverted from here, and a review-gated
    /// publish never moves `current`, so there would be nothing to put back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<String>,
    /// The origin-demotion disclosure for an imported skill (publishing makes the team copy the source
    /// of truth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_note: Option<String>,
    /// The default placement's mode annotation: present when a brand-new skill's `everyone`
    /// placement will be WITHHELD at the apply (the channel is curated and the caller is a
    /// member) — the publish lands catalog-only and a curator places it afterwards. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_note: Option<String>,
    /// The PREDICTED conflict verdict when the draft's base is BEHIND the last-known observed
    /// `current` (the apply would refuse with CONFLICT — rebase first): a pure in-memory three-way
    /// dry run of the draft onto that current, from already-local bytes only. Absent = unknown
    /// (up to date, or the needed version is not locally held — the describe never adds a network
    /// call for it). **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_preview: Option<MergePreview>,
    /// The PREDICTED governance transfer: the manifest whose local-path line the apply would
    /// rewrite to the governed workspace reference. Absent when no manifest references the bundle
    /// by path. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The canonical workspace reference the manifest would store. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The local-path spelling the manifest carries today (what the transfer would replace).
    /// **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<String>,
    /// The BUNDLE KIND this publish would ship, when it is not the ordinary skill: `"mcp"` for a
    /// server document. Named on the describe because it decides what the workspace records and
    /// how every receiving machine places the bytes. Absent = a skill. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whether the bytes would come from the MACHINE copy while the command stands in a project
    /// checkout — the cross-scope ship, which `from_placement` names the folder of. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_machine: bool,
    /// The OTHER scope's copy, when it carries edits this publish would leave alone. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_scope_draft: Option<ScopeDraft>,
    /// The whole `topos diff …` command that reads the exact copy this publish would ship —
    /// spelled for the folder and the scope that resolved, so it is runnable as printed. Absent on
    /// a GENESIS publish: there is no earlier version for a diff to be against, so the command
    /// would print nothing. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// Present when the copy EQUALS an older version than the workspace's live `current`: the
    /// apply would carry that version's content forward as a new version, and the preview says
    /// which version stands as `current` now. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub republish: Option<Republish>,
    /// What the draft changes against the live `current`, counted per file. A GENESIS publish
    /// counts every file as added (there is no current to count against). **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<ChangeSummary>,
    /// The channels the published version reaches: the `--to` target (or `everyone` for a
    /// brand-new bundle) plus the channels that already carry the bundle. Empty when the bundle
    /// sits in the catalog alone. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lands_in: Vec<String>,
}

/// A publish that ships an OLDER version's content forward: the copy equalled `copy_version_id`,
/// the workspace's live `current` was `current_version_id`, and the apply mints a new version
/// parented on that current with the copy's bytes. Re-publishing old content is an ordinary act
/// (a commit can restate an earlier tree); what the reader is owed is which version `current` is.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct Republish {
    /// The workspace's live `current` at the time of the publish.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub current_version_id: String,
    /// That version's history line (`topos: revert`, or the message its publish carried), when
    /// the server named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_message: Option<String>,
    /// The older version the copy's bytes equal — the version being carried forward.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub copy_version_id: String,
    /// The version the forward publish minted — on the RECEIPT only. A preview does not predict
    /// it: the id folds in the message and the parent the apply will see, and a preview that
    /// named one landed beside a receipt naming another the moment a `-m` was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub new_version_id: Option<String>,
}

/// A publish preview's per-file count of what the draft changes against the live `current`.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ChangeSummary {
    /// Every file that differs: added, removed, or changed in content or mode.
    pub files: u64,
    /// Files the draft adds.
    pub added: u64,
    /// Files the draft removes.
    pub removed: u64,
    /// Files that are executable in the draft and were not (added executable, or the bit flipped).
    pub executable: u64,
}

/// `publish` when the copy already matches the published `current`: a SUCCESS with nothing to ship
/// — not a failure, and never a reason for an agent to retry. **INFERRED.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PublishNoChangesData {
    /// The branch key: this publish shipped nothing because there was nothing to ship.
    pub result: PublishResult,
    /// The skill the command named.
    pub skill: String,
    /// The OTHER scope's copy, when THAT is where the edits are — the pointer across, so a
    /// "nothing to ship" answer never hides a draft one command away. **INFERRED**
    /// (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_scope_draft: Option<ScopeDraft>,
}

/// Why a `publish` moved nothing. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum PublishResult {
    /// The scanned copy is byte-identical to the version the workspace already serves.
    NoChanges,
}

/// The gate a `publish` describe predicts. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PublishGate {
    /// The bundle is open — publishing moves `current` directly.
    Lands,
    /// The bundle is reviewed — publishing opens a proposal instead of moving `current`.
    Proposal,
}

// =================================================================================================
// `status` (the offline health panel).
//
// Like `list`'s, this payload was REDESIGNED WHOLESALE pre-1.0 in the split that made `status` the
// health panel: the resolved-item table, the profile counts, and the flat regime list are gone,
// replaced by per-scope bodies carrying attention counts. THIS shape is the stable one from here,
// additive-only.
// =================================================================================================

/// `status` result — the health panel: the binary version, the server + signed-in state, the
/// sessions, the auto-update trigger states, and per shown SCOPE what governs it and what needs
/// attention. NO skill inventory — that is `list`'s. Computed ENTIRELY from local state (no
/// network). **The stable shape** (see the section note: it replaced the pre-1.0 item table
/// outright); additive-only from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct StatusData {
    /// The `topos` binary's own version.
    pub version: String,
    /// The server base URL the sessions dial, when logged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Whether a LIVE (non-ended) session's credential is stored — the signed-in state.
    pub signed_in: bool,
    /// This installation's SESSIONS — one per logged-into workspace (the session model; empty =
    /// logged into nothing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<StatusSession>,
    /// Per-agent auto-update trigger state, probed READ-ONLY over the picked agents (nothing is
    /// registered or repaired by `status`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<StatusTrigger>,
    /// The scope bodies shown in full (here-scope; both under `--all`; machine under `-g` and
    /// outside a project).
    pub scopes: Vec<StatusScope>,
    /// The machine scope's one-line summary with ITS pending counts, present only when the
    /// machine body is NOT shown in full (the default view inside a project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_summary: Option<StatusScopeSummary>,
    /// The last auto-update check of every external source the shown scopes name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forge: Vec<ForgeSource>,
    /// The agents picked where the panel stands (the project's effective pick, or the machine's
    /// under `-g`), as the pick file spells them. Empty = no pick yet. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Where `agents` comes from: `"project"`, `"machine"`, or `"legacy"` (the hooks an earlier
    /// build registered, not yet recorded as a pick). Absent with no pick. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_source: Option<String>,
    /// The pick file `agents` was read from, as a person reads it. Absent with no pick or a
    /// legacy record. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_path: Option<String>,
}

/// One scope body of a [`StatusData`]: what governs it, the per-workspace regimes (machine
/// scope), the disclosure notes, and the attention counts. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct StatusScope {
    /// `"project"` or `"machine"`.
    pub scope: String,
    /// The governing manifest file; `None` = no machine-wide file — nothing is demanded
    /// machine-wide (`topos login` writes a feed row on this machine's first connection to a
    /// workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// Each connected workspace's REGIME on this machine (machine scope only): adopting its whole
    /// feed, explicit line-by-line control, or feed rows withheld by a hand-written file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regimes: Vec<StatusRegime>,
    /// The disclosure lines — plain sentences no count can carry: the loud no-feed-row note,
    /// redundant rows, inert `"off"` switches, set-collision winners, declined-but-delivered
    /// notes, feeds that have not delivered yet, cross-scope version splits. Rendered verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What needs attention in this scope, one count per kind — each with the exact command that
    /// resolves it. Only non-zero counts appear; empty = nothing pending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<AttentionCount>,
    /// Whether the BUILT-IN `topos` bundle has copies on this machine (machine scope only). It
    /// ships with the binary and no manifest row asks for it, so a panel reading only manifest
    /// rows said nothing was demanded machine-wide while `topos list -g` showed the bundle right
    /// there. **Additive.**
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin_in_place: bool,
}

/// One attention count in a [`StatusScope`] / [`StatusScopeSummary`]. **INFERRED**
/// (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AttentionCount {
    /// `"updates-pending"` (applied version behind the last-known served target),
    /// `"assignments-not-applied"` (delivered but never applied here), or `"drafts-ahead"`
    /// (local edits ahead of the applied version).
    pub kind: String,
    /// How many rows the kind counts.
    pub count: u64,
    /// The exact command that resolves it.
    pub command: String,
}

/// The machine scope's one-line summary in a [`StatusData`] whose body shows only the project
/// scope. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct StatusScopeSummary {
    /// The machine scope's attention counts (empty = nothing pending).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<AttentionCount>,
    /// The exact command that expands the summary into the full body.
    pub command: String,
}

/// One workspace's regime line in a [`StatusScope`]. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct StatusRegime {
    /// The workspace this line is about, as a person addresses it.
    pub workspace: WorkspaceRef,
    /// The regime sentence (e.g. "adopting all assigned, 2 off" or "explicit: 3 bundles; 1
    /// assigned not adopted here").
    pub regime: String,
}

/// One session in a [`StatusData`] — this installation logged into one workspace. **INFERRED**
/// (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct StatusSession {
    pub workspace_id: String,
    /// The workspace this session is in, as a person addresses it — what you logged in by.
    pub workspace: WorkspaceRef,
    pub display_name: String,
    /// The session's status, when it is NOT plainly active: `"pending"` (awaiting an owner's
    /// approval — delivery starts automatically once approved) or `"ended"` (revoked or gone —
    /// `topos login <address>` starts a fresh one). Absent = active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_status: Option<String>,
}

/// A resolved line's state — the offline resolution's shared vocabulary ([`ListDetail::state`]
/// and the internal per-scope rows). **INFERRED value set** (additive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum StatusItemState {
    /// The bytes are applied and current against the last-known delivered target (see
    /// `applied_as_of` — the offline cache's stamp, not a live confirmation).
    Applied,
    /// A newer target is known than what is applied — the next `update` lands it.
    Behind,
    /// Local edits sit ahead of the applied version (a draft).
    LocalEdits,
    /// A merge is undecided here: the team published a version that changes lines this person also
    /// changed, so the folders still hold THEIR version, publishing is refused, and the two exits
    /// (`update <name> --keep-mine` / `--reset`) are the only ways forward. The row's
    /// version/digest name what the folders hold, never the team's.
    Blocked,
    /// A machine-local `"off"` row in the global manifest withholds this bundle from the feed
    /// here (the everywhere-path is the web decline).
    Off,
    /// Referenced here but NOT deliverable with your current access — phrased from LOCAL
    /// knowledge only (no live session for its workspace); never from server confirmation.
    NotAvailable,
    /// The session that would deliver it is pending an owner's approval.
    PendingSession,
    /// An adopted feed this installation has never had a delivery from: the next `update` performs
    /// the first exchange. What that exchange brings — including nothing — is not knowable here,
    /// so the line promises an exchange, never an apply.
    NoDeliveryYet,
    /// Not applied here yet — `topos update` applies it (or the state is not determinable
    /// offline).
    Unknown,
}

/// One picked agent's auto-update trigger presence in a [`StatusData`] — a read-only probe of
/// the same artifact the arming sweep manages. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct StatusTrigger {
    /// The registry slug.
    pub agent: String,
    /// Provable presence of the topos trigger ARTIFACT right now — a footprint read, which is
    /// why every surface built on it says "registered" rather than "armed": the artifact stands,
    /// and nothing here watched it fire. Absent = unknowable without a live probe `status`
    /// refuses to run (a scheduler that must be dialed to answer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed: Option<bool>,
    /// A short honesty note (e.g. why `armed` is unknown), when one is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// How long ago this agent's hook last RAN a sweep on this machine (milliseconds), when one
    /// has ever been observed — the evidence that turns "registered" into "seen working". Only
    /// hooks that name their caller can leave it (`--hook`/`--from`), so absence never proves a
    /// trigger dead. **INFERRED** (additive-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_age_ms: Option<i64>,
}

// =================================================================================================
// `auth status` (the per-session access panel — side-effect-free).
// =================================================================================================

/// `topos workspace list` — the workspaces this machine is signed into, and which one is the
/// machine default (the one ambient commands act on outside a project).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct WorkspaceListData {
    /// One row per signed-in workspace, sorted by address.
    pub workspaces: Vec<WorkspaceListRow>,
    /// The machine default's address (`<host>/<name>`), when one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// One `workspace list` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct WorkspaceListRow {
    /// The address: `<host>/<name>`.
    pub address: String,
    /// The same two halves apart — the ONE shape every other verb names a workspace with.
    pub workspace: WorkspaceRef,
    /// The workspace's display name, for humans.
    pub display_name: String,
    /// `active` / `pending` / `ended`.
    pub status: String,
    /// Whether this is the machine default.
    pub default: bool,
}

/// `topos workspace use` — the machine default changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct WorkspaceUseData {
    /// The new default's address.
    pub address: String,
    /// The new default, as a person addresses it.
    pub workspace: WorkspaceRef,
    /// The previous default, when one stood.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

/// `auth status` result — per-SESSION access health (each probed under that session's own
/// credential), the active agent's trigger health, and the reporting posture. **INFERRED**
/// (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AuthStatusData {
    /// The server base of the first live session, for orientation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The principal the first successful probe answered with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// Whether any live session exists (the signed-in state).
    pub signed_in: bool,
    /// One row per session this installation holds.
    pub workspaces: Vec<AuthWorkspaceStatus>,
    /// Whether the session-start auto-update trigger is armed in the active harness's config.
    pub hook_armed: bool,
    /// Per-workspace reporting posture, read from local state (no network).
    pub reporting: Vec<AuthReportingStatus>,
}

/// One session's access health in an [`AuthStatusData`]. **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AuthWorkspaceStatus {
    pub workspace_id: String,
    /// The workspace this session is in, as a person addresses it.
    pub workspace: WorkspaceRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether this session's credential is stored locally (always true for a live session row).
    pub credential: bool,
    /// The probe verdict: `healthy` / `pending — awaiting owner approval` / `no access — ended,
    /// removed, or gone` / `unreachable` / `ended`.
    pub health: String,
    /// The role the probe answered (healthy only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// One workspace's reporting posture in an [`AuthStatusData`] (from the local sync-status
/// document). **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct AuthReportingStatus {
    pub workspace_id: String,
    /// The workspace this posture is about, as a person addresses it. Absent when no session on
    /// this installation names it (the local document keys by id alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report_at: Option<i64>,
    pub staleness_window_ms: u64,
    /// Whether the last delivery is older than the window (the sessions page shows the same).
    pub stale: bool,
}

// =================================================================================================
// `uninstall` (the two-phase teardown: what would go, then what went).
// =================================================================================================

/// The bare `uninstall` DESCRIBE — what `--yes` would remove (nothing has changed).
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct UninstallDescribe {
    /// Every artifact the auto-update-trigger scrub would REACH, across EVERY harness the apply
    /// touches — not just the active one, and not only the ones that leave a file behind: a path
    /// where the trigger is one, and the named registration where it lives in the harness's own
    /// program (empty = nothing anywhere). Rendered rows, in the disclosure's own order and
    /// wording — the preview says what the apply will attempt, so a person reads the same sentence
    /// the receipt answers for.
    pub trigger_artifacts: Vec<String>,
    /// The `~/.topos/` sidecar tree that would be deleted (the signed-in credential lives inside it).
    pub sidecar_path: String,
    /// Whether the sidecar tree currently exists (a fresh/already-removed install has none).
    pub sidecar_present: bool,
    /// The running binary's own path — NOT deleted; disclosed so the human can remove it themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// The BUILT-IN `topos` skill's placed copies — topos-authored artifacts (like the hook entry),
    /// so they go with the teardown; YOUR skill files still stay untouched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_dirs: Vec<String>,
    /// The MCP config files the apply will take topos-placed server entries OUT of. Those entries
    /// point live agents at servers, and the ledger proving they are topos's dies with the sidecar
    /// — so the preview names every file it will edit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_files: Vec<String>,
    /// The MCP config files holding a topos entry someone edited by hand. Those are LEFT — the
    /// never-clobber rule does not lapse because the command is a teardown — and the preview says
    /// so, so a person is not surprised by a leftover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_drifted: Vec<String>,
    /// The auto-update hook files the checkout the command ran in holds (the four project-capable
    /// harnesses', each named only while it is provably topos's). The apply scrubs the topos entry
    /// out of each, the way it scrubs the machine's; other checkouts keep theirs. Empty outside a
    /// project. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_hook_files: Vec<String>,
}

/// The applied `uninstall` — what was removed. On a teardown that FAILED partway this is the
/// receipt of the work that actually landed, spelled exactly as the success receipt spells it.
/// **INFERRED** (additive-only).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct UninstallApplied {
    /// The ACTIVE harness's trigger scrub report (surfaced honestly — `Inactive` when nothing was
    /// armed). Absent on a teardown that failed before the scrubs ran: the trigger is still armed,
    /// and a report saying otherwise would be the falsehood this field exists to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<crate::TriggerReport>,
    /// The breadth scrub's outcomes — other agents whose trigger the sweep removed (or could not,
    /// disclosed); clean no-ops stay off the receipt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::TriggerReport>,
    /// The scrub of the hooks the checkout the command ran in held (`project_hook_files` on the
    /// describe): one row per file that held a topos entry, removed or (disclosed) not. Empty
    /// outside a project, and when no project hook was topos's. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_hooks: Vec<crate::TriggerReport>,
    /// Whether the `~/.topos/` sidecar tree was deleted (false = there was nothing to delete, or
    /// the teardown never got that far).
    pub sidecar_removed: bool,
    /// The built-in `topos` skill's placed copies that were removed (topos-authored artifacts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_dirs: Vec<String>,
    /// The MCP config files topos-placed server entries were removed from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_files: Vec<String>,
    /// The MCP config files whose hand-edited topos entries were LEFT in place.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_drifted: Vec<String>,
    /// The running binary's own path — left in place; the human removes it with their installer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_pull_shape_round_trips() {
        let data = PullData {
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                workspace_id: Some("w_acme".to_owned()),
                observed: 42,
                applied: 42,
                action: PullAction::UpToDate,
                merge: None,
                synced_placements: None,
                scope: None,
                destinations: Vec::new(),
                kept: Vec::new(),
                display: None,
                note: None,
                harnesses: Vec::new(),
                kind: None,
                draft: false,
                narrowed: None,
            }],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            triggers: Vec::new(),
            scope: None,
        };
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["skills"][0]["action"], "up_to_date");
        assert_eq!(v["skills"][0]["workspace_id"], "w_acme");
        assert_eq!(v["proposals_awaiting"], 0);
        // The additive fields OMIT when empty (an older consumer sees the unchanged pinned shape)
        // — `draft` included: a bundle with nothing unshared says nothing about drafts at all.
        assert!(v.get("notices").is_none() && v.get("sync").is_none());
        assert!(v["skills"][0].get("draft").is_none());
        let back: PullData = serde_json::from_value(v).unwrap();
        assert_eq!(back.skills[0].action, PullAction::UpToDate);
    }

    /// The catch-up outcome is `updated` on the wire — the same word the terminal prints for it.
    /// One outcome cannot carry two names: an agent reading the envelope and a person reading the
    /// receipt have to be able to talk about the same run.
    #[test]
    fn the_catch_up_action_is_spelled_updated_on_the_wire() {
        assert_eq!(
            serde_json::to_value(PullAction::Refreshed).unwrap(),
            serde_json::json!("updated")
        );
        let back: PullAction = serde_json::from_value(serde_json::json!("updated")).unwrap();
        assert_eq!(back, PullAction::Refreshed);
    }

    #[test]
    fn publish_data_carries_the_move_and_omits_an_absent_added_note() {
        let done = PublishData {
            project_lock: None,
            republish: None,
            manifest: None,
            reference: None,
            converted_from: None,
            skill_id: "topos_t00".to_owned(),
            name: "pr-describe".to_owned(),
            version_id: "a".repeat(64),
            bundle_digest: "c".repeat(64),
            current_generation: 1,
            added: None,
            placement_withheld: None,
            placement_missing: None,
            invite_line: None,
            origin_note: None,
            rewrite_pending: None,
            rewrite_skipped: None,
            kind: None,
            workspace: None,
            share_line: None,
            undo: None,
            from_placement: None,
            from_machine: false,
            other_scope_draft: None,
            other_edited: Vec::new(),
        };
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["version_id"], "a".repeat(64));
        assert_eq!(v["current_generation"], 1);
        assert!(v.get("added").is_none(), "an absent added note omits");
        assert!(
            v.get("placement_withheld").is_none(),
            "an absent withheld placement omits"
        );
        assert!(
            v.get("invite_line").is_none(),
            "an absent teammate handoff omits"
        );
        // The receipt's address-derived lines and its undo are all additive: each omits when the
        // producer withheld it (a failed `me` read, or an undo that would not restore the state).
        assert!(
            v.get("workspace").is_none() && v.get("share_line").is_none(),
            "the address-derived lines omit when the address is unknown"
        );
        assert!(v.get("undo").is_none(), "a withheld undo omits");
        let back: PublishData = serde_json::from_value(v).unwrap();
        assert_eq!(back.bundle_digest, "c".repeat(64));
    }

    #[test]
    fn additive_budget_and_preview_fields_omit_when_absent() {
        // An uncapped diff serializes byte-identically to the pre-budget shape: no `truncated`, no
        // `files`.
        let diff = DiffData {
            source: DiffSource::Local,
            version_id: "a".repeat(64),
            bundle_digest: "b".repeat(64),
            diff: String::new(),
            truncated: false,
            files: Vec::new(),
            dest: None,
            skill: None,
        };
        let v = serde_json::to_value(&diff).unwrap();
        assert!(v.get("truncated").is_none() && v.get("files").is_none());
        // The copy the local side was read from is named only where more than one exists — a
        // single-copy diff keeps the exact prior shape.
        assert!(v.get("dest").is_none() && v.get("skill").is_none());

        // An unpaged log/list likewise.
        let log = LogData::default();
        let v = serde_json::to_value(&log).unwrap();
        assert!(v.get("truncated").is_none() && v.get("total").is_none());
        let list = ListData::default();
        let v = serde_json::to_value(&list).unwrap();
        assert!(v.get("truncated").is_none());

        // A capped diff carries the per-file rows + the flag; the preview verdict is snake_case.
        let capped = DiffData {
            truncated: true,
            files: vec![DiffPatchInfo {
                path: "SKILL.md".to_owned(),
                patch_omitted: true,
                patch_bytes: 9,
            }],
            ..diff
        };
        let v = serde_json::to_value(&capped).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["files"][0]["patch_omitted"], true);
        assert_eq!(
            serde_json::to_string(&MergePreviewVerdict::Conflicted).unwrap(),
            "\"conflicted\""
        );
    }

    #[test]
    fn diff_source_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&DiffSource::Plane).unwrap(),
            "\"plane\""
        );
        assert_eq!(
            serde_json::to_string(&DiffSource::Local).unwrap(),
            "\"local\""
        );
    }
}
