//! `import-local` — the ONE-SHOT cutover from the pre-object-store on-disk layout (per-workspace
//! bare repos + the size-routed large-object root) into the configured object store, run with the
//! vault STOPPED.
//!
//! It does ALL of, idempotently (a rerun resumes):
//! 1. copy every loose object byte-for-byte to its key (skipped when the local store root IS the
//!    old git root — the keys are the same paths);
//! 2. convert every large-store raw file into git-blob framing, verifying BOTH identities
//!    (`sha256(bytes)` = the filename, and the framed blob's git OID = the presence row's
//!    recorded locator), then flip the row's `location`;
//! 3. walk `refs/topos/versions/*` + the commit objects to BACKFILL `git_commit_oid` and
//!    `message` into version rows (upsert; a row already backfilled to a DIFFERENT locator fails
//!    loudly);
//! 4. verify completeness — every non-purged version row carries a non-NULL locator whose commit
//!    object exists in the store; every reachable blob's loose object exists; no live row still
//!    reads a pre-import location — and report per-workspace counts + a final PASS/FAIL.
//!
//! Packfiles are rejected loudly (the vault's repos were loose-only by construction; a packed one
//! was produced by outside tooling and this tool will not guess at it).

use std::path::{Path, PathBuf};

use topos_gitstore::codec;

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{ObjectId, WorkspaceId};

/// One workspace's import counts.
#[derive(Debug, Clone)]
pub struct ImportWorkspaceReport {
    pub workspace_id: String,
    /// Loose objects copied byte-for-byte (0 when the store root IS the old git root).
    pub loose_copied: usize,
    /// Large-store raw files converted to git-blob framing.
    pub large_converted: usize,
    /// Large-store files with no presence row (orphans; skipped, not an error).
    pub large_orphans: usize,
    /// Version rows that gained `git_commit_oid`/`message`.
    pub versions_backfilled: usize,
}

/// The whole import's outcome. `failures` non-empty ⇔ FAIL.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub workspaces: Vec<ImportWorkspaceReport>,
    pub failures: Vec<String>,
}

impl ImportReport {
    /// Whether the import completed and verified whole.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// See the module docs. `git_root`/`large_root` are the OLD layout's roots.
pub(crate) async fn import_local(
    authority: &Authority,
    git_root: &Path,
    large_root: &Path,
) -> Result<ImportReport> {
    let mut failures = Vec::new();

    // The workspace set: every dir under the old git root (skip the reserved `.quarantine`),
    // UNION every workspace the database knows — so a DB-known workspace whose repo dir is
    // missing still gets VERIFIED (and fails loudly there).
    let mut ws_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if git_root.exists() {
        for entry in std::fs::read_dir(git_root).map_err(AuthorityError::internal)? {
            let entry = entry.map_err(AuthorityError::internal)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() && !name.starts_with('.') {
                ws_names.insert(name);
            }
        }
    }
    for ws in authority.db().workspaces_with_objects().await? {
        ws_names.insert(ws.as_str().to_owned());
    }

    // When the LOCAL store root is the old git root itself, the loose objects are already at
    // their keys — the copy is a no-op by construction.
    let in_place = authority.store().local_root_is(git_root);

    let mut workspaces = Vec::new();
    for name in ws_names {
        let ws = match WorkspaceId::parse(&name) {
            Ok(ws) => ws,
            Err(e) => {
                failures.push(format!(
                    "{name}: not a valid workspace id ({e}); refusing to guess"
                ));
                continue;
            }
        };
        let report = import_workspace(
            authority,
            &ws,
            git_root,
            large_root,
            in_place,
            &mut failures,
        )
        .await?;
        workspaces.push(report);
    }
    Ok(ImportReport {
        workspaces,
        failures,
    })
}

async fn import_workspace(
    authority: &Authority,
    ws: &WorkspaceId,
    git_root: &Path,
    large_root: &Path,
    in_place: bool,
    failures: &mut Vec<String>,
) -> Result<ImportWorkspaceReport> {
    let name = ws.as_str().to_owned();
    let repo_dir = git_root.join(ws.as_str());
    let objects_dir = repo_dir.join("objects");

    // Packfiles are refused loudly — the vault only ever wrote loose objects.
    let pack_dir = objects_dir.join("pack");
    if pack_dir.exists() {
        let packs = list_dir(&pack_dir)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "pack" || e == "idx"))
            .count();
        if packs > 0 {
            failures.push(format!(
                "{name}: {} packfile artifact(s) under {} — the vault's repos are loose-only; \
                 unpack them (`git unpack-objects`) before importing",
                packs,
                pack_dir.display()
            ));
            return Ok(ImportWorkspaceReport {
                workspace_id: name,
                loose_copied: 0,
                large_converted: 0,
                large_orphans: 0,
                versions_backfilled: 0,
            });
        }
    }

    // 1. Loose copy, byte-for-byte (skipped in place).
    let mut loose_copied = 0;
    if !in_place && objects_dir.exists() {
        for shard in list_dir(&objects_dir)? {
            let Some(shard_name) = two_hex(&shard) else {
                continue; // `info/`, `pack/` (empty), stray files
            };
            for file in list_dir(&shard)? {
                let Some(rest) = hex_name(&file, 38) else {
                    continue; // a temp file some crash left; not an object
                };
                let mut oid = [0u8; 20];
                decode_hex(&format!("{shard_name}{rest}"), &mut oid)?;
                let bytes = std::fs::read(&file).map_err(AuthorityError::internal)?;
                authority.store().put_loose(ws, &oid, bytes).await?;
                loose_copied += 1;
            }
        }
    }

    // 2. Large-store conversion: raw sha256-named files → git-blob frames, both identities
    // verified, presence row flipped.
    let mut large_converted = 0;
    let mut large_orphans = 0;
    let large_objects = large_root.join(ws.as_str()).join("objects");
    if large_objects.exists() {
        for shard_a in list_dir(&large_objects)? {
            if two_hex(&shard_a).is_none() {
                continue;
            }
            for shard_b in list_dir(&shard_a)? {
                if two_hex(&shard_b).is_none() {
                    continue;
                }
                for file in list_dir(&shard_b)? {
                    let Some(hex64) = hex_name(&file, 64) else {
                        continue;
                    };
                    let mut object_id = [0u8; 32];
                    decode_hex(&hex64, &mut object_id)?;
                    let bytes = std::fs::read(&file).map_err(AuthorityError::internal)?;
                    // Identity 1: the raw bytes hash to the filename.
                    if topos_core::digest::sha256(&bytes) != object_id {
                        failures.push(format!(
                            "{name}: large object {hex64} does not hash to its filename \
                             (at-rest corruption) — not imported"
                        ));
                        continue;
                    }
                    let framed = codec::encode_blob(&bytes).map_err(AuthorityError::internal)?;
                    // Identity 2: the framed blob's git OID equals the presence row's recorded
                    // locator (the tree-entry bridge key written at install).
                    match authority
                        .db()
                        .import_object_git_oid(ws, ObjectId(object_id))
                        .await?
                    {
                        None => {
                            large_orphans += 1;
                            continue;
                        }
                        Some(row_oid) => {
                            if row_oid != Some(framed.git_oid) {
                                failures.push(format!(
                                    "{name}: large object {hex64}'s framed git OID does not match \
                                     its presence row's locator — not imported"
                                ));
                                continue;
                            }
                        }
                    }
                    authority
                        .store()
                        .put_loose(ws, &framed.git_oid, framed.zlib_bytes)
                        .await?;
                    authority
                        .db()
                        .import_flip_large_location(ws, ObjectId(object_id))
                        .await?;
                    large_converted += 1;
                }
            }
        }
    }

    // 3. Refs → version-row backfill. The commit object was copied in step 1 (or is already at
    // its key in place), so its message reads from the STORE — what the serving reads will see.
    let mut versions_backfilled = 0;
    let refs_dir = repo_dir.join("refs").join("topos").join("versions");
    if refs_dir.exists() {
        for ref_file in list_dir(&refs_dir)? {
            let Some(version_hex) = hex_name(&ref_file, 64) else {
                continue;
            };
            let content = std::fs::read_to_string(&ref_file).map_err(AuthorityError::internal)?;
            let commit_hex = content.trim();
            let mut commit_oid = [0u8; 20];
            if commit_hex.len() != 40 || decode_hex(commit_hex, &mut commit_oid).is_err() {
                failures.push(format!(
                    "{name}: ref {version_hex} does not name a 40-hex commit oid — not backfilled"
                ));
                continue;
            }
            let Some(zlib) = authority.store().get_loose(ws, &commit_oid).await? else {
                failures.push(format!(
                    "{name}: ref {version_hex}'s commit object {commit_hex} is not in the store \
                     — not backfilled"
                ));
                continue;
            };
            let message = match codec::decode_loose(&zlib, commit_oid)
                .map_err(AuthorityError::integrity)
                .and_then(|(kind, payload)| {
                    if matches!(kind, codec::ObjectKind::Commit) {
                        codec::decode_commit(&payload).map_err(AuthorityError::integrity)
                    } else {
                        Err(AuthorityError::integrity(NotACommit))
                    }
                }) {
                Ok(meta) => meta.message,
                Err(e) => {
                    failures.push(format!(
                        "{name}: ref {version_hex}'s commit object is undecodable ({e}) — not \
                         backfilled"
                    ));
                    continue;
                }
            };
            let outcome = authority
                .db()
                .import_backfill_version(ws, &version_hex, &commit_oid, &message)
                .await?;
            if outcome.conflicted {
                failures.push(format!(
                    "{name}: version {version_hex} is already backfilled to a DIFFERENT commit \
                     locator — refusing to overwrite"
                ));
            }
            versions_backfilled += usize::try_from(outcome.updated).unwrap_or(usize::MAX);
        }
    }

    // 4. Verification — completeness against the DATABASE's view (the serving truth).
    for (bundle, version) in authority.db().import_unbackfilled(ws).await? {
        failures.push(format!(
            "{name}: non-purged version {bundle}/{version} still has no commit locator"
        ));
    }
    for commit_oid in authority.db().import_version_locators(ws).await? {
        if !authority.store().exists(ws, &commit_oid).await? {
            failures.push(format!(
                "{name}: commit object {} is missing from the store",
                crate::store::hex_lower(&commit_oid)
            ));
        }
    }
    for blob_oid in authority.db().import_live_blob_locators(ws).await? {
        if !authority.store().exists(ws, &blob_oid).await? {
            failures.push(format!(
                "{name}: blob object {} is missing from the store",
                crate::store::hex_lower(&blob_oid)
            ));
        }
    }
    let unreachable_edges = authority.db().import_edges_without_present(ws).await?;
    if unreachable_edges > 0 {
        failures.push(format!(
            "{name}: {unreachable_edges} reachability edge(s) of live versions have no present \
             object row"
        ));
    }
    let stale_locations = authority.db().import_non_store_present(ws).await?;
    if stale_locations > 0 {
        failures.push(format!(
            "{name}: {stale_locations} present object(s) still record a pre-import location"
        ));
    }

    Ok(ImportWorkspaceReport {
        workspace_id: name,
        loose_copied,
        large_converted,
        large_orphans,
        versions_backfilled,
    })
}

/// A dir's entries (empty for a missing dir — import is resumable over partial layouts).
fn list_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.path()).map_err(AuthorityError::internal))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(AuthorityError::internal(e)),
    }
}

/// The file name when it is exactly two lowercase-hex chars (a shard dir), else `None`.
fn two_hex(path: &Path) -> Option<String> {
    hex_name(path, 2)
}

/// The file name when it is exactly `len` lowercase-hex chars, else `None`.
fn hex_name(path: &Path, len: usize) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    (name.len() == len
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    .then(|| name.to_owned())
}

/// Decode lowercase hex into `out` (whose length fixes the expected width).
fn decode_hex(hex: &str, out: &mut [u8]) -> Result<()> {
    if hex.len() != out.len() * 2 {
        return Err(AuthorityError::internal(BadHex));
    }
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| AuthorityError::internal(BadHex))?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("malformed hex name")]
struct BadHex;

#[derive(Debug, thiserror::Error)]
#[error("a version ref resolves to a non-commit object")]
struct NotACommit;
