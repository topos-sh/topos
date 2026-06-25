//! `topos` (client lib) — the 12-verb domain ops over the pure kernel + the gix mechanics + the
//! harness port.
//!
//! The lib holds: the 12-verb domain ops; the `pull` (checkForUpdates → plan → apply) state
//! machine; the **sidecar** (a PARTIAL `topos-gitstore` store + crash-safe JSON docs — the durability
//! protocol); namespace-atomic materialization (a private fs/syscall seam,
//! fault-injectable); the device-flow client + the device-key signer. The bin is `clap` wiring +
//! `--json` (no prompts) + a thin TTY renderer over the SAME typed outcomes.
//!
//! **Depends on NO SQL and NO `plane-store`** — the client is a thin sync tool, never an authority.
//! The 12 verbs: `add` `follow` `unfollow` `pull` `list` `diff` `log` `publish`
//! `revert` `review` `invite` (+ `publish --propose`).

/// The 12-verb surface lands here later (the local accountless core first).
pub const CLIENT_READY: bool = false;
