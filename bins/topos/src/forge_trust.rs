//! `state/forge_trust.json` — the MACHINE's forge first-trust registry.
//!
//! Consent to fetch and install from an external forge origin is a MACHINE fact, never a checkout
//! fact: it is granted exactly once, at `add`'s consent moment (the accepted describe / `--yes`),
//! and recorded here in the HOME sidecar. It is deliberately NOT derived from store contents — a
//! project's `.topos/` store travels with the checkout, so a malicious repository can commit
//! valid-looking `lock.json`/`origin.json` documents; store contents are demand-shaped evidence
//! anyone could have authored, and they must never open the first-trust gate. Both the `add`
//! predicate and the reconcile's forge arms consult ONLY this registry, in every scope.
//!
//! Legacy machines SEED once: origins already imported in the HOME store passed the add gate
//! historically, so the first consult copies them in and sets the durable `seeded` flag — from
//! then on the registry is the single authority (deleting a home import does not revoke; only the
//! registry speaks). A PLAIN document (origins, never a secret) through the ordinary crash-safe
//! [`crate::doc`] writers (atomic, fail-closed schema).
//!
//! UNDECIPHERABLE IS NOT EMPTY. A corrupt document — or one written by a NEWER build, which
//! [`crate::doc::read_doc`] refuses rather than guesses at — answers **no grants** (a trust
//! question must fail closed) and REFUSES every write with the typed upgrade-required error. An
//! older binary must never overwrite a newer registry: silently re-seeding an empty document over
//! it would delete consent the person really gave, and then re-grant it without asking.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::ctx::Ctx;
use crate::doc;
use crate::error::ClientError;

/// The whole document: the granted origins (`<host>/<owner>/<repo>`), plus the one-time seed
/// marker. A `BTreeSet`, so the on-disk bytes are deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ForgeTrust {
    #[serde(default)]
    pub schema_version: u32,
    /// Whether the one-time home-store seed ran. Set on the first persisted write; once true the
    /// registry never re-reads store contents.
    #[serde(default)]
    pub seeded: bool,
    /// The granted origin sources (`github.com/<owner>/<repo>`).
    #[serde(default)]
    pub origins: BTreeSet<String>,
}

/// Read the registry through the one-time seed: an unseeded (or absent) document takes the HOME
/// store's already-imported origins as granted — they passed the add gate historically. Reads
/// only; the caller decides whether to persist. `ctx` must be the BASE ctx (home layout).
///
/// # Errors
/// The document is UNDECIPHERABLE — corrupt, or a schema version this build does not know
/// ([`ClientError::UnknownSchemaVersion`], the upgrade-required answer). Never `Ok` with an empty
/// document in that case: an empty answer would be indistinguishable from "nothing granted yet",
/// and the caller would write over bytes it could not read.
fn read_with_seed(ctx: &Ctx<'_>) -> Result<(ForgeTrust, bool), ClientError> {
    let mut doc: ForgeTrust =
        doc::read_doc(ctx.fs, &ctx.layout.forge_trust_path())?.unwrap_or_default();
    if doc.seeded {
        return Ok((doc, false));
    }
    for import in crate::ops::forge_imports(ctx) {
        doc.origins.insert(import.origin.source);
    }
    doc.seeded = true;
    Ok((doc, true))
}

/// Whether `origin` (`<host>/<owner>/<repo>`) has passed this machine's first-trust gate. The
/// one-time legacy seed is persisted best-effort on the way through (a failed write just re-seeds
/// next time — the trust answer is the same either way).
///
/// FAILS CLOSED: an unreadable/undecipherable registry grants NOTHING. The caller's refusal names
/// the `topos add … --yes` gate, and that gate's own [`grant`] surfaces the real reason.
pub(crate) fn is_trusted(ctx: &Ctx<'_>, origin: &str) -> bool {
    let Ok((doc, seeded_now)) = read_with_seed(ctx) else {
        return false;
    };
    if seeded_now {
        let _ = write(ctx, &doc);
    }
    doc.origins.contains(origin)
}

/// Record `origin` as granted — `add`'s consent moment (the accepted describe / `--yes`).
/// Idempotent.
///
/// # Errors
/// [`ClientError::UnknownSchemaVersion`] (or [`ClientError::Corrupt`]) when the standing registry
/// cannot be read — the write is REFUSED rather than clobbering a document this build does not
/// understand; otherwise the filesystem failure persisting the registry (consent that cannot be
/// recorded must not be silently assumed recorded).
pub(crate) fn grant(ctx: &Ctx<'_>, origin: &str) -> Result<(), ClientError> {
    let (mut doc, _) = read_with_seed(ctx)?;
    doc.origins.insert(origin.to_owned());
    write(ctx, &doc)
}

/// Persist through the ordinary atomic writers (`state/` is created on demand).
fn write(ctx: &Ctx<'_>, doc_val: &ForgeTrust) -> Result<(), ClientError> {
    ctx.fs.create_dir_all(&ctx.layout.state_dir())?;
    let mut doc_val = doc_val.clone();
    doc_val.schema_version = PERSISTED_SCHEMA_VERSION;
    doc::write_doc(ctx.fs, &ctx.layout.forge_trust_path(), &doc_val)
}
