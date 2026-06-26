//! The local, accountless verbs over the kernel + the embedded-git store + the crash-safe sidecar.

mod add;
mod diff;
mod list;
mod log;
mod uninstall;

pub(crate) use add::add;
pub(crate) use diff::diff;
pub(crate) use list::list;
pub(crate) use log::log;
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

/// Parse 64 lowercase-hex chars into a 32-byte id (a sidecar doc field).
fn parse_hex32(hex: &str) -> Result<[u8; 32], ClientError> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(ClientError::Corrupt("expected 64 hex chars".into()));
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(bytes[2 * i])?;
        let lo = hex_val(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, ClientError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(ClientError::Corrupt("non-hex character".into())),
    }
}
