//! The harness registry FILE — its grammar, its parser, its fences, and the three-level resolution
//! [`super::known_harnesses`] runs once per process.
//!
//! The table is DATA. One canonical file (`crates/topos-harness/registry.toml`) is compiled into
//! the binary with `include_str!`, served byte-identically by the web app, and re-read from disk at
//! startup so a machine can carry a NEWER table than the build it is running:
//!
//! 1. **the local override** — `~/.topos/harness-registry/override.toml`, always wins when it is
//!    present and valid (the escape hatch for a person whose agent moved its dirs yesterday);
//! 2. **the downloaded copy** — `~/.topos/harness-registry/registry.toml`, used only when its
//!    [`RegistryVersion`] is strictly NEWER than the bundled one and this build's engine supports
//!    it (a downgrade after a `self-update` is not an update);
//! 3. **the bundled table** — the bytes this binary was built with, and the floor nothing can take
//!    away.
//!
//! Every failure downgrades exactly ONE level and says why on stderr, once. There is no hot reload:
//! a CLI process is one command long, so the next process picks up whatever the refresher wrote.
//!
//! Two things are deliberately NOT said here. A DOWNLOADED file's skipped rows are the refresher's
//! line to say, because the served table naming a harness this build does not know yet is the
//! system working — one warning as it lands, not one on every command until the next
//! `self-update`. And the whole machine-local resolution is switched off by
//! [`BUNDLED_ONLY_ENV`]`=bundled`, which the workspace's `.cargo/config.toml` sets for everything
//! cargo runs: a gate, a shape pin, or a suite must answer for the COMMIT under test, never for
//! whatever the developer running it happens to keep in `~/.topos/harness-registry/`. An installed
//! binary sets no such variable and resolves all three levels as usual.
//!
//! ## What a downloaded file may and may not do
//!
//! The file is the WHOLE table (a row is the unit of change, never a field patch), but a downloaded
//! or override file may only REFINE slugs this build already knows: a row whose slug is not in the
//! BUNDLED set is warned about and skipped, so downstream data can never introduce a write target
//! the binary has no compiled trigger, adapter, or id for. Its dirs are re-validated too —
//! relative, `/`-separated, no `..`, no empty or `.` components, a conservative charset, and no
//! absolute root at all (the grammar spells one, and only the bundled table may carry it — but
//! no bundled row does, because the SERVED copy is those same bytes and every client reads them
//! under exactly these fences; `the_bundled_table_is_landable_as_a_downloaded_copy` is the gate).
//! A file that names no known harness is refused rather than believed: an empty table is
//! indistinguishable from a machine with no agents on it.
//!
//! The consequence worth stating plainly: a downloaded table can SHRINK (a row it omits stops being
//! known). That is the deliberate cost of "the file is the table" — a delivery decision, not a
//! privilege escalation, and the bundled floor returns the moment the copy is deleted.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use toml_edit::{DocumentMut, Item, TableLike};

use super::{DirSpec, KnownHarness, McpBridge, McpConflictPath, McpSurface, McpSurfaces, Root};
use crate::coverage::SharedDirSupport;
use crate::mcp::descriptor::{EnvRef, McpDialect};

/// The GRAMMAR version this build reads. A file naming any other number is refused whole — the
/// shape of the rows is what changed, and guessing at a shape is how a table starts writing to the
/// wrong directory.
pub const SCHEMA_VERSION: u32 = 1;

/// The ENGINE version this build provides: the ceiling on what a file may demand through its
/// `min_engine_version`. A file that needs more is refused (and the level below it answers) — it is
/// saying, honestly, that it carries semantics this binary does not implement.
///
/// 2 — the `[[harness.mcp.conflict_paths]]` sub-table (a row may name files a harness READS
/// servers from that topos never writes: a build that cannot read the column would place entries
/// on top of servers the agent already has and never say so) AND the MCP capability columns
/// `remote` / `stdio` / `env_ref` beside the fenced `[mcp_bridge]` pin (a build that cannot read
/// THOSE would dial an address into a harness that cannot dial, or run nothing at all where a
/// bridge was meant to). Both are exactly the refusal `min_engine_version` exists for.
///
/// 3 — the `appSupport` root ([`Root::AppSupport`], the platform's own application-config dir) and
/// the MCP dialects spoken by the agents that keep their servers there. A build that knows neither
/// would refuse the file one row at a time with a puzzle of a message; the version says the same
/// thing once, quietly, and the bundled table answers instead.
pub const REGISTRY_ENGINE_VERSION: u32 = 3;

/// The widest registry file anything here will read, fetch, or accept from disk. The real table is
/// a few tens of kilobytes; this is the "no surprises" ceiling on a lane that runs unattended.
pub const MAX_REGISTRY_BYTES: usize = 256 * 1024;

/// The bundled table — the ONE canonical file, compiled in. The web app serves these exact bytes.
const BUNDLED: &str = include_str!("../../registry.toml");

/// The directory both machine-local registry files live in, under a `~/.topos` home.
fn registry_dir(topos_home: &Path) -> PathBuf {
    topos_home.join("harness-registry")
}

/// The downloaded table under a `~/.topos` home — what the refresher writes and level 2 reads.
/// Parameterized so the refresher and the loader can never disagree about where the file is.
#[must_use]
pub fn cache_path(topos_home: &Path) -> PathBuf {
    registry_dir(topos_home).join("registry.toml")
}

/// The local OVERRIDE under a `~/.topos` home (level 1) — never written by topos, only read.
#[must_use]
pub fn override_path(topos_home: &Path) -> PathBuf {
    registry_dir(topos_home).join("override.toml")
}

/// `$TOPOS_HOME`, else `$HOME/.topos` — the same resolution the client's composition root makes,
/// restated here because the table is loaded before any of that machinery exists.
fn sidecar_home() -> PathBuf {
    super::env_override("TOPOS_HOME").unwrap_or_else(|| super::real_home().join(".topos"))
}

/// The knob that pins resolution to the table BUNDLED in this binary: `TOPOS_HARNESS_REGISTRY`,
/// whose one honored value is [`BUNDLED_ONLY`]. Anything else (including nothing) leaves the three
/// levels resolving normally.
///
/// It exists for a checkout, not for a machine. The workspace's `.cargo/config.toml` sets it under
/// `[env]` — the SQLX_OFFLINE precedent — so every binary cargo builds and runs here reads this
/// commit's table: without it, the first data-only version bump makes a dogfooding developer's own
/// `~/.topos/harness-registry/registry.toml` the table their `cargo test` reads, and shape pins
/// start failing on a property of the laptop.
pub const BUNDLED_ONLY_ENV: &str = "TOPOS_HARNESS_REGISTRY";

/// The one value [`BUNDLED_ONLY_ENV`] is read for.
pub const BUNDLED_ONLY: &str = "bundled";

/// Whether this process was told to read the bundled table and nothing else.
fn bundled_only() -> bool {
    super::env_override(BUNDLED_ONLY_ENV).is_some_and(|v| v.to_str() == Some(BUNDLED_ONLY))
}

// =================================================================================================
// The refusal
// =================================================================================================

/// Why a registry file was refused. The message always names WHAT was refused — the key, the row,
/// the value — because the only reader is a person deciding whether their own file is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError(String);

impl RegistryError {
    /// The human-readable refusal.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RegistryError {}

fn refuse<T>(what: impl fmt::Display) -> Result<T, RegistryError> {
    Err(RegistryError(what.to_string()))
}

// =================================================================================================
// The version
// =================================================================================================

/// A registry file's CONTENT version: dotted numeric (`2026.8.11.1`), bumped by hand on every edit
/// and compared segment by segment. A segment either side runs out of counts as zero, so `1.2` and
/// `1.2.0` are one version and lengthening the scheme is never silently a downgrade.
#[derive(Debug, Clone, Eq)]
pub struct RegistryVersion(String);

impl RegistryVersion {
    /// Parse a dotted-numeric version, naming what was refused.
    ///
    /// # Errors
    /// An empty string, an empty segment, a non-digit segment, or a segment too large for `u64`.
    pub fn parse(raw: &str) -> Result<Self, RegistryError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return refuse(
                "version: empty (expected a dotted-numeric version like \"2026.8.11.1\")",
            );
        }
        for segment in trimmed.split('.') {
            if segment.is_empty() {
                return refuse(format!("version {trimmed:?}: an empty segment"));
            }
            if !segment.chars().all(|c| c.is_ascii_digit()) {
                return refuse(format!(
                    "version {trimmed:?}: segment {segment:?} is not a number"
                ));
            }
            if segment.parse::<u64>().is_err() {
                return refuse(format!(
                    "version {trimmed:?}: segment {segment:?} is too large"
                ));
            }
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// The version as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RegistryVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Ord for RegistryVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut mine = self.0.split('.');
        let mut theirs = other.0.split('.');
        loop {
            let (a, b) = (mine.next(), theirs.next());
            if a.is_none() && b.is_none() {
                return Ordering::Equal;
            }
            let a = a.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let b = b.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                verdict => return verdict,
            }
        }
    }
}

impl PartialOrd for RegistryVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RegistryVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

// =================================================================================================
// The parse product
// =================================================================================================

/// Where a registry file came from — which fences apply to it. The bundled table is the trusted
/// one: it DEFINES the known slugs and is the only level allowed to name an absolute root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The bytes compiled into this binary.
    Bundled,
    /// The downloaded table at `~/.topos/harness-registry/registry.toml`.
    Downloaded,
    /// The person's own `~/.topos/harness-registry/override.toml`.
    Override,
}

impl Origin {
    /// Whether this level is fenced (everything but the bundled table).
    const fn fenced(self) -> bool {
        !matches!(self, Origin::Bundled)
    }

    /// How the level is named in a refusal or a warning.
    const fn label(self) -> &'static str {
        match self {
            Origin::Bundled => "the harness registry built into this topos",
            Origin::Downloaded => "the downloaded harness registry",
            Origin::Override => "the harness registry override",
        }
    }
}

/// A whole registry file, parsed and fenced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRegistry {
    version: RegistryVersion,
    rows: Vec<HarnessRow>,
    warnings: Vec<String>,
    bridge: Option<OwnedBridge>,
}

impl ParsedRegistry {
    /// The file's content version.
    #[must_use]
    pub fn version(&self) -> &RegistryVersion {
        &self.version
    }

    /// How many harness rows survived the fences.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Whatever the fences said about rows they skipped (never a reason to refuse the file).
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// One parsed row, OWNED — the shape the parser produces before the table is leaked into the
/// `&'static` slice every accessor hands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessRow {
    slug: String,
    display_name: String,
    user_dirs: Vec<OwnedDir>,
    project_dir: String,
    detect_dirs: Vec<OwnedDir>,
    mcp: Option<OwnedMcp>,
    shared_dir: SharedDirSupport,
}

/// A [`DirSpec`] before it is leaked: the resolution root + its `/`-separated suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedDir {
    root: Root,
    suffix: String,
}

impl OwnedDir {
    /// The canonical raw spec string this dir renders as — [`DirSpec::raw`] before the leak. The
    /// round-trip assertions are its one reader; production compares leaked rows.
    #[cfg(test)]
    fn raw(&self) -> String {
        let tag = self.root.tag();
        match (tag.is_empty(), self.suffix.is_empty()) {
            (true, true) => "/".to_owned(),
            (true, false) => format!("/{}", self.suffix),
            (false, true) => tag.to_owned(),
            (false, false) => format!("{tag}/{}", self.suffix),
        }
    }
}

/// One row's MCP surfaces before they are leaked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedMcp {
    user: Option<(OwnedDir, McpDialect)>,
    project: Option<(String, McpDialect)>,
    reload_note: String,
    conflict_paths: Vec<OwnedConflictPath>,
    remote: bool,
    stdio: bool,
    env_ref: EnvRef,
}

/// The file's bridge pin before it is leaked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedBridge {
    package: String,
    version: String,
}

/// One read-only conflict file before it is leaked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedConflictPath {
    file: OwnedDir,
    dialect: McpDialect,
    selector: String,
}

// =================================================================================================
// The parser — the inverse of `DirSpec::raw()`, plus the row grammar
// =================================================================================================

/// Parse a whole registry file, applying the fences [`Origin`] selects.
///
/// # Errors
/// [`RegistryError`] naming exactly what was refused: invalid TOML, a wrong `schema_version`, a
/// `min_engine_version` past this build, a malformed version, a missing or mistyped key, an unknown
/// root tag or dialect, a path that fails validation, a duplicate slug, or a file that names no
/// known harness at all.
pub fn parse_registry(text: &str, origin: Origin) -> Result<ParsedRegistry, RegistryError> {
    if text.len() > MAX_REGISTRY_BYTES {
        return refuse(format!(
            "{}: {} bytes is past the {MAX_REGISTRY_BYTES}-byte ceiling",
            origin.label(),
            text.len()
        ));
    }
    let doc = match text.parse::<DocumentMut>() {
        Ok(doc) => doc,
        Err(e) => return refuse(format!("{}: not valid TOML — {e}", origin.label())),
    };
    let root: &dyn TableLike = doc.as_table();

    let schema = int_at(root, "schema_version")?;
    if schema != u64::from(SCHEMA_VERSION) {
        return refuse(format!(
            "schema_version {schema}: this topos reads harness registries at schema_version \
             {SCHEMA_VERSION}"
        ));
    }
    let version = RegistryVersion::parse(str_at(root, "version")?)?;
    let min_engine = int_at(root, "min_engine_version")?;
    if min_engine > u64::from(REGISTRY_ENGINE_VERSION) {
        return refuse(format!(
            "min_engine_version {min_engine}: this topos provides harness-registry engine \
             {REGISTRY_ENGINE_VERSION} — a newer topos reads this file"
        ));
    }

    let Some(tables) = doc.get("harness").and_then(Item::as_array_of_tables) else {
        return refuse(format!(
            "{}: no `[[harness]]` rows (the table IS the file)",
            origin.label()
        ));
    };

    let known = origin.fenced().then(bundled_slugs);
    let mut rows: Vec<HarnessRow> = Vec::with_capacity(tables.len());
    let mut warnings = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (index, table) in tables.iter().enumerate() {
        let row = parse_row(table, index, origin)?;
        if let Some(known) = known
            && !known.contains(&row.slug)
        {
            warnings.push(format!(
                "topos: {} names the agent {:?}, which this topos does not know — that row was \
                 skipped",
                origin.label(),
                row.slug
            ));
            continue;
        }
        if !seen.insert(row.slug.clone()) {
            return refuse(format!(
                "harness row {index}: the slug {:?} appears twice",
                row.slug
            ));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return refuse(format!(
            "{}: names no agent this topos knows",
            origin.label()
        ));
    }
    Ok(ParsedRegistry {
        version,
        rows,
        warnings,
        bridge: mcp_bridge_at(root, origin)?,
    })
}

/// One `[[harness]]` row. Every refusal names the row — by slug once it has one, by index before.
fn parse_row(
    table: &dyn TableLike,
    index: usize,
    origin: Origin,
) -> Result<HarnessRow, RegistryError> {
    let slug = str_at(table, "slug")
        .map_err(|e| RegistryError(format!("harness row {index}: {}", e.message())))?
        .to_owned();
    let at = |what: &str| RegistryError(format!("harness {slug:?}: {what}"));
    let display_name = str_at(table, "display_name")
        .map_err(|e| at(e.message()))?
        .to_owned();
    let project_dir = str_at(table, "project_dir")
        .map_err(|e| at(e.message()))?
        .to_owned();
    if origin.fenced() && !project_dir.is_empty() {
        validate_suffix(&project_dir).map_err(|e| at(&format!("project_dir {e}")))?;
    }
    let user_dirs = dirs_at(table, "user_dirs", origin).map_err(|e| at(e.message()))?;
    let detect_dirs = dirs_at(table, "detect_dirs", origin).map_err(|e| at(e.message()))?;
    let shared_dir = shared_dir_at(table).map_err(|e| at(e.message()))?;
    let mcp = mcp_at(table, origin).map_err(|e| at(e.message()))?;
    Ok(HarnessRow {
        slug,
        display_name,
        user_dirs,
        project_dir,
        detect_dirs,
        mcp,
        shared_dir,
    })
}

/// A row's `user_dirs` / `detect_dirs` array (absent ⇒ empty — the two cwd-only harnesses have no
/// user dir, and the `universal` sentinel has no detection probe).
fn dirs_at(
    table: &dyn TableLike,
    key: &str,
    origin: Origin,
) -> Result<Vec<OwnedDir>, RegistryError> {
    let Some(found) = table.get(key) else {
        return Ok(Vec::new());
    };
    let Some(array) = found.as_array() else {
        return refuse(format!("{key}: expected an array of dir specs"));
    };
    let mut out = Vec::with_capacity(array.len());
    for (i, value) in array.iter().enumerate() {
        let Some(raw) = value.as_str() else {
            return refuse(format!("{key}[{i}]: expected a dir-spec string"));
        };
        out.push(parse_dir(raw, origin).map_err(|e| RegistryError(format!("{key}[{i}]: {e}")))?);
    }
    Ok(out)
}

/// The inverse of [`DirSpec::raw`]: a root tag, then the `/`-separated suffix under it. A leading
/// `/` is the absolute root (bundled rows only); an empty suffix is the root itself.
fn parse_dir(raw: &str, origin: Origin) -> Result<OwnedDir, RegistryError> {
    if raw.is_empty() {
        return refuse(
            "empty dir spec (expected a root tag like `home`, then an optional `/`-suffix)",
        );
    }
    if let Some(rest) = raw.strip_prefix('/') {
        if origin.fenced() {
            return refuse(format!(
                "{raw:?}: an absolute path is only allowed in the registry built into topos"
            ));
        }
        return Ok(OwnedDir {
            root: Root::Abs,
            suffix: rest.to_owned(),
        });
    }
    let (tag, suffix) = raw.split_once('/').unwrap_or((raw, ""));
    let Some(root) = Root::from_tag(tag) else {
        return refuse(format!(
            "{raw:?}: {tag:?} is not a root this topos knows (expected one of {})",
            known_tags()
        ));
    };
    if origin.fenced() {
        // The bare home dir, with no suffix at all, is the one root that is never a DIR a row may
        // name: as a skills dir it places bundles into `$HOME` itself, and as a detect dir it says
        // the harness is installed on every machine there is. No bundled row uses it.
        if root == Root::Home && suffix.is_empty() {
            return refuse(format!(
                "{raw:?}: the home directory itself is not a dir a registry row may name"
            ));
        }
        if !suffix.is_empty() {
            validate_suffix(suffix).map_err(|e| RegistryError(format!("{raw:?}: {e}")))?;
        }
    }
    Ok(OwnedDir {
        root,
        suffix: suffix.to_owned(),
    })
}

/// The path fence every dir a DOWNLOADED or OVERRIDE row carries has to pass: relative,
/// `/`-separated, no `..`, no empty or `.` components, and a conservative component charset. The
/// bundled table skips it — that file is the artifact this build was compiled from, and this
/// crate's own suite is its gate.
fn validate_suffix(suffix: &str) -> Result<(), String> {
    if suffix.contains('\\') {
        return Err(format!("{suffix:?}: paths are `/`-separated"));
    }
    for component in suffix.split('/') {
        match component {
            "" => return Err(format!("{suffix:?}: an empty path component")),
            "." => return Err(format!("{suffix:?}: a `.` path component")),
            ".." => return Err(format!("{suffix:?}: a `..` path component")),
            _ => {}
        }
        if let Some(bad) = component
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ' ' | '-')))
        {
            return Err(format!(
                "{suffix:?}: {bad:?} is not allowed in a path component"
            ));
        }
    }
    Ok(())
}

/// A row's `shared_dir = { level = "probed"|"docs", covered = <bool> }` (absent ⇒ no claim of its
/// own, which is what makes [`crate::coverage`] derive one from the user dirs).
fn shared_dir_at(table: &dyn TableLike) -> Result<SharedDirSupport, RegistryError> {
    let Some(found) = table.get("shared_dir") else {
        return Ok(SharedDirSupport::Unknown);
    };
    let Some(claim) = found.as_table_like() else {
        return refuse("shared_dir: expected { level = \"probed\"|\"docs\", covered = <bool> }");
    };
    let level = claim
        .get("level")
        .and_then(Item::as_str)
        .ok_or_else(|| RegistryError("shared_dir.level: expected \"probed\" or \"docs\"".into()))?;
    let covered = claim
        .get("covered")
        .and_then(Item::as_bool)
        .ok_or_else(|| RegistryError("shared_dir.covered: expected true or false".into()))?;
    match level {
        "probed" => Ok(SharedDirSupport::Probed(covered)),
        "docs" => Ok(SharedDirSupport::Docs(covered)),
        other => refuse(format!(
            "shared_dir.level {other:?}: expected \"probed\" or \"docs\""
        )),
    }
}

/// A row's `[harness.mcp]` block (absent ⇒ the harness has no MCP surface, which is the majority).
fn mcp_at(table: &dyn TableLike, origin: Origin) -> Result<Option<OwnedMcp>, RegistryError> {
    let Some(found) = table.get("mcp") else {
        return Ok(None);
    };
    let Some(mcp) = found.as_table_like() else {
        return refuse("mcp: expected a [harness.mcp] table");
    };
    let reload_note = str_at(mcp, "reload_note")
        .map_err(|e| RegistryError(format!("mcp.{}", e.message())))?
        .to_owned();

    let user = match mcp.get("user") {
        None => None,
        Some(entry) => {
            let surface = entry.as_table_like().ok_or_else(|| {
                RegistryError("mcp.user: expected { dir = …, dialect = … }".into())
            })?;
            let dir = str_at(surface, "dir")
                .map_err(|e| RegistryError(format!("mcp.user.{}", e.message())))?;
            let dialect = str_at(surface, "dialect")
                .map_err(|e| RegistryError(format!("mcp.user.{}", e.message())))?;
            Some((
                parse_dir(dir, origin).map_err(|e| RegistryError(format!("mcp.user.dir: {e}")))?,
                parse_dialect(dialect).map_err(|e| RegistryError(format!("mcp.user.{e}")))?,
            ))
        }
    };
    let project = match mcp.get("project") {
        None => None,
        Some(entry) => {
            let surface = entry.as_table_like().ok_or_else(|| {
                RegistryError("mcp.project: expected { path = …, dialect = … }".into())
            })?;
            let path = str_at(surface, "path")
                .map_err(|e| RegistryError(format!("mcp.project.{}", e.message())))?;
            let dialect = str_at(surface, "dialect")
                .map_err(|e| RegistryError(format!("mcp.project.{}", e.message())))?;
            if origin.fenced() {
                validate_suffix(path)
                    .map_err(|e| RegistryError(format!("mcp.project.path {e}")))?;
            }
            Some((
                path.to_owned(),
                parse_dialect(dialect).map_err(|e| RegistryError(format!("mcp.project.{e}")))?,
            ))
        }
    };
    let conflict_paths = conflict_paths_at(mcp, origin)?;
    // The CAPABILITY columns. Both default to the six rows' original behaviour — a harness that
    // dials addresses itself and runs local programs — so a row states only what makes it unusual.
    let flag = |key: &str, default: bool| -> Result<bool, RegistryError> {
        match mcp.get(key) {
            None => Ok(default),
            Some(v) => v
                .as_bool()
                .ok_or_else(|| RegistryError(format!("mcp.{key}: expected true or false"))),
        }
    };
    let remote = flag("remote", true)?;
    let stdio = flag("stdio", true)?;
    let env_ref = match mcp.get("env_ref") {
        None => EnvRef::default(),
        Some(v) => {
            let word = v
                .as_str()
                .ok_or_else(|| RegistryError("mcp.env_ref: expected a string".into()))?;
            parse_env_ref(word).map_err(|e| RegistryError(format!("mcp.{e}")))?
        }
    };
    Ok(Some(OwnedMcp {
        user,
        project,
        reload_note,
        conflict_paths,
        remote,
        stdio,
        env_ref,
    }))
}

/// The word an [`EnvRef`] is spelled with in the file — named for the SHAPE rather than for the
/// harness that uses it, because the same shape serves several.
const fn env_ref_word(env_ref: EnvRef) -> &'static str {
    match env_ref {
        EnvRef::DollarBrace => "dollar-brace",
        EnvRef::BraceEnv => "brace-env",
        EnvRef::DollarBraceEnv => "dollar-brace-env",
    }
}

fn parse_env_ref(word: &str) -> Result<EnvRef, String> {
    const KNOWN: [EnvRef; 3] = [
        EnvRef::DollarBrace,
        EnvRef::BraceEnv,
        EnvRef::DollarBraceEnv,
    ];
    KNOWN
        .into_iter()
        .find(|e| env_ref_word(*e) == word)
        .ok_or_else(|| {
            format!(
                "env_ref {word:?}: expected one of {}",
                KNOWN
                    .iter()
                    .map(|e| env_ref_word(*e))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The file's `[mcp_bridge]` table (absent ⇒ this table names no bridge, and a harness that
/// cannot dial an address simply does not get one).
///
/// **FENCED like an absolute path, and for the same reason.** A downloaded or override table may
/// say WHICH harnesses need bridging; it may never change WHICH PROGRAM runs on the machine. So a
/// fenced file's bridge must be exactly the bundled one — which the SERVED copy is, byte for byte
/// — and anything else refuses the whole file.
fn mcp_bridge_at(
    root: &dyn TableLike,
    origin: Origin,
) -> Result<Option<OwnedBridge>, RegistryError> {
    let parsed = match root.get("mcp_bridge") {
        None => None,
        Some(found) => {
            let table = found
                .as_table_like()
                .ok_or_else(|| RegistryError("mcp_bridge: expected a [mcp_bridge] table".into()))?;
            let package = str_at(table, "package")
                .map_err(|e| RegistryError(format!("mcp_bridge.{}", e.message())))?;
            let version = str_at(table, "version")
                .map_err(|e| RegistryError(format!("mcp_bridge.{}", e.message())))?;
            if package.trim().is_empty() || version.trim().is_empty() {
                return refuse(
                    "mcp_bridge: the package and its version are both named, or neither",
                );
            }
            Some(OwnedBridge {
                package: package.to_owned(),
                version: version.to_owned(),
            })
        }
    };
    // A fenced file that names NO bridge changes nothing (the pin is read from the bundled table
    // either way); one that names a DIFFERENT bridge is trying to choose the program this machine
    // runs, and is refused whole rather than quietly ignored.
    if origin.fenced()
        && let Some(named) = &parsed
        && bundled_bridge().is_none_or(|b| b.package != named.package || b.version != named.version)
    {
        return refuse(
            "mcp_bridge: a downloaded harness registry may say which agents need the bridge, and \
             never which program runs — this one names a different bridge than the topos reading it",
        );
    }
    Ok(parsed)
}

/// A row's `[[harness.mcp.conflict_paths]]` entries (absent ⇒ none, which is every row whose
/// harness reads servers only where topos writes them). Each names a file, the dialect it speaks,
/// and optionally the slot inside it. A malformed entry refuses the FILE rather than being
/// skipped: a conflict path silently dropped is a placement that lands on a server the agent
/// already has, which is the failure this column exists to prevent.
fn conflict_paths_at(
    mcp: &dyn TableLike,
    origin: Origin,
) -> Result<Vec<OwnedConflictPath>, RegistryError> {
    let Some(found) = mcp.get("conflict_paths") else {
        return Ok(Vec::new());
    };
    let entries: Vec<&dyn TableLike> = match (found.as_array_of_tables(), found.as_array()) {
        (Some(tables), _) => tables.iter().map(|t| t as &dyn TableLike).collect(),
        (None, Some(array)) => {
            let mut out = Vec::with_capacity(array.len());
            for (i, value) in array.iter().enumerate() {
                out.push(value.as_inline_table().map(|t| t as &dyn TableLike).ok_or_else(|| {
                    RegistryError(format!(
                        "mcp.conflict_paths[{i}]: expected {{ file = …, dialect = …, selector = … }}"
                    ))
                })?);
            }
            out
        }
        (None, None) => {
            return refuse("mcp.conflict_paths: expected an array of { file, dialect, selector }");
        }
    };
    let mut out = Vec::with_capacity(entries.len());
    for (i, entry) in entries.into_iter().enumerate() {
        let at = |what: &str| RegistryError(format!("mcp.conflict_paths[{i}]: {what}"));
        let file = str_at(entry, "file").map_err(|e| at(e.message()))?;
        let dialect = str_at(entry, "dialect").map_err(|e| at(e.message()))?;
        // The selector is OPTIONAL — a file whose entries sit in the dialect's own slot names none.
        let selector = match entry.get("selector") {
            None => String::new(),
            Some(v) => v
                .as_str()
                .ok_or_else(|| at("selector: expected a string"))?
                .to_owned(),
        };
        out.push(OwnedConflictPath {
            file: parse_dir(file, origin).map_err(|e| at(&format!("file: {e}")))?,
            dialect: parse_dialect(dialect).map_err(|e| at(&e))?,
            selector,
        });
    }
    Ok(out)
}

/// The kebab-case spelling of an [`McpDialect`] variant — the file's dialect vocabulary.
const fn dialect_word(dialect: McpDialect) -> &'static str {
    match dialect {
        McpDialect::ClaudePluginDir => "claude-plugin-dir",
        McpDialect::ClaudeProjectJson => "claude-project-json",
        McpDialect::CursorJson => "cursor-json",
        McpDialect::OpencodeJson => "opencode-json",
        McpDialect::OpenclawJson => "openclaw-json",
        McpDialect::CodexToml => "codex-toml",
        McpDialect::HermesYaml => "hermes-yaml",
        McpDialect::GooseYaml => "goose-yaml",
        McpDialect::VscodeJson => "vscode-json",
        McpDialect::CopilotCliJson => "copilot-cli-json",
        McpDialect::LmStudioJson => "lm-studio-json",
        McpDialect::ClaudeDesktopJson => "claude-desktop-json",
        McpDialect::GeminiJson => "gemini-json",
        McpDialect::WindsurfJson => "windsurf-json",
        McpDialect::ClineJson => "cline-json",
        McpDialect::RooJson => "roo-json",
        McpDialect::ZedJson => "zed-json",
    }
}

/// Every dialect this build speaks — the refusal's teaching half, and the renderer's source.
const DIALECTS: &[McpDialect] = &[
    McpDialect::ClaudePluginDir,
    McpDialect::ClaudeProjectJson,
    McpDialect::CursorJson,
    McpDialect::OpencodeJson,
    McpDialect::OpenclawJson,
    McpDialect::CodexToml,
    McpDialect::HermesYaml,
    McpDialect::GooseYaml,
    McpDialect::VscodeJson,
    McpDialect::CopilotCliJson,
    McpDialect::LmStudioJson,
    McpDialect::ClaudeDesktopJson,
    McpDialect::GeminiJson,
    McpDialect::WindsurfJson,
    McpDialect::ClineJson,
    McpDialect::RooJson,
    McpDialect::ZedJson,
];

fn known_dialects() -> String {
    DIALECTS
        .iter()
        .map(|d| dialect_word(*d))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_dialect(word: &str) -> Result<McpDialect, String> {
    DIALECTS
        .iter()
        .copied()
        .find(|d| dialect_word(*d) == word)
        .ok_or_else(|| format!("dialect {word:?}: expected one of {}", known_dialects()))
}

/// Every root tag a file may name (the absolute root has no tag — it is spelled by a leading `/`).
fn known_tags() -> String {
    Root::ALL
        .iter()
        .map(|r| r.tag())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A required string key.
fn str_at<'a>(table: &'a dyn TableLike, key: &str) -> Result<&'a str, RegistryError> {
    match table.get(key) {
        None => refuse(format!("{key}: missing")),
        Some(found) => found
            .as_str()
            .ok_or_else(|| RegistryError(format!("{key}: expected a string"))),
    }
}

/// A required non-negative integer key.
fn int_at(table: &dyn TableLike, key: &str) -> Result<u64, RegistryError> {
    match table.get(key) {
        None => refuse(format!("{key}: missing")),
        Some(found) => match found.as_integer() {
            Some(n) if n >= 0 => Ok(n.unsigned_abs()),
            _ => refuse(format!("{key}: expected a non-negative integer")),
        },
    }
}

// =================================================================================================
// The three-level resolution
// =================================================================================================

/// The bundled table, parsed once. A refusal here is a BUILD defect (the file ships inside the
/// binary and this crate's suite parses it on every run), so it is the one place that panics.
fn bundled() -> &'static ParsedRegistry {
    static BUNDLED_TABLE: OnceLock<ParsedRegistry> = OnceLock::new();
    BUNDLED_TABLE.get_or_init(|| {
        parse_registry(BUNDLED, Origin::Bundled).expect(
            "the bundled registry.toml parses (a compiled-in artifact gated by this crate's suite)",
        )
    })
}

/// The bundled table's content version — what a downloaded copy has to beat to be used.
#[must_use]
pub fn bundled_version() -> &'static RegistryVersion {
    &bundled().version
}

/// The BUNDLED rows alone — the table this binary was BUILT with, whatever any machine-local file
/// says. Leaked once, exactly as the resolved table is, so it hands out the same `&'static` rows.
///
/// Two callers, and both need "this build's table" rather than "this machine's":
///
/// - the REPO's own tooling — `cargo xtask`'s codegen and drift gates, and this crate's shape-pin
///   tests. A gate that read the resolved table would pass or fail on whether the developer
///   running it happens to keep a `~/.topos/harness-registry/override.toml`, which is a property
///   of a laptop and not of the commit under test;
/// - a TEARDOWN, which must reach a row a newer downloaded table has since dropped
///   ([`super::teardown_harnesses`]).
///
/// Everything that acts on THIS machine — discovery, detection, placement, arming — reads
/// [`super::known_harnesses`] instead.
#[must_use]
pub fn bundled_harnesses() -> &'static [KnownHarness] {
    static TABLE: OnceLock<&'static [KnownHarness]> = OnceLock::new();
    TABLE.get_or_init(|| leak(bundled().rows.clone()))
}

/// **The bridge this BUILD runs** — the `mcp-remote` package and the exact version pinned in the
/// bundled table, or `None` when this build's table names none.
///
/// It is deliberately read from the bundled table and nowhere else. This module's own fence
/// already refuses a downloaded table that names a different bridge, so the two answers are always
/// the same; reading the bundled one makes that a property of the code rather than of a check
/// having run.
#[must_use]
pub fn mcp_bridge() -> Option<&'static McpBridge> {
    bundled_bridge()
}

/// [`mcp_bridge`]'s leaked value — separate so the fence can call it during a parse without
/// recursing into a resolution.
fn bundled_bridge() -> Option<&'static McpBridge> {
    static BRIDGE: OnceLock<Option<McpBridge>> = OnceLock::new();
    BRIDGE
        .get_or_init(|| {
            bundled().bridge.as_ref().map(|b| McpBridge {
                package: leak_str(b.package.clone()),
                version: leak_str(b.version.clone()),
            })
        })
        .as_ref()
}

/// The slugs the BUNDLED table defines — the known-ids fence every fenced level is measured
/// against. A downloaded row naming anything else never becomes a write target.
fn bundled_slugs() -> &'static BTreeSet<String> {
    static SLUGS: OnceLock<BTreeSet<String>> = OnceLock::new();
    SLUGS.get_or_init(|| bundled().rows.iter().map(|r| r.slug.clone()).collect())
}

/// One candidate level: the bytes, plus where they came from (a warning has to name the file a
/// person can go and fix).
#[derive(Debug, Clone)]
pub(super) struct Candidate {
    path: String,
    text: String,
}

/// Resolve the table from the two machine-local levels over the bundled floor, collecting the
/// warnings the caller says once. Pure — the loader does the reading, this does the deciding.
///
/// **A downloaded file's skipped rows are not warned about here.** A served table naming a harness
/// this build does not know is the ordinary consequence of the table moving faster than releases:
/// the row is skipped either way, and repeating that on every command until the next `self-update`
/// would be guaranteed fleet-wide noise on the system's expected event. The client's refresher says
/// it ONCE, as the new version lands. An OVERRIDE is the opposite case — a file a person wrote by
/// hand, whose skipped row is a typo they are waiting to hear about — so its warnings ride through.
pub(super) fn resolve(
    over: Option<Candidate>,
    downloaded: Option<Candidate>,
) -> (Vec<HarnessRow>, Vec<String>) {
    let bundled = bundled();
    let mut warnings = Vec::new();

    if let Some(candidate) = over {
        match parse_registry(&candidate.text, Origin::Override) {
            Ok(parsed) => {
                warnings.extend(parsed.warnings);
                return (parsed.rows, warnings);
            }
            // Name the level it falls TO, which is the next one that exists — if that one is
            // refused as well it says so itself, on its own line.
            Err(e) => warnings.push(format!(
                "topos: the harness registry override at {} was refused ({e}) — using {} instead",
                candidate.path,
                if downloaded.is_some() {
                    "the downloaded copy"
                } else {
                    "the one built into this topos"
                }
            )),
        }
    }
    if let Some(candidate) = downloaded {
        match parse_registry(&candidate.text, Origin::Downloaded) {
            // Older or equal is the ordinary state after a `self-update`: the binary caught up with
            // what it had downloaded, and there is nothing to say about it.
            Ok(parsed) if parsed.version <= bundled.version => {}
            // `parsed.warnings` are deliberately dropped — see this function's doc.
            Ok(parsed) => return (parsed.rows, warnings),
            Err(e) => warnings.push(format!(
                "topos: the downloaded harness registry at {} was refused ({e}) — using the one \
                 built into this topos",
                candidate.path
            )),
        }
    }
    (bundled.rows.clone(), warnings)
}

/// Read one level off disk, `None` when it is not there. A file that exists but cannot be read (or
/// is past the ceiling) is reported and treated as absent — the level below it still answers.
fn read_level(path: &Path, warnings: &mut Vec<String>) -> Option<Candidate> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_REGISTRY_BYTES as u64 => {
            warnings.push(format!(
                "topos: the harness registry at {} is larger than {MAX_REGISTRY_BYTES} bytes — it \
                 was not read",
                path.display()
            ));
            return None;
        }
        Ok(_) => {}
        Err(_) => return None,
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Some(Candidate {
            path: path.display().to_string(),
            text,
        }),
        Err(e) => {
            warnings.push(format!(
                "topos: the harness registry at {} could not be read ({e}) — it was skipped",
                path.display()
            ));
            None
        }
    }
}

/// THE table for this process: the three levels resolved, the warnings said once on stderr, the
/// rows leaked so every accessor can keep handing out `&'static` rows.
pub(super) fn load_table() -> &'static [KnownHarness] {
    if bundled_only() {
        // Told to answer for this BUILD: the machine-local levels are not read at all, so nothing
        // on the developer's disk can decide what a gate or a suite sees.
        return bundled_harnesses();
    }
    let home = sidecar_home();
    let mut warnings = Vec::new();
    let over = read_level(&override_path(&home), &mut warnings);
    let downloaded = read_level(&cache_path(&home), &mut warnings);
    let (rows, resolution_warnings) = resolve(over, downloaded);
    warnings.extend(resolution_warnings);
    // stderr, once per process: the table is resolved behind a `OnceLock`, so there is exactly one
    // resolution however many callers ask for it. stdout stays byte-clean for `--json`.
    say(&mut std::io::stderr().lock(), &warnings);
    leak(rows)
}

/// Write the resolution's warnings, one per line, DROPPING any that cannot be delivered.
///
/// Not `eprintln!`: the macro PANICS when the write fails, and the ordinary way for a stderr write
/// to fail is that the reader went away (`topos update --quiet 2> >(head -1)`). A gone stderr
/// reader has stopped listening to diagnostics and nothing more — it never ends the run, and it
/// certainly never ends it by panicking out of a table load that had already succeeded. This is
/// the same posture the CLI's own two streams take; the table is loaded far below that machinery,
/// so it takes the posture directly.
fn say(out: &mut impl std::io::Write, warnings: &[String]) {
    for warning in warnings {
        let _ = writeln!(out, "{warning}");
    }
}

/// Leak an owned table into the `&'static` slice the public signatures promise. ONE table per
/// process, deliberately: it is resolved once and read until the process exits, so the memory is
/// live for exactly as long as anything can reach it.
fn leak(rows: Vec<HarnessRow>) -> &'static [KnownHarness] {
    let table: Vec<KnownHarness> = rows
        .into_iter()
        .map(|row| KnownHarness {
            slug: leak_str(row.slug),
            display_name: leak_str(row.display_name),
            user_dirs: leak_dirs(row.user_dirs),
            project_dir: leak_str(row.project_dir),
            detect_dirs: leak_dirs(row.detect_dirs),
            mcp: row.mcp.map(|mcp| McpSurfaces {
                user: mcp.user.map(|(dir, dialect)| McpSurface {
                    dir: leak_dir(dir),
                    dialect,
                }),
                project: mcp.project.map(|(path, dialect)| (leak_str(path), dialect)),
                reload_note: leak_str(mcp.reload_note),
                conflict_paths: Vec::leak(
                    mcp.conflict_paths
                        .into_iter()
                        .map(|c| McpConflictPath {
                            file: leak_dir(c.file),
                            dialect: c.dialect,
                            selector: leak_str(c.selector),
                        })
                        .collect::<Vec<_>>(),
                ),
                remote: mcp.remote,
                stdio: mcp.stdio,
                env_ref: mcp.env_ref,
            }),
            shared_dir: row.shared_dir,
        })
        .collect();
    Vec::leak(table)
}

fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn leak_dir(dir: OwnedDir) -> DirSpec {
    DirSpec {
        root: dir.root,
        suffix: leak_str(dir.suffix),
    }
}

fn leak_dirs(dirs: Vec<OwnedDir>) -> &'static [DirSpec] {
    Vec::leak(dirs.into_iter().map(leak_dir).collect::<Vec<_>>())
}

// =================================================================================================
// The renderer — the parser's inverse, in TESTS only. The committed file is edited by hand (its
// evidence comments are the point), so nothing generates it; what a round trip proves is that
// reading and writing agree about every field of every row.
// =================================================================================================

#[cfg(test)]
pub(super) fn render_table(version: &RegistryVersion, rows: &[KnownHarness]) -> String {
    let mut out = format!(
        "schema_version = {SCHEMA_VERSION}\nversion = \"{version}\"\nmin_engine_version = \
         {REGISTRY_ENGINE_VERSION}\n"
    );
    for row in rows {
        out.push('\n');
        out.push_str(&render_row(row));
    }
    out
}

#[cfg(test)]
pub(super) fn render_row(row: &KnownHarness) -> String {
    use std::fmt::Write as _;

    let quote = |s: &str| format!("{s:?}");
    let list = |dirs: &[DirSpec]| {
        format!(
            "[{}]",
            dirs.iter()
                .map(|d| quote(&d.raw()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut out = String::from("[[harness]]\n");
    let _ = writeln!(out, "slug = {}", quote(row.slug));
    let _ = writeln!(out, "display_name = {}", quote(row.display_name));
    let _ = writeln!(out, "user_dirs = {}", list(row.user_dirs));
    let _ = writeln!(out, "project_dir = {}", quote(row.project_dir));
    let _ = writeln!(out, "detect_dirs = {}", list(row.detect_dirs));
    match row.shared_dir {
        SharedDirSupport::Unknown => {}
        SharedDirSupport::Probed(covered) => {
            let _ = writeln!(
                out,
                "shared_dir = {{ level = \"probed\", covered = {covered} }}"
            );
        }
        SharedDirSupport::Docs(covered) => {
            let _ = writeln!(
                out,
                "shared_dir = {{ level = \"docs\", covered = {covered} }}"
            );
        }
    }
    if let Some(mcp) = &row.mcp {
        out.push_str("\n[harness.mcp]\n");
        let _ = writeln!(out, "reload_note = {}", quote(mcp.reload_note));
        if !mcp.remote {
            let _ = writeln!(out, "remote = false");
        }
        if !mcp.stdio {
            let _ = writeln!(out, "stdio = false");
        }
        if mcp.env_ref != EnvRef::default() {
            let _ = writeln!(out, "env_ref = {}", quote(env_ref_word(mcp.env_ref)));
        }
        if let Some(user) = mcp.user {
            let _ = writeln!(
                out,
                "user = {{ dir = {}, dialect = {} }}",
                quote(&user.dir.raw()),
                quote(dialect_word(user.dialect))
            );
        }
        if let Some((path, dialect)) = mcp.project {
            let _ = writeln!(
                out,
                "project = {{ path = {}, dialect = {} }}",
                quote(path),
                quote(dialect_word(dialect))
            );
        }
        for c in mcp.conflict_paths {
            out.push_str("\n[[harness.mcp.conflict_paths]]\n");
            let _ = writeln!(out, "file = {}", quote(&c.file.raw()));
            let _ = writeln!(out, "dialect = {}", quote(dialect_word(c.dialect)));
            if !c.selector.is_empty() {
                let _ = writeln!(out, "selector = {}", quote(c.selector));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid file naming one row this build knows.
    fn one_row(slug: &str, version: &str) -> String {
        format!(
            "schema_version = 1\nversion = \"{version}\"\nmin_engine_version = 1\n\n\
             [[harness]]\nslug = \"{slug}\"\ndisplay_name = \"X\"\nuser_dirs = \
             [\"home/.x/skills\"]\nproject_dir = \".x/skills\"\ndetect_dirs = [\"home/.x\"]\n"
        )
    }

    fn candidate(text: &str) -> Candidate {
        Candidate {
            path: "/tmp/fixture.toml".to_owned(),
            text: text.to_owned(),
        }
    }

    /// The bundled table's own version, plus a segment — the shape a real downloaded file has.
    fn newer_than_bundled() -> String {
        format!("{}.1", bundled_version())
    }

    // ---- the version -------------------------------------------------------------------------

    #[test]
    fn versions_compare_by_dotted_numeric_segment() {
        let v = |s: &str| RegistryVersion::parse(s).expect(s);
        assert!(v("2026.8.11.2") > v("2026.8.11.1"));
        assert!(v("2026.9.1") > v("2026.8.99"));
        assert_eq!(v("1.2"), v("1.2.0"));
        assert!(v("1.2.1") > v("1.2"));
    }

    #[test]
    fn a_version_that_is_not_dotted_numeric_names_what_it_refused() {
        for bad in ["", "2026.8.alpha", "2026..8", "99999999999999999999999"] {
            let e = RegistryVersion::parse(bad).expect_err(bad);
            assert!(e.message().contains("version"), "{bad}: {e}");
        }
    }

    // ---- the header fences -------------------------------------------------------------------

    #[test]
    fn a_foreign_schema_version_refuses_the_file() {
        let text = one_row("cursor", "9.0.0").replace("schema_version = 1", "schema_version = 2");
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("schema_version 2"), "{e}");
    }

    #[test]
    fn a_min_engine_version_past_this_build_refuses_the_file() {
        let past = REGISTRY_ENGINE_VERSION + 1;
        let text = one_row("cursor", "9.0.0").replace(
            "min_engine_version = 1",
            &format!("min_engine_version = {past}"),
        );
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(
            e.message().contains(&format!("min_engine_version {past}")),
            "{e}"
        );
        // …and the engine this build DOES provide is named, so a person can tell which side is
        // behind.
        assert!(
            e.message().contains(&REGISTRY_ENGINE_VERSION.to_string()),
            "{e}"
        );
    }

    #[test]
    fn a_file_that_is_not_toml_or_names_no_rows_is_refused() {
        let e = parse_registry("not = = toml", Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("not valid TOML"), "{e}");
        let e = parse_registry(
            "schema_version = 1\nversion = \"1\"\nmin_engine_version = 1\n",
            Origin::Downloaded,
        )
        .expect_err("refused");
        assert!(e.message().contains("[[harness]]"), "{e}");
    }

    #[test]
    fn a_file_past_the_size_ceiling_is_refused_unread() {
        let huge = format!(
            "{}{}",
            one_row("cursor", "9.0.0"),
            "#".repeat(MAX_REGISTRY_BYTES)
        );
        let e = parse_registry(&huge, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("ceiling"), "{e}");
    }

    // ---- the row fences ----------------------------------------------------------------------

    #[test]
    fn an_unknown_slug_is_warned_about_and_skipped_never_refused() {
        let text = format!(
            "{}\n[[harness]]\nslug = \"brand-new-agent\"\ndisplay_name = \"New\"\nuser_dirs = \
             [\"home/.new/skills\"]\nproject_dir = \".new/skills\"\ndetect_dirs = \
             [\"home/.new\"]\n",
            one_row("cursor", "9.0.0")
        );
        let parsed = parse_registry(&text, Origin::Downloaded).expect("the known row survives");
        assert_eq!(parsed.row_count(), 1, "only the row this build knows");
        assert_eq!(parsed.warnings().len(), 1, "{:?}", parsed.warnings());
        assert!(
            parsed.warnings()[0].contains("brand-new-agent"),
            "{:?}",
            parsed.warnings()
        );
        // The BUNDLED table DEFINES the set, so the same file is taken whole when it is the bundle.
        assert_eq!(
            parse_registry(&text, Origin::Bundled)
                .expect("bundled takes it")
                .row_count(),
            2
        );
    }

    #[test]
    fn a_file_whose_every_row_is_unknown_is_refused_rather_than_believed_empty() {
        let e = parse_registry(&one_row("brand-new-agent", "9.0.0"), Origin::Downloaded)
            .expect_err("refused");
        assert!(e.message().contains("names no agent"), "{e}");
    }

    #[test]
    fn a_duplicate_slug_refuses_the_file() {
        let row = one_row("cursor", "9.0.0");
        let body = &row[row.find("[[harness]]").expect("a row")..];
        let text = format!("{row}\n{body}");
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("twice"), "{e}");
    }

    #[test]
    fn a_downloaded_row_may_not_escape_its_root_or_name_an_absolute_path() {
        let cases = [
            ("home/../../etc", ".."),
            ("home/./skills", "`.`"),
            ("home//skills", "empty path component"),
            ("home/sk;ills", "not allowed"),
            ("home/back\\slash", "`/`-separated"),
            ("/etc/codex", "absolute"),
            ("nosuchroot/skills", "not a root"),
        ];
        for (dir, needle) in cases {
            let text =
                one_row("cursor", "9.0.0").replace("[\"home/.x/skills\"]", &format!("[{dir:?}]"));
            let e = parse_registry(&text, Origin::Downloaded).expect_err(dir);
            assert!(e.message().contains(needle), "{dir}: {e}");
            assert!(e.message().contains("cursor"), "{dir}: {e}");
        }
        // The GRAMMAR still allows an absolute root in the bundled table — no row uses one
        // (`the_bundled_table_is_landable_as_a_downloaded_copy` is why), but the fence is the
        // origin's, not the parser's.
        let text = one_row("cursor", "9.0.0").replace("[\"home/.x/skills\"]", "[\"/etc/codex\"]");
        assert!(parse_registry(&text, Origin::Bundled).is_ok());
    }

    /// **The bundled table has to survive the DOWNLOADED fences, or the refresh lane is dead.**
    /// The web app serves these exact bytes, a client reads them at [`Origin::Downloaded`], and a
    /// single refused dir refuses the WHOLE file — so one absolute path in one row is enough to
    /// make every refresh a permanent no-op that only says so in a state document. This is the
    /// gate that keeps the served copy landable; a row that needs a path the fence refuses does
    /// not get an exception, it gets a different probe.
    #[test]
    fn the_bundled_table_is_landable_as_a_downloaded_copy() {
        let parsed = parse_registry(BUNDLED, Origin::Downloaded)
            .expect("the served copy IS the bundled bytes — it must pass the fenced rules");
        assert_eq!(
            parsed.row_count(),
            bundled().rows.len(),
            "every bundled row survives the fences"
        );
        assert!(parsed.warnings().is_empty(), "{:?}", parsed.warnings());
    }

    /// **The bundled table demands exactly the engine this build provides** — the boundary that
    /// makes a new COLUMN safe to ship.
    ///
    /// The served copy IS these bytes, so its `min_engine_version` is what every older client is
    /// measured against: a build whose engine is lower refuses the download whole and keeps its own
    /// bundled table, which is the honest outcome — it cannot act on the column and would otherwise
    /// read a row's silence as a default it never meant. Adding a column without moving BOTH
    /// numbers is exactly the mistake this pins.
    #[test]
    fn the_bundled_table_demands_the_engine_this_build_provides() {
        let header: toml_edit::DocumentMut = BUNDLED.parse().expect("the bundled table parses");
        assert_eq!(
            header
                .get("min_engine_version")
                .and_then(Item::as_integer)
                .expect("the bundled table states a min_engine_version"),
            i64::from(REGISTRY_ENGINE_VERSION),
            "a column older clients cannot act on rides a bump of BOTH numbers"
        );
        assert_eq!(
            header
                .get("schema_version")
                .and_then(Item::as_integer)
                .expect("the bundled table states a schema_version"),
            i64::from(SCHEMA_VERSION)
        );
    }

    /// The MCP capability columns: stated only where a row is unusual, defaulting to the six
    /// original rows' behaviour, and refusing a word this build cannot act on rather than
    /// guessing at one.
    #[test]
    fn the_mcp_capability_columns_default_to_dialing_and_running() {
        let with = |extra: &str| {
            format!(
                "schema_version = 1\nversion = \"9.0.0\"\nmin_engine_version = 2\n\n\
                 [[harness]]\nslug = \"cursor\"\ndisplay_name = \"X\"\nproject_dir = \
                 \".x/skills\"\nuser_dirs = [\"home/.x/skills\"]\ndetect_dirs = [\"home/.x\"]\n\n\
                 [harness.mcp]\nreload_note = \"x\"\nuser = {{ dir = \"home/.x/mcp.json\", \
                 dialect = \"cursor-json\" }}\n{extra}"
            )
        };
        let mcp = |text: &str| {
            parse_registry(text, Origin::Downloaded)
                .expect("parses")
                .rows[0]
                .mcp
                .clone()
                .expect("an mcp block")
        };
        let plain = mcp(&with(""));
        assert!(plain.remote, "an agent dials addresses unless it says not");
        assert!(plain.stdio, "an agent runs programs unless it says not");
        assert_eq!(plain.env_ref, EnvRef::DollarBrace);

        let stated = mcp(&with(
            "remote = false\nstdio = false\nenv_ref = \"brace-env\"\n",
        ));
        assert!(!stated.remote);
        assert!(!stated.stdio);
        assert_eq!(stated.env_ref, EnvRef::BraceEnv);
        assert_eq!(
            mcp(&with("env_ref = \"dollar-brace-env\"\n")).env_ref,
            EnvRef::DollarBraceEnv
        );

        for (bad, says) in [
            ("remote = \"yes\"\n", "expected true or false"),
            ("stdio = 1\n", "expected true or false"),
            ("env_ref = \"shell\"\n", "expected one of"),
            ("env_ref = 7\n", "expected a string"),
        ] {
            let e = parse_registry(&with(bad), Origin::Downloaded).expect_err(bad);
            assert!(e.message().contains(says), "{bad}: {e}");
        }

        // The three shapes render what a person would recognize.
        assert_eq!(EnvRef::DollarBrace.render("ACME_TOKEN"), "${ACME_TOKEN}");
        assert_eq!(EnvRef::BraceEnv.render("ACME_TOKEN"), "{env:ACME_TOKEN}");
        assert_eq!(
            EnvRef::DollarBraceEnv.render("ACME_TOKEN"),
            "${env:ACME_TOKEN}"
        );
    }

    /// The bridge is DATA, and the one datum a downloaded table may never move: it names the
    /// program topos runs on the machine. A copy that names a different one is refused whole; the
    /// served copy (these same bytes) lands, as the landability gate above requires.
    #[test]
    fn a_downloaded_table_may_not_change_which_bridge_program_runs() {
        let bundled = mcp_bridge().expect("the bundled table pins a bridge");
        assert_eq!(bundled.package, crate::mcp::BRIDGE_PACKAGE);
        assert_eq!(bundled.spec(), format!("mcp-remote@{}", bundled.version));

        let naming = |package: &str, version: &str| {
            format!(
                "schema_version = 1\nversion = \"9.0.0\"\nmin_engine_version = 2\n\n\
                 [mcp_bridge]\npackage = {package:?}\nversion = {version:?}\n\n\
                 [[harness]]\nslug = \"cursor\"\ndisplay_name = \"X\"\nproject_dir = \
                 \".x/skills\"\nuser_dirs = [\"home/.x/skills\"]\ndetect_dirs = [\"home/.x\"]\n"
            )
        };
        // The bundled pin, restated: ordinary.
        assert!(
            parse_registry(
                &naming(bundled.package, bundled.version),
                Origin::Downloaded
            )
            .is_ok()
        );
        // A different program, or a different version of it: refused whole.
        for (package, version) in [
            ("mcp-remote", "9.9.9"),
            ("mcp-remote-evil", bundled.version),
        ] {
            let e = parse_registry(&naming(package, version), Origin::Downloaded)
                .expect_err("{package}@{version}");
            assert!(e.message().contains("never which program runs"), "{e}");
        }
        // A table naming NO bridge is ordinary — the pin is this build's either way — and half a
        // bridge is a malformed file, not a lean one.
        let rows = "\n[[harness]]\nslug = \"cursor\"\ndisplay_name = \"X\"\nproject_dir = \
                    \".x/skills\"\nuser_dirs = [\"home/.x/skills\"]\ndetect_dirs = [\"home/.x\"]\n";
        let header = "schema_version = 1\nversion = \"9.0.0\"\nmin_engine_version = 2\n";
        assert!(parse_registry(&format!("{header}{rows}"), Origin::Downloaded).is_ok());
        let e = parse_registry(
            &format!("{header}\n[mcp_bridge]\npackage = \"mcp-remote\"\n{rows}"),
            Origin::Downloaded,
        )
        .expect_err("half a bridge");
        assert!(e.message().contains("mcp_bridge.version"), "{e}");
    }

    /// Review follow-up: the bare home dir is the one root a fenced row may never name — a skills
    /// dir at `$HOME` places bundles into the home directory itself, and a detect dir at `$HOME`
    /// says the harness is installed on every machine there is.
    #[test]
    fn a_fenced_row_may_not_name_the_home_directory_itself() {
        let naming = |dir: &str| {
            format!(
                "schema_version = 1\nversion = \"9.0.0\"\nmin_engine_version = 1\n\n\
                 [[harness]]\nslug = \"cursor\"\ndisplay_name = \"X\"\nproject_dir = \
                 \".x/skills\"\nuser_dirs = [{dir:?}]\ndetect_dirs = [{dir:?}]\n"
            )
        };
        let e = parse_registry(&naming("home"), Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("home directory itself"), "{e}");
        assert!(e.message().contains("cursor"), "{e}");

        // A home root WITH a suffix, and another root's bare form, are both ordinary.
        for ordinary in ["home/.x/skills", "codexHome"] {
            assert!(
                parse_registry(&naming(ordinary), Origin::Downloaded).is_ok(),
                "{ordinary}"
            );
        }

        // …and no row this build ships names it either, so nothing legitimate is being fenced out.
        for row in bundled_harnesses() {
            for spec in row.user_dirs().iter().chain(row.detect_dirs) {
                assert!(
                    !(spec.root() == Root::Home && spec.suffix().is_empty()),
                    "{}: a bundled row names the bare home dir",
                    row.slug
                );
            }
        }
    }

    #[test]
    fn a_downloaded_project_dir_or_mcp_path_is_fenced_too() {
        let text = one_row("cursor", "9.0.0").replace(".x/skills", "../../etc");
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("project_dir"), "{e}");

        let text = format!(
            "{}\n[harness.mcp]\nreload_note = \"x\"\nproject = {{ path = \"../mcp.json\", dialect \
             = \"cursor-json\" }}\n",
            one_row("cursor", "9.0.0")
        );
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("mcp.project.path"), "{e}");
    }

    #[test]
    fn an_unknown_dialect_names_what_it_refused_and_what_it_knows() {
        let text = format!(
            "{}\n[harness.mcp]\nreload_note = \"x\"\nuser = {{ dir = \"home/.x/mcp.json\", dialect \
             = \"telepathy\" }}\n",
            one_row("cursor", "9.0.0")
        );
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("telepathy"), "{e}");
        assert!(e.message().contains("cursor-json"), "{e}");
    }

    /// The read-only conflict files: parsed as a sub-table, fenced like every other path, and
    /// REFUSING the file rather than skipping an entry it cannot read — a conflict path dropped in
    /// silence is a placement that lands on a server the agent already has.
    #[test]
    fn conflict_paths_parse_as_a_sub_table_and_refuse_what_they_cannot_read() {
        let with = |body: &str| {
            format!(
                "{}\n[harness.mcp]\nreload_note = \"x\"\nuser = {{ dir = \"home/.x/mcp.json\", \
                 dialect = \"cursor-json\" }}\n{body}",
                one_row("cursor", "9.0.0")
            )
        };
        let parsed = parse_registry(
            &with(
                "\n[[harness.mcp.conflict_paths]]\nfile = \"home/.claude.json\"\ndialect = \
                 \"claude-project-json\"\nselector = \"projects.*.mcpServers\"\n\n\
                 [[harness.mcp.conflict_paths]]\nfile = \"home/.claude.json\"\ndialect = \
                 \"claude-project-json\"\n",
            ),
            Origin::Downloaded,
        )
        .expect("two conflict paths");
        let mcp = parsed.rows[0].mcp.as_ref().expect("an mcp block");
        assert_eq!(
            mcp.conflict_paths
                .iter()
                .map(|c| (c.file.raw(), c.selector.clone()))
                .collect::<Vec<_>>(),
            [
                (
                    "home/.claude.json".to_owned(),
                    "projects.*.mcpServers".to_owned()
                ),
                ("home/.claude.json".to_owned(), String::new()),
            ],
            "the selector is optional — no selector means the dialect's own slot"
        );
        // The inline-array spelling of the same sub-table reads identically.
        let inline = parse_registry(
            &with(
                "conflict_paths = [{ file = \"home/.claude.json\", dialect = \
                 \"claude-project-json\" }]\n",
            ),
            Origin::Downloaded,
        )
        .expect("inline conflict paths");
        assert_eq!(
            inline.rows[0]
                .mcp
                .as_ref()
                .expect("an mcp block")
                .conflict_paths
                .len(),
            1
        );

        // Every refusal names the entry, and refuses the FILE.
        for (body, needle) in [
            (
                "\n[[harness.mcp.conflict_paths]]\ndialect = \"cursor-json\"\n",
                "file: missing",
            ),
            (
                "\n[[harness.mcp.conflict_paths]]\nfile = \"home/.x.json\"\n",
                "dialect: missing",
            ),
            (
                "\n[[harness.mcp.conflict_paths]]\nfile = \"home/.x.json\"\ndialect = \
                 \"telepathy\"\n",
                "telepathy",
            ),
            (
                "\n[[harness.mcp.conflict_paths]]\nfile = \"home/.x.json\"\ndialect = \
                 \"cursor-json\"\nselector = 7\n",
                "selector: expected a string",
            ),
            (
                "\n[[harness.mcp.conflict_paths]]\nfile = \"/etc/claude.json\"\ndialect = \
                 \"cursor-json\"\n",
                "absolute",
            ),
            ("conflict_paths = \"nope\"\n", "expected an array"),
        ] {
            let e = parse_registry(&with(body), Origin::Downloaded).expect_err(body);
            assert!(e.message().contains(needle), "{body}: {e}");
            assert!(e.message().contains("conflict_paths"), "{body}: {e}");
        }
    }

    /// The one row that names them today: Claude Code reads `~/.claude.json` at user scope and once
    /// per project it remembers, so both slots are read before a placement — under both roots the
    /// file can sit at.
    #[test]
    fn claude_code_names_the_file_it_also_reads_servers_from() {
        let claude = bundled_harnesses()
            .iter()
            .find(|h| h.slug == "claude-code")
            .expect("claude-code is in the table");
        let paths: Vec<(String, String)> = claude
            .mcp
            .as_ref()
            .expect("an mcp block")
            .conflict_paths
            .iter()
            .map(|c| (c.file.raw(), c.selector.to_owned()))
            .collect();
        assert_eq!(
            paths,
            [
                ("home/.claude.json".to_owned(), "mcpServers".to_owned()),
                (
                    "home/.claude.json".to_owned(),
                    "projects.*.mcpServers".to_owned()
                ),
                (
                    "claudeHome/.claude.json".to_owned(),
                    "mcpServers".to_owned()
                ),
                (
                    "claudeHome/.claude.json".to_owned(),
                    "projects.*.mcpServers".to_owned()
                ),
            ]
        );
        // …and no other row claims to read a file topos does not write.
        for row in bundled_harnesses() {
            if row.slug == "claude-code" {
                continue;
            }
            assert!(
                row.mcp.as_ref().is_none_or(|m| m.conflict_paths.is_empty()),
                "{}: an unexplained conflict path",
                row.slug
            );
        }
    }

    #[test]
    fn a_missing_or_mistyped_key_names_the_key_and_the_row() {
        let text = one_row("cursor", "9.0.0").replace("display_name = \"X\"\n", "");
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("display_name"), "{e}");
        assert!(e.message().contains("cursor"), "{e}");

        let text =
            one_row("cursor", "9.0.0").replace("project_dir = \".x/skills\"", "project_dir = 7");
        let e = parse_registry(&text, Origin::Downloaded).expect_err("refused");
        assert!(e.message().contains("expected a string"), "{e}");
    }

    // ---- the three-level resolution -----------------------------------------------------------

    #[test]
    fn the_override_wins_whatever_its_version_says() {
        // Deliberately OLDER than the bundled table: an override is the person's decision, not a
        // race the newest copy wins.
        let (rows, warnings) = resolve(Some(candidate(&one_row("cursor", "0.0.1"))), None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "cursor");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_refused_override_falls_to_the_downloaded_copy_with_one_warning() {
        let (rows, warnings) = resolve(
            Some(candidate("not = = toml")),
            Some(candidate(&one_row("cursor", &newer_than_bundled()))),
        );
        assert_eq!(rows.len(), 1, "the downloaded copy answered");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("override"), "{warnings:?}");
        assert!(warnings[0].contains("downloaded"), "{warnings:?}");
    }

    #[test]
    fn a_refused_downloaded_copy_falls_to_the_bundled_table_with_one_warning() {
        let (rows, warnings) = resolve(None, Some(candidate("not = = toml")));
        assert_eq!(rows.len(), bundled().rows.len(), "the built-in table");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("built into this topos"),
            "{warnings:?}"
        );
    }

    #[test]
    fn both_levels_refused_downgrades_twice_and_still_answers() {
        let (rows, warnings) = resolve(
            Some(candidate("not = = toml")),
            Some(candidate("also not = = toml")),
        );
        assert_eq!(rows.len(), bundled().rows.len());
        assert_eq!(warnings.len(), 2, "one warning per level: {warnings:?}");
    }

    #[test]
    fn a_downloaded_copy_no_newer_than_the_bundled_table_is_ignored_silently() {
        // The ordinary state after a `self-update`: the binary caught up with what it downloaded.
        for version in [bundled_version().to_string(), "0.0.1".to_owned()] {
            let (rows, warnings) = resolve(None, Some(candidate(&one_row("cursor", &version))));
            assert_eq!(rows.len(), bundled().rows.len(), "{version}");
            assert!(warnings.is_empty(), "{version}: {warnings:?}");
        }
        // Strictly newer, and it is used.
        let (rows, warnings) = resolve(
            None,
            Some(candidate(&one_row("cursor", &newer_than_bundled()))),
        );
        assert_eq!(rows.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// A row the fences SKIPPED is worth one line from the person's own file, and none at all from
    /// the downloaded copy: the served table naming an agent this build does not know is the system
    /// working, and a warning per command until the next `self-update` would be fleet-wide noise on
    /// an expected event. The client's refresher says that one as the new version lands.
    #[test]
    fn a_skipped_row_is_the_overrides_warning_to_say_and_never_the_downloaded_copys() {
        let with_a_new_agent = |version: &str| {
            format!(
                "{}\n[[harness]]\nslug = \"brand-new-agent\"\ndisplay_name = \"New\"\nuser_dirs = \
                 []\nproject_dir = \".new/skills\"\ndetect_dirs = []\n",
                one_row("cursor", version)
            )
        };

        let (rows, warnings) = resolve(Some(candidate(&with_a_new_agent("0.0.1"))), None);
        assert_eq!(rows.len(), 1, "the known row survives");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("brand-new-agent"), "{warnings:?}");

        let (rows, warnings) = resolve(
            None,
            Some(candidate(&with_a_new_agent(&newer_than_bundled()))),
        );
        assert_eq!(rows.len(), 1, "the same row is still skipped");
        assert!(warnings.is_empty(), "silently: {warnings:?}");
    }

    // ---- the bundled-only knob ------------------------------------------------------------------

    /// **The suite reads the table this COMMIT ships.** `.cargo/config.toml` sets
    /// `TOPOS_HARNESS_REGISTRY=bundled` under `[env]`, so every binary cargo builds and runs here —
    /// this suite, the xtask gates, the spawned CLI in the composed e2e — resolves the bundled
    /// table alone. Without it the first data-only version bump would hand a dogfooding developer's
    /// own `~/.topos/harness-registry/registry.toml` to their `cargo test`, and shape pins would
    /// start failing on a property of the laptop rather than of the commit.
    #[test]
    fn everything_cargo_runs_reads_the_bundled_table() {
        assert_eq!(BUNDLED_ONLY_ENV, "TOPOS_HARNESS_REGISTRY");
        assert!(
            bundled_only(),
            "run through cargo, so `[env]` in .cargo/config.toml applies"
        );
        let slugs =
            |table: &'static [KnownHarness]| table.iter().map(|h| h.slug).collect::<Vec<_>>();
        assert_eq!(
            slugs(super::super::known_harnesses()),
            slugs(bundled_harnesses())
        );
    }

    // ---- the warnings' own stream -------------------------------------------------------------

    /// A writer whose reader is gone — every write fails the way a closed pipe fails.
    struct ClosedPipe;
    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the reader is gone",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// **A gone stderr reader silences the warnings; it never ends the run.** The table had
    /// already resolved by the time these lines are said, and `eprintln!` would have panicked the
    /// whole process over a diagnostic nobody was listening to (`topos update --quiet 2> >(head
    /// -1)` is enough to close that pipe).
    #[test]
    fn a_closed_stderr_drops_the_warnings_instead_of_panicking() {
        say(
            &mut ClosedPipe,
            &[
                "topos: the harness registry override at /tmp/x was refused".to_owned(),
                "topos: and the downloaded copy too".to_owned(),
            ],
        );
    }

    #[test]
    fn every_warning_is_said_on_its_own_line() {
        let mut out: Vec<u8> = Vec::new();
        say(&mut out, &["first".to_owned(), "second".to_owned()]);
        assert_eq!(String::from_utf8(out).expect("utf-8"), "first\nsecond\n");
    }

    // ---- the round trip -----------------------------------------------------------------------

    #[test]
    fn rendering_the_parsed_table_re_parses_to_the_same_rows() {
        let rendered = render_table(bundled_version(), bundled_harnesses());
        let again = parse_registry(&rendered, Origin::Bundled).expect("the render re-parses");
        assert_eq!(again.rows, bundled().rows, "field-for-field");
        // …and the rendering is stable: a second pass is byte-identical to the first.
        assert_eq!(render_table(bundled_version(), leak(again.rows)), rendered);
    }

    #[test]
    fn every_row_round_trips_through_its_own_rendering() {
        for row in bundled_harnesses() {
            let text = format!(
                "schema_version = 1\nversion = \"1\"\nmin_engine_version = 1\n\n{}",
                render_row(row)
            );
            let parsed = parse_registry(&text, Origin::Bundled)
                .unwrap_or_else(|e| panic!("{}: {e}", row.slug));
            assert_eq!(parsed.rows.len(), 1, "{}", row.slug);
            let back = &parsed.rows[0];
            assert_eq!(back.slug, row.slug);
            assert_eq!(back.display_name, row.display_name);
            assert_eq!(back.project_dir, row.project_dir);
            assert_eq!(
                back.user_dirs.iter().map(OwnedDir::raw).collect::<Vec<_>>(),
                row.user_dir_specs(),
                "{}",
                row.slug
            );
            assert_eq!(
                back.detect_dirs
                    .iter()
                    .map(OwnedDir::raw)
                    .collect::<Vec<_>>(),
                row.detect_dirs.iter().map(|s| s.raw()).collect::<Vec<_>>(),
                "{}",
                row.slug
            );
            assert_eq!(back.shared_dir, row.shared_dir, "{}", row.slug);
            assert_eq!(back.mcp.is_some(), row.mcp.is_some(), "{}", row.slug);
            if let (Some(back), Some(mine)) = (&back.mcp, &row.mcp) {
                assert_eq!(back.reload_note, mine.reload_note, "{}", row.slug);
                assert_eq!(
                    back.user.as_ref().map(|(d, dialect)| (d.raw(), *dialect)),
                    mine.user.map(|s| (s.dir.raw(), s.dialect)),
                    "{}",
                    row.slug
                );
                assert_eq!(
                    back.project
                        .as_ref()
                        .map(|(p, dialect)| (p.as_str(), *dialect)),
                    mine.project,
                    "{}",
                    row.slug
                );
                assert_eq!(
                    back.conflict_paths
                        .iter()
                        .map(|c| (c.file.raw(), c.dialect, c.selector.clone()))
                        .collect::<Vec<_>>(),
                    mine.conflict_paths
                        .iter()
                        .map(|c| (c.file.raw(), c.dialect, c.selector.to_owned()))
                        .collect::<Vec<_>>(),
                    "{}",
                    row.slug
                );
            }
        }
    }
}
