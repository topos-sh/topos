//! The verb execution context — the injectable seams (filesystem, ids, clock, device identity, the
//! `~/.topos/` layout). Production wires the real seams; tests inject deterministic ones.

use topos_harness::HarnessAdapter;

use crate::fs_seam::FsOps;
use crate::ids::{Clock, IdSource};
use crate::plane::{FollowSource, PlaneSource};
use crate::sidecar::Layout;

/// Everything a verb needs, behind seams so the same code is deterministic under test and real in prod.
pub(crate) struct Ctx<'a> {
    pub fs: &'a dyn FsOps,
    pub ids: &'a dyn IdSource,
    pub clock: &'a dyn Clock,
    /// The device identity that authors local commits (a controlled-ASCII token, e.g. `d_<hex>`).
    pub device_id: String,
    pub layout: Layout,
    /// The harness adapter (Claude Code today): discovery, placement targeting, and the content-blind
    /// currency-trigger (un)install. Content-blind — it never sees a skill's bytes.
    pub harness: &'a dyn HarnessAdapter,
    /// The plane's read side (the signed `current` pointer + version bytes). The real `ureq` transport
    /// when enrolled; the inert no-op before any enrollment; fixture-driven in tests.
    pub plane: &'a dyn PlaneSource,
    /// The pinned plane public key the signed `current` pointer is verified against — TOFU-pinned by
    /// `follow` into `instance.json` and loaded from there when enrolled (all-zero with the inert plane,
    /// which never serves a record, so the placeholder is never the integrity authority).
    pub plane_key: [u8; 32],
    /// The durable follow-state (which skills are followed, in which mode/workspace) — `follows.json`
    /// when enrolled; the inert source (nothing followed) before that; fixture-driven in tests.
    pub follow: &'a dyn FollowSource,
}
