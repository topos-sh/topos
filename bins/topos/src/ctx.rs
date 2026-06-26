//! The verb execution context — the injectable seams (filesystem, ids, clock, device identity, the
//! `~/.topos/` layout). Production wires the real seams; tests inject deterministic ones.

use crate::fs_seam::FsOps;
use crate::ids::{Clock, IdSource};
use crate::sidecar::Layout;

/// Everything a verb needs, behind seams so the same code is deterministic under test and real in prod.
pub(crate) struct Ctx<'a> {
    pub fs: &'a dyn FsOps,
    pub ids: &'a dyn IdSource,
    pub clock: &'a dyn Clock,
    /// The device identity that authors local commits (a controlled-ASCII token, e.g. `d_<hex>`).
    pub device_id: String,
    pub layout: Layout,
}
