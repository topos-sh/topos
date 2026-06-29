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
    /// The plane's read side (the signed `current` pointer + version bytes). Fixture-driven in tests; an
    /// inert no-op in production until the HTTP transport lands.
    pub plane: &'a dyn PlaneSource,
    /// The pinned plane public key the signed `current` pointer is verified against. Fixture-supplied
    /// this increment (TOFU key pinning lands with enrollment); the inert production plane never serves a
    /// record, so the placeholder key is never the integrity authority.
    pub plane_key: [u8; 32],
    /// The durable follow-state (which skills are followed, in which mode/workspace). Fixture-driven in
    /// tests; the inert production source follows nothing, so production `pull` is a no-op.
    pub follow: &'a dyn FollowSource,
}
