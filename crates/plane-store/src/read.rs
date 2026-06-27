//! The skill-scoped object read — the one auditable access surface.
//!
//! Authorization is one database join that yields a *witness* commit (or nothing); only then is the
//! per-workspace git store touched, to fetch the bytes by content id. There is no read-by-bare-hash
//! path anywhere, and the two outcomes are kept textually separate so the distinction cannot rot: an
//! empty join is the single not-found; a store failure on an already-authorized object is a corruption
//! alarm, never a not-found.

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{ObjectId, Principal, SkillId, WorkspaceId};

pub(crate) async fn read_object(
    authority: &Authority,
    principal: &Principal,
    ws: &WorkspaceId,
    skill: &SkillId,
    object_id: ObjectId,
) -> Result<Vec<u8>> {
    // Step one (async DB): authorize. The witness commit proves BOTH facts at once — the principal is
    // rostered for the skill, and that skill reaches the object. The borrow on the database is released
    // before the synchronous git read below (no git borrow ever crosses an await).
    let witness = match authority
        .db()
        .authorize_object_read(ws, skill, principal, object_id)
        .await?
    {
        Some(witness) => witness,
        // Not rostered, the skill does not reach the object, or the object does not exist — all one
        // indistinguishable not-found.
        None => return Err(AuthorityError::NotFound),
    };

    // Step two (sync git): fetch + verify the bytes from the per-workspace store. The witness already
    // proved reachability, so there is no benign "object not in this version" case left here: ANY
    // failure is a divergence between the authority's provenance and its store (corruption). It maps to
    // an integrity fault, kept distinct from the not-found path, and leaks nothing because it is
    // reachable only after entitlement was proven.
    let store = authority.open_store(ws)?;
    store
        .read_object_in_version(witness.0, object_id.0)
        .map_err(AuthorityError::integrity)
}
