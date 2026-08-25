//! The MCP placement engine + ownership custody, end to end: the per-scope converge over a fake
//! HOME (synthetic descriptors — every surface Home-rooted, so no test can touch a real machine's
//! config), and the reconcile integration over the same fakes the manifest suite drives.
//!
//! The trust spine under test: entries land EXACTLY per dialect and byte-identical to the pure
//! drivers' rendering; a hand-edited entry is drift — never overwritten, never removed, disclosed;
//! a foreign occupant is never touched or claimed; removal converges only what the custody proves
//! ours; the intent journal makes every config write crash-recoverable; a project surface passes
//! the containment rail; capability and narrowing filters withhold placement honestly; and the
//! whole loop — demand → store → config → applied report → offline cache — carries the per-agent
//! states.

mod channel;
mod claude_json;
mod engine;
mod marker;
mod reconcile;
mod rig;
