//! `log <skill>` — the local action log (this skill's `log.jsonl` events) followed by the per-skill
//! embedded-git history. The `--team` plane-side audit lands later.

use serde_json::json;
use topos_gitstore::Store;

use super::{parse_hex32, resolve_skill};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::{logfile, sidecar::Layout};
use topos_core::digest::to_hex;
use topos_types::results::LogData;

/// History for `skill`: local action events for this skill, then its git versions (newest first).
///
/// # Errors
/// Name-resolution errors; a store/io failure.
pub(crate) fn log(ctx: &Ctx<'_>, skill: &str) -> Result<LogData, ClientError> {
    let (id, lock) = resolve_skill(ctx, skill)?;

    let mut events: Vec<serde_json::Value> =
        logfile::read_events(ctx.fs, &Layout::log_path(&ctx.layout))?
            .into_iter()
            .filter(|e| e.get("skill_id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .collect();

    let store = Store::open(&ctx.layout.published(&id).store)?;
    for node in store.log(parse_hex32(&lock.base_commit)?)? {
        events.push(json!({
            "action": "version",
            "version_id": to_hex(&node.version_id),
            "author": node.author,
            "message": node.message,
            "parents": node.parents.iter().map(|p| to_hex(p)).collect::<Vec<_>>(),
        }));
    }

    Ok(LogData { events, team: None })
}
