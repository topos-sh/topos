//! The domain ⇄ wire mappers — the ONE place a [`SetCurrentReceipt`] becomes a canonical [`JsonEnvelope`], a
//! [`WireCandidate`] becomes a [`CandidateUpload`], and a [`VersionMeta`] becomes a [`WireVersionMeta`]. A
//! handler NEVER builds these by hand (no string-format drift, one home for the outcome→action policy).

use base64::Engine as _;
use plane_store::{
    AppliedSkill, CandidateUpload, ChannelIndexEntry, ChannelMembershipOutcome, CommitId,
    CurationOutcome, Delivery, DeploymentMode as StoreDeploymentMode, DeviceAuthPoll,
    DeviceAuthStart, GovernanceOutcome, InviteBootstrap, LoginOutcome, LoginSeat, Me,
    OpenProposalSummary, PasscodeComplete, ProposalIndexEntry, ProtectOutcome, Reach,
    RedeemOutcome, SessionIntent as StoreSessionIntent, SetCurrentReceipt, SkillId, SkillIndexRow,
    SkillLog, SubscriptionOutcome, UploadedFile, VerificationContext, VersionMeta,
};
use topos_types::bootstrap::{
    BootstrapData, BootstrapInvite, BootstrapPlane, BootstrapWorkspace, ConsentMode,
    DeploymentMode, VerifiedDomainStatus,
};
use topos_types::requests::{
    DeviceAuthorizeResponse, DeviceTokenResponse, DeviceTokenStatus, DeviceTokenWorkspace,
    InvitationData, LoginData, LoginMembership, PasscodeConfirmResponse, PasscodeConfirmStatus,
    RedeemResponse, SessionIntent, VerificationContextResponse, WireAppliedReport, WireCandidate,
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireDelivery, WireDeliverySkill,
    WireLogProposal, WireLogVersion, WireMe, WireNotice, WireOpenProposal, WireProposalEntry,
    WireProposalIndex, WireProposalList, WireProtocolCard, WireReach, WireSkillIndex,
    WireSkillIndexEntry, WireSkillLog, WireVersionFile, WireVersionMeta, WireVia,
};
use topos_types::{
    ActionCode, Affected, JsonEnvelope, NextAction, RECEIPT_SCHEMA_VERSION, Receipt,
    TerminalOutcome, WIRE_SCHEMA_VERSION, WireCurrentRecord, WireError,
};

use super::error::PlaneHttpError;

/// Build the canonical [`JsonEnvelope`] for a returned pointer-move/contribute [`SetCurrentReceipt`].
///
/// HTTP status is ALWAYS 200 for a returned receipt — EVERY protocol outcome rides in the body. `ok` is true
/// for `OK` / `NEEDS_REVIEW`; on a failure outcome a flat [`WireError`] carries the code + retryability + the
/// right next-actions (mirrored onto the envelope). On `OK` the parsed `WireCurrentRecord` lands in `data`
/// (so a client can read the moved pointer straight from the response); otherwise `data` is `{}`.
pub(crate) fn write_envelope(receipt: &SetCurrentReceipt, ws: &str) -> JsonEnvelope {
    let outcome = receipt.outcome;
    let version_hex = receipt.version_id.map(|c| hex::encode(c.0));
    let command = wire_command(&receipt.command).to_owned();

    let wire_receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        op_id: receipt.op_id.clone(),
        command: command.clone(),
        outcome,
        workspace_id: ws.to_owned(),
        skill_id: Some(receipt.skill_id.clone()),
        version_id: version_hex.clone(),
        bundle_digest: receipt.bundle_digest.map(hex::encode),
        expected_generation: Some(receipt.expected),
        current_generation: receipt.current,
        created_at: receipt.created_at.clone(),
        details: receipt.details.clone(),
    };

    let ok = matches!(outcome, TerminalOutcome::Ok | TerminalOutcome::NeedsReview);

    // On OK, surface the current record in `data` (a client reads the moved pointer from it); else `data = {}`.
    let data = if outcome == TerminalOutcome::Ok {
        receipt
            .record
            .as_ref()
            .and_then(|bytes| serde_json::from_slice::<WireCurrentRecord>(bytes).ok())
            .and_then(|record| serde_json::to_value(record).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // A failure outcome carries the flat WireError (mirrored onto the envelope's next_actions); OK /
    // NEEDS_REVIEW carry neither an error nor next-actions.
    let (error, next_actions) = if ok {
        (None, vec![])
    } else {
        let actions = next_actions_for(outcome);
        let error = WireError {
            code: error_code(receipt),
            outcome,
            retryable: retryable(outcome),
            affected: Affected {
                workspace: Some(ws.to_owned()),
                skill: Some(receipt.skill_id.clone()),
                version: version_hex,
                proposal: None,
            },
            expected_generation: Some(receipt.expected),
            current_generation: receipt.current,
            context: receipt
                .details
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
            next_actions: actions.clone(),
        };
        (Some(error), actions)
    };

    JsonEnvelope {
        schema_version: WIRE_SCHEMA_VERSION,
        command,
        ok,
        data,
        warnings: vec![],
        next_actions,
        receipt: Some(wire_receipt),
        error,
    }
}

/// Map an inbound [`WireCandidate`] to the authority's [`CandidateUpload`]: base64-decode each file's bytes
/// (the server then rehashes them), hex-decode each parent into a [`CommitId`], translate the modes.
pub(crate) fn candidate_to_domain(c: WireCandidate) -> Result<CandidateUpload, PlaneHttpError> {
    let mut files = Vec::with_capacity(c.files.len());
    for f in c.files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(f.content_base64.as_bytes())
            .map_err(|_| {
                PlaneHttpError::BadBody(format!("file {:?}: invalid base64 content", f.path))
            })?;
        files.push(UploadedFile {
            path: f.path,
            mode: super::domain_mode(f.mode),
            bytes,
        });
    }
    let mut parents = Vec::with_capacity(c.parents.len());
    for p in c.parents {
        let bytes = super::hex32(&p)
            .ok_or_else(|| PlaneHttpError::BadBody(format!("invalid parent commit id {p:?}")))?;
        parents.push(CommitId(bytes));
    }
    Ok(CandidateUpload {
        files,
        parents,
        author: c.author,
        message: c.message,
    })
}

/// Map a [`VersionMeta`] to its wire shape — hex-encode each 32-byte id, translate each file mode.
pub(crate) fn version_meta_to_wire(meta: VersionMeta) -> WireVersionMeta {
    WireVersionMeta {
        version_id: hex::encode(meta.version_id),
        parents: meta.parents.iter().map(hex::encode).collect(),
        author: meta.author,
        message: meta.message,
        bundle_digest: hex::encode(meta.bundle_digest),
        files: meta
            .files
            .into_iter()
            .map(|f| WireVersionFile {
                path: f.path,
                mode: super::wire_mode(f.mode),
                object_id: hex::encode(f.object_id),
            })
            .collect(),
    }
}

/// Map a skill's OPEN-proposal summaries to the wire [`WireProposalList`] — hex-encode each `version_id`,
/// carry the base generation + `created_at` through. NO bytes, no proposer (the domain summary holds none).
pub(crate) fn open_proposals_to_wire(v: Vec<OpenProposalSummary>) -> WireProposalList {
    WireProposalList {
        proposals: v
            .into_iter()
            .map(|p| WireOpenProposal {
                version_id: hex::encode(p.version_id),
                base_generation: p.base,
                created_at: p.created_at,
            })
            .collect(),
    }
}

/// Map the authority's workspace skill index (`Vec<SkillIndexRow>`) to the wire [`WireSkillIndex`] — the
/// device-credential catalog read's body. Hex-encode each 32-byte `version_id` + `bundle_digest` (the same
/// `hex::encode` [`version_meta_to_wire`] uses), carrying the generation / display name / update time /
/// open-proposal count through. NO bytes — the catalog is metadata only.
pub(crate) fn skill_index_to_wire(rows: Vec<SkillIndexRow>) -> WireSkillIndex {
    WireSkillIndex {
        skills: rows
            .into_iter()
            .map(|r| WireSkillIndexEntry {
                skill_id: r.skill_id,
                name: r.name,
                status: r.status,
                version_id: hex::encode(r.version_id),
                bundle_digest: hex::encode(r.bundle_digest),
                generation: r.generation,
                display_name: r.display_name,
                updated_at: r.updated_at,
                open_proposals: r.open_proposals,
            })
            .collect(),
    }
}

/// Map the authority's [`Delivery`] (the per-device currency answer) to the wire [`WireDelivery`] —
/// hex-encode each 32-byte id (the same `hex::encode` [`skill_index_to_wire`] uses), carry the `via`
/// attribution / protection / timestamps through, and stamp the `workspace_id` from the request path. **NO
/// bytes** — the client fetches each version through the per-blob bundle read after reconciling.
pub(crate) fn delivery_to_wire(d: Delivery, ws: &str) -> WireDelivery {
    WireDelivery {
        schema_version: 1,
        workspace_id: ws.to_owned(),
        staleness_window_ms: d.staleness_window_ms,
        skills: d
            .skills
            .into_iter()
            .map(|s| WireDeliverySkill {
                skill_id: s.skill_id,
                name: s.name,
                display_name: s.display_name,
                protection: s.protection,
                version_id: hex::encode(s.version_id),
                bundle_digest: hex::encode(s.bundle_digest),
                generation: s.generation,
                updated_at: s.updated_at,
                via: WireVia {
                    channels: s.via_channels,
                    direct: s.direct,
                },
            })
            .collect(),
        detached: d.detached,
        excluded: d.excluded,
        notices: d
            .notices
            .into_iter()
            .map(|n| WireNotice {
                id: n.id,
                kind: n.kind,
                skill_id: n.skill_id,
                skill_name: n.skill_name,
                version_id: n.version_id.map(hex::encode),
                actor: n.actor,
                outcome: n.outcome,
                reason: n.reason,
                message: n.message,
                created_at: n.created_at,
            })
            .collect(),
        proposals_awaiting: d.proposals_awaiting,
    }
}

/// Map an inbound applied-state report to the authority's `Vec<AppliedSkill>`: parse each skill id and
/// hex-decode each version id at the edge, mapping a bad id to a 400 (mirroring [`candidate_to_domain`]).
pub(crate) fn applied_report_to_domain(
    req: WireAppliedReport,
) -> Result<Vec<AppliedSkill>, PlaneHttpError> {
    let mut applied = Vec::with_capacity(req.applied.len());
    for a in req.applied {
        let skill_id =
            SkillId::parse(&a.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
        let version_id = super::hex32(&a.version_id).map(CommitId).ok_or_else(|| {
            PlaneHttpError::BadId(format!("invalid version id {:?}", a.version_id))
        })?;
        applied.push(AppliedSkill {
            skill_id,
            version_id,
        });
    }
    Ok(applied)
}

/// The CLI verb a domain command string maps to (the envelope's `command`).
fn wire_command(domain: &str) -> &str {
    match domain {
        "publish-direct" | "publish-propose" => "publish",
        "revert" => "revert",
        "review-approve" | "review-reject" => "review",
        other => other,
    }
}

/// The next-actions for a failure outcome (each with an empty `argv` — the client maps the `code` to its own
/// command; the plane does not know the client's local skill name or invocation).
fn next_actions_for(outcome: TerminalOutcome) -> Vec<NextAction> {
    let codes = match outcome {
        TerminalOutcome::Conflict => vec![ActionCode::RebaseAndRetry],
        TerminalOutcome::Denied => vec![ActionCode::RequestAccess, ActionCode::ContactAdmin],
        TerminalOutcome::Unavailable | TerminalOutcome::RetryableFailure => vec![ActionCode::Retry],
        _ => vec![],
    };
    codes
        .into_iter()
        .map(|code| NextAction { code, argv: vec![] })
        .collect()
}

/// Whether a failure outcome is worth a blind retry. A CAS `Conflict` is NOT: replaying the identical
/// request against a moved pointer conflicts forever — the caller must pull/rebase first (the client
/// computes the same `false`; both halves agree).
fn retryable(outcome: TerminalOutcome) -> bool {
    matches!(
        outcome,
        TerminalOutcome::Unavailable | TerminalOutcome::RetryableFailure
    )
}

/// The `WireError.code`: prefer a richer code the authority stamped into `details` (e.g.
/// `FIRST_PARENT_MISMATCH` on a `DENIED`), else the outcome's default code.
fn error_code(receipt: &SetCurrentReceipt) -> String {
    receipt
        .details
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| default_code(receipt.outcome).to_owned())
}

fn default_code(outcome: TerminalOutcome) -> &'static str {
    match outcome {
        TerminalOutcome::Ok => "OK",
        TerminalOutcome::NeedsReview => "NEEDS_REVIEW",
        TerminalOutcome::Conflict => "CONFLICT",
        TerminalOutcome::Diverged => "DIVERGED",
        TerminalOutcome::Denied => "DENIED",
        TerminalOutcome::Unavailable => "UNAVAILABLE",
        TerminalOutcome::AmbiguousName => "AMBIGUOUS_NAME",
        TerminalOutcome::RetryableFailure => "RETRYABLE_FAILURE",
        TerminalOutcome::PermanentFailure => "PERMANENT_FAILURE",
    }
}

// =================================================================================================
// Enrollment / governance mappers — the domain ⇄ wire edge for the issuance routes. The unauthenticated
// reads (bootstrap, device-auth, verification, passcode) map to a plain typed DTO (a miss is the route's
// 404); the op_id-carrying WRITES (redeem, admin-claim, invite, roster, revoke) map every protocol outcome
// to a 200 all-outcome envelope (a DENIED is a 200 + the flat error, never a 403 — I-404 is only for
// skill-scoped object reads).
// =================================================================================================

/// Map an [`InviteBootstrap`] to the wire [`BootstrapData`] — the pre-enrollment payload. `token` is the
/// invite link token the client used, echoed as the non-secret `token_id` — for an INVITE only; the claim
/// route passes an empty placeholder instead, because a claim token is the live one-time bearer owner
/// capability and must never be repeated into a response body. The payload carries no trust root: the client
/// enrolls against the declared API base, and a `current` pointer's authority is the database row behind it.
pub(crate) fn bootstrap_to_wire(token: &str, b: InviteBootstrap) -> BootstrapData {
    BootstrapData {
        schema_version: WIRE_SCHEMA_VERSION,
        invite: BootstrapInvite {
            token_id: token.to_owned(),
            // The domain `InviteBootstrap` does not carry the invite's own expiry; the bootstrap omits it
            // (enrollment fails closed if the invite has expired, so the field is advisory only).
            expires_at: None,
            consent: ConsentMode::DirectHumanFirstReceive,
            // ALWAYS false — a first-received skill is offered, never silently landed.
            first_receive_auto_land: false,
        },
        plane: BootstrapPlane {
            base_url: b.base_url,
            deployment_mode: deployment_mode_to_wire(b.deployment_mode),
            enrollment_method: b.enrollment_method,
        },
        workspace: BootstrapWorkspace {
            workspace_id: b.workspace_id.as_str().to_owned(),
            display_name: b.display_name,
            verified_domain: b.verified_domain,
            verified_domain_status: verified_domain_status_to_wire(&b.verified_domain_status),
        },
        // The only bootstrap the `/i/` door serves now is a one-time admin CLAIM, which offers no skills
        // (the tokened INVITE door was interred; invitations became roster writes with no `/i/` link).
        offered_skills: vec![],
    }
}

/// Map a [`DeviceAuthStart`] to the wire [`DeviceAuthorizeResponse`]. `now` (the server clock the start was
/// stamped with) converts the absolute `expires_at` (epoch-ms) into the RFC-8628 relative `expires_in` (s).
/// `plane` is the plane block the start carries (the API base + posture + method the client re-roots onto):
/// every start now carries it, so enroll (by address, no `/i/` bootstrap), standup, and login all reach the
/// same base without a prior fetch.
pub(crate) fn device_auth_to_wire(
    s: DeviceAuthStart,
    now: i64,
    plane: Option<BootstrapPlane>,
) -> DeviceAuthorizeResponse {
    DeviceAuthorizeResponse {
        device_code: s.device_code,
        user_code: s.user_code,
        verification_uri: s.verification_uri,
        verification_uri_complete: Some(s.verification_uri_complete),
        expires_in: u64::try_from((s.expires_at - now) / 1000).unwrap_or(0),
        interval: u64::try_from(s.interval_secs).unwrap_or(0),
        plane,
    }
}

/// The plane block a `device/authorize` response carries — the base URL, deployment posture, and
/// enrollment method. Every start carries it now (an enroll start is by address, not an `/i/` bootstrap;
/// standup + login have no link either), so the client learns the API base from the response itself. It
/// carries no trust root: the client re-roots onto the declared base.
pub(crate) fn plane_block(
    state: &crate::state::PlaneState,
) -> Result<BootstrapPlane, PlaneHttpError> {
    // The AUTHORITY's enrollment config is the one source (the `/i/` bootstrap serves the same facts
    // from it) — a composition that builds through `PlaneState::new` never fills the state-side copy.
    let disclosure = state.authority().enrollment_disclosure()?;
    Ok(BootstrapPlane {
        base_url: disclosure.base_url,
        deployment_mode: deployment_mode_to_wire(disclosure.deployment_mode),
        enrollment_method: disclosure.enrollment_method,
    })
}

/// Map a [`DeviceAuthPoll`] to the wire [`DeviceTokenResponse`]. Only `Granted` carries the opaque grant.
/// A workspace-anchored grant (enroll / standup) also carries the workspace context a client lacks (id +
/// display name + address); a workspace-less LOGIN grant carries none (its seats come back at
/// `POST /v1/login`), so `workspace` is `None` there.
pub(crate) fn device_poll_to_wire(poll: DeviceAuthPoll) -> DeviceTokenResponse {
    let (status, grant, workspace) = match poll {
        DeviceAuthPoll::Pending => (DeviceTokenStatus::Pending, None, None),
        DeviceAuthPoll::SlowDown => (DeviceTokenStatus::SlowDown, None, None),
        DeviceAuthPoll::Denied => (DeviceTokenStatus::Denied, None, None),
        DeviceAuthPoll::Expired => (DeviceTokenStatus::Expired, None, None),
        DeviceAuthPoll::Granted(g) => {
            // A workspace-anchored grant carries its `{id, display name, address}`; a login grant is
            // workspace-less, so the block is absent (the login seats arrive at the redeem).
            let workspace = g.workspace_id.map(|id| DeviceTokenWorkspace {
                workspace_id: id.as_str().to_owned(),
                display_name: g.workspace_display_name,
                address: g.workspace_address,
            });
            (DeviceTokenStatus::Granted, Some(g.grant_token), workspace)
        }
    };
    DeviceTokenResponse {
        status,
        grant,
        workspace,
    }
}

/// Map a [`VerificationContext`] to the wire [`VerificationContextResponse`] (the verification-page disclosure).
pub(crate) fn verification_to_wire(v: VerificationContext) -> VerificationContextResponse {
    VerificationContextResponse {
        intent: Some(session_intent_to_wire(v.intent)),
        machine_name: v.machine_name,
        device_fingerprint: v.device_fingerprint,
        workspace_display_name: v.workspace_display_name,
        verified_domain: v.verified_domain,
        verified_domain_status: verified_domain_status_to_wire(&v.verified_domain_status),
        // The verification context no longer carries pre-offered skills (invitations are roster writes,
        // and enrollment is by address); the page renders its copy from `intent`.
        offered_skills: vec![],
    }
}

/// A stored session intent → its wire mirror.
fn session_intent_to_wire(intent: StoreSessionIntent) -> SessionIntent {
    match intent {
        StoreSessionIntent::Enroll => SessionIntent::Enroll,
        StoreSessionIntent::Standup => SessionIntent::Standup,
        StoreSessionIntent::Login => SessionIntent::Login,
    }
}

/// Map a [`PasscodeComplete`] to the wire [`PasscodeConfirmResponse`] — only the status crosses (a wrong-code
/// attempt count never reaches the wire).
pub(crate) fn passcode_complete_to_wire(c: PasscodeComplete) -> PasscodeConfirmResponse {
    let status = match c {
        PasscodeComplete::Confirmed => PasscodeConfirmStatus::Confirmed,
        PasscodeComplete::WrongCode { .. } => PasscodeConfirmStatus::WrongCode,
        PasscodeComplete::Expired => PasscodeConfirmStatus::Expired,
        PasscodeComplete::TooManyAttempts => PasscodeConfirmStatus::TooManyAttempts,
    };
    PasscodeConfirmResponse { status }
}

/// The all-outcome envelope for a redeem / admin-claim ([`RedeemOutcome`]): `Redeemed` → a 200 carrying the
/// [`RedeemResponse`] (the registered device + its ONE workspace credential, NEVER a user token, never a
/// per-skill token); `Denied` → a 200
/// carrying the uniform flat DENIED error (no static reason — never an oracle).
pub(crate) fn redeem_envelope(command: &str, outcome: RedeemOutcome) -> JsonEnvelope {
    match outcome {
        RedeemOutcome::Redeemed(r) => {
            let resp = RedeemResponse {
                workspace_id: r.workspace_id.as_str().to_owned(),
                device_key_id: r.device_key_id,
                principal: Some(r.principal.as_str().to_owned()),
                credential: r.credential,
            };
            ok_envelope(command, to_data(&resp))
        }
        RedeemOutcome::Denied(_) => denied_envelope(command),
    }
}

/// The all-outcome envelope for a LOGIN redeem ([`LoginOutcome`]): `Redeemed` → a 200 carrying the
/// [`LoginData`] (the proven identity + one re-minted credential — or a `blocked` marker — per confirmed
/// seat); `Denied` → the uniform flat DENIED error.
pub(crate) fn login_envelope(outcome: LoginOutcome) -> JsonEnvelope {
    match outcome {
        LoginOutcome::Redeemed(r) => {
            let data = LoginData {
                principal: r.principal.as_str().to_owned(),
                memberships: r.memberships.into_iter().map(login_membership).collect(),
            };
            ok_envelope("login", to_data(&data))
        }
        LoginOutcome::Denied(_) => denied_envelope("login"),
    }
}

/// One login seat → its wire [`LoginMembership`] (the freshly re-minted credential, or the `blocked` reason).
fn login_membership(s: LoginSeat) -> LoginMembership {
    LoginMembership {
        workspace_id: s.workspace_id.as_str().to_owned(),
        name: s.name,
        display_name: s.display_name,
        role: s.role,
        device_key_id: s.device_key_id,
        credential: s.credential,
        blocked: s.blocked.map(str::to_owned),
    }
}

/// The success envelope for an [`InviteOutcome::Invited`] — the workspace ADDRESS the invitees join at, the
/// folded invited set, and the honest `mailed` flag (the mailing itself is the handler's fire-and-forget).
/// The two typed refusals are mapped by the handler through [`denied_code_envelope`].
pub(crate) fn invitation_envelope(
    address: String,
    invited: Vec<String>,
    mailed: bool,
) -> JsonEnvelope {
    let data = InvitationData {
        address,
        invited,
        mailed,
    };
    ok_envelope("invite", to_data(&data))
}

/// Map the caller's own membership ([`Me`]) to the wire [`WireMe`] (1:1 — plain owned fields).
pub(crate) fn me_to_wire(me: Me) -> WireMe {
    WireMe {
        workspace_id: me.workspace_id,
        name: me.name,
        display_name: me.display_name,
        address: me.address,
        principal: me.principal,
        role: me.role,
        invited_by: me.invited_by,
        invite_policy: me.invite_policy,
    }
}

/// Map the workspace channels index to the wire [`WireChannelIndex`] — each channel's mode / builtin flag /
/// caller membership / member count and its (name-sorted) skill references.
pub(crate) fn channels_to_wire(entries: Vec<ChannelIndexEntry>) -> WireChannelIndex {
    WireChannelIndex {
        channels: entries
            .into_iter()
            .map(|c| WireChannelEntry {
                name: c.name,
                mode: c.mode,
                builtin: c.builtin,
                member: c.member,
                member_count: c.member_count,
                skills: c
                    .skills
                    .into_iter()
                    .map(|s| WireChannelSkill {
                        skill_id: s.skill_id,
                        name: s.name,
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Map the review inbox (`Vec<ProposalIndexEntry>`) to the wire [`WireProposalIndex`] — hex-encode each
/// 32-byte id, carry the author message + `stale` flag through.
pub(crate) fn proposals_index_to_wire(entries: Vec<ProposalIndexEntry>) -> WireProposalIndex {
    WireProposalIndex {
        proposals: entries
            .into_iter()
            .map(|p| WireProposalEntry {
                skill_id: p.skill_id,
                skill_name: p.skill_name,
                version_id: hex::encode(p.version_id),
                base_version_id: hex::encode(p.base_version_id),
                proposer: p.proposer,
                message: p.message,
                created_at: p.created_at,
                stale: p.stale,
            })
            .collect(),
    }
}

/// Map a skill's [`SkillLog`] to the wire [`WireSkillLog`] — hex-encode each version id, carry the purge
/// tombstones + proposal events through.
pub(crate) fn skill_log_to_wire(log: SkillLog) -> WireSkillLog {
    WireSkillLog {
        skill_id: log.skill_id,
        name: log.name,
        status: log.status,
        base_name: log.base_name,
        versions: log
            .versions
            .into_iter()
            .map(|v| WireLogVersion {
                version_id: hex::encode(v.version_id),
                author: v.author,
                message: v.message,
                current: v.current,
                purged_at: v.purged_at,
                purged_by: v.purged_by,
            })
            .collect(),
        proposals: log
            .proposals
            .into_iter()
            .map(|p| WireLogProposal {
                version_id: hex::encode(p.version_id),
                proposer: p.proposer,
                status: p.status,
                resolved_by: p.resolved_by,
                resolved_reason: p.resolved_reason,
                resolved_at: p.resolved_at,
                created_at: p.created_at,
            })
            .collect(),
    }
}

/// Map a skill's [`Reach`] to the wire [`WireReach`] (1:1).
pub(crate) fn reach_to_wire(r: Reach) -> WireReach {
    WireReach {
        persons: r.persons,
        devices: r.devices,
    }
}

/// The constant protocol card's MACHINE face — the discriminant a client dispatches on plus the API base it
/// re-roots onto (no content, no existence signal).
pub(crate) fn protocol_card(api_base_url: String) -> WireProtocolCard {
    WireProtocolCard {
        schema_version: 1,
        card: "topos-protocol-card".to_owned(),
        api_base_url,
    }
}

// ── the member-lane row-op outcome envelopes ───────────────────────────────────────────────────────────
//
// Every naturally-idempotent row op (follow/unfollow/exclude, channel join/leave, curation place/unplace,
// protect) has no receipt; its wire answer is a 200 all-outcome envelope: an OK outcome carries a `status`
// string in `data`; a role/gate refusal is a 200 + DENIED with a specific code (the actor is an
// authenticated member — nothing to hide, so never a 403). ONE consistent code family (`*_ROLE_REQUIRED` /
// `CHANNEL_BUILTIN` / `SKILL_NOT_ACTIVE` / `BAD_NAME` / `UNKNOWN_CHANNEL`).

/// A curation write's outcome ([`CurationOutcome`]) → its all-outcome envelope.
pub(crate) fn curation_envelope(command: &str, outcome: CurationOutcome) -> JsonEnvelope {
    match outcome {
        CurationOutcome::Placed => ok_status_envelope(command, "placed"),
        CurationOutcome::Created => ok_status_envelope(command, "created"),
        CurationOutcome::Removed => ok_status_envelope(command, "removed"),
        CurationOutcome::NotPlaced => ok_status_envelope(command, "not_placed"),
        CurationOutcome::CuratedRoleRequired => {
            denied_code_envelope(command, "CURATED_ROLE_REQUIRED")
        }
        CurationOutcome::BadName => denied_code_envelope(command, "BAD_NAME"),
        CurationOutcome::SkillNotActive => denied_code_envelope(command, "SKILL_NOT_ACTIVE"),
    }
}

/// A channel-membership change's outcome ([`ChannelMembershipOutcome`]) → its all-outcome envelope.
pub(crate) fn membership_envelope(
    command: &str,
    outcome: ChannelMembershipOutcome,
) -> JsonEnvelope {
    match outcome {
        ChannelMembershipOutcome::Joined => ok_status_envelope(command, "joined"),
        ChannelMembershipOutcome::Left => ok_status_envelope(command, "left"),
        ChannelMembershipOutcome::NotMember => ok_status_envelope(command, "not_member"),
        ChannelMembershipOutcome::Builtin => denied_code_envelope(command, "CHANNEL_BUILTIN"),
    }
}

/// A person-scoped subscription write's outcome ([`SubscriptionOutcome`]) → its all-outcome envelope.
pub(crate) fn subscription_envelope(command: &str, outcome: SubscriptionOutcome) -> JsonEnvelope {
    match outcome {
        SubscriptionOutcome::Followed => ok_status_envelope(command, "followed"),
        SubscriptionOutcome::Unfollowed => ok_status_envelope(command, "unfollowed"),
        SubscriptionOutcome::Excluded => ok_status_envelope(command, "excluded"),
        SubscriptionOutcome::SkillNotActive => denied_code_envelope(command, "SKILL_NOT_ACTIVE"),
    }
}

/// A `protect` write's outcome ([`ProtectOutcome`]) → its all-outcome envelope.
pub(crate) fn protect_envelope(command: &str, outcome: ProtectOutcome) -> JsonEnvelope {
    match outcome {
        ProtectOutcome::Set => ok_status_envelope(command, "set"),
        ProtectOutcome::ReviewerRoleRequired => {
            denied_code_envelope(command, "REVIEWER_ROLE_REQUIRED")
        }
        ProtectOutcome::OwnerRoleRequired => denied_code_envelope(command, "OWNER_ROLE_REQUIRED"),
    }
}

/// The all-outcome envelope for a roster/revoke [`GovernanceOutcome`]: `Ok` → a 200 carrying `data` (`{}` for
/// these data-less mutations); `Denied` → the uniform flat DENIED error. A role-denial is a 200+DENIED (the
/// actor is an authenticated member — nothing to hide), NOT a 403.
pub(crate) fn governance_envelope(
    command: &str,
    outcome: &GovernanceOutcome,
    data: serde_json::Value,
) -> JsonEnvelope {
    match outcome {
        GovernanceOutcome::Ok => ok_envelope(command, data),
        GovernanceOutcome::Denied(_) => denied_envelope(command),
    }
}

/// A success envelope (`ok = true`) carrying `data`, no error, no receipt (enrollment/governance ops have no
/// `SetCurrentReceipt`; their idempotency record is the authority's `workspace_events`/deterministic credential).
fn ok_envelope(command: &str, data: serde_json::Value) -> JsonEnvelope {
    JsonEnvelope {
        schema_version: WIRE_SCHEMA_VERSION,
        command: command.to_owned(),
        ok: true,
        data,
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    }
}

/// A success envelope carrying only a `status` string in `data` — the naturally-idempotent row ops'
/// answer (`placed` / `joined` / `followed` / `set` / …; the client narrates it, no receipt).
pub(crate) fn ok_status_envelope(command: &str, status: &str) -> JsonEnvelope {
    ok_envelope(command, serde_json::json!({ "status": status }))
}

/// The uniform DENIED envelope — a flat [`WireError`] with the `DENIED` code + the access-recovery next
/// actions, carrying NO static reason (the per-op reason is for server logs, never an enumeration oracle).
fn denied_envelope(command: &str) -> JsonEnvelope {
    denied_code_envelope(command, "DENIED")
}

/// A DENIED envelope carrying a SPECIFIC `code` (the row ops' `*_ROLE_REQUIRED` / `CHANNEL_BUILTIN` / …
/// family) — the actor is an authenticated member, so a refusal names WHY it was refused (never a 403).
/// The flat [`WireError`] rides the access-recovery next actions like every other DENIED.
pub(crate) fn denied_code_envelope(command: &str, code: &str) -> JsonEnvelope {
    let outcome = TerminalOutcome::Denied;
    let actions = next_actions_for(outcome);
    let error = WireError {
        code: code.to_owned(),
        outcome,
        retryable: retryable(outcome),
        affected: Affected::default(),
        expected_generation: None,
        current_generation: None,
        context: serde_json::json!({}),
        next_actions: actions.clone(),
    };
    JsonEnvelope {
        schema_version: WIRE_SCHEMA_VERSION,
        command: command.to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: actions,
        receipt: None,
        error: Some(error),
    }
}

/// Serialize a typed payload into the envelope's `data` slot (an unrepresentable value degrades to `{}`).
fn to_data<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({}))
}

/// The plane-store deployment posture → its wire mirror.
fn deployment_mode_to_wire(mode: StoreDeploymentMode) -> DeploymentMode {
    match mode {
        StoreDeploymentMode::Cloud => DeploymentMode::Cloud,
        StoreDeploymentMode::SelfHost => DeploymentMode::SelfHost,
    }
}

/// A stored domain-verification discriminant → the wire enum (an unknown value degrades to `unverified`).
fn verified_domain_status_to_wire(status: &str) -> VerifiedDomainStatus {
    match status {
        "pending" => VerifiedDomainStatus::Pending,
        "verified" => VerifiedDomainStatus::Verified,
        _ => VerifiedDomainStatus::Unverified,
    }
}
