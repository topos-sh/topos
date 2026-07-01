//! The local, accountless verbs over the kernel + the embedded-git store + the crash-safe sidecar.

mod add;
mod contribute;
mod diff;
mod follow;
mod invite;
mod list;
mod log;
mod merge_resolve;
mod publish;
mod pull;
mod revert;
mod review;
mod sync_engine;
mod unfollow;
mod uninstall;

pub(crate) use add::add;
pub(crate) use diff::diff;
pub(crate) use follow::{FollowConnectors, FollowOpts, follow};
pub(crate) use invite::invite;
pub(crate) use list::list;
pub(crate) use log::log;
pub(crate) use publish::{PublishOutcome, publish};
pub(crate) use pull::{PullScope, TargetMode, pull};
pub(crate) use revert::revert;
pub(crate) use review::review;
pub(crate) use unfollow::unfollow;
pub(crate) use uninstall::{UninstallOutcome, uninstall};

use topos_types::persisted::Lock;

use crate::ctx::Ctx;
use crate::doc;
use crate::error::ClientError;

/// Resolve a skill name to its `(id, lock)` across the tracked skills. A name is the user-facing handle;
/// two same-name skills are distinct, so an ambiguous name is a typed error carrying the count.
fn resolve_skill(ctx: &Ctx<'_>, name: &str) -> Result<(String, Lock), ClientError> {
    let mut matches: Vec<(String, Lock)> = Vec::new();
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        if let Some(lock) = doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(id).lock)?
            && lock.name == name
        {
            matches.push((id.to_owned(), lock));
        }
    }
    // Deterministic across same-name skills.
    matches.sort_by(|a, b| a.0.cmp(&b.0));
    match matches.len() {
        0 => Err(ClientError::NoSuchSkill {
            name: name.to_owned(),
        }),
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        count => Err(ClientError::AmbiguousName {
            name: name.to_owned(),
            count,
        }),
    }
}

/// Parse 64 lowercase-hex chars into a 32-byte id (a sidecar doc field) via the shared `hex` codec.
/// Fails **closed** on uppercase: the persisted + result schemas pin `^[0-9a-f]{64}$`, and `diff` echoes
/// the original string straight into its `--json`, so an uppercase byte (which `hex::decode_to_slice`
/// would accept case-insensitively) must be rejected here, not passed through as schema-invalid output.
pub(crate) fn parse_hex32(hex_str: &str) -> Result<[u8; 32], ClientError> {
    if hex_str.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(ClientError::Corrupt("hex id must be lowercase".into()));
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut out)
        .map_err(|e| ClientError::Corrupt(format!("invalid hex id: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::parse_hex32;

    #[test]
    fn parse_hex32_is_lowercase_only_and_length_checked() {
        // 64 lowercase hex chars round-trips.
        assert!(parse_hex32(&"abcdef0123456789".repeat(4)).is_ok());
        // Uppercase must fail closed — the schema pins lowercase and `diff` echoes the raw string.
        assert!(parse_hex32(&"ABCDEF0123456789".repeat(4)).is_err());
        // Wrong length and non-hex are rejected by the codec.
        assert!(parse_hex32("abc").is_err());
        assert!(parse_hex32(&"g".repeat(64)).is_err());
    }
}
