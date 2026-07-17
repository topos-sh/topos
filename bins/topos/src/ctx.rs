//! The verb execution context — the injectable seams (filesystem, ids, clock, device identity, the
//! `~/.topos/` layout). Production wires the real seams; tests inject deterministic ones.

use std::path::PathBuf;

use topos_harness::HarnessAdapter;

use crate::fs_seam::FsOps;
use crate::ids::{Clock, IdSource};
use crate::plane::{FollowSource, PlaneSource};
use crate::sidecar::Layout;

/// The machine roots agent DETECTION and shared-dir placement resolve against — the user's home dir
/// (`$HOME`, resolved once at the composition root) and the project cwd. Injected so tests never
/// probe the developer's real machine: `Ctx::roots = None` keeps the classic single-dir placement
/// (the active adapter's), which is also the honest degraded behavior with no `$HOME`.
#[derive(Debug, Clone)]
pub(crate) struct AgentRoots {
    pub home: PathBuf,
    pub cwd: Option<PathBuf>,
}

/// Everything a verb needs, behind seams so the same code is deterministic under test and real in prod.
pub(crate) struct Ctx<'a> {
    pub fs: &'a dyn FsOps,
    pub ids: &'a dyn IdSource,
    pub clock: &'a dyn Clock,
    /// The device identity that authors local commits (a controlled-ASCII token, e.g. `d_<hex>`).
    pub device_id: String,
    pub layout: Layout,
    /// The harness adapter (Claude Code today): discovery, placement targeting, and the content-blind
    /// auto-update-trigger (un)install. Content-blind — it never sees a skill's bytes.
    pub harness: &'a dyn HarnessAdapter,
    /// The plane's read side (the unsigned `current` pointer + version bytes). The real `ureq` transport
    /// when enrolled; the inert no-op before any enrollment; fixture-driven in tests. Integrity is the
    /// content-addressed `version_id`, re-verified by digest on every apply — never a pointer signature.
    pub plane: &'a dyn PlaneSource,
    /// The durable follow-state (which skills are followed, in which mode/workspace) — `follows.json`
    /// when enrolled; the inert source (nothing followed) before that; fixture-driven in tests.
    pub follow: &'a dyn FollowSource,
    /// The machine roots the placement engine detects agents against (`None` = no detection: the
    /// classic active-adapter placement — production with no `$HOME`, and every test that does not
    /// exercise the engine).
    pub roots: Option<AgentRoots>,
}
