//! Shared orchestration for the device-signed contribute writes (publish / propose / revert / review).
//!
//! The per-verb modules (`publish`/`review`/`revert`) build a [`OpRecord`] (the durable bound identity) and
//! hand it here. [`run_write`] persists the WAL (`0600`, before the first send), sends, and reconciles; on an
//! UNCERTAIN send it leaves the WAL so the next attempt replays the SAME `op_id` and the plane returns the
//! byte-identical receipt (I-OPID-DURABLE). [`send_op`] re-derives the signature + rebuilds the wire request
//! from the `OpRecord` (publish/propose: re-rendering the candidate from the local store; revert/review: from
//! the record's fields), so the fresh send and a crash replay are byte-identical by construction. The
//! local-state advances ([`apply_publish_ok`] / [`apply_light_advance`]) verify the signed pointer against the
//! pinned plane key before raising the anti-rollback floor.

use base64::Engine as _;
use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::sign::{self, Commit, DeviceOpFields};
use topos_gitstore::{Store, VerifyError};
use topos_types::persisted::{Lock, OpKind, OpRecord, PlacementMap, RecordedTuple, SyncState};
use topos_types::requests::{
    ProposeRequest, PublishRequest, RevertRequest, ReviewRequest, WireCandidate, WireFile,
    WireFileMode,
};
use topos_types::results::ReviewDecision;
use topos_types::{Generation, SignedCurrentRecord, TerminalOutcome};

use core::cmp::Ordering;

use super::parse_hex32;
use super::sync_engine;
use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::error::ClientError;
use crate::plane::{
    ContributeSource, FetchedVersion, PlaneError, PointerFetch, WriteReceipt, gen_cmp,
};
use crate::sidecar::SkillPaths;
use crate::{doc, materialize, op_wal, scan};

/// The fixed, controlled-ASCII publish message — folded into the candidate `version_id`, so it must stay
/// constant for a reproducible, retry-stable id (no `-m` flag in v0; vocab exposes none).
pub(crate) const PUBLISH_MESSAGE: &str = "topos: publish";
/// The fixed forward-revert message — folded into the forward commit id (which the plane reconstructs).
pub(crate) const REVERT_MESSAGE: &str = "topos: revert";

/// Builds the device-signed contribute transport for a plane base URL — known only after reading
/// `instance.json`, so it can't be pre-built in the composition root (mirrors `invite`'s connector).
pub(crate) type ContributeConnect<'a> = dyn Fn(&str) -> Box<dyn ContributeSource> + 'a;

/// Mint a client `op_id`: the raw 16 bytes are bound into the signed frame; the canonical hyphenated UUID
/// rides the wire (the plane re-parses it back to the SAME 16 bytes, so a lost-ack retry replays it).
pub(crate) fn new_op_id(ctx: &Ctx<'_>) -> String {
    uuid::Uuid::from_bytes(ctx.ids.new_op_id())
        .as_hyphenated()
        .to_string()
}

fn wire_mode(mode: FileMode) -> WireFileMode {
    match mode {
        FileMode::Regular => WireFileMode::Regular,
        FileMode::Executable => WireFileMode::Executable,
    }
}

/// base64 **STANDARD** (padded) — the frozen wire codec for `content_base64`. DISTINCT from the
/// URL_SAFE_NO_PAD the signature header uses; the wrong alphabet would corrupt every byte → a digest /
/// commit-id mismatch → DENIED.
fn content_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Map a transport-level [`PlaneError`] into the client error family.
fn plane_err(e: PlaneError) -> ClientError {
    match e {
        PlaneError::NotFound => ClientError::Plane("not served here".to_owned()),
        PlaneError::Unavailable(m) | PlaneError::Unreachable(m) | PlaneError::Malformed(m) => {
            ClientError::Plane(m)
        }
    }
}

/// Fetch + authenticate the LIVE `current` pointer (signature + scope, against the pinned plane key),
/// returning its `(version_id, generation)`. A revert binds the forward commit's parent + CAS target to
/// this fresh current (NOT a stale local view — the server builds the forward commit from its live parent
/// and verifies the signature against it BEFORE the CAS, so a stale parent would be a DENIED, not a clean
/// CONFLICT); a review uses the generation as `expected` (== a reviewable proposal's base).
///
/// # Errors
/// [`ClientError::Corrupt`] if the pointer fails verification; [`ClientError::Plane`] on a transport fault.
pub(crate) fn fresh_current(
    ctx: &Ctx<'_>,
    skill_id: &str,
    workspace_id: &str,
) -> Result<([u8; 32], Generation), ClientError> {
    match ctx.plane.get_current(skill_id, None) {
        Ok(PointerFetch::Record(rec)) => {
            let vid =
                sync_engine::authenticated_version_id(&rec, skill_id, workspace_id, &ctx.plane_key)
                    .ok_or_else(|| {
                        ClientError::Corrupt(
                            "the current pointer failed signature/scope verification".to_owned(),
                        )
                    })?;
            Ok((vid, rec.record.generation))
        }
        Ok(PointerFetch::NotModified) => Err(ClientError::Corrupt(
            "an unconditional current read returned not-modified".to_owned(),
        )),
        Err(e) => Err(plane_err(e)),
    }
}

/// Fetch a version ONCE, recompute its `bundle_digest` from the bytes, and ASSERT
/// `commit_id(parents, digest, author, message)` reproduces the named version id (consent re-derivation —
/// never trust the plane's word; a tampered response can't masquerade as the version). Returns the verified
/// digest + the SAME fetched bytes, so a caller that displays them (e.g. `diff`) shows exactly what was
/// verified — never a second, unverified fetch.
///
/// # Errors
/// [`ClientError::Verify`] if the fetched bytes do not reproduce the named id; [`ClientError::Plane`] on a
/// transport fault; [`ClientError::Scan`] on a canonical-manifest reject.
pub(crate) fn fetch_verified_bundle(
    ctx: &Ctx<'_>,
    skill_id: &str,
    version_commit: [u8; 32],
) -> Result<([u8; 32], FetchedVersion), ClientError> {
    let v = ctx
        .plane
        .fetch_version(skill_id, version_commit)
        .map_err(plane_err)?;
    let manifest: Vec<ManifestEntry> = v
        .files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let bundle_digest =
        digest::bundle_digest(&manifest).map_err(|r| ClientError::Scan(format!("{r:?}")))?;
    let recomputed = sign::commit_id(&Commit {
        parents: &v.parents,
        tree: bundle_digest,
        author: &v.author,
        message: &v.message,
    })
    .map_err(|_| ClientError::Corrupt("version commit id preimage".to_owned()))?;
    if recomputed != version_commit {
        return Err(ClientError::Verify(VerifyError::Malformed(
            "the fetched version does not reproduce its named version id".to_owned(),
        )));
    }
    Ok((bundle_digest, v))
}

/// As [`fetch_verified_bundle`], returning only the verified digest (for ops that bind the digest but do not
/// display the bytes — review / revert).
///
/// # Errors
/// As [`fetch_verified_bundle`].
pub(crate) fn verified_version_digest(
    ctx: &Ctx<'_>,
    skill_id: &str,
    version_commit: [u8; 32],
) -> Result<[u8; 32], ClientError> {
    fetch_verified_bundle(ctx, skill_id, version_commit).map(|(digest, _)| digest)
}

/// Persist the WAL (idempotent — a replay re-writes the same record), send, and reconcile. On any terminal
/// 200 receipt (OK / NEEDS_REVIEW / CONFLICT / APPROVAL_REQUIRED / DENIED) the op is complete → delete the
/// WAL. On an UNCERTAIN send (transport / non-200 / malformed) the WAL STAYS so the next attempt replays the
/// SAME `op_id`.
///
/// # Errors
/// An [`FsOps`](crate::fs_seam::FsOps) write failure, or the uncertain-send transport error (the WAL is kept).
pub(crate) fn run_write(
    ctx: &Ctx<'_>,
    transport: &dyn ContributeSource,
    signer: &DeviceSigner,
    sp: &SkillPaths,
    rec: &OpRecord,
) -> Result<WriteReceipt, ClientError> {
    op_wal::write(ctx.fs, &ctx.layout, rec)?;
    match send_op(transport, signer, ctx, sp, rec) {
        Ok(receipt) => {
            // A terminal protocol outcome (any 200): the op is settled — drop the WAL. A subsequent retry
            // is a fresh op (after a rebase, say), never a replay of a settled one.
            op_wal::delete(ctx.fs, &ctx.layout, &rec.op_id)?;
            Ok(receipt)
        }
        // A DEFINITIVE rejection (a 4xx other than 429 — the op provably did NOT land): drop the WAL so the
        // verb is not stuck replaying a request the plane will always reject.
        Err(e @ ClientError::PlaneRejected(_)) => {
            op_wal::delete(ctx.fs, &ctx.layout, &rec.op_id)?;
            Err(e)
        }
        // Uncertain (5xx / 429 / timeout / malformed — the op MAY have landed): keep the WAL so the next
        // attempt replays the SAME op_id and the plane replays the byte-identical receipt.
        Err(e) => Err(e),
    }
}

/// Build a [`ClientError::PlaneTerminal`] from a write receipt whose outcome the verb does not special-case
/// (RetryableFailure / Unavailable / PermanentFailure / …) — surfacing the plane's TRUE terminal class +
/// code rather than a generic transport error.
pub(crate) fn plane_terminal(receipt: &WriteReceipt) -> ClientError {
    let outcome = receipt.outcome();
    let code = receipt
        .error
        .as_ref()
        .map(|e| e.code.clone())
        .unwrap_or_else(|| format!("{outcome:?}"));
    let retryable = matches!(
        outcome,
        TerminalOutcome::RetryableFailure | TerminalOutcome::Unavailable
    );
    ClientError::PlaneTerminal {
        outcome,
        code,
        retryable,
    }
}

/// Re-sign the device-op frame the [`OpRecord`] binds, rebuild the wire request, and POST it. BOTH the fresh
/// path and a crash replay call this with the SAME record, so a replayed send is byte-identical.
fn send_op(
    transport: &dyn ContributeSource,
    signer: &DeviceSigner,
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    rec: &OpRecord,
) -> Result<WriteReceipt, ClientError> {
    let op = op_wal::device_op(rec.op);
    let op_id_bytes = op_wal::op_id_bytes(&rec.op_id)?;
    let commit_id = parse_hex32(&rec.candidate_commit)?;
    let bundle_digest = parse_hex32(&rec.bundle_digest)?;
    let fields = DeviceOpFields {
        workspace_id: &rec.workspace_id,
        skill_id: &rec.skill_id,
        op,
        op_id: op_id_bytes,
        device_key_id: signer.device_key_id(),
        expected_epoch: rec.expected_generation.epoch,
        expected_seq: rec.expected_generation.seq,
        commit_id,
        bundle_digest,
    };
    let sig = signer.sign_device_op(&fields)?;
    let device_key_id = signer.device_key_id().to_owned();
    match rec.op {
        OpKind::PublishDirect | OpKind::PublishPropose => {
            let candidate = render_candidate(sp, commit_id, bundle_digest)?;
            if matches!(rec.op, OpKind::PublishDirect) {
                transport.publish(
                    PublishRequest {
                        workspace_id: rec.workspace_id.clone(),
                        skill_id: rec.skill_id.clone(),
                        op_id: rec.op_id.clone(),
                        device_key_id,
                        expected: rec.expected_generation,
                        candidate,
                    },
                    sig,
                )
            } else {
                transport.propose(
                    ProposeRequest {
                        workspace_id: rec.workspace_id.clone(),
                        skill_id: rec.skill_id.clone(),
                        op_id: rec.op_id.clone(),
                        device_key_id,
                        expected: rec.expected_generation,
                        candidate,
                    },
                    sig,
                )
            }
        }
        OpKind::Revert => {
            let good = rec.good.clone().ok_or_else(|| {
                ClientError::Corrupt("revert op record missing `good`".to_owned())
            })?;
            transport.revert(
                RevertRequest {
                    workspace_id: rec.workspace_id.clone(),
                    skill_id: rec.skill_id.clone(),
                    op_id: rec.op_id.clone(),
                    device_key_id,
                    expected: rec.expected_generation,
                    good,
                    author: ctx.device_id.clone(),
                    message: REVERT_MESSAGE.to_owned(),
                },
                sig,
            )
        }
        OpKind::ReviewApprove | OpKind::ReviewReject => {
            let decision = if matches!(rec.op, OpKind::ReviewApprove) {
                ReviewDecision::Approve
            } else {
                ReviewDecision::Reject
            };
            transport.review(
                ReviewRequest {
                    workspace_id: rec.workspace_id.clone(),
                    skill_id: rec.skill_id.clone(),
                    op_id: rec.op_id.clone(),
                    device_key_id,
                    expected: rec.expected_generation,
                    proposal: rec.candidate_commit.clone(),
                    decision,
                },
                sig,
            )
        }
    }
}

/// Rebuild the byte-identical [`WireCandidate`] for a publish/propose from the local store: render the
/// committed candidate's bytes (re-verified against its digest) + read its commit frame
/// (parents/author/message). A replay reconstructs exactly what was first signed.
fn render_candidate(
    sp: &SkillPaths,
    commit_id: [u8; 32],
    bundle_digest: [u8; 32],
) -> Result<WireCandidate, ClientError> {
    let store = Store::open(&sp.store)?;
    let meta = store.read_commit_meta(commit_id)?;
    let bundle = store.render_verified(commit_id, bundle_digest)?;
    let files = bundle
        .files
        .iter()
        .map(|f| WireFile {
            path: f.path.clone(),
            mode: wire_mode(f.mode),
            content_base64: content_b64(&f.bytes),
        })
        .collect();
    let parents = meta.parents.iter().map(|p| to_hex(p)).collect();
    Ok(WireCandidate {
        files,
        parents,
        author: meta.author,
        message: meta.message,
    })
}

/// Verify the signed `current` pointer the plane returned (signature + scope, against the pinned plane key)
/// and confirm it names the version this op moved to. Returns the verified new generation.
fn verified_new_generation(
    ctx: &Ctx<'_>,
    rec: &OpRecord,
    signed_record: &SignedCurrentRecord,
) -> Result<Generation, ClientError> {
    let moved_to = parse_hex32(&rec.candidate_commit)?;
    let authed = sync_engine::authenticated_version_id(
        signed_record,
        &rec.skill_id,
        &rec.workspace_id,
        &ctx.plane_key,
    )
    .ok_or_else(|| {
        ClientError::Corrupt("the OK pointer failed signature/scope verification".to_owned())
    })?;
    if authed != moved_to {
        return Err(ClientError::Corrupt(
            "the OK pointer names a different version than the op moved".to_owned(),
        ));
    }
    Ok(signed_record.record.generation)
}

/// Append a `(generation → commit)` record if absent (idempotent).
fn record_tuple(recorded: &mut Vec<RecordedTuple>, generation: Generation, commit_hex: &str) {
    if !recorded.iter().any(|t| t.generation == generation) {
        recorded.push(RecordedTuple {
            generation,
            commit_id: commit_hex.to_owned(),
        });
    }
}

/// The publish-OK read-your-writes advance (I-RESPECT-DIVERGENCE). On a CLEAN publish (the working tree still
/// equals the bytes just published) the author's own write fast-forwards the local state to `current` (state
/// ① — `applied = observed = new_gen`, no dir-swap: the bytes are already placed). If the working tree
/// changed during the round-trip (DIRTY), the anti-rollback floor still rises to the published generation
/// (read-your-writes — the author's write is in the floor, T4), but `applied` is RETAINED and the working
/// draft is left untouched, so the next `pull` resolves the divergence rather than this path silently
/// clobbering it. Returns the new generation.
///
/// # Errors
/// [`ClientError::Corrupt`] if the OK pointer fails verification; a store/fs/scan failure.
pub(crate) fn apply_publish_ok(
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    lock: &Lock,
    map: &PlacementMap,
    rec: &OpRecord,
    signed_record: &SignedCurrentRecord,
) -> Result<Generation, ClientError> {
    let new_gen = verified_new_generation(ctx, rec, signed_record)?;
    let commit_id = parse_hex32(&rec.candidate_commit)?;
    let published_digest = parse_hex32(&rec.bundle_digest)?;
    let published_digest_hex = rec.bundle_digest.clone();

    let sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("missing sync state".to_owned()))?;
    // The anti-rollback floor only ever RISES: if a later move (e.g. a cross-family revert that advanced
    // `observed` while this op's ack was lost) already carried the floor past this publish, a replay of the
    // settled receipt is a no-op locally — never regress `observed`/`applied` (the next pull reconciles).
    if gen_cmp(new_gen, sync.observed) != Ordering::Greater {
        return Ok(new_gen);
    }
    let mut recorded = sync.recorded.clone();
    record_tuple(&mut recorded, new_gen, &rec.candidate_commit);

    // Re-scan the placement: did the working tree change during the round-trip?
    let placement = sync_engine::first_placement(map)?;
    let scanned = scan::scan(std::path::Path::new(&placement))?;
    if scanned.bundle_digest == published_digest {
        // CLEAN → state ①. The placement already holds the published bytes (no dir-swap); update the docs
        // to point `current`/`applied`/`base` at the new version, re-deriving the lock from the store.
        let store = Store::open(&sp.store)?;
        let bundle = store.render_verified(commit_id, published_digest)?;
        let next_lock = sync_engine::lock_from_bundle(lock, commit_id, &bundle);
        let next_map = PlacementMap {
            schema_version: map.schema_version,
            placements: map.placements.clone(),
            applied_commit: rec.candidate_commit.clone(),
            materialized_sha: published_digest_hex.clone(),
            pre_existing_sha: materialize::derive_pre_existing_sha(map, true),
            swap_capability: map.swap_capability,
            harness: map.harness,
            harness_layer: map.harness_layer.clone(),
        };
        let next_sync = SyncState {
            schema_version: sync.schema_version,
            observed: new_gen,
            applied: new_gen,
            recorded,
            base_commit: rec.candidate_commit.clone(),
            work_hash: published_digest_hex,
            held: false,
        };
        materialize::commit_docs(ctx.fs, sp, &next_map, &next_lock, &next_sync)?;
    } else {
        // DIRTY (edited during the round-trip): raise the floor to the published generation (read-your-
        // writes) + record the tuple, but RETAIN `applied` and leave the working draft. The next `pull`
        // derives ④ DIVERGED and merges — never a silent clobber here. The published version is in the
        // store (committed before send), so the merge/render can reach it.
        let next_sync = SyncState {
            schema_version: sync.schema_version,
            observed: new_gen,
            applied: sync.applied,
            recorded,
            base_commit: sync.base_commit.clone(),
            work_hash: sync.work_hash.clone(),
            held: sync.held,
        };
        doc::write_doc(ctx.fs, &sp.sync, &next_sync)?;
    }
    Ok(new_gen)
}

/// The revert / review-approve local advance: the new `current` bytes land on the actor's own copy at its
/// NEXT `pull` (which fast-forwards or surfaces a divergence). Here we only raise the anti-rollback floor +
/// record the `(generation → commit)` tuple, so the actor's next pull recognizes the move (rather than
/// re-alarming on it). Returns the new generation.
///
/// # Errors
/// [`ClientError::Corrupt`] if the OK pointer fails verification; an fs failure writing the sync doc.
pub(crate) fn apply_light_advance(
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    rec: &OpRecord,
    signed_record: &SignedCurrentRecord,
) -> Result<Generation, ClientError> {
    let new_gen = verified_new_generation(ctx, rec, signed_record)?;
    let sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("missing sync state".to_owned()))?;
    // The floor only ever rises — a replay of a move already superseded locally is a no-op (see
    // [`apply_publish_ok`]).
    if gen_cmp(new_gen, sync.observed) != Ordering::Greater {
        return Ok(new_gen);
    }
    let mut recorded = sync.recorded.clone();
    record_tuple(&mut recorded, new_gen, &rec.candidate_commit);
    let next_sync = SyncState {
        schema_version: sync.schema_version,
        observed: new_gen,
        applied: sync.applied,
        recorded,
        base_commit: sync.base_commit,
        work_hash: sync.work_hash,
        held: sync.held,
    };
    doc::write_doc(ctx.fs, &sp.sync, &next_sync)?;
    Ok(new_gen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_core::digest::{self, FileMode, ManifestEntry};
    use topos_core::sign::{self, Commit, DeviceOp, DeviceOpFields, verify_device_op};
    use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
    use topos_types::requests::{ProposeRequest, PublishRequest, RevertRequest, ReviewRequest};
    use topos_types::{
        CurrencyKind, Generation, HarnessId, Receipt, TerminalOutcome, TriggerReport, TriggerState,
    };

    use crate::device_signer::DeviceSigner;
    use crate::fs_seam::RealFs;
    use crate::ids::test_sources::{FixedClock, SeqIds};
    use crate::plane::{InertFollow, InertPlane};

    // ── a self-cleaning home ──
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-contrib-ut-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct NullHarness;
    impl HarnessAdapter for NullHarness {
        fn id(&self) -> HarnessId {
            HarnessId::ClaudeCode
        }
        fn discover(&self) -> Vec<DiscoveredPlacement> {
            Vec::new()
        }
        fn placement_for(
            &self,
            skill_id: &str,
            _d: Option<&DiscoveredPlacement>,
        ) -> PlacementTarget {
            PlacementTarget {
                dir: PathBuf::from("/nonexistent").join(skill_id),
            }
        }
        fn currency_kind(&self) -> CurrencyKind {
            CurrencyKind::ExplicitPullOnly
        }
        fn install_currency_trigger(&self) -> TriggerReport {
            no_trigger()
        }
        fn remove_currency_trigger(&self) -> TriggerReport {
            no_trigger()
        }
        fn uninstall_footprint(&self) -> Vec<PathBuf> {
            Vec::new()
        }
    }
    fn no_trigger() -> TriggerReport {
        TriggerReport {
            harness: HarnessId::ClaudeCode,
            currency_kind: CurrencyKind::ExplicitPullOnly,
            touched_path: None,
            marker_id: "t".into(),
            state: TriggerState::Inactive,
        }
    }

    // ── I-COMMIT-PARITY two-halves wire test ──
    //
    // The CLIENT computes (commit_id, bundle_digest) over a candidate + signs a `DeviceOpFields` binding
    // them. The SERVER half rehashes the SAME bytes through the SAME kernel to reconstruct the identity and
    // `verify_device_op`. The positive must pass; tweaking ANY bound input AFTER signing must FAIL.

    fn manifest_digest(files: &[(&str, FileMode, &[u8])]) -> [u8; 32] {
        let m: Vec<ManifestEntry> = files
            .iter()
            .map(|(p, mode, b)| ManifestEntry {
                path: (*p).to_owned(),
                mode: *mode,
                content_sha256: digest::sha256(b),
            })
            .collect();
        digest::bundle_digest(&m).expect("bundle digest")
    }

    #[test]
    fn two_halves_parity_passes_and_fails_on_any_post_sign_tweak() {
        let scratch = Scratch::new();
        let fs = RealFs;
        let layout = crate::sidecar::Layout::new(&scratch.0);
        let signer = DeviceSigner::load_or_generate(&fs, &layout).unwrap();
        let pubkey = signer.public_key();

        let author = "d_author";
        let message = PUBLISH_MESSAGE;
        let parent = [0x11u8; 32];
        let files: &[(&str, FileMode, &[u8])] = &[
            ("SKILL.md", FileMode::Regular, b"hello\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\n"),
        ];

        // CLIENT: compute the bound identity + sign.
        let digest = manifest_digest(files);
        let commit_id = sign::commit_id(&Commit {
            parents: &[parent],
            tree: digest,
            author,
            message,
        })
        .unwrap();
        let op_id = [3u8; 16];
        let expected = Generation { epoch: 1, seq: 1 };
        let fields = DeviceOpFields {
            workspace_id: "w_acme",
            skill_id: "s_deploy",
            op: DeviceOp::PublishDirect,
            op_id,
            device_key_id: signer.device_key_id(),
            expected_epoch: expected.epoch,
            expected_seq: expected.seq,
            commit_id,
            bundle_digest: digest,
        };
        let sig = signer.sign_device_op(&fields).unwrap();

        // SERVER half: rehash the SAME bytes → SAME commit_id + digest → reconstruct + verify. PASSES.
        let server_digest = manifest_digest(files);
        let server_commit = sign::commit_id(&Commit {
            parents: &[parent],
            tree: server_digest,
            author,
            message,
        })
        .unwrap();
        let server_fields = DeviceOpFields {
            commit_id: server_commit,
            bundle_digest: server_digest,
            ..fields
        };
        assert!(
            verify_device_op(&server_fields, &sig, &pubkey),
            "byte-identical reconstruction must verify (I-COMMIT-PARITY)"
        );

        // NEGATIVES — each tweak changes a bound value, so the SAME signature must NOT verify.
        // (a) a content byte flipped (→ different digest → different commit_id):
        let tweaked_files: &[(&str, FileMode, &[u8])] = &[
            ("SKILL.md", FileMode::Regular, b"HELLO\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\n"),
        ];
        let d2 = manifest_digest(tweaked_files);
        let c2 = sign::commit_id(&Commit {
            parents: &[parent],
            tree: d2,
            author,
            message,
        })
        .unwrap();
        assert!(
            !verify_device_op(
                &DeviceOpFields {
                    commit_id: c2,
                    bundle_digest: d2,
                    ..fields
                },
                &sig,
                &pubkey
            ),
            "a flipped content byte must fail to verify"
        );
        // (b) author changed:
        let c_auth = sign::commit_id(&Commit {
            parents: &[parent],
            tree: digest,
            author: "d_other",
            message,
        })
        .unwrap();
        assert!(
            !verify_device_op(
                &DeviceOpFields {
                    commit_id: c_auth,
                    ..fields
                },
                &sig,
                &pubkey
            ),
            "a changed author must fail"
        );
        // (c) message changed:
        let c_msg = sign::commit_id(&Commit {
            parents: &[parent],
            tree: digest,
            author,
            message: "topos: other",
        })
        .unwrap();
        assert!(
            !verify_device_op(
                &DeviceOpFields {
                    commit_id: c_msg,
                    ..fields
                },
                &sig,
                &pubkey
            ),
            "a changed message must fail"
        );
        // (d) a parent changed:
        let c_par = sign::commit_id(&Commit {
            parents: &[[0x22u8; 32]],
            tree: digest,
            author,
            message,
        })
        .unwrap();
        assert!(
            !verify_device_op(
                &DeviceOpFields {
                    commit_id: c_par,
                    ..fields
                },
                &sig,
                &pubkey
            ),
            "a changed parent must fail"
        );
        // (e) expected (the CAS target) off by one:
        assert!(
            !verify_device_op(
                &DeviceOpFields {
                    expected_seq: 2,
                    ..fields
                },
                &sig,
                &pubkey
            ),
            "a changed expected generation must fail"
        );
    }

    // ── op_id idempotent replay: an UNCERTAIN send keeps the WAL + replays the SAME op_id ──

    struct FakeContribute {
        calls: RefCell<Vec<String>>,
        fail_next: RefCell<bool>,
    }
    fn ok_receipt(op_id: &str) -> WriteReceipt {
        WriteReceipt {
            receipt: Receipt {
                schema_version: 1,
                op_id: op_id.to_owned(),
                command: "review-approve".to_owned(),
                outcome: TerminalOutcome::Ok,
                workspace_id: "w_acme".to_owned(),
                skill_id: Some("s_deploy".to_owned()),
                version_id: Some("a".repeat(64)),
                bundle_digest: Some("b".repeat(64)),
                expected_generation: None,
                current_generation: Some(Generation { epoch: 1, seq: 2 }),
                created_at: "2026-06-30T00:00:00Z".to_owned(),
                key_id: None,
                details: None,
            },
            error: None,
            signed_record: None,
        }
    }
    impl FakeContribute {
        fn step(&self, op_id: &str) -> Result<WriteReceipt, ClientError> {
            self.calls.borrow_mut().push(op_id.to_owned());
            if *self.fail_next.borrow() {
                *self.fail_next.borrow_mut() = false;
                Err(ClientError::Plane("uncertain send".to_owned()))
            } else {
                Ok(ok_receipt(op_id))
            }
        }
    }
    impl ContributeSource for FakeContribute {
        fn publish(&self, b: PublishRequest, _s: [u8; 64]) -> Result<WriteReceipt, ClientError> {
            self.step(&b.op_id)
        }
        fn propose(&self, b: ProposeRequest, _s: [u8; 64]) -> Result<WriteReceipt, ClientError> {
            self.step(&b.op_id)
        }
        fn revert(&self, b: RevertRequest, _s: [u8; 64]) -> Result<WriteReceipt, ClientError> {
            self.step(&b.op_id)
        }
        fn review(&self, b: ReviewRequest, _s: [u8; 64]) -> Result<WriteReceipt, ClientError> {
            self.step(&b.op_id)
        }
    }

    #[test]
    fn op_id_replay_keeps_the_wal_on_uncertain_and_resends_the_same_op_id() {
        let scratch = Scratch::new();
        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = NullHarness;
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let layout = crate::sidecar::Layout::new(&scratch.0);
        let signer = DeviceSigner::load_or_generate(&fs, &layout).unwrap();
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: "d_author".to_owned(),
            layout: layout.clone(),
            harness: &harness,
            plane: &inert_p,
            plane_key: [0u8; 32],
            follow: &inert_f,
        };
        let sp = layout.published(&crate::id::SkillId::parse("s_deploy").unwrap());
        let op_id = "c0000000-0000-4000-8000-000000000001".to_owned();
        // A review op needs no candidate render (send_op builds ReviewRequest from the record's fields).
        let rec = OpRecord {
            schema_version: 1,
            op_id: op_id.clone(),
            workspace_id: "w_acme".to_owned(),
            skill_id: "s_deploy".to_owned(),
            op: OpKind::ReviewApprove,
            candidate_commit: "a".repeat(64),
            bundle_digest: "b".repeat(64),
            expected_generation: Generation { epoch: 1, seq: 1 },
            good: None,
            last_receipt: None,
        };

        let fake = FakeContribute {
            calls: RefCell::new(Vec::new()),
            fail_next: RefCell::new(true),
        };

        // First attempt: the send is UNCERTAIN — run_write keeps the WAL.
        let first = run_write(&ctx, &fake, &signer, &sp, &rec);
        assert!(first.is_err(), "an uncertain send surfaces the error");
        assert!(
            crate::op_wal::read(&fs, &layout, &op_id).unwrap().is_some(),
            "the WAL survives an uncertain send"
        );

        // The next attempt finds the pending op + replays the SAME op_id.
        let pending = crate::op_wal::find_pending_for_skill(
            &fs,
            &layout,
            "w_acme",
            "s_deploy",
            &[OpKind::ReviewApprove, OpKind::ReviewReject],
        )
        .unwrap()
        .expect("a pending review op to resume");
        assert_eq!(pending.op_id, op_id, "the replay reuses the same op_id");

        let second = run_write(&ctx, &fake, &signer, &sp, &pending);
        assert!(second.is_ok(), "the replay settles");
        assert!(
            crate::op_wal::read(&fs, &layout, &op_id).unwrap().is_none(),
            "a settled op deletes its WAL"
        );
        assert_eq!(
            *fake.calls.borrow(),
            vec![op_id.clone(), op_id],
            "BOTH sends carried the identical op_id (no double-advance — the server replays the receipt)"
        );
    }
}
