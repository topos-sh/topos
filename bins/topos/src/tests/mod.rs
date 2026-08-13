//! The multi-file test suites (unit tests stay inline in their production modules as
//! `#[cfg(test)] mod tests`). Each file here is a cross-module suite over the client's public seams:
//! the built-in `topos` skill's placement and custody, the identity claim (`add --as`) and its
//! inverse, the crash/durability gate, the manifest reconcile over fakes, the `--kind mcp` doors,
//! the MCP placement engine's ownership custody, `publish`'s auto-add pre-step, the pull/apply sync
//! engine, the verb surface, and `verify`'s live check against stub servers.

mod builtin_skill;
mod claim;
mod durability;
mod manifest_reconcile;
mod mcp_add;
mod mcp_engine;
mod publish_autoadd;
mod sync;
mod verbs;
mod verify;
