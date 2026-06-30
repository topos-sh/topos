//! The skill-scoped object read — the one auditable access surface.
//!
//! Authorization is one database join that yields a *witness* commit (or nothing); only then is the
//! per-workspace git store touched, to fetch the bytes by content id. There is no read-by-bare-hash
//! path anywhere, and the two outcomes are kept textually separate so the distinction cannot rot: an
//! empty join is the single not-found; a store failure on an already-authorized object is a corruption
//! alarm, never a not-found.

use std::collections::HashMap;

use topos_core::digest::{self, FileMode, ManifestEntry, RejectReason};
use topos_gitstore::{LargeObjectStore, RenderedBundle, RenderedFile};
use topos_types::{Generation, SignedCurrentRecord};

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};
use crate::sqlite::Location;

pub(crate) async fn read_object(
    authority: &Authority,
    principal: &Principal,
    ws: &WorkspaceId,
    skill: &SkillId,
    object_id: ObjectId,
) -> Result<Vec<u8>> {
    // Step one (async DB): authorize. The witness commit proves BOTH facts at once — the principal is
    // rostered for the skill, and that skill reaches the object. The borrow on the database is released
    // before the synchronous store read below (no git borrow ever crosses an await).
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

    // Step two: fetch + verify the bytes from the store the database records, dispatching on `location`. The
    // witness already proved reachability, so a clean run has no benign miss: a post-authz failure is a
    // provenance/store divergence (corruption) → an Integrity fault, kept distinct from the not-found path
    // (so the indistinguishable 404 holds across the large-object surface), never by bare hash.
    let fetched = match authority.db().object_dispatch(ws, object_id).await? {
        // Offloaded: fetch from the large store (its `get` re-verifies sha256 == object_id).
        Some((Location::LargeLocal, _)) => authority
            .large_store(ws)
            .get(object_id.0)
            .map_err(AuthorityError::integrity),
        // Git-resident: read the loose object DIRECTLY by its locator and re-verify the content id — NOT a
        // whole-version-tree walk, which would fault on an offloaded sibling's absent git object in a mixed
        // bundle before reaching the requested blob.
        Some((Location::Git, git_oid)) => {
            let store = authority.open_store(ws)?;
            store
                .read_git_blob_verified(git_oid)
                .map_err(AuthorityError::integrity)
                .and_then(|(bytes, content_sha256)| {
                    if content_sha256 == object_id.0 {
                        Ok(bytes)
                    } else {
                        Err(AuthorityError::integrity(GitLocatorMismatch))
                    }
                })
        }
        // No live presence row: a legacy straight-to-git object — its version is all-git, so the tree walk is
        // safe — read by content id from the witness version. (A reclaimed object also lands here, because
        // `object_dispatch` filters `status = 'present'`; the re-authorize guard below catches that case.)
        None => {
            let store = authority.open_store(ws)?;
            store
                .read_object_in_version(witness.0, object_id.0)
                .map_err(AuthorityError::integrity)
        }
    };

    // Re-authorize-on-miss (the read-time TOCTOU guard). The authorization above and this fetch are two
    // steps; between them a proposal can go stale (an eventless derived transition — a concurrent publish
    // advances `current`) or be rejected, and a GC can then reclaim the proposal's now-unrooted unique bytes.
    // Any of the fetch arms would then fault Integrity over bytes that are gone BY DESIGN, not corrupt. So on
    // a post-authz failure, re-run the authorization: if the object is no longer authorized, it was legitimately
    // reclaimed → the indistinguishable `NotFound` (404), preserving "reclaimed ⇒ 404, never Integrity" across
    // the window. A still-authorized object that fails to load IS genuine corruption → the Integrity fault stands.
    if let Err(AuthorityError::Integrity(_)) = &fetched
        && authority
            .db()
            .authorize_object_read(ws, skill, principal, object_id)
            .await?
            .is_none()
    {
        return Err(AuthorityError::NotFound);
    }
    fetched
}

#[derive(Debug, thiserror::Error)]
#[error("a present object's git locator does not resolve to its content id")]
struct GitLocatorMismatch;

// ── the authenticated read surface (resolve a read token → an opaque scope → the bound reads) ───────────

/// An **opaque read capability** — the (workspace, skill, principal) a presented read token resolves to.
///
/// The fields are private on purpose: a consumer (the HTTP layer) holds this as a token and passes it back to
/// the bound reads ([`serve_object`] / [`read_current`] / [`read_version_metadata`]); it never inspects the
/// principal (no public accessor exposes it), so the credential cannot be re-used to forge a different scope.
/// Built ONLY by [`resolve_read_token`] from a trusted database row — never by parsing a client value.
#[derive(Debug, Clone)]
pub struct ReadScope {
    ws: WorkspaceId,
    skill: SkillId,
    principal: Principal,
}

impl ReadScope {
    /// The resolved workspace (`pub(crate)` — internal reads bind it; no public accessor).
    pub(crate) fn ws(&self) -> &WorkspaceId {
        &self.ws
    }
    /// The resolved skill (`pub(crate)`).
    pub(crate) fn skill(&self) -> &SkillId {
        &self.skill
    }
    /// The resolved principal (`pub(crate)` — never public: the scope stays an opaque capability).
    pub(crate) fn principal(&self) -> &Principal {
        &self.principal
    }
}

/// A skill's signed `current` pointer, ready to serve: the raw `SignedCurrentRecord` bytes a follower
/// verifies, plus the `(epoch, seq)` AND the `version_id` extracted from them (so the caller can build a
/// **commit-sensitive** ETag / `304` — a clean field comparison against the client's known commit — without
/// re-parsing the blob in the handler).
#[derive(Debug, Clone)]
pub struct CurrentPointer {
    pub generation: Generation,
    /// The commit id `current` names — pulled from the deserialized `record.record.version_id` so the
    /// current handler can compare it to the client's `Topos-Known-Version-Id` for the commit-sensitive 304.
    pub version_id: [u8; 32],
    pub signed_record: Vec<u8>,
}

/// One file of a version's metadata — its bundle-relative path, mode, and content id (`object_id`). The
/// bytes are NOT here: a client fetches each by id through [`serve_object`].
#[derive(Debug, Clone)]
pub struct VersionFile {
    pub path: String,
    pub mode: FileMode,
    pub object_id: [u8; 32],
}

/// A version's authenticated metadata — its id, the COMPLETE parent set, display author + message, the
/// consent `bundle_digest`, and the per-file `(path, mode, object_id)` leaves. Assembled WITHOUT reading any
/// blob bytes (a client walks the files via [`serve_object`]); the digest is the pin those byte fetches +
/// the client's own re-hash must reproduce.
#[derive(Debug, Clone)]
pub struct VersionMeta {
    pub version_id: [u8; 32],
    pub parents: Vec<[u8; 32]>,
    pub author: String,
    pub message: String,
    pub bundle_digest: [u8; 32],
    pub files: Vec<VersionFile>,
}

/// Resolve a presented read token to its opaque [`ReadScope`] — the read-credential resolver.
///
/// Hashes the token (the table stores ONLY the sha256 — the plaintext is a `0600` secret at rest on the
/// follower, never recoverable from a database read) and does one indexed lookup on the hash. A miss is the
/// single indistinguishable [`AuthorityError::NotFound`], so a caller can never probe which tokens,
/// workspaces, or skills exist; a stored row that fails to re-parse is store corruption (handled in
/// [`crate::sqlite`], not surfaced as not-found).
///
/// # Errors
/// [`AuthorityError::NotFound`] on an unknown token; [`AuthorityError::Internal`] on a database fault;
/// [`AuthorityError::Integrity`] if a stored row is corrupt.
pub(crate) async fn resolve_read_token(authority: &Authority, token: &str) -> Result<ReadScope> {
    let token_sha256 = digest::sha256(token.as_bytes());
    match authority.db().lookup_read_token(&token_sha256).await? {
        Some((ws, skill, principal)) => Ok(ReadScope {
            ws,
            skill,
            principal,
        }),
        None => Err(AuthorityError::NotFound),
    }
}

/// Read a skill's signed `current` pointer for an authenticated scope. `None` until the pointer has been
/// moved (signed). Reuses the unauthenticated [`Authority::read_signed_record`] for the raw bytes, then
/// extracts the generation from the deserialized record.
///
/// # Errors
/// [`AuthorityError::Integrity`] if the stored record blob is unparseable — corruption, NEVER a not-found
/// (the record exists; it is the STORE that is wrong); [`AuthorityError::Internal`] on a database fault.
pub(crate) async fn read_current(
    authority: &Authority,
    scope: &ReadScope,
) -> Result<Option<CurrentPointer>> {
    let Some(signed_record) = authority
        .read_signed_record(scope.ws(), scope.skill())
        .await?
    else {
        return Ok(None);
    };
    let record: SignedCurrentRecord =
        serde_json::from_slice(&signed_record).map_err(AuthorityError::integrity)?;
    // Pull the version_id (hex64 → [u8;32]) alongside the generation: the record exists, so a malformed
    // version_id field is store corruption (an Integrity fault), never a not-found.
    let version_id = parse_hex32(&record.record.version_id)
        .ok_or_else(|| AuthorityError::integrity(BadVersionIdHex))?;
    Ok(Some(CurrentPointer {
        generation: record.record.generation,
        version_id,
        signed_record,
    }))
}

#[derive(Debug, thiserror::Error)]
#[error("a stored signed record carries a malformed version_id")]
struct BadVersionIdHex;

/// Serve one object's bytes for an authenticated scope, asserting the scope's `(ws, skill)` matches the
/// request path's. A scope/path mismatch — or a malformed object id — is the single indistinguishable
/// [`AuthorityError::NotFound`] (the capability is bound to exactly one skill; a bad hex id is never a `400`
/// from here, so a caller cannot probe). Then the read goes through the skill-scoped [`read_object`].
///
/// # Errors
/// [`AuthorityError::NotFound`] on a scope/path mismatch, a malformed id, or a not-reachable object;
/// [`AuthorityError::Integrity`]/[`AuthorityError::Internal`] as [`read_object`].
pub(crate) async fn serve_object(
    authority: &Authority,
    scope: &ReadScope,
    req_ws: &str,
    req_skill: &str,
    object_id_hex: &str,
) -> Result<Vec<u8>> {
    if scope.ws().as_str() != req_ws || scope.skill().as_str() != req_skill {
        return Err(AuthorityError::NotFound);
    }
    let Some(object_id) = parse_hex32(object_id_hex) else {
        return Err(AuthorityError::NotFound);
    };
    read_object(
        authority,
        scope.principal(),
        scope.ws(),
        scope.skill(),
        ObjectId(object_id),
    )
    .await
}

/// Read a version's authenticated metadata for a scope (the version-metadata route's core). Asserts the
/// scope/path match, parses the version id (a bad hex is the uniform not-found), R1-authorizes the version
/// read, then assembles the metadata WITHOUT reading any blob bytes.
///
/// Authorization is [`crate::sqlite::Db::authorize_version_read`] (rostered ∧ accepted-trunk-or-open-non-
/// stale-proposal); an empty/unauthorized result is the single indistinguishable [`AuthorityError::NotFound`]
/// (never a `403`, never a probe). Every fault in the assembly below is reachable ONLY after authz, so an
/// [`AuthorityError::Integrity`] there discloses nothing about existence (mirroring [`read_object`]).
///
/// # Errors
/// [`AuthorityError::NotFound`] on scope/path mismatch, a bad id, or an unauthorized/unreachable version;
/// [`AuthorityError::Integrity`] on a provenance/store divergence (a missing digest, an unmapped parent, a
/// tree leaf with no recorded object); [`AuthorityError::Internal`] on a database fault.
pub(crate) async fn read_version_metadata(
    authority: &Authority,
    scope: &ReadScope,
    req_ws: &str,
    req_skill: &str,
    version_id_hex: &str,
) -> Result<VersionMeta> {
    if scope.ws().as_str() != req_ws || scope.skill().as_str() != req_skill {
        return Err(AuthorityError::NotFound);
    }
    let Some(version_id) = parse_hex32(version_id_hex) else {
        return Err(AuthorityError::NotFound);
    };
    let commit = CommitId(version_id);
    // R1: rostered ∧ (accepted-trunk OR open-non-stale proposal). Unauthorized/unreachable → the one not-found.
    if !authority
        .db()
        .authorize_version_read(scope.ws(), scope.skill(), scope.principal(), commit)
        .await?
    {
        return Err(AuthorityError::NotFound);
    }

    // Authorized — assemble. All async DB reads run FIRST so the synchronous git-store borrow below never
    // crosses an await (mirrors `read_object`). An authorized version always carries a recorded digest; its
    // absence is a provenance divergence (corruption), never a not-found.
    let bundle_digest = authority
        .db()
        .skill_commit_bundle_digest(scope.ws(), scope.skill(), commit)
        .await?
        .ok_or_else(|| AuthorityError::integrity(MissingProvenanceDigest))?;
    let by_git_oid = authority.db().objects_by_git_oid(scope.ws()).await?;

    let store = authority.open_store(scope.ws())?;
    let node = store
        .read_commit_meta(version_id)
        .map_err(AuthorityError::integrity)?;
    let leaves = store
        .read_tree_structure(version_id)
        .map_err(AuthorityError::integrity)?;

    let mut files = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        // Each tree-entry git OID joins to its content id over the workspace's PRESENT rows. A leaf with no
        // present row is a provenance/store divergence — reachable only after authz, so it discloses nothing.
        let object_id = by_git_oid
            .get(&leaf.git_oid)
            .copied()
            .ok_or_else(|| AuthorityError::integrity(VersionObjectMissing))?;
        files.push(VersionFile {
            path: leaf.path,
            mode: leaf.mode,
            object_id,
        });
    }
    Ok(VersionMeta {
        version_id: node.version_id,
        parents: node.parents,
        author: node.author,
        message: node.message,
        bundle_digest,
        files,
    })
}

/// Parse EXACTLY 64 lowercase-hex characters into a 32-byte content id. `None` on any other length or a
/// non-lowercase-hex byte — the read routes map this to the uniform not-found (a malformed id is never a
/// distinguishable error, and a non-canonical spelling is simply not a known id).
fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("an authorized version has no recorded bundle digest")]
struct MissingProvenanceDigest;

#[derive(Debug, thiserror::Error)]
#[error("a version's tree leaf has no present object row")]
struct VersionObjectMissing;

/// Assemble + verify a whole bundle for a version, dispatching each file to the store the database records.
///
/// **Tree-driven** — the fenced migrate writes no `commit_object` edges, so render anchors on the version's
/// git **tree structure** (`(path, mode, git_oid)` per file), not reachability. The offloaded subset is the
/// workspace's present `large-local` rows, joined in memory by `git_oid → object_id`; each file's bytes come
/// from the large store (offloaded) or git (git-resident / legacy), re-verified to its content id; the
/// recomputed `bundle_digest` must then equal the pin. Offload never forks identity (the digest is over real
/// bytes) and never adds a pointer object. **Authorization is the caller's job** (mirrors [`read_object`]:
/// authorize first, then assemble) — this is the assembly primitive the future read-bundle / review-diff op
/// builds on; it is test-driven this increment (no public verb yet), like the rest of the fence.
///
/// # Errors
/// [`AuthorityError::Integrity`] if a file's bytes are missing/corrupt in either store, a stored path is
/// illegal, or the recomputed digest does not match `expected_bundle_digest`; [`AuthorityError::Internal`]
/// on a database fault.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn render_version(
    authority: &Authority,
    ws: &WorkspaceId,
    version_id: [u8; 32],
    expected_bundle_digest: [u8; 32],
) -> Result<RenderedBundle> {
    // The offloaded set for this workspace: git_oid -> object_id (small — big blobs are rare). A git-resident
    // leaf is absent from this map and recovers its id by rehashing the git blob, with no DB dependency. Read
    // it FIRST (the only `.await` here) so the non-`Send` gix `Store` opened below is never held across an
    // await — keeping every authority future that renders `Send` (axum's handlers require it).
    let offloaded: HashMap<[u8; 20], [u8; 32]> = authority
        .db()
        .large_local_objects(ws)
        .await?
        .into_iter()
        .map(|(git_oid, object_id)| (git_oid, object_id.0))
        .collect();

    let store = authority.open_store(ws)?;
    let structure = store
        .read_tree_structure(version_id)
        .map_err(AuthorityError::integrity)?;

    let mut files = Vec::with_capacity(structure.len());
    let mut manifest = Vec::with_capacity(structure.len());
    for leaf in structure {
        let (bytes, content_sha256) = match offloaded.get(&leaf.git_oid) {
            Some(&object_id) => {
                // Offloaded: fetch from the large store (its `get` re-verifies sha256 == object_id).
                let bytes = authority
                    .large_store(ws)
                    .get(object_id)
                    .map_err(AuthorityError::integrity)?;
                (bytes, object_id)
            }
            None => store
                .read_git_blob_verified(leaf.git_oid)
                .map_err(AuthorityError::integrity)?,
        };
        manifest.push(ManifestEntry {
            path: leaf.path.clone(),
            mode: leaf.mode,
            content_sha256,
        });
        files.push(RenderedFile {
            path: leaf.path,
            mode: leaf.mode,
            bytes,
            content_sha256,
        });
    }

    // Recompute the consent digest over the assembled real bytes and assert it equals the pin — the integrity
    // gate that makes "reviewed-bytes == run-bytes" hold regardless of which store each blob came from.
    let recomputed = digest::bundle_digest(&manifest)
        .map_err(|r| AuthorityError::integrity(RenderPathRejected(r)))?;
    if recomputed != expected_bundle_digest {
        return Err(AuthorityError::integrity(RenderDigestMismatch));
    }
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(RenderedBundle {
        files,
        bundle_digest: recomputed,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("recomputed bundle digest does not match the pinned digest")]
struct RenderDigestMismatch;

#[derive(Debug, thiserror::Error)]
#[error("a rendered bundle path was rejected by the canonical rules: {0:?}")]
struct RenderPathRejected(RejectReason);
