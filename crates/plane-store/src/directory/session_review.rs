//! The web-session review leg — the orchestration half (outside the transaction).
//!
//! The hosted cloud's "Approve / Reject in the browser" surface: a composing plane whose WEB layer has
//! verified a session email calls these PRIVILEGED lib-level ops (there is no OSS HTTP route) to
//! approve or reject an open proposal. The write terminates in the SAME serializable transaction the
//! device lane runs ([`crate::db`]'s `run` / `reject_run`) — one approve predicate, one CAS, one
//! pointer advance, one four-eyes gate — with the lane branching ONLY at the authorization step:
//! the session gate is a confirmed **owner or reviewer** workspace seat (the first enforcement of the
//! reviewer role), checked in-transaction; the composing caller's session verification is the
//! authentication. Mirrors [`crate::session_roster`]'s trust shape (no signature; request_id-idempotent
//! under a fresh domain-tagged identity — the acting gate is the confirmed-seat role check, the same on
//! a self-host plane and a hosted one) and [`crate::session_read`]'s
//! gate-before-reach posture: a pool-level membership pre-gate runs BEFORE any proposal/digest/render
//! work, so an unproven caller never reaches workspace data — the in-txn gate stays the authority.
//!
//! CONSENT POSTURE (deliberate, decided): a session approve carries no reviewer attestation over the
//! candidate commit + digest — followers re-verify bytes against the approved content-addressed digest
//! before applying, and the receipt records `method = web_session` + the acting principal (with a
//! reserved step-up-attestation slot) as the compensating audit.

use topos_types::{Generation, TerminalOutcome};

use crate::actor::WriteActor;
use crate::authority::Authority;
use crate::custody::set_current::DeviceOp;
use crate::enroll::{DeploymentMode, parse_op_id};
use crate::error::Result;
use crate::governance::Role;
use crate::id::{BundleId, CommitId, OpId, Principal, WorkspaceId};
use crate::set_current::{self, SetCurrentReceipt, device_op_command};

/// The domain tag of the session review request identity (`request_sha256`) — versioned, and distinct
/// from every kernel signing-frame tag AND the roster leg's tag, so no stored identity from another
/// domain can ever byte-match a review request.
const SESSION_REVIEW_TAG: &[u8] = b"TOPOS_SESSION_REVIEW_V1\0";

/// The domain tag of the session REVERT request identity — a FRESH tag, distinct from the review tag
/// (and every kernel/roster tag), so a revert request can never byte-match an approve/reject request under
/// a reused id: the two session write verbs live in separate idempotency domains.
const SESSION_REVERT_TAG: &[u8] = b"TOPOS_SESSION_REVERT_V1\0";

/// The forward-commit message a session revert records — the SAME fixed string the CLI's device revert
/// uses (`bins/topos`'s `REVERT_MESSAGE`), so team history reads identically whichever lane rolled back.
const SESSION_REVERT_MESSAGE: &str = "topos: revert";

// The uniform acting-gate denial + the durable role-denial code/message live in the shared lane
// vocabulary (`crate::actor`) — custody's transaction writes them too; re-exported here so the public
// names keep their home on the review leg.
pub(crate) use crate::actor::REVIEWER_ROLE_REQUIRED_MSG;
pub use crate::actor::{REVIEWER_ROLE_REQUIRED_CODE, SESSION_REVIEW_ACTING_DENIED};

/// The machine-branchable code on a reject with an empty reason (an orchestration belt; the composing
/// route rejects it earlier with a 400).
pub const REASON_REQUIRED_CODE: &str = "REASON_REQUIRED";

/// The session review request identity: sha256 over the versioned domain tag + u64-be length-prefixed
/// parts (verb, workspace, acting email, skill, candidate commit, expected epoch/seq, then — reject
/// only — the reason). Deterministic — a lost-ack retry recomputes the identical identity; any divergent
/// payload under a reused request id (a re-worded reason included) mismatches and fails closed.
fn review_request_sha256(
    verb: &str,
    ws: &WorkspaceId,
    acting: &Principal,
    skill: &BundleId,
    candidate: CommitId,
    expected: Generation,
    reason: Option<&str>,
) -> [u8; 32] {
    let epoch_be = expected.epoch.to_be_bytes();
    let seq_be = expected.seq.to_be_bytes();
    let mut parts: Vec<&[u8]> = vec![
        verb.as_bytes(),
        ws.as_str().as_bytes(),
        acting.as_str().as_bytes(),
        skill.as_str().as_bytes(),
        candidate.0.as_slice(),
        epoch_be.as_slice(),
        seq_be.as_slice(),
    ];
    if let Some(reason) = reason {
        parts.push(reason.as_bytes());
    }
    let mut buf = Vec::with_capacity(
        SESSION_REVIEW_TAG.len() + parts.iter().map(|p| p.len() + 8).sum::<usize>(),
    );
    buf.extend_from_slice(SESSION_REVIEW_TAG);
    for part in parts {
        buf.extend_from_slice(&(part.len() as u64).to_be_bytes());
        buf.extend_from_slice(part);
    }
    topos_core::digest::sha256(&buf)
}

/// The session REVERT request identity: sha256 over [`SESSION_REVERT_TAG`] + u64-be length-prefixed parts
/// (ws, acting email, skill, the GOOD TARGET commit id, expected epoch/seq). Binds the target COMMIT (not
/// just its tree digest — two accepted versions can share a tree, so the commit id is what makes the request
/// a unique target key; codex design-gate finding 2), and its fresh tag makes it impossible for a revert
/// request to byte-match a review approve/reject under a reused id. Deterministic — a lost-ack retry
/// recomputes the identical identity; any divergent target/generation mismatches and fails closed.
fn revert_request_sha256(
    ws: &WorkspaceId,
    acting: &Principal,
    skill: &BundleId,
    good: CommitId,
    expected: Generation,
) -> [u8; 32] {
    let epoch_be = expected.epoch.to_be_bytes();
    let seq_be = expected.seq.to_be_bytes();
    let parts: Vec<&[u8]> = vec![
        b"revert",
        ws.as_str().as_bytes(),
        acting.as_str().as_bytes(),
        skill.as_str().as_bytes(),
        good.0.as_slice(),
        epoch_be.as_slice(),
        seq_be.as_slice(),
    ];
    let mut buf = Vec::with_capacity(
        SESSION_REVERT_TAG.len() + parts.iter().map(|p| p.len() + 8).sum::<usize>(),
    );
    buf.extend_from_slice(SESSION_REVERT_TAG);
    for part in parts {
        buf.extend_from_slice(&(part.len() as u64).to_be_bytes());
        buf.extend_from_slice(part);
    }
    topos_core::digest::sha256(&buf)
}

/// A SYNTHESIZED session DENIED (never persisted): the pre-gate misses and the parse belts all use it.
/// Deterministic per input; only `created_at` re-stamps — an unproven caller is owed no byte-stable replay.
fn synth_denied(
    op: DeviceOp,
    skill: &BundleId,
    candidate: CommitId,
    expected: Generation,
    request_id: &str,
    created_at: &str,
    details: serde_json::Value,
) -> SetCurrentReceipt {
    SetCurrentReceipt {
        op_id: request_id.to_owned(),
        command: device_op_command(op).to_owned(),
        bundle_id: skill.as_str().to_owned(),
        version_id: Some(candidate),
        bundle_digest: None,
        expected,
        outcome: TerminalOutcome::Denied,
        current: None,
        record: None,
        created_at: created_at.to_owned(),
        details: Some(details),
    }
}

fn msg_details(msg: &str) -> serde_json::Value {
    serde_json::json!({ "message": msg })
}

fn code_details(code: &str, msg: &str) -> serde_json::Value {
    serde_json::json!({ "code": code, "message": msg })
}

/// The shared session preamble: the parse belts + the POOL-LEVEL membership pre-gate (the
/// gate-before-reach fence — no proposal/digest/render work for an unproven caller; the in-txn role
/// gate remains the authority). The acting gate is the confirmed-seat role check, identical on a
/// self-host plane and a hosted one. `Ok(Err(receipt))` is a synthesized refusal; `Ok(Ok(acting))` proceeds.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
async fn session_preamble(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &BundleId,
    candidate: CommitId,
    expected: Generation,
    op: DeviceOp,
    request_id: &str,
    acting_email: &str,
    // `plane_mode` no longer gates this op — the acting gate is the confirmed-seat role check, the same on both postures.
    _plane_mode: DeploymentMode,
    created_at: &str,
) -> Result<std::result::Result<Principal, SetCurrentReceipt>> {
    if parse_op_id(request_id).is_none() {
        return Ok(Err(synth_denied(
            op,
            skill,
            candidate,
            expected,
            request_id,
            created_at,
            msg_details("request_id is not a canonical UUID"),
        )));
    }
    let Ok(acting) = Principal::parse(acting_email) else {
        return Ok(Err(synth_denied(
            op,
            skill,
            candidate,
            expected,
            request_id,
            created_at,
            msg_details(SESSION_REVIEW_ACTING_DENIED),
        )));
    };
    if !authority.db().confirmed_member(ws, &acting).await? {
        return Ok(Err(synth_denied(
            op,
            skill,
            candidate,
            expected,
            request_id,
            created_at,
            msg_details(SESSION_REVIEW_ACTING_DENIED),
        )));
    }
    Ok(Ok(acting))
}

/// Approve an open proposal from a verified session (the orchestration half of
/// [`Authority::review_approve_session`]). `expected` is the `(epoch, seq)` the caller's rendered diff
/// was computed against (== the proposal's base whenever approve is offered); the shared CAS refuses a
/// moved pointer with the same CONFLICT the CLI gets.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn review_approve_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &BundleId,
    candidate: CommitId,
    expected: Generation,
    request_id: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    let acting = match session_preamble(
        authority,
        ws,
        skill,
        candidate,
        expected,
        DeviceOp::ReviewApprove,
        request_id,
        acting_email,
        plane_mode,
        created_at,
    )
    .await?
    {
        Ok(acting) => acting,
        Err(refusal) => return Ok(refusal),
    };
    let request_sha256 = review_request_sha256(
        "review_approve",
        ws,
        &acting,
        skill,
        candidate,
        expected,
        None,
    );
    // `parse_op_id` above proved the canonical form, so this parse cannot fail for a well-formed id.
    let op_id = OpId::parse(request_id).map_err(crate::error::AuthorityError::internal)?;
    set_current::review_approve(
        authority,
        ws,
        skill,
        candidate,
        WriteActor::Session {
            acting: &acting,
            request_sha256,
        },
        expected,
        &op_id,
        created_at,
        now,
    )
    .await
}

/// Reject an open proposal from a verified session (the orchestration half of
/// [`Authority::review_reject_session`]). `expected` is the proposal's base generation (reject moves no
/// pointer — the base is the row key); `reason` is mandatory and non-empty after trimming.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn review_reject_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &BundleId,
    candidate: CommitId,
    expected: Generation,
    reason: &str,
    request_id: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
    created_at: &str,
) -> Result<SetCurrentReceipt> {
    let acting = match session_preamble(
        authority,
        ws,
        skill,
        candidate,
        expected,
        DeviceOp::ReviewReject,
        request_id,
        acting_email,
        plane_mode,
        created_at,
    )
    .await?
    {
        Ok(acting) => acting,
        Err(refusal) => return Ok(refusal),
    };
    let reason = reason.trim();
    if reason.is_empty() {
        return Ok(synth_denied(
            DeviceOp::ReviewReject,
            skill,
            candidate,
            expected,
            request_id,
            created_at,
            code_details(
                REASON_REQUIRED_CODE,
                "a rejection requires a non-empty reason",
            ),
        ));
    }
    let request_sha256 = review_request_sha256(
        "review_reject",
        ws,
        &acting,
        skill,
        candidate,
        expected,
        Some(reason),
    );
    let op_id = OpId::parse(request_id).map_err(crate::error::AuthorityError::internal)?;
    set_current::review_reject(
        authority,
        ws,
        skill,
        candidate,
        WriteActor::Session {
            acting: &acting,
            request_sha256,
        },
        expected,
        Some(reason),
        &op_id,
        created_at,
    )
    .await
}

/// **Revert a skill's `current` to a known-good prior version from a verified session** — the browser's
/// "Roll back to this version" (the orchestration half of [`Authority::revert_session`]). `good` is the full
/// target commit id; `expected` is the live `current` generation the caller's version page rendered against —
/// a moved pointer refuses with the same `CONFLICT` the CLI gets. Revert bypasses the review gate + four-eyes
/// by design (it restores already-consented bytes — the safety net); the session gate here is the SAME
/// confirmed **owner|reviewer** seat the approve lane enforces (a deliberate lane asymmetry: the device lane
/// gates revert on per-skill roster).
///
/// Unlike approve (which promotes an already-rooted proposal), a revert CONSTRUCTS + leases a forward commit
/// BEFORE the transaction, so this leg runs a CHEAP PRE-STAGE owner|reviewer fence (a pool read) to turn a
/// confirmed plain member away BEFORE that git work — synthesized, never persisted, exactly like the
/// non-member gate above; the in-transaction role gate stays authoritative (it re-checks server-trusted rows,
/// catching a role that changes between the fence and the txn). This is the lane's gate-before-reach posture
/// applied to a staging write (codex design-gate finding 3).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn revert_session(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &BundleId,
    good: CommitId,
    expected: Generation,
    request_id: &str,
    acting_email: &str,
    plane_mode: DeploymentMode,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    let acting = match session_preamble(
        authority,
        ws,
        skill,
        good,
        expected,
        DeviceOp::Revert,
        request_id,
        acting_email,
        plane_mode,
        created_at,
    )
    .await?
    {
        Ok(acting) => acting,
        Err(refusal) => return Ok(refusal),
    };

    let request_sha256 = revert_request_sha256(ws, &acting, skill, good, expected);

    // Replay BEFORE the role fence — mirroring the in-transaction path, whose `run` replays BEFORE its role
    // gate. A recorded result for THIS request must replay byte-identically on a lost-ack retry REGARDLESS
    // of a later role change: an owner|reviewer whose successful revert's ack was lost, then demoted to a
    // plain member, retries the same `request_id` and is owed the stored `Reverted`, never a fresh role
    // denial (codex review P2). The stable match needs good's recorded tree digest; an ABSENT one means no
    // stored OK can exist (a session digest-absent failure is synthesized, never persisted), so fall through
    // to the fence. (`set_current::revert` re-runs this same stable replay for a FRESH request — a cheap
    // no-op here on the common path.)
    if let Some(good_digest) = authority.db().commit_bundle_digest(ws, skill, good).await?
        && let Some(replayed) = authority
            .db()
            .replay_revert_session(
                ws,
                acting.as_str(),
                request_id,
                skill,
                good_digest,
                expected,
                request_sha256,
            )
            .await?
    {
        return Ok(replayed);
    }

    // The pre-stage owner|reviewer fence (only a FRESH request reaches it — a recorded one replayed above).
    // A confirmed member who is neither owner nor reviewer gets the machine-branchable
    // `REVIEWER_ROLE_REQUIRED` — synthesized (a pre-stage session refusal writes nothing: the only actor
    // that mints a durable revert receipt on this lane is an authorized owner|reviewer, and a member's
    // denied attempt must not grow the ledger or stage a forward commit). A role that changes between here
    // and the transaction is caught by the authoritative in-txn gate.
    let is_reviewer = matches!(
        authority.db().member_role(ws, &acting).await?,
        Some((role, status))
            if status == "confirmed"
                && (role == Role::Owner.as_str() || role == Role::Reviewer.as_str())
    );
    if !is_reviewer {
        return Ok(synth_denied(
            DeviceOp::Revert,
            skill,
            good,
            expected,
            request_id,
            created_at,
            code_details(REVIEWER_ROLE_REQUIRED_CODE, REVIEWER_ROLE_REQUIRED_MSG),
        ));
    }

    // `parse_op_id` in the preamble proved the canonical form, so this parse cannot fail for a well-formed id.
    let op_id = OpId::parse(request_id).map_err(crate::error::AuthorityError::internal)?;
    set_current::revert(
        authority,
        ws,
        skill,
        good,
        WriteActor::Session {
            acting: &acting,
            request_sha256,
        },
        expected,
        // The forward commit's author is the acting principal (the session lane's identity — the device lane
        // records the device key id); the message is the shared fixed revert string, for uniform history.
        acting.as_str(),
        SESSION_REVERT_MESSAGE,
        &op_id,
        created_at,
        now,
    )
    .await
}

#[cfg(test)]
mod tests {
    use topos_types::Generation;

    use super::review_request_sha256;
    use crate::id::{BundleId, CommitId, Principal, WorkspaceId};

    fn fixture() -> (WorkspaceId, Principal, BundleId, CommitId, Generation) {
        (
            WorkspaceId::parse("w_1234").expect("workspace id"),
            Principal::parse("reviewer@acme.com").expect("principal"),
            BundleId::parse("s_demo").expect("skill id"),
            CommitId([7u8; 32]),
            Generation { epoch: 1, seq: 3 },
        )
    }

    #[test]
    fn review_request_identity_is_deterministic_and_payload_bound() {
        let (ws, acting, skill, candidate, expected) = fixture();
        let a = review_request_sha256(
            "review_approve",
            &ws,
            &acting,
            &skill,
            candidate,
            expected,
            None,
        );
        let b = review_request_sha256(
            "review_approve",
            &ws,
            &acting,
            &skill,
            candidate,
            expected,
            None,
        );
        assert_eq!(a, b);
        // A different verb, generation, or reason each changes the identity.
        let c = review_request_sha256(
            "review_reject",
            &ws,
            &acting,
            &skill,
            candidate,
            expected,
            None,
        );
        assert_ne!(a, c);
        let moved = Generation { epoch: 1, seq: 4 };
        let d = review_request_sha256(
            "review_approve",
            &ws,
            &acting,
            &skill,
            candidate,
            moved,
            None,
        );
        assert_ne!(a, d);
        let e = review_request_sha256(
            "review_reject",
            &ws,
            &acting,
            &skill,
            candidate,
            expected,
            Some("too broad"),
        );
        let f = review_request_sha256(
            "review_reject",
            &ws,
            &acting,
            &skill,
            candidate,
            expected,
            Some("too narrow"),
        );
        assert_ne!(e, f);
    }

    #[test]
    fn part_boundaries_cannot_be_shifted_and_tags_differ_across_domains() {
        let (ws, acting, skill, candidate, expected) = fixture();
        // The roster leg's hasher over a byte-identical payload must NEVER collide with this domain's:
        // rebuild this domain's preimage under the roster tag and compare.
        let ours = review_request_sha256("x", &ws, &acting, &skill, candidate, expected, None);
        let theirs = {
            let epoch_be = expected.epoch.to_be_bytes();
            let seq_be = expected.seq.to_be_bytes();
            let parts: Vec<&[u8]> = vec![
                b"x",
                ws.as_str().as_bytes(),
                acting.as_str().as_bytes(),
                skill.as_str().as_bytes(),
                candidate.0.as_slice(),
                epoch_be.as_slice(),
                seq_be.as_slice(),
            ];
            let tag = b"TOPOS_SESSION_ROSTER_V1\0";
            let mut buf = Vec::new();
            buf.extend_from_slice(tag);
            for part in parts {
                buf.extend_from_slice(&(part.len() as u64).to_be_bytes());
                buf.extend_from_slice(part);
            }
            topos_core::digest::sha256(&buf)
        };
        assert_ne!(ours, theirs);
        // Length-prefixing: an empty reason part is distinct from no reason part at all.
        let with_empty = review_request_sha256(
            "review_reject",
            &ws,
            &acting,
            &skill,
            candidate,
            expected,
            Some(""),
        );
        let without = review_request_sha256(
            "review_reject",
            &ws,
            &acting,
            &skill,
            candidate,
            expected,
            None,
        );
        assert_ne!(with_empty, without);
    }
}
