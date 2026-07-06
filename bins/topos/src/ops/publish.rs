//! `publish [skill] [--propose] --approve <skill>@<digest>` — ship a draft to the team.
//!
//! `publish` moves `current` to the freshly-scanned draft (a direct publish, or a genesis create for a
//! never-published skill); `--propose` opens a PR without moving `current`. The client computes the
//! byte-identical `commit_id`/`bundle_digest` the plane re-derives (I-COMMIT-PARITY), gates the outward
//! ship behind `--approve <skill>@<digest>` matching the scanned bytes (refusing on mismatch — never a
//! silent mode-flip), persists an op-WAL before the first send (so an uncertain retry replays the same
//! `op_id`), and maps the plane's typed outcome.
//!
//! **The workspace-standup branch.** An UN-ENROLLED direct publish does not fail — it stands the
//! workspace up: after the FULL normal pre-flight (skill resolution, scan, digest computation, the
//! `--approve` consent gate — so consent binds BEFORE any network), the client starts a standup device
//! authorization against the hosted plane (`TOPOS_PLANE_URL` override, else the compiled-in default),
//! TOFU-pins the plane key from the response, writes an `AuthorizingStandup` WAL, and returns a PENDING
//! receipt whose `ENROLL_RESUME` next-action is the SAME publish command. Re-invoking it polls once;
//! once a signed-in human approves (creating the workspace and seating them as owner), the same
//! invocation redeems, promotes the enrollment, and CONTINUES into the ordinary publish — disclosing
//! "workspace X — owner Y" so a hijacked approval is visible. `--propose` never stands up (a proposal
//! against a workspace that does not exist yet is meaningless) and keeps the typed not-enrolled error.

use topos_core::digest::{self, to_hex};
use topos_core::sign::{self, Commit, EnrollFields};
use topos_gitstore::{ImportFile, Store};
use topos_types::bootstrap::VerifiedDomainStatus;
use topos_types::persisted::{ConflictState, Lock, OpKind, OpRecord, PlacementMap, SyncState};
use topos_types::results::{
    ProposeData, PublishData, PublishPending, PublishPendingStatus, StandupReceipt,
};
use topos_types::{Generation, PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect, PUBLISH_MESSAGE};
use super::follow::{
    EnrollConnect, complete_uri, device_fingerprint, machine_name, promote_core, resolve_api_base,
    tofu_decide_key,
};
use super::invite::{GovernanceConnect, invite as mint_invite};
use super::sync_engine;
use super::{parse_hex32, resolve_skill};
use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::{TokenPoll, WriteReceipt};
use crate::{doc, op_wal, scan, sidecar};

/// The result of `publish`: either `current` moved (a direct publish), a proposal opened (`--propose`), or
/// the publish is PENDING the workspace-standup sign-in (the un-enrolled first publish).
#[derive(Debug)]
pub(crate) enum PublishOutcome {
    /// A direct publish moved `current` to the draft.
    Published(PublishData),
    /// `--propose` opened a proposal (NEEDS_REVIEW); `current` did NOT move.
    Proposed(ProposeData),
    /// The un-enrolled standup branch is waiting on a human sign-in: nothing was published; the envelope
    /// stays `ok = true` with `data.pending` set, and the `ENROLL_RESUME` next-action carries the ORIGINAL
    /// publish argv (re-invoking it IS the resume — consent re-derives from `--approve` each invocation).
    Pending {
        data: PublishData,
        /// The argv the agent re-invokes to resume (the canonical spelling of this same command).
        resume_argv: Vec<String>,
    },
}

/// The standup branch's network seam + base URL — the enroll transport factory (the same creds-free
/// connector `follow` uses) and the RESOLVED plane base (the `TOPOS_PLANE_URL` override, else the
/// compiled-in hosted default; tests always pass an explicit base).
pub(crate) struct StandupConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub base_url: String,
}

/// The genesis base — a skill whose `current` does not exist yet is published as a zero-parent commit at
/// `(0,0)` (the plane's genesis branch creates `current` at `(1,1)`).
const GENESIS: Generation = Generation { epoch: 0, seq: 0 };

/// Ship `skill`'s draft (or, with `propose`, open a proposal). Un-enrolled + direct dispatches to the
/// workspace-standup branch (see the module doc); un-enrolled + `--propose` keeps the typed error.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled (`--propose`) or a standup step fails typed;
/// [`ClientError::ApprovalMismatch`] if `--approve` does not match the scanned bytes;
/// [`ClientError::PublishBlocked`] if an unresolved merge conflict is present; [`ClientError::Conflict`] /
/// [`ClientError::ApprovalRequired`] / [`ClientError::Denied`] on the plane's typed verdict; a signing /
/// transport / store failure otherwise.
pub(crate) fn publish(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    gov_connect: &GovernanceConnect<'_>,
    standup: &StandupConnectors<'_>,
    skill_arg: Option<&str>,
    propose: bool,
    approve: &str,
) -> Result<PublishOutcome, ClientError> {
    // The branch gate is enrollment itself: `instance.json` present ⇒ the ordinary enrolled publish (an
    // enrolled device NEVER hits the standup branch); absent + direct ⇒ standup; absent + propose ⇒ the
    // typed error (a proposal against a workspace that does not exist yet is meaningless).
    if enroll::read_instance(ctx.fs, &ctx.layout)?.is_none() {
        if propose {
            return Err(ClientError::Enrollment(
                "not enrolled; run `topos follow <link>` first".into(),
            ));
        }
        return standup_publish(ctx, connect, gov_connect, standup, skill_arg, approve);
    }
    // `instance.json` PRESENT does not yet mean the promotion COMPLETED: promote_core writes it FIRST and
    // `user.json` later, so a crash inside a standup promotion leaves instance present, user absent, and
    // the standup `Redeemed` WAL still holding the recovery fence. The enrolled path below would then fail
    // "could not determine your workspace" without ever consulting the WAL — a wedge, because the standup
    // receipt's own next-action is to re-run THIS command. Route a standup-rooted Redeemed WAL back
    // through the standup dispatch, whose Redeemed arm completes the promotion idempotently and continues
    // into the publish. (An invite/claim-rooted Redeemed WAL keeps its own recovery door, `follow
    // --resume` — exactly what the enrolled path's error message points at. And once the WAL is gone — a
    // crash AFTER the delete — a retry publishes without the "workspace X — owner Y" standup disclosure:
    // an accepted cosmetic residual; the workspace and the genesis are correct, and no durable-receipt
    // machinery is worth re-creating that one line.)
    if !propose
        && let Some(wal) = enroll::read_wal(ctx.fs, &ctx.layout)?
        && let enroll::EnrollPhase::Redeemed { context, .. } = &wal.state
        && matches!(context.root, enroll::EnrollRoot::Standup)
    {
        return standup_publish(ctx, connect, gov_connect, standup, skill_arg, approve);
    }
    enrolled_publish(ctx, connect, gov_connect, skill_arg, propose, approve, None)
}

/// The ordinary ENROLLED publish (the pre-standup body, unchanged in behavior). `standup_receipt` is the
/// disclosure a workspace-creating invocation attaches to its Published outcome (`None` normally).
fn enrolled_publish(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    gov_connect: &GovernanceConnect<'_>,
    skill_arg: Option<&str>,
    propose: bool,
    approve: &str,
    standup_receipt: Option<StandupReceipt>,
) -> Result<PublishOutcome, ClientError> {
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    let workspace_id = enroll::read_user(ctx.fs, &ctx.layout)?
        .map(|u| u.workspace_id)
        .ok_or_else(|| {
            ClientError::Enrollment(
                "could not determine your workspace; complete enrollment with `topos follow` first"
                    .into(),
            )
        })?;

    let (skill_name, approved_digest) = agree_skill_and_digest(skill_arg, approve)?;
    let skill_name = skill_name.as_str();

    let (id, lock) = resolve_skill(ctx, skill_name)?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Publish guard (presence-based, never a marker scan): an unresolved author merge blocks publish.
    if doc::read_doc::<ConflictState>(ctx.fs, &sp.conflict)?.is_some() {
        return Err(ClientError::PublishBlocked {
            skill: skill_name.to_owned(),
        });
    }

    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let transport = connect(&instance.base_url);
    let map: PlacementMap = doc::read_doc(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;

    // Resume a crashed prior publish/propose for this skill (replay the SAME op_id) before minting a new
    // one — the plane returns the byte-identical receipt, so there is no double-advance / duplicate commit.
    let kinds = [OpKind::PublishDirect, OpKind::PublishPropose];
    let rec = match op_wal::find_pending_for_skill(
        ctx.fs,
        &ctx.layout,
        &workspace_id,
        id.as_str(),
        &kinds,
    )? {
        // A crashed prior publish is still in-flight: replay it ONLY if it matches THIS command (same
        // approved digest + same direct/propose mode) — otherwise refuse, so a new intent never silently
        // rides the old op's mode/bytes (the consent gate covers the replay path too).
        Some(pending) => {
            let pending_propose = matches!(pending.op, OpKind::PublishPropose);
            if pending.bundle_digest != approved_digest || pending_propose != propose {
                return Err(ClientError::PendingOp {
                    skill: skill_name.to_owned(),
                    detail: format!(
                        "a {} of {skill_name}@{} is in flight — settle it (re-run that publish), then retry",
                        if pending_propose {
                            "proposal"
                        } else {
                            "direct publish"
                        },
                        pending.bundle_digest
                    ),
                });
            }
            pending
        }
        None => build_publish_op(
            ctx,
            &sp,
            id.as_str(),
            &lock,
            &map,
            &workspace_id,
            propose,
            &approved_digest,
        )?,
    };

    let receipt = contribute::run_write(ctx, &*transport, &signer, &sp, &rec)?;
    let mut outcome = map_outcome(
        ctx,
        &sp,
        &lock,
        &map,
        &rec,
        &receipt,
        skill_name,
        &approved_digest,
    )?;

    // First-publish invite fold: a GENESIS publish (no prior `current`) also mints a shareable `/i/` link
    // pre-offering the just-published skill, so a first publish stands up a door to it. BEST-EFFORT +
    // owner-gated — minting the link signs a governance op the plane DENIES for a non-owner; on any failure
    // the publish STILL succeeds with `invite_link: None` (the pointer move is the real outcome, the link a
    // convenience). Fires only for a genesis publish (`expected == (0,0)`), never an ordinary one.
    if let PublishOutcome::Published(data) = &mut outcome
        && rec.expected_generation == GENESIS
    {
        data.invite_link = mint_invite(
            ctx,
            gov_connect,
            Vec::new(),
            None,
            vec![rec.skill_id.clone()],
        )
        .ok()
        .map(|inv| inv.invite_link);
    }
    // A workspace-creating invocation discloses what it stood up and who owns it (hijack visibility).
    if let PublishOutcome::Published(data) = &mut outcome {
        data.standup = standup_receipt;
    }
    Ok(outcome)
}

// =================================================================================================
// The workspace-standup branch — the un-enrolled direct publish. Two calls share one WAL: call 1 runs
// the full consent pre-flight, starts the standup device authorization, TOFU-pins the plane key, and
// returns PENDING; call 2 (the SAME command re-invoked) re-runs the pre-flight (consent re-binds — bytes
// that drifted since call 1 are refused, never silently shipped), polls ONCE, and on a granted poll
// redeems, promotes the enrollment, and continues into the ordinary publish above.
// =================================================================================================

fn standup_publish(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    gov_connect: &GovernanceConnect<'_>,
    standup: &StandupConnectors<'_>,
    skill_arg: Option<&str>,
    approve: &str,
) -> Result<PublishOutcome, ClientError> {
    // FULL pre-flight FIRST — skill resolution, scan, digest computation, the --approve match — so the
    // consent decision binds BEFORE any network call ever happens.
    let (skill_name, approved_digest) = agree_skill_and_digest(skill_arg, approve)?;
    standup_preflight(ctx, &skill_name, &approved_digest)?;

    // The WAL decides which call this is. Any OTHER in-flight enrollment owns the shared WAL slot — this
    // publish neither hijacks nor clobbers it (typed guidance instead).
    match enroll::read_wal(ctx.fs, &ctx.layout)?.map(|w| w.state) {
        None => standup_begin(ctx, standup, &skill_name, &approved_digest),
        Some(enroll::EnrollPhase::AuthorizingStandup {
            base_url,
            pinned_plane_key,
            plane_key_id,
            deployment_mode,
            enrollment_method,
            device_code,
            user_code,
            verification_uri_complete,
            expires_at_millis,
        }) => standup_resume(
            ctx,
            connect,
            gov_connect,
            standup,
            &skill_name,
            &approved_digest,
            StandupWal {
                base_url,
                pinned_plane_key,
                plane_key_id,
                deployment_mode,
                enrollment_method,
                device_code,
                user_code,
                verification_uri_complete,
                expires_at_millis,
            },
        ),
        // A crash between redeem and promotion: complete the promotion from the persisted facts (the
        // existing follow fence — never re-redeem a spent grant), then continue into the publish.
        Some(enroll::EnrollPhase::Redeemed {
            context,
            read_creds,
            device_key_id,
            principal,
            enrolled_at_millis,
        }) => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            promote_core(
                ctx,
                &context,
                &read_creds,
                &device_key_id,
                principal.as_deref(),
                enrolled_at_millis,
                &signer,
            )?;
            // Only a STANDUP-rooted enrollment claims the standup receipt — a crashed invite/claim
            // promotion completed here is disclosed as the ordinary publish it then is.
            let receipt =
                matches!(context.root, enroll::EnrollRoot::Standup).then(|| StandupReceipt {
                    workspace_display_name: context.workspace_display_name.clone(),
                    owner_principal: principal.clone(),
                });
            continue_enrolled(
                ctx,
                connect,
                gov_connect,
                &context.pinned_plane_key,
                &skill_name,
                approve,
                receipt,
            )
        }
        Some(enroll::EnrollPhase::Authorizing { .. }) => Err(ClientError::Enrollment(
            "an invite enrollment is in progress; run `topos follow --resume` first".into(),
        )),
        Some(enroll::EnrollPhase::ClaimPending { .. }) => Err(ClientError::Enrollment(
            "a claim enrollment is in progress; run `topos follow --resume` first".into(),
        )),
    }
}

/// The standup pre-flight: resolve the skill, refuse an unresolved merge conflict, scan the live draft,
/// and run the `--approve` consent gate — all BEFORE any network. The per-skill lock is held only for the
/// scan (the continuation re-acquires it and re-runs the authoritative consent gate, so bytes that drift
/// after this check are still refused, never silently shipped).
fn standup_preflight(
    ctx: &Ctx<'_>,
    skill_name: &str,
    approved_digest: &str,
) -> Result<(), ClientError> {
    let (id, lock) = resolve_skill(ctx, skill_name)?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;
    if doc::read_doc::<ConflictState>(ctx.fs, &sp.conflict)?.is_some() {
        return Err(ClientError::PublishBlocked {
            skill: skill_name.to_owned(),
        });
    }
    let map: PlacementMap = doc::read_doc(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
    let placement = sync_engine::first_placement(&map)?;
    let scanned = scan::scan(std::path::Path::new(&placement))?;
    let digest_hex = to_hex(&scanned.bundle_digest);
    if digest_hex != approved_digest {
        return Err(ClientError::ApprovalMismatch {
            skill: lock.name,
            expected: digest_hex,
            got: approved_digest.to_owned(),
        });
    }
    Ok(())
}

/// Call 1: start the standup device authorization, TOFU-pin the plane key from the response's plane
/// block, write the `AuthorizingStandup` WAL, and return the PENDING receipt.
fn standup_begin(
    ctx: &Ctx<'_>,
    standup: &StandupConnectors<'_>,
    skill_name: &str,
    approved_digest: &str,
) -> Result<PublishOutcome, ClientError> {
    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let enroll_src = (standup.enroll)(&standup.base_url);
    let start = enroll_src.device_authorize_standup(signer.public_key(), &machine_name(&signer))?;

    // RE-ROOT + TOFU, exactly as a link follow does: the response's plane block declares the API base
    // this device pins and every later call dials (normally the dialed base itself — the compiled-in
    // hosted default IS the API base). Pinning the declared base keeps the standup pin and a later
    // `/i/`-link pin string-identical, so neither door ever refuses the other as cross-plane.
    let base_url = resolve_api_base(&standup.base_url, &start.plane.base_url)?;
    let pinned_plane_key = tofu_decide_key(ctx, &base_url, &start.plane.signing_key)?;

    let expires_at_millis = i64::try_from(ctx.clock.now_unix_millis())
        .unwrap_or(i64::MAX)
        .saturating_add(
            i64::try_from(start.auth.expires_in)
                .unwrap_or(0)
                .saturating_mul(1000),
        );
    // The SERVER-built complete URI, verbatim when present (the reconstruction is only the fallback).
    let complete = start
        .auth
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| complete_uri(&start.auth.verification_uri, &start.auth.user_code));
    enroll::write_wal(
        ctx.fs,
        &ctx.layout,
        &enroll::PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: enroll::EnrollPhase::AuthorizingStandup {
                base_url,
                pinned_plane_key,
                plane_key_id: start.plane.signing_key.key_id.clone(),
                deployment_mode: start.plane.deployment_mode,
                enrollment_method: start.plane.enrollment_method.clone(),
                device_code: start.auth.device_code.clone(),
                user_code: start.auth.user_code.clone(),
                verification_uri_complete: complete.clone(),
                expires_at_millis,
            },
        },
    )?;

    Ok(pending_outcome(
        ctx,
        skill_name,
        approved_digest,
        complete,
        start.auth.user_code,
        device_fingerprint(&signer),
        expires_at_millis,
    ))
}

/// The persisted `AuthorizingStandup` facts (destructured out of the WAL for the resume path).
struct StandupWal {
    base_url: String,
    pinned_plane_key: String,
    plane_key_id: String,
    deployment_mode: topos_types::bootstrap::DeploymentMode,
    enrollment_method: String,
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_at_millis: i64,
}

/// Call 2 (the same command re-invoked): poll ONCE; pending re-emits the pending receipt (the WAL stays),
/// a terminal denial/expiry clears the WAL typed, and a granted poll redeems + promotes + CONTINUES into
/// the ordinary publish in this same invocation.
#[allow(clippy::too_many_arguments)]
fn standup_resume(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    gov_connect: &GovernanceConnect<'_>,
    standup: &StandupConnectors<'_>,
    skill_name: &str,
    approved_digest: &str,
    wal: StandupWal,
) -> Result<PublishOutcome, ClientError> {
    // The in-flight session's base URL is authoritative for this session (a changed TOPOS_PLANE_URL
    // affects the NEXT standup, never a half-done one).
    let enroll_src = (standup.enroll)(&wal.base_url);
    match enroll_src.poll_token(&wal.device_code)? {
        // Still waiting on the human — re-emit the pending receipt verbatim; the WAL stays put. The
        // device key is deterministic, so the re-emitted fingerprint matches the one on the sign-in page.
        TokenPoll::Pending | TokenPoll::SlowDown => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            Ok(pending_outcome(
                ctx,
                skill_name,
                approved_digest,
                wal.verification_uri_complete,
                wal.user_code,
                device_fingerprint(&signer),
                wal.expires_at_millis,
            ))
        }
        TokenPoll::Denied => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the workspace standup was denied at the sign-in page".into(),
            ))
        }
        TokenPoll::Expired => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the standup sign-in expired; re-run this publish to start over".into(),
            ))
        }
        TokenPoll::Granted(granted) => {
            // The workspace context is what a standup client (no /i/ bootstrap) signs its possession
            // frame over — a granted standup poll without it is unusable.
            let workspace = granted.workspace.ok_or_else(|| {
                ClientError::WireInvalid("a granted standup poll carried no workspace".into())
            })?;
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            // The possession proof over the SERVER-trusted framed fields: the standup session offered NO
            // skills (there was no invite), so the bound offered-set is empty.
            let grant_hash = digest::sha256(granted.grant.as_str().as_bytes());
            let fields = EnrollFields {
                workspace_id: &workspace.workspace_id,
                grant_hash,
                device_auth_id: &wal.user_code,
                device_key_id: signer.device_key_id(),
                device_public_key: signer.public_key(),
                offered_skill_ids: &[],
            };
            let sig = signer.sign_enroll(&fields)?;
            let redeem = enroll_src.redeem(
                &workspace.workspace_id,
                granted.grant.as_str(),
                signer.public_key(),
                sig,
            )?;
            if redeem.workspace_id != workspace.workspace_id {
                return Err(ClientError::Enrollment(
                    "the redeemed workspace does not match the granted session".into(),
                ));
            }
            let context = enroll::EnrollContext {
                base_url: wal.base_url,
                pinned_plane_key: wal.pinned_plane_key,
                plane_key_id: wal.plane_key_id,
                deployment_mode: wal.deployment_mode,
                enrollment_method: wal.enrollment_method,
                workspace_id: workspace.workspace_id,
                workspace_display_name: workspace.display_name,
                verified_domain: None,
                verified_domain_status: VerifiedDomainStatus::Unverified,
                offered_skills: Vec::new(),
                mode: enroll::FollowModeDoc::Auto,
                root: enroll::EnrollRoot::Standup,
            };
            let read_creds: Vec<enroll::RedeemedCredDoc> = redeem
                .read_creds
                .iter()
                .map(|c| enroll::RedeemedCredDoc {
                    skill_id: c.skill_id.clone(),
                    read_token: c.read_token.clone(),
                    expires_at: c.expires_at,
                })
                .collect();
            let enrolled_at = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
            // The lockout fence: record the redeemed facts BEFORE promotion, so a crash mid-promote
            // completes from the WAL without re-redeeming the spent grant.
            enroll::write_wal(
                ctx.fs,
                &ctx.layout,
                &enroll::PendingEnrollment {
                    schema_version: PERSISTED_SCHEMA_VERSION,
                    state: enroll::EnrollPhase::Redeemed {
                        context: context.clone(),
                        read_creds: read_creds.clone(),
                        device_key_id: redeem.device_key_id.clone(),
                        principal: redeem.principal.clone(),
                        enrolled_at_millis: enrolled_at,
                    },
                },
            )?;
            promote_core(
                ctx,
                &context,
                &read_creds,
                &redeem.device_key_id,
                redeem.principal.as_deref(),
                enrolled_at,
                &signer,
            )?;
            // CONTINUE into the ordinary enrolled publish in this SAME invocation (the consent gate runs
            // again inside — bytes that drifted since the pre-flight are refused, never silently shipped),
            // carrying the standup disclosure onto the receipt.
            let approve_token = format!("{skill_name}@{approved_digest}");
            continue_enrolled(
                ctx,
                connect,
                gov_connect,
                &context.pinned_plane_key,
                skill_name,
                &approve_token,
                Some(StandupReceipt {
                    workspace_display_name: context.workspace_display_name.clone(),
                    owner_principal: redeem.principal,
                }),
            )
        }
    }
}

/// Continue a just-promoted standup invocation into the ordinary enrolled publish. The AMBIENT `ctx` was
/// composed while un-enrolled (its `plane_key` is the inert all-zero placeholder), so the continuation
/// rebuilds the context with the key THIS flow TOFU-pinned — the OK receipt's signed pointer is verified
/// against the real pin, never the placeholder.
fn continue_enrolled(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    gov_connect: &GovernanceConnect<'_>,
    pinned_plane_key: &str,
    skill_name: &str,
    approve: &str,
    standup_receipt: Option<StandupReceipt>,
) -> Result<PublishOutcome, ClientError> {
    let plane_key = parse_hex32(pinned_plane_key)?;
    let continuation = Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: ctx.layout.clone(),
        harness: ctx.harness,
        plane: ctx.plane,
        plane_key,
        follow: ctx.follow,
    };
    enrolled_publish(
        &continuation,
        connect,
        gov_connect,
        Some(skill_name),
        false,
        approve,
        standup_receipt,
    )
}

/// Build the PENDING outcome: `ok = true`, no version (nothing shipped), the sign-in block, and the
/// resume argv (this same command, canonically spelled).
fn pending_outcome(
    ctx: &Ctx<'_>,
    skill_name: &str,
    approved_digest: &str,
    verification_uri_complete: String,
    user_code: String,
    device_fingerprint: String,
    expires_at_millis: i64,
) -> PublishOutcome {
    // The sidecar skill id is the receipt's stable handle (matching the enrolled receipt's skill_id).
    let skill_id = resolve_skill(ctx, skill_name)
        .map(|(id, _)| id.into_string())
        .unwrap_or_else(|_| skill_name.to_owned());
    PublishOutcome::Pending {
        data: PublishData {
            skill_id,
            version_id: None,
            bundle_digest: approved_digest.to_owned(),
            current_generation: None,
            invite_link: None,
            pending: Some(PublishPending {
                status: PublishPendingStatus::SigninRequired,
                verification_uri_complete,
                user_code,
                device_fingerprint,
                expires_at: Some(fmt_rfc3339_millis(expires_at_millis)),
            }),
            standup: None,
        },
        resume_argv: vec![
            "topos".to_owned(),
            "publish".to_owned(),
            skill_name.to_owned(),
            "--approve".to_owned(),
            format!("{skill_name}@{approved_digest}"),
            "--json".to_owned(),
        ],
    }
}

/// Epoch-millis → an RFC-3339 `YYYY-MM-DDTHH:MM:SSZ` string (UTC, second precision) — enough for the
/// pending receipt's expiry disclosure. Negative inputs clamp to the epoch.
fn fmt_rfc3339_millis(millis: i64) -> String {
    let secs = millis.max(0) as u64 / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = crate::render::civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `--approve <skill>@<digest>` names the skill + the consent digest; a positional skill must agree.
fn agree_skill_and_digest(
    skill_arg: Option<&str>,
    approve: &str,
) -> Result<(String, String), ClientError> {
    let (approve_skill, approved_digest) = parse_skill_at(approve)?;
    let skill_name = match skill_arg {
        Some(s) if s != approve_skill => {
            return Err(ClientError::ApprovalMismatch {
                skill: s.to_owned(),
                expected: format!("skill '{approve_skill}' (from --approve)"),
                got: format!("skill '{s}' (positional)"),
            });
        }
        Some(s) => s.to_owned(),
        None => approve_skill,
    };
    Ok((skill_name, approved_digest))
}

/// Build the fresh op: precondition the state, run the consent gate over the scanned draft, compute the
/// byte-identical `(commit_id, bundle_digest)`, commit the candidate into the local store (renderable for a
/// replay + local history), and assemble the [`OpRecord`] (the WAL write itself happens in `run_write`).
#[allow(clippy::too_many_arguments)]
fn build_publish_op(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    id: &str,
    lock: &Lock,
    map: &PlacementMap,
    workspace_id: &str,
    propose: bool,
    approved_digest: &str,
) -> Result<OpRecord, ClientError> {
    let sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("missing sync state".to_owned()))?;

    // Be current before publishing: a behind state (a newer `current` not yet applied) would publish on a
    // stale base and could clobber the unapplied version — surface it as a locally-detected CONFLICT
    // (pull to rebase), never a confusing server DENIED.
    if sync.applied != sync.observed {
        return Err(ClientError::Conflict {
            skill: lock.name.clone(),
            current: Some(sync.observed),
        });
    }

    // The draft = the live placement, scanned (the SAME source `diff` uses).
    let placement = sync_engine::first_placement(map)?;
    let scanned = scan::scan(std::path::Path::new(&placement))?;
    let digest = scanned.bundle_digest;
    let digest_hex = to_hex(&digest);

    // Consent gate: `--approve <skill>@<digest>` must match the digest of the bytes being shipped. Refuse
    // BEFORE signing or sending — the disclosure/integrity gate (never a silent mode-flip).
    if digest_hex != approved_digest {
        return Err(ClientError::ApprovalMismatch {
            skill: lock.name.clone(),
            expected: digest_hex,
            got: approved_digest.to_owned(),
        });
    }

    // Genesis (no `current` yet) is a zero-parent commit at (0,0); a normal publish parents on `current`.
    let (parents, expected): (Vec<[u8; 32]>, Generation) = if sync.observed == GENESIS {
        (Vec::new(), GENESIS)
    } else {
        (vec![parse_hex32(&lock.base_commit)?], sync.observed)
    };

    // The byte-identical id the plane re-derives (I-COMMIT-PARITY): author = the device id (NOT the signing
    // key id), message = the fixed publish message — both folded into `commit_id`.
    let commit_id = sign::commit_id(&Commit {
        parents: &parents,
        tree: digest,
        author: &ctx.device_id,
        message: PUBLISH_MESSAGE,
    })
    .map_err(|_| ClientError::Corrupt("commit id preimage".to_owned()))?;

    // Pin the candidate in the local store (so a replay re-renders the byte-identical snapshot, and the
    // local history/diff can reach it) BEFORE the WAL/send.
    let store = Store::open(&sp.store)?;
    let import: Vec<ImportFile<'_>> = scanned
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    store.commit(commit_id, &parents, &tree, &ctx.device_id, PUBLISH_MESSAGE)?;
    // The candidate's own objects + ref — durable before the WAL names it; never the whole store.
    sync_engine::fsync_batch(ctx, &store.version_durability(&commit_id)?)?;

    let op_id_bytes = ctx.ids.new_op_id();
    let op_id = uuid::Uuid::from_bytes(op_id_bytes)
        .as_hyphenated()
        .to_string();
    Ok(OpRecord {
        schema_version: PERSISTED_SCHEMA_VERSION,
        op_id,
        workspace_id: workspace_id.to_owned(),
        skill_id: id.to_owned(),
        op: if propose {
            OpKind::PublishPropose
        } else {
            OpKind::PublishDirect
        },
        candidate_commit: to_hex(&commit_id),
        bundle_digest: digest_hex,
        expected_generation: expected,
        good: None,
        // The author's folder name — advisory, so the plane can name the followers' folders + dashboard
        // entry after it (a revert/review carries no name and preserves the stored one).
        display_name: Some(lock.name.clone()),
        last_receipt: None,
    })
}

/// Map the plane's typed write outcome to a [`PublishOutcome`] (or a typed [`ClientError`]).
#[allow(clippy::too_many_arguments)]
fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    map: &PlacementMap,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    skill_name: &str,
    approved_digest: &str,
) -> Result<PublishOutcome, ClientError> {
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            // A direct publish moved `current` — advance the local state (read-your-writes).
            let signed = receipt.signed_record.as_ref().ok_or_else(|| {
                ClientError::Corrupt("an OK publish carried no signed pointer".to_owned())
            })?;
            let new_gen = contribute::apply_publish_ok(ctx, sp, lock, map, rec, signed)?;
            Ok(PublishOutcome::Published(PublishData {
                skill_id: rec.skill_id.clone(),
                version_id: Some(rec.candidate_commit.clone()),
                bundle_digest: rec.bundle_digest.clone(),
                current_generation: Some(new_gen),
                invite_link: None,
                pending: None,
                standup: None,
            }))
        }
        TerminalOutcome::NeedsReview => Ok(PublishOutcome::Proposed(ProposeData {
            proposal: format!("{skill_name}@{}", rec.candidate_commit),
            base_version_id: lock.base_commit.clone(),
            title: skill_name.to_owned(),
            body: None,
        })),
        TerminalOutcome::ApprovalRequired => Err(ClientError::ApprovalRequired {
            skill: skill_name.to_owned(),
            digest: approved_digest.to_owned(),
        }),
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: skill_name.to_owned(),
            current: receipt.error.as_ref().and_then(|e| e.current_generation),
        }),
        TerminalOutcome::Denied => Err(ClientError::Denied(denied_code(receipt))),
        // Any other terminal class (RetryableFailure / Unavailable / PermanentFailure / …) is surfaced
        // verbatim, not flattened to a generic transport error.
        _ => Err(contribute::plane_terminal(receipt)),
    }
}

/// The wire error code on a DENIED (for the agent to branch on); never a secret.
fn denied_code(receipt: &WriteReceipt) -> String {
    receipt
        .error
        .as_ref()
        .map(|e| e.code.clone())
        .unwrap_or_else(|| "DENIED".to_owned())
}

/// Split a `<skill>@<digest>` consent token on its first `@`.
///
/// # Errors
/// [`ClientError::Corrupt`] if the token is not `<skill>@<digest>`.
fn parse_skill_at(token: &str) -> Result<(String, String), ClientError> {
    match token.split_once('@') {
        Some((skill, rest)) if !skill.is_empty() && !rest.is_empty() => {
            Ok((skill.to_owned(), rest.to_owned()))
        }
        _ => Err(ClientError::Corrupt(format!(
            "--approve must be `<skill>@<digest>`, got `{token}`"
        ))),
    }
}
