//! The MANIFEST model — the demand side of "demand ∩ entitlement".
//!
//! A scope IS a manifest. Four layers, resolved nearest-first:
//!
//! 1. this folder's `topos.toml` (committed; travels with the repo),
//! 2. ancestor folders' `topos.toml` (monorepo nesting; walking up from cwd, nearest wins),
//! 3. per-workspace PROFILES — the person's manifest for each workspace, stored ON THE SERVER
//!    (they roam and are web-editable; the delivery answer is the profile already resolved
//!    server-side, so the client treats each session's delivery as one ready-made layer),
//! 4. the local personal manifest (`~/.topos/topos.toml`) — machine-local personal bundles.
//!
//! Manifest entries are REFERENCES ([`refs`]) with optional version pins; `*` = track `current`
//! silently. Negative state exists in exactly ONE form: an `exclude` entry in a manifest —
//! removing an item a broader layer provides records an exclude in your layer, and a nearer
//! layer's mention (include or exclude) shadows every broader one. There is no other negative
//! state anywhere.
//!
//! The submodules: [`refs`] — the shape-determined reference grammar; [`file`] — the
//! `topos.toml` read/edit (format-preserving, comments survive); [`walk`] — layer discovery
//! from a working directory; [`resolve`] — the nearest-wins combination the verbs and the
//! reconcile run.

pub(crate) mod file;
pub(crate) mod refs;
pub(crate) mod resolve;
pub(crate) mod walk;
