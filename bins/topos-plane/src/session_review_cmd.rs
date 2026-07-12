//! Session-review wrappers — the leak-free [`PlaneState`] surface for the PRIVILEGED web-session
//! review ops (approve / reject / the proposal-detail read).
//!
//! Deliberately LIB-ONLY (there is no OSS HTTP route for any of these): a downstream composition's
//! authenticated admin routes call them with a session-verified acting email. Like
//! [`roster_cmd`](crate::roster_cmd), every signature carries only plain/owned types, and each
//! wrapper parses the plane's deployment mode STRICTLY (fail closed) — though the mode no longer gates
//! these ops: the acting gate is the confirmed-seat role check, the same on a self-host plane and a
//! hosted one. The write terminates in the authority's one serializable pointer-move
//! transaction — same approve predicate, same compare-and-set, same pointer advance, same
//! four-eyes gate as the device-credential lane.
//!
//! CLASSIFICATION POSTURE: reviews disclose nothing on a malformed or unknown identity — a
//! non-parsing workspace/skill/version id, the uniform acting-gate denial, and a synthesized
//! pre-transaction miss (an unknown candidate) all fold to [`SessionReviewSummary::NotFound`]. The
//! member-entitled protocol refusals (the role gate, four-eyes, a resolved target, a reused id)
//! stay typed [`SessionReviewSummary::Denied`] so the composing surface can say why.

use plane_store::{
    AuthorityError, REVIEWER_ROLE_REQUIRED_CODE, SESSION_REVIEW_ACTING_DENIED, SetCurrentReceipt,
    SkillId, WorkspaceId,
};
use topos_types::TerminalOutcome;

use crate::state::PlaneState;
use crate::wire;

/// The outcome of [`PlaneState::review_approve_session`] / [`PlaneState::review_reject_session`].
/// Plain owned fields only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReviewSummary {
    /// The approve promoted the candidate (or the identical request replayed its OK).
    Approved,
    /// The reject resolved the proposal (a fresh rejection, or the idempotent already-rejected).
    Rejected,
    /// The compare-and-set refused a moved pointer — the same stale-base refusal the CLI gets
    /// (approve only; a reject has no pointer to conflict with).
    Conflict,
    /// A member-entitled typed refusal (the role gate, four-eyes, a resolved target, an empty
    /// reason, a reused request id). `reason` is the plane's static string, relayed verbatim.
    Denied {
        /// The static, typed reason (a plane→composition byte contract; never an oracle).
        reason: String,
    },
    /// The uniform miss: a malformed id, an unproven caller, or an unknown candidate — none of which
    /// discloses anything.
    NotFound,
}

/// One proposal's detail, as [`PlaneState::read_proposal_session`] discloses it to a confirmed
/// member. Plain owned fields only; `version_id` is the 64-char lowercase-hex candidate commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProposalDetail {
    pub version_id: String,
    /// The STORED status (`open` / `accepted` / `rejected`) — `stale` stays derived by the reader
    /// (an `open` row whose base no longer equals the live current generation).
    pub status: String,
    pub base_epoch: u64,
    pub base_seq: u64,
    pub created_at: String,
    /// The proposer's canonical email (the four-eyes display surface; session-lane-only).
    pub proposer: String,
    /// The workspace's review-required policy at read time (display-only; the in-transaction gate
    /// is the authority).
    pub review_required: bool,
    pub resolved_by: Option<String>,
    pub resolved_reason: Option<String>,
    pub resolved_at: Option<String>,
}

/// The outcome of [`PlaneState::read_proposal_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionProposalDetailSummary {
    /// The proposal's stored facts.
    Detail(Box<SessionProposalDetail>),
    /// The uniform miss (malformed ids, unproven caller, never-proposed candidate).
    NotFound,
}

/// The outcome of [`PlaneState::revert_session`]. Plain owned fields only. Distinct from
/// [`SessionReviewSummary`] because a revert PROMOTES (`Reverted`, never `Approved`/`Rejected`) and its
/// member-entitled refusals are the reviewer-role gate + the target refusals, not the four-eyes/not-open
/// family (codex design-gate finding 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRevertSummary {
    /// The revert moved `current` to the target's bytes (or the identical request replayed its OK).
    Reverted,
    /// The compare-and-set refused a moved pointer — the same stale refusal the CLI's revert gets.
    Conflict,
    /// A member-entitled typed refusal: the reviewer-role gate, a non-accepted / digest-less / no-current
    /// target, or a reused request id. `reason` is the plane's static string, relayed verbatim.
    Denied {
        /// The static, typed reason (a plane→composition byte contract; never an oracle).
        reason: String,
    },
    /// The uniform miss: a malformed id or an unproven caller — disclosing nothing.
    NotFound,
}

/// Classify a review receipt into the wrapper summary (the single table both verbs share).
fn classify(receipt: &SetCurrentReceipt, is_approve: bool) -> SessionReviewSummary {
    let code = receipt
        .details
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let message = receipt
        .details
        .as_ref()
        .and_then(|d| d.get("message"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    match receipt.outcome {
        TerminalOutcome::Ok => {
            if is_approve {
                SessionReviewSummary::Approved
            } else {
                SessionReviewSummary::Rejected
            }
        }
        TerminalOutcome::Conflict => SessionReviewSummary::Conflict,
        TerminalOutcome::Denied => {
            // The uniform acting-gate denial (which also covers an unparseable acting email)
            // discloses nothing.
            if message.as_deref() == Some(SESSION_REVIEW_ACTING_DENIED) {
                return SessionReviewSummary::NotFound;
            }
            // Everything else here is member-entitled: the role gate (a typed code + message),
            // four-eyes / not-open / already-accepted (messages), REASON_REQUIRED (code+message),
            // and the remaining in-txn denials. Relay the most specific string we have.
            let reason = match (code.as_deref(), message) {
                (Some(REVIEWER_ROLE_REQUIRED_CODE), Some(m)) => m,
                (_, Some(m)) => m,
                (Some(c), None) => c.to_owned(),
                (None, None) => "denied".to_owned(),
            };
            SessionReviewSummary::Denied { reason }
        }
        TerminalOutcome::PermanentFailure => {
            if code.as_deref() == Some("OP_ID_REUSED") {
                SessionReviewSummary::Denied {
                    reason: "op id reused with a different request".to_owned(),
                }
            } else {
                // A synthesized pre-transaction miss (no such candidate / no proposal) — an
                // unknown candidate discloses nothing.
                SessionReviewSummary::NotFound
            }
        }
        // No other terminal outcome is reachable on the session review verbs; fold the remainder
        // to the uniform miss rather than invent a new disclosure.
        _ => SessionReviewSummary::NotFound,
    }
}

/// Classify a REVERT receipt into [`SessionRevertSummary`]. Mirrors [`classify`], but a revert has no
/// four-eyes / not-open family: its member-entitled failures are the reviewer-role gate and the target
/// refusals (no recorded digest / not an accepted version / no current), all disclosable to the authorized
/// owner|reviewer that reached the staging path (never an oracle — a reviewer already reads the catalog).
fn classify_revert(receipt: &SetCurrentReceipt) -> SessionRevertSummary {
    let code = receipt
        .details
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let message = receipt
        .details
        .as_ref()
        .and_then(|d| d.get("message"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    match receipt.outcome {
        TerminalOutcome::Ok => SessionRevertSummary::Reverted,
        TerminalOutcome::Conflict => SessionRevertSummary::Conflict,
        TerminalOutcome::Denied => {
            // The uniform acting-gate denial (non-member / unparseable email) discloses nothing.
            if message.as_deref() == Some(SESSION_REVIEW_ACTING_DENIED) {
                return SessionRevertSummary::NotFound;
            }
            // Member-entitled: the reviewer-role gate (code + message). Relay the most specific string.
            let reason = match (code.as_deref(), message) {
                (Some(REVIEWER_ROLE_REQUIRED_CODE), Some(m)) => m,
                (_, Some(m)) => m,
                (Some(c), None) => c.to_owned(),
                (None, None) => "denied".to_owned(),
            };
            SessionRevertSummary::Denied { reason }
        }
        TerminalOutcome::PermanentFailure => {
            if code.as_deref() == Some("OP_ID_REUSED") {
                SessionRevertSummary::Denied {
                    reason: "op id reused with a different request".to_owned(),
                }
            } else {
                // A synthesized pre-transaction refusal about the TARGET (no recorded digest / not an
                // accepted version / no current pointer) — reached only past the owner|reviewer pre-gate, so
                // the specific reason is member-entitled.
                SessionRevertSummary::Denied {
                    reason: message
                        .unwrap_or_else(|| "the revert target is not a valid version".to_owned()),
                }
            }
        }
        // No other terminal outcome is reachable on the session revert verb; fold to the uniform miss.
        _ => SessionRevertSummary::NotFound,
    }
}

impl PlaneState {
    /// Approve an open proposal from a session-verified email (the composing web surface proves
    /// the email; this wrapper never does). `version_id_hex` names the candidate commit;
    /// `expected_epoch`/`expected_seq` are the generation the caller's rendered diff was computed
    /// against — a moved pointer refuses with [`SessionReviewSummary::Conflict`]. Idempotent per
    /// `request_id` (a canonical UUID).
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault; every
    /// protocol refusal is a typed summary, never an error.
    #[allow(clippy::too_many_arguments)]
    pub async fn review_approve_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version_id_hex: &str,
        expected_epoch: u64,
        expected_seq: u64,
        request_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionReviewSummary> {
        let mode = self.strict_mode()?;
        let Some((ws, skill, candidate)) = parse_review_ids(workspace_id, skill_id, version_id_hex)
        else {
            return Ok(SessionReviewSummary::NotFound);
        };
        let (created_at, now) = wire::now_utc();
        let receipt = self
            .authority()
            .review_approve_session(
                &ws,
                &skill,
                candidate,
                topos_types::Generation {
                    epoch: expected_epoch,
                    seq: expected_seq,
                },
                request_id,
                acting_email,
                mode,
                &created_at,
                now,
            )
            .await
            .map_err(|error| anyhow::anyhow!("approving the proposal: {error}"))?;
        Ok(classify(&receipt, true))
    }

    /// Reject an open proposal from a session-verified email, with a MANDATORY non-empty `reason`
    /// (recorded on the proposal; relayed to the CLI's detail-reading surfaces). `expected_*` is
    /// the proposal's base generation (a reject moves no pointer). Idempotent per `request_id`.
    ///
    /// # Errors
    /// As [`review_approve_session`](Self::review_approve_session).
    #[allow(clippy::too_many_arguments)]
    pub async fn review_reject_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version_id_hex: &str,
        expected_epoch: u64,
        expected_seq: u64,
        reason: &str,
        request_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionReviewSummary> {
        let mode = self.strict_mode()?;
        let Some((ws, skill, candidate)) = parse_review_ids(workspace_id, skill_id, version_id_hex)
        else {
            return Ok(SessionReviewSummary::NotFound);
        };
        let (created_at, _now) = wire::now_utc();
        let receipt = self
            .authority()
            .review_reject_session(
                &ws,
                &skill,
                candidate,
                topos_types::Generation {
                    epoch: expected_epoch,
                    seq: expected_seq,
                },
                reason,
                request_id,
                acting_email,
                mode,
                &created_at,
            )
            .await
            .map_err(|error| anyhow::anyhow!("rejecting the proposal: {error}"))?;
        Ok(classify(&receipt, false))
    }

    /// One proposal's detail for a confirmed member — the review surface's read (status + base +
    /// proposer + resolution + the review-required policy at read time). Every miss is the single
    /// indistinguishable [`SessionProposalDetailSummary::NotFound`].
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault.
    pub async fn read_proposal_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version_id_hex: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionProposalDetailSummary> {
        let mode = self.strict_mode()?;
        let Ok(ws) = WorkspaceId::parse(workspace_id) else {
            return Ok(SessionProposalDetailSummary::NotFound);
        };
        let detail = match self
            .authority()
            .read_proposal_detail_session(&ws, skill_id, version_id_hex, acting_email, mode)
            .await
        {
            Ok(Some(detail)) => detail,
            Ok(None) | Err(AuthorityError::NotFound) => {
                return Ok(SessionProposalDetailSummary::NotFound);
            }
            Err(error) => {
                return Err(anyhow::anyhow!("reading the proposal detail: {error}"));
            }
        };
        Ok(SessionProposalDetailSummary::Detail(Box::new(
            SessionProposalDetail {
                version_id: topos_core::digest::to_hex(&detail.version_id),
                status: detail.status,
                base_epoch: detail.base.epoch,
                base_seq: detail.base.seq,
                created_at: detail.created_at,
                proposer: detail.proposer,
                review_required: detail.review_required,
                resolved_by: detail.resolved_by,
                resolved_reason: detail.resolved_reason,
                resolved_at: detail.resolved_at,
            },
        )))
    }

    /// Revert a skill's `current` to a known-good prior version from a session-verified email (the
    /// browser's "Roll back to this version"). `good_version_id_hex` names the target commit;
    /// `expected_epoch`/`expected_seq` are the live current generation the caller's version page rendered
    /// against — a moved pointer refuses with [`SessionRevertSummary::Conflict`]. Restricted to a confirmed
    /// owner|reviewer seat (the plane's in-transaction gate + a pre-stage fence). Idempotent per
    /// `request_id` (a canonical UUID).
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault; every protocol
    /// refusal is a typed summary, never an error.
    #[allow(clippy::too_many_arguments)]
    pub async fn revert_session(
        &self,
        workspace_id: &str,
        skill_id: &str,
        good_version_id_hex: &str,
        expected_epoch: u64,
        expected_seq: u64,
        request_id: &str,
        acting_email: &str,
    ) -> anyhow::Result<SessionRevertSummary> {
        let mode = self.strict_mode()?;
        let Some((ws, skill, good)) = parse_review_ids(workspace_id, skill_id, good_version_id_hex)
        else {
            return Ok(SessionRevertSummary::NotFound);
        };
        let (created_at, now) = wire::now_utc();
        let receipt = self
            .authority()
            .revert_session(
                &ws,
                &skill,
                good,
                topos_types::Generation {
                    epoch: expected_epoch,
                    seq: expected_seq,
                },
                request_id,
                acting_email,
                mode,
                &created_at,
                now,
            )
            .await
            .map_err(|error| anyhow::anyhow!("reverting the skill: {error}"))?;
        Ok(classify_revert(&receipt))
    }
}

/// Parse the three review-scoped ids; `None` (the uniform miss) on any malformed one. The version
/// id must be exactly 64 lowercase-hex chars — the authority re-checks, but failing here keeps the
/// wrapper's NotFound uniform without a round trip.
fn parse_review_ids(
    workspace_id: &str,
    skill_id: &str,
    version_id_hex: &str,
) -> Option<(WorkspaceId, SkillId, plane_store::CommitId)> {
    let ws = WorkspaceId::parse(workspace_id).ok()?;
    let skill = SkillId::parse(skill_id).ok()?;
    let commit = parse_version_hex(version_id_hex)?;
    Some((ws, skill, commit))
}

/// Parse a version id as exactly 64 lowercase-hex chars; `None` on any other shape. Shared by every
/// session wrapper that names a version (review/revert here, the lifecycle purge in
/// [`lifecycle_cmd`](crate::lifecycle_cmd)).
pub(crate) fn parse_version_hex(version_id_hex: &str) -> Option<plane_store::CommitId> {
    if version_id_hex.len() != 64
        || !version_id_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, slot) in bytes.iter_mut().enumerate() {
        let hi = hex_val(version_id_hex.as_bytes()[2 * i]);
        let lo = hex_val(version_id_hex.as_bytes()[2 * i + 1]);
        *slot = (hi << 4) | lo;
    }
    Some(plane_store::CommitId(bytes))
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        _ => b - b'a' + 10,
    }
}

#[cfg(test)]
mod tests {
    use plane_store::SetCurrentReceipt;
    use topos_types::{Generation, TerminalOutcome};

    use super::{SessionReviewSummary, classify, parse_review_ids};

    fn receipt(outcome: TerminalOutcome, details: Option<serde_json::Value>) -> SetCurrentReceipt {
        SetCurrentReceipt {
            op_id: "8c4b1a52-6f0f-4a3e-9a51-0e2f6f5a7c11".to_owned(),
            command: "review-approve".to_owned(),
            skill_id: "s_demo".to_owned(),
            version_id: None,
            bundle_digest: None,
            expected: Generation { epoch: 1, seq: 1 },
            outcome,
            current: None,
            record: None,
            created_at: "2026-07-07T00:00:00Z".to_owned(),
            details,
        }
    }

    #[test]
    fn the_classification_table_holds() {
        // OK maps by verb.
        assert_eq!(
            classify(&receipt(TerminalOutcome::Ok, None), true),
            SessionReviewSummary::Approved
        );
        assert_eq!(
            classify(
                &receipt(
                    TerminalOutcome::Ok,
                    Some(serde_json::json!({"code": "PROPOSAL_ALREADY_REJECTED"}))
                ),
                false
            ),
            SessionReviewSummary::Rejected
        );
        // CONFLICT is the stale-base refusal.
        assert_eq!(
            classify(&receipt(TerminalOutcome::Conflict, None), true),
            SessionReviewSummary::Conflict
        );
        // The uniform acting-gate denial discloses nothing.
        assert_eq!(
            classify(
                &receipt(
                    TerminalOutcome::Denied,
                    Some(serde_json::json!({
                        "message": plane_store::SESSION_REVIEW_ACTING_DENIED
                    }))
                ),
                true
            ),
            SessionReviewSummary::NotFound
        );
        // The role gate is member-entitled and typed.
        assert_eq!(
            classify(
                &receipt(
                    TerminalOutcome::Denied,
                    Some(serde_json::json!({
                        "code": plane_store::REVIEWER_ROLE_REQUIRED_CODE,
                        "message": "approving or rejecting needs an owner or reviewer seat"
                    }))
                ),
                true
            ),
            SessionReviewSummary::Denied {
                reason: "approving or rejecting needs an owner or reviewer seat".to_owned()
            }
        );
        // Key reuse is typed; other permanent failures (synthesized pre-txn misses) fold to the miss.
        assert_eq!(
            classify(
                &receipt(
                    TerminalOutcome::PermanentFailure,
                    Some(serde_json::json!({"code": "OP_ID_REUSED"}))
                ),
                true
            ),
            SessionReviewSummary::Denied {
                reason: "op id reused with a different request".to_owned()
            }
        );
        assert_eq!(
            classify(
                &receipt(
                    TerminalOutcome::PermanentFailure,
                    Some(serde_json::json!({"message": "no proposal for this candidate and base"}))
                ),
                true
            ),
            SessionReviewSummary::NotFound
        );
    }

    #[test]
    fn review_ids_parse_strictly() {
        let hex = "ab".repeat(32);
        assert!(parse_review_ids("w_1234", "s_demo", &hex).is_some());
        // Uppercase hex, short hex, and malformed ids are the uniform miss.
        assert!(parse_review_ids("w_1234", "s_demo", &hex.to_uppercase()).is_none());
        assert!(parse_review_ids("w_1234", "s_demo", &hex[1..]).is_none());
        assert!(parse_review_ids("", "s_demo", &hex).is_none());
        assert!(parse_review_ids("w_1234", "not a skill id!", &hex).is_none());
    }
}
