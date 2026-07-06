//! `follow` — the device-flow enrollment + first-receive client.
//!
//! Three motions, one verb (the harness drives it non-interactively):
//! - **`follow <link>`** (call 1) — read the `/i/` TOFU bootstrap, pin the plane key, start a device
//!   authorization, write a `0600` WAL, and return `ENROLLMENT_PENDING` + the verification URL.
//! - **`follow --resume`** (call 2) — poll once; on a granted poll, sign the enroll possession proof,
//!   redeem the grant into per-skill read creds, record them in the WAL (the lockout fence), PROMOTE
//!   (write `instance.json` / `follows.json` / `user.json` / the device key + lay the first-receive
//!   baselines), delete the WAL, and disclose the offers.
//! - **`follow --approve <skill>[@<digest>] …`** (post-enroll) — drive the existing pull engine to place
//!   the named, already-disclosed first-receive bytes (the I-TOFU "one --approve"). On a retained entry
//!   `unfollow` paused (`following == false`) it RESUMES the follow instead: the flag flips back on, a
//!   still-pending first-receive offer is placed, and otherwise the next `pull` lands the team's current.
//!
//! **I-NO-USER-TOKEN.** The agent only ever holds the opaque grant + the minted read creds — never a user
//! token; enrollment completes by POLLING. **Secrets** (the device code, the grant, the read tokens) live
//! only in the `0600` WAL / `follows.json`, are redacted in `Debug`, and never reach a URL / log / error.

use std::collections::HashMap;

use topos_core::digest::{self, ManifestEntry, to_hex};
use topos_core::sign::EnrollFields;
use topos_gitstore::Store;
use topos_types::bootstrap::VerifiedDomainStatus;
use topos_types::persisted::{Lock, PlacementMap, SwapCapability, SyncState};
use topos_types::results::{EnrollmentPending, FollowData, FollowOffer, Offer};
use topos_types::{Generation, PERSISTED_SCHEMA_VERSION};

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::error::ClientError;
use crate::identity::{self, DeviceKeyRef};
use crate::plane::{EnrollSource, FollowContext, PlaneSource, PointerFetch, TokenPoll};
use crate::plane_http::SkillCred;
use crate::{doc, enroll, sidecar};

use super::sync_engine::{self, Invocation};

/// The 64-char all-zero hex sentinel a never-received baseline uses for its (absent) base commit / digest.
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// The genesis generation sentinel — `(0,0)` means "nothing authenticated / applied yet".
const GENESIS: Generation = Generation { epoch: 0, seq: 0 };

/// `follow`'s flags, parsed from argv.
pub(crate) struct FollowOpts {
    /// `--manual` ⇒ confirm-each adoption (else auto).
    pub manual: bool,
    /// `--resume` ⇒ poll + complete a pending enrollment.
    pub resume: bool,
    /// `--approve <skill>[@<digest>] …` ⇒ place the named, already-disclosed first-receive bytes.
    pub approve: Vec<String>,
}

/// Builds the creds-free enrollment transport for a plane base URL.
pub(crate) type EnrollConnect<'a> = dyn Fn(&str) -> Box<dyn EnrollSource> + 'a;
/// Builds the read transport (the offer-disclosure source) for a base URL + the minted read creds.
pub(crate) type PlaneConnect<'a> =
    dyn Fn(&str, HashMap<String, SkillCred>) -> Box<dyn PlaneSource> + 'a;

/// The network seams the op needs, as factories — the base URL is known only after the op parses the
/// `/i/` link (call 1) or reads the WAL (resume), so the transports can't be pre-built in the composition
/// root. Production wires the `ureq` transports; the tests wire fakes (no HTTP).
pub(crate) struct FollowConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub plane: &'a PlaneConnect<'a>,
}

/// The verb's outcome: the schema-pinned wire payload, plus TTY-only disclosure (`FollowData`'s pinned
/// shape has no resume field, so the resumed names ride alongside it — never on the `--json` surface).
pub(crate) struct FollowOutcome {
    pub data: FollowData,
    /// Display names of skills whose retained `following == false` entry this `--approve` flipped back on.
    pub resumed: Vec<String>,
}

/// Dispatch the `follow` verb. `--approve` and `--resume` ignore `link`; a bare `follow <link>` begins.
///
/// # Errors
/// [`ClientError::Enrollment`] for a missing link / denied / expired session; [`ClientError::KeyRepinRequired`]
/// / [`ClientError::PlacementUnsupported`] for a TOFU mismatch; otherwise a transport / io / store failure.
pub(crate) fn follow(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    link: Option<String>,
    opts: FollowOpts,
) -> Result<FollowOutcome, ClientError> {
    if !opts.approve.is_empty() {
        return approve(ctx, &opts.approve);
    }
    if opts.resume {
        return Ok(plain(resume(ctx, connectors)?));
    }
    let link = link.ok_or_else(|| {
        ClientError::Enrollment("follow needs an /i/ link (or --resume / --approve)".into())
    })?;
    Ok(plain(begin(ctx, connectors, &link, opts.manual)?))
}

/// Wrap a non-`--approve` result (those paths never resume a paused follow).
fn plain(data: FollowData) -> FollowOutcome {
    FollowOutcome {
        data,
        resumed: Vec::new(),
    }
}

// =================================================================================================
// Call 1 — `follow <link>`: bootstrap → TOFU → device-authorize → WAL → ENROLLMENT_PENDING.
// =================================================================================================

fn begin(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    link: &str,
    manual: bool,
) -> Result<FollowData, ClientError> {
    let (link_base, token) = parse_link(ctx, link)?;

    // An unsettled claim redeem for this same link retries the POST directly — NEVER refetching `/i/`
    // (the first send may have consumed the claim, whose bootstrap then serves 404 by design; the
    // server's same-device replay re-answers Redeemed). The match is on the TOKEN alone: it is
    // HMAC-derived and unique per plane, and the WAL's `base_url` is the re-rooted API base while a
    // re-pasted link may ride the team's share host — the token, not the host string, is the claim's
    // identity. A different token is refused typed.
    if let Some(wal) = enroll::read_wal(ctx.fs, &ctx.layout)?
        && let enroll::EnrollPhase::ClaimPending { claim_token, .. } = &wal.state
    {
        if *claim_token == token {
            return retry_claim(ctx, connectors, &wal);
        }
        return Err(ClientError::Enrollment(
            "a different claim enrollment is in progress; run `topos follow --resume` to settle it \
             first"
                .into(),
        ));
    }

    let bootstrap = (connectors.enroll)(&link_base).fetch_bootstrap(&token)?;
    // RE-ROOT onto the plane's declared API base. The link is only where the bootstrap lives (a hosted
    // team's share links ride its web origin); the bootstrap declares the plane every later call — the
    // device flow, the redeem, every pull — must dial. The declared base passes the same gate as the
    // link base and may never downgrade the transport. This adds no attacker capability: whoever mints
    // the link already controls both the bootstrap and the key it pins (the link IS the TOFU channel).
    let base_url = resolve_api_base(&link_base, &bootstrap.plane.base_url)?;
    // I-TOFU: pin the plane key over the unauthenticated `/i/` channel (or refuse a cross-plane / re-pin).
    // Keyed on the RE-ROOTED API base — the base every later verify dials and `instance.json` records —
    // so a second link from the same plane (whatever host the link string rides) matches the pin.
    let pinned_plane_key = tofu_decide_key(ctx, &base_url, &bootstrap.plane.signing_key)?;

    // Branch on the enrollment method the bootstrap disclosed. A method this build does not understand
    // fails CLOSED — proceeding would enroll under a posture the human was never able to review.
    match bootstrap.plane.enrollment_method.as_str() {
        // The one-shot admin-claim door (self-host bearer): no device-auth session, no `--resume`.
        "admin_claim" => {
            return claim_follow(
                ctx,
                connectors,
                &base_url,
                &token,
                &bootstrap,
                &pinned_plane_key,
            );
        }
        // The device-authorization flow (with or without the passcode identity leg on the verify page).
        "device_code" | "passcode" => {}
        other => {
            return Err(ClientError::Enrollment(format!(
                "this plane offers enrollment method '{other}', which this topos build does not \
                 support; upgrade topos"
            )));
        }
    }

    // Load (or, on first use, mint) the device signer — its public key starts the device authorization.
    // The transport is (re)built on the RE-ROOTED API base: only the one bootstrap GET rode the link base.
    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let enroll_src = (connectors.enroll)(&base_url);
    let auth = enroll_src.device_authorize(&token, signer.public_key(), &machine_name(&signer))?;

    let context = enroll::EnrollContext {
        base_url,
        pinned_plane_key,
        plane_key_id: bootstrap.plane.signing_key.key_id.clone(),
        deployment_mode: bootstrap.plane.deployment_mode,
        enrollment_method: bootstrap.plane.enrollment_method.clone(),
        workspace_id: bootstrap.workspace.workspace_id.clone(),
        workspace_display_name: bootstrap.workspace.display_name.clone(),
        verified_domain: bootstrap.workspace.verified_domain.clone(),
        verified_domain_status: bootstrap.workspace.verified_domain_status,
        offered_skills: bootstrap
            .offered_skills
            .iter()
            .map(|s| enroll::OfferedSkill {
                skill_id: s.skill_id.clone(),
                name: s.name.clone(),
            })
            .collect(),
        mode: if manual {
            enroll::FollowModeDoc::ConfirmEach
        } else {
            enroll::FollowModeDoc::Auto
        },
        root: enroll::EnrollRoot::Invite,
    };

    // The 0600 WAL — written BEFORE returning, so a `--resume` can pick up exactly this session.
    let expires_at_millis = now_millis(ctx).saturating_add(
        i64::try_from(auth.expires_in)
            .unwrap_or(0)
            .saturating_mul(1000),
    );
    let wal = enroll::PendingEnrollment {
        schema_version: PERSISTED_SCHEMA_VERSION,
        state: enroll::EnrollPhase::Authorizing {
            context: context.clone(),
            device_code: auth.device_code,
            user_code: auth.user_code.clone(),
            verification_uri_complete: auth.verification_uri_complete.clone(),
            interval: auth.interval,
            expires_at_millis,
        },
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;

    // The SERVER-built complete URI wins verbatim; the client-side embed is only the older-plane fallback.
    let complete = auth
        .verification_uri_complete
        .unwrap_or_else(|| complete_uri(&auth.verification_uri, &auth.user_code));
    Ok(pending_followdata(
        &context,
        &auth.user_code,
        complete,
        device_fingerprint(&signer),
    ))
}

/// The TOFU decision (I-TOFU), shared by every pre-enrollment door (`/i/` bootstrap, standup authorize).
/// `base_url` is the plane's API base — for a link follow that is the RE-ROOTED base the bootstrap
/// declared (never the share host the link string rode), so the pin matches what every later verify dials.
/// Decode the plane signing key (`alg` is the CLOSED `SignatureAlg` enum — a non-Ed25519 alg already
/// failed the deserialize; the value is raw-32B base64url → 64-hex). Then: ABSENT `instance.json` ⇒
/// first-ever pin; PRESENT with a different `base_url` ⇒ refuse a cross-plane follow (v0 is one plane per
/// install); PRESENT, same `base_url`, different key ⇒ `KEY_REPIN_REQUIRED`; same key ⇒ OK. Returns the
/// pinned key as 64-char lowercase hex.
pub(super) fn tofu_decide_key(
    ctx: &Ctx<'_>,
    base_url: &str,
    signing_key: &topos_types::bootstrap::BootstrapSigningKey,
) -> Result<String, ClientError> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signing_key.value.as_bytes())
        .map_err(|_| ClientError::Corrupt("plane signing key is not base64url".into()))?;
    let raw: [u8; 32] = raw
        .try_into()
        .map_err(|_| ClientError::Corrupt("plane signing key is not 32 bytes".into()))?;
    let key_hex = to_hex(&raw);

    match enroll::read_instance(ctx.fs, &ctx.layout)? {
        None => Ok(key_hex),
        Some(instance) => {
            if instance.base_url != base_url {
                return Err(ClientError::PlacementUnsupported {
                    reason: "v0 is one plane per install; this client is enrolled with a different plane"
                        .into(),
                });
            }
            if instance.plane_key != key_hex {
                return Err(ClientError::KeyRepinRequired);
            }
            Ok(key_hex)
        }
    }
}

// =================================================================================================
// Call 2 — `follow --resume`: poll → (granted) redeem → Redeemed WAL → promote.
// =================================================================================================

fn resume(ctx: &Ctx<'_>, connectors: &FollowConnectors<'_>) -> Result<FollowData, ClientError> {
    let wal = enroll::read_wal(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("no enrollment in progress; run `follow <link>` first".into())
    })?;

    match wal.state {
        // A Redeemed-but-unpromoted WAL: PROMOTE without re-redeeming (the single-use grant is spent;
        // recovery completes from the persisted creds).
        enroll::EnrollPhase::Redeemed {
            context,
            read_creds,
            device_key_id,
            principal,
            enrolled_at_millis,
        } => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            promote(
                ctx,
                connectors,
                &context,
                &read_creds,
                &device_key_id,
                principal.as_deref(),
                enrolled_at_millis,
                &signer,
            )
        }
        // A standup session belongs to `publish` — its resume is the ORIGINAL publish command (the
        // consent digest re-derives from that command's `--approve` each invocation, which `follow`
        // cannot supply).
        enroll::EnrollPhase::AuthorizingStandup { .. } => Err(ClientError::Enrollment(
            "a workspace standup is in progress; re-run the `topos publish … --approve …` command \
             that started it"
                .into(),
        )),
        // An unsettled claim redeem: retry the POST directly (never refetch the possibly-consumed /i/).
        state @ enroll::EnrollPhase::ClaimPending { .. } => {
            let wal = enroll::PendingEnrollment {
                schema_version: PERSISTED_SCHEMA_VERSION,
                state,
            };
            retry_claim(ctx, connectors, &wal)
        }
        enroll::EnrollPhase::Authorizing {
            context,
            device_code,
            user_code,
            verification_uri_complete,
            ..
        } => {
            let enroll_src = (connectors.enroll)(&context.base_url);
            match enroll_src.poll_token(&device_code)? {
                // Still pending — re-surface the persisted SERVER-built URL, verbatim. There is no
                // client-side reconstruction: the plane's verification page lives on its (possibly
                // separate) verify base, which this client cannot derive — a fabricated URL would point
                // the human at a page that does not exist. A WAL an older build wrote without the URL
                // restarts cleanly.
                TokenPoll::Pending | TokenPoll::SlowDown => {
                    let complete = verification_uri_complete.ok_or_else(|| {
                        ClientError::Enrollment(
                            "this enrollment session carries no verification URL; start over with \
                             `follow <link>`"
                                .into(),
                        )
                    })?;
                    // The device key is deterministic (load-or-generate returns the same key), so the
                    // re-surfaced pending discloses the same fingerprint the human sees on the page.
                    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
                    Ok(pending_followdata(
                        &context,
                        &user_code,
                        complete,
                        device_fingerprint(&signer),
                    ))
                }
                // A terminal denial / expiry — sweep the WAL, surface a typed error.
                TokenPoll::Denied => {
                    enroll::delete_wal(ctx.fs, &ctx.layout)?;
                    Err(ClientError::Enrollment(
                        "the enrollment was denied at the verification page".into(),
                    ))
                }
                TokenPoll::Expired => {
                    enroll::delete_wal(ctx.fs, &ctx.layout)?;
                    Err(ClientError::Enrollment(
                        "the enrollment session expired; start over with `follow <link>`".into(),
                    ))
                }
                TokenPoll::Granted(granted) => {
                    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
                    // Sign the enroll possession proof over the SERVER-trusted framed fields (the server
                    // re-derives each from the grant, so they must match) + redeem.
                    let redeem = redeem_grant(
                        &*enroll_src,
                        &context,
                        &user_code,
                        granted.grant.as_str(),
                        &signer,
                    )?;
                    let read_creds: Vec<enroll::RedeemedCredDoc> = redeem
                        .read_creds
                        .iter()
                        .map(|c| enroll::RedeemedCredDoc {
                            skill_id: c.skill_id.clone(),
                            read_token: c.read_token.clone(),
                            expires_at: c.expires_at,
                        })
                        .collect();
                    let enrolled_at = now_millis(ctx);
                    // The lockout fence: record the redeemed creds (a single-use grant cannot be
                    // re-redeemed) BEFORE promotion, so a crash mid-promote completes from this WAL.
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
                    promote(
                        ctx,
                        connectors,
                        &context,
                        &read_creds,
                        &redeem.device_key_id,
                        redeem.principal.as_deref(),
                        enrolled_at,
                        &signer,
                    )
                }
            }
        }
    }
}

// =================================================================================================
// The admin-claim door — `follow <claim-link>` in ONE invocation (self-host bearer). The `/i/` bootstrap
// disclosed `enrollment_method: "admin_claim"`; there is no device-auth session and no `--resume` on the
// happy path. A pre-send WAL makes an uncertain send safely retryable: the retry POSTs `/v1/admin-claim`
// directly (a consumed claim's bootstrap serves 404 by design; the server's same-device replay of a
// consumed claim deterministically re-answers Redeemed).
// =================================================================================================

fn claim_follow(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    // The RE-ROOTED API base (the WAL records it; the redeem POST + the promote ride it).
    base_url: &str,
    token: &str,
    bootstrap: &topos_types::BootstrapData,
    pinned_plane_key: &str,
) -> Result<FollowData, ClientError> {
    // The pre-send WAL (0600 — the claim token is a bearer secret), BEFORE the first POST.
    let wal = enroll::PendingEnrollment {
        schema_version: PERSISTED_SCHEMA_VERSION,
        state: enroll::EnrollPhase::ClaimPending {
            base_url: base_url.to_owned(),
            pinned_plane_key: pinned_plane_key.to_owned(),
            plane_key_id: bootstrap.plane.signing_key.key_id.clone(),
            deployment_mode: bootstrap.plane.deployment_mode,
            enrollment_method: bootstrap.plane.enrollment_method.clone(),
            workspace_id: bootstrap.workspace.workspace_id.clone(),
            workspace_display_name: bootstrap.workspace.display_name.clone(),
            claim_token: token.to_owned(),
        },
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;
    retry_claim(ctx, connectors, &wal)
}

/// Send (or re-send) the claim redeem recorded in a `ClaimPending` WAL, then convert to the ordinary
/// `Redeemed` fence and promote. Callable any number of times: the server treats a same-device replay of a
/// consumed claim as the SAME redeem (lost-200 recovery), so the retry is idempotent by construction.
fn retry_claim(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    wal: &enroll::PendingEnrollment,
) -> Result<FollowData, ClientError> {
    let enroll::EnrollPhase::ClaimPending {
        base_url,
        pinned_plane_key,
        plane_key_id,
        deployment_mode,
        enrollment_method,
        workspace_id,
        workspace_display_name,
        claim_token,
    } = &wal.state
    else {
        return Err(ClientError::Corrupt(
            "retry_claim needs a claim_pending WAL".into(),
        ));
    };
    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let enroll_src = (connectors.enroll)(base_url);
    // The display name rides for DISCLOSURE only (the seated name comes from the mint-time claim row).
    let redeem =
        match enroll_src.admin_claim(claim_token, signer.public_key(), workspace_display_name) {
            Ok(redeem) => redeem,
            // A TERMINAL plane denial — per the seam's contract, `admin_claim` returns
            // [`ClientError::Enrollment`] ONLY for the 200+DENIED claim verdict (consumed by another device /
            // expired / the workspace already exists). The claim is definitively dead, so the ClaimPending WAL
            // is cleared BEFORE the error surfaces (mirroring the poll Denied/Expired arms clearing the
            // Authorizing WAL) — otherwise the sweep-exempt WAL wedges every later `follow <other-link>`
            // behind the begin-guard while `--resume` re-denies forever.
            Err(e @ ClientError::Enrollment(_)) => {
                enroll::delete_wal(ctx.fs, &ctx.layout)?;
                return Err(e);
            }
            // Everything else is an UNCERTAIN fault (a transport error, a non-200, a malformed body — the
            // send may or may not have consumed the claim): KEEP the WAL, so the next invocation retries the
            // POST directly and the server's same-device replay re-answers Redeemed.
            Err(e) => return Err(e),
        };
    // The redeem names the claim row's authoritative workspace — it must match what the link disclosed.
    if redeem.workspace_id != *workspace_id {
        return Err(ClientError::Enrollment(
            "the claimed workspace does not match the claim link".into(),
        ));
    }
    let context = enroll::EnrollContext {
        base_url: base_url.clone(),
        pinned_plane_key: pinned_plane_key.clone(),
        plane_key_id: plane_key_id.clone(),
        deployment_mode: *deployment_mode,
        enrollment_method: enrollment_method.clone(),
        workspace_id: workspace_id.clone(),
        workspace_display_name: workspace_display_name.clone(),
        verified_domain: None,
        verified_domain_status: VerifiedDomainStatus::Unverified,
        offered_skills: Vec::new(),
        mode: enroll::FollowModeDoc::Auto,
        root: enroll::EnrollRoot::Claim,
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
    let enrolled_at = now_millis(ctx);
    // The same lockout fence as the grant flow: the claim is consumed server-side, so the redeemed facts
    // are recorded BEFORE promotion and a crash mid-promote completes from here without re-sending.
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
    promote(
        ctx,
        connectors,
        &context,
        &read_creds,
        &redeem.device_key_id,
        redeem.principal.as_deref(),
        enrolled_at,
        &signer,
    )
}

/// Build the enroll possession proof + redeem the grant into a registered device + per-skill read creds.
fn redeem_grant(
    enroll_src: &dyn EnrollSource,
    context: &enroll::EnrollContext,
    user_code: &str,
    grant: &str,
    signer: &DeviceSigner,
) -> Result<crate::plane::Redeem, ClientError> {
    let grant_hash = digest::sha256(grant.as_bytes());
    // The offered skill ids are bound as a SET; the kernel sorts + dedups them, matching the server.
    let offered_ids: Vec<&str> = context
        .offered_skills
        .iter()
        .map(|s| s.skill_id.as_str())
        .collect();
    let fields = EnrollFields {
        workspace_id: &context.workspace_id,
        grant_hash,
        // `device_auth_id` is the session's `user_code` — what the authority binds.
        device_auth_id: user_code,
        device_key_id: signer.device_key_id(),
        device_public_key: signer.public_key(),
        offered_skill_ids: &offered_ids,
    };
    let sig = signer.sign_enroll(&fields)?;
    let redeem = enroll_src.redeem(&context.workspace_id, grant, signer.public_key(), sig)?;
    // The redeem echoes the grant's authoritative workspace — it must match the one we enrolled against.
    if redeem.workspace_id != context.workspace_id {
        return Err(ClientError::Enrollment(
            "the redeemed workspace does not match the invite".into(),
        ));
    }
    Ok(redeem)
}

// =================================================================================================
// Promotion — the sidecar writers (crash-safe; idempotent so a re-resume re-promotes cleanly).
// =================================================================================================

#[allow(clippy::too_many_arguments)]
fn promote(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    context: &enroll::EnrollContext,
    read_creds: &[enroll::RedeemedCredDoc],
    device_key_id: &str,
    principal: Option<&str>,
    enrolled_at: i64,
    signer: &DeviceSigner,
) -> Result<FollowData, ClientError> {
    let currency = promote_core(
        ctx,
        context,
        read_creds,
        device_key_id,
        principal,
        enrolled_at,
        signer,
    )?;

    // Disclose the batched offers (a READ-ONLY metadata fetch — places NOTHING, never mutates the
    // sidecar, so first-receive stays an OFFER). Best-effort: a fetch hiccup omits that skill's offer.
    let skills = disclose_offers(connectors, context, read_creds);

    Ok(FollowData {
        workspace_id: context.workspace_id.clone(),
        enrolled: true,
        skills,
        deployment_mode: Some(context.deployment_mode),
        workspace_display_name: Some(context.workspace_display_name.clone()),
        verified_domain: context.verified_domain.clone(),
        verified_domain_status: Some(context.verified_domain_status),
        plane_base_url: Some(context.base_url.clone()),
        pending: None,
        currency: Some(currency),
    })
}

/// The sidecar half of a promotion — every durable write, WITHOUT the follow-shaped offer disclosure (the
/// standup `publish` promotes through this too, then continues into its own publish body). Returns the
/// currency-trigger report. Idempotent, so a re-resume re-promotes cleanly.
#[allow(clippy::too_many_arguments)]
pub(super) fn promote_core(
    ctx: &Ctx<'_>,
    context: &enroll::EnrollContext,
    read_creds: &[enroll::RedeemedCredDoc],
    device_key_id: &str,
    principal: Option<&str>,
    enrolled_at: i64,
    signer: &DeviceSigner,
) -> Result<topos_types::TriggerReport, ClientError> {
    // 1) instance.json — PUBLIC (the plane key is a public key) → ordinary perms.
    enroll::write_instance(
        ctx.fs,
        &ctx.layout,
        &enroll::Instance {
            schema_version: PERSISTED_SCHEMA_VERSION,
            base_url: context.base_url.clone(),
            plane_key: context.pinned_plane_key.clone(),
            plane_key_id: context.plane_key_id.clone(),
            deployment_mode: context.deployment_mode,
            enrollment_method: context.enrollment_method.clone(),
            workspace_display_name: Some(context.workspace_display_name.clone()),
            verified_domain: context.verified_domain.clone(),
            verified_domain_status: context.verified_domain_status,
        },
    )?;

    // 2) follows.json — 0600 (the read tokens are secret); READ-MERGE-WRITE so a second follow never
    // clobbers the first. One entry per minted read cred (= one per followed skill).
    let additions: Vec<enroll::FollowEntry> = read_creds
        .iter()
        .map(|c| enroll::FollowEntry {
            skill_id: c.skill_id.clone(),
            workspace_id: context.workspace_id.clone(),
            read_token: c.read_token.clone(),
            mode: context.mode,
            review_required: false,
            following: true,
        })
        .collect();
    enroll::write_follows_merged(ctx.fs, &ctx.layout, &additions)?;

    // 3) user.json — metadata only (no secret) → ordinary perms. The seated principal (when the plane
    // disclosed one) is persisted for the receipt disclosures; an email-shaped principal also fills
    // `email` (a device-rooted `dev.…` id is NOT an email and never pretends to be one).
    enroll::write_user(
        ctx.fs,
        &ctx.layout,
        &enroll::UserDoc {
            schema_version: PERSISTED_SCHEMA_VERSION,
            workspace_id: context.workspace_id.clone(),
            deployment_mode: context.deployment_mode,
            email: principal.filter(|p| p.contains('@')).map(str::to_owned),
            principal: principal.map(str::to_owned),
            roles: Vec::new(),
            invite_rooted: matches!(context.root, enroll::EnrollRoot::Invite),
            enrolled_at,
        },
    )?;

    // 4) Record the device key reference in host.json (the PUBLIC key + a pointer to the 0600 seed).
    identity::set_device_key(
        ctx.fs,
        &ctx.layout,
        &DeviceKeyRef {
            alg: "Ed25519".to_owned(),
            device_key_id: device_key_id.to_owned(),
            public_key: to_hex(&signer.public_key()),
            private_key_ref: "device.key".to_owned(),
        },
    )?;

    // 5) Lay the first-receive baseline for each followed skill (so the pull engine treats it as state-②).
    // The minted skill id is parsed at this boundary too (the redeem transport already validated it; a
    // WAL-resumed promote revalidates) — only the validated newtype reaches the path joins below.
    for cred in read_creds {
        let skill_id = crate::id::SkillId::parse(&cred.skill_id)?;
        lay_first_receive_baseline(
            ctx,
            &skill_id,
            display_name(context, &cred.skill_id),
            &context.workspace_display_name,
        )?;
    }

    // 6) Delete the WAL — enrollment is complete.
    enroll::delete_wal(ctx.fs, &ctx.layout)?;

    // 7) Arm session-start currency for this follower — best-effort + idempotent, mirroring `add`. A pure
    // follower never runs `add`, so this is the one place their hook gets installed; it edits the harness
    // CONFIG (never a skill dir), and the sweep no-ops until the first bytes land. Infallible (a
    // TriggerReport, degraded on a config hiccup), so it can never roll back the completed enrollment;
    // the outcome is disclosed on the result.
    Ok(ctx.harness.install_currency_trigger())
}

/// Lay the NEVER-RECEIVED sidecar baseline for `skill_id` (mirrors `ops::add`'s staged-then-renamed,
/// all-or-nothing publish). A fresh `sync` (`observed = applied = (0,0)`, empty `recorded`), an empty
/// `lock` (the name + zero base/digest, no files), and a `map` carrying the harness placement target (so
/// the existing apply path can first-install there) but no applied content. Idempotent: a skill dir that
/// already exists (already baselined, or received) is left untouched — `follow` never clobbers bytes.
fn lay_first_receive_baseline(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    name: String,
    workspace_slug: &str,
) -> Result<(), ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    if ctx.fs.exists(&ctx.layout.skill_dir(skill_id)) {
        return Ok(());
    }

    let (staging_base, sp) = ctx.layout.staging(skill_id);
    if ctx.fs.exists(&staging_base) {
        ctx.fs.remove_dir_all(&staging_base)?;
    }
    ctx.fs.create_dir_all(&sp.store)?;
    // An empty embedded-git store the first received version is later written into. The full-tree
    // durability set is exactly right HERE (and only here + `add`'s staging import): the store is a
    // fresh `init_bare`, so the whole tree IS this op's writes (the repo scaffolding — HEAD / config /
    // objects/ / refs/) and never carries history.
    let store = Store::init(&sp.store)?;
    super::sync_engine::fsync_batch(ctx, &store.durability_set()?)?;

    // The adapter keeps a `&str` seam; the id here is the validated newtype, honoring its "callers pass
    // an already-validated id" contract. The display name + workspace slug are UNTRUSTED advisory hints —
    // the adapter sanitizes them and falls back to the id, so they can never redirect the placement.
    let placement = ctx
        .harness
        .placement_for(
            skill_id.as_str(),
            topos_harness::PlacementNaming {
                name: Some(&name),
                workspace_slug: Some(workspace_slug),
            },
            None,
        )
        .dir;
    doc::write_doc(
        ctx.fs,
        &sp.sync,
        &SyncState {
            schema_version: PERSISTED_SCHEMA_VERSION,
            observed: GENESIS,
            applied: GENESIS,
            recorded: Vec::new(),
            base_commit: ZERO_HEX.to_owned(),
            work_hash: ZERO_HEX.to_owned(),
            held: false,
        },
    )?;
    doc::write_doc(
        ctx.fs,
        &sp.map,
        &PlacementMap {
            schema_version: PERSISTED_SCHEMA_VERSION,
            placements: vec![placement.to_string_lossy().into_owned()],
            applied_commit: ZERO_HEX.to_owned(),
            materialized_sha: ZERO_HEX.to_owned(),
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
            harness: Some(ctx.harness.id()),
            harness_layer: None,
        },
    )?;
    // lock LAST — the commit marker (recovery keeps a dir only when lock.json is present).
    doc::write_doc(
        ctx.fs,
        &sp.lock,
        &Lock {
            schema_version: PERSISTED_SCHEMA_VERSION,
            skill_id: skill_id.to_string(),
            name,
            base_commit: ZERO_HEX.to_owned(),
            bundle_digest: ZERO_HEX.to_owned(),
            files: Vec::new(),
        },
    )?;

    match ctx
        .fs
        .rename_dir_noreplace(&staging_base, &ctx.layout.skill_dir(skill_id))
    {
        Ok(()) => {}
        // Raced a concurrent baseline/receive — keep theirs, clean our staging.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            ctx.fs.remove_dir_all(&staging_base)?;
            return Ok(());
        }
        Err(e) => return Err(ClientError::Io(format!("publish baseline {skill_id}: {e}"))),
    }
    ctx.fs.fsync_dir(&ctx.layout.skills_dir())?;
    Ok(())
}

/// Disclose the batched first-receive offers via a READ-ONLY, authenticated metadata fetch (get the signed
/// `current`, verify it against the pinned key, fetch the version bytes, recompute the `bundle_digest`). It
/// writes NOTHING to the sidecar, so the skill stays never-received (an OFFER on the next pull). Best-effort.
fn disclose_offers(
    connectors: &FollowConnectors<'_>,
    context: &enroll::EnrollContext,
    read_creds: &[enroll::RedeemedCredDoc],
) -> Vec<FollowOffer> {
    if read_creds.is_empty() {
        return Vec::new();
    }
    let Ok(plane_key) = super::parse_hex32(&context.pinned_plane_key) else {
        return Vec::new();
    };
    let creds: HashMap<String, SkillCred> = read_creds
        .iter()
        .map(|c| {
            (
                c.skill_id.clone(),
                SkillCred::new(context.workspace_id.clone(), c.read_token.clone()),
            )
        })
        .collect();
    let plane = (connectors.plane)(&context.base_url, creds);

    let mut offers = Vec::new();
    for cred in read_creds {
        if let Some(offer) =
            disclose_one(&*plane, &plane_key, &cred.skill_id, &context.workspace_id)
        {
            offers.push(FollowOffer {
                skill_id: cred.skill_id.clone(),
                name: display_name(context, &cred.skill_id),
                offer,
            });
        }
    }
    offers
}

/// One skill's offer: authenticate its `current` pointer, fetch the version, recompute the digest. `None`
/// on any read/verify failure (the offer is then simply not disclosed — the subsequent `pull` discloses it).
fn disclose_one(
    plane: &dyn PlaneSource,
    plane_key: &[u8; 32],
    skill_id: &str,
    workspace_id: &str,
) -> Option<Offer> {
    let PointerFetch::Record(rec) = plane.get_current(skill_id, None).ok()? else {
        return None;
    };
    let version_id =
        sync_engine::authenticated_version_id(&rec, skill_id, workspace_id, plane_key)?;
    let fetched = plane.fetch_version(skill_id, version_id).ok()?;
    let entries: Vec<ManifestEntry> = fetched
        .files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let digest = digest::bundle_digest(&entries).ok()?;
    Some(Offer {
        version_id: to_hex(&version_id),
        bundle_digest: to_hex(&digest),
    })
}

// =================================================================================================
// `follow --approve` — drive the existing pull engine to place the named first-receive bytes.
// =================================================================================================

fn approve(ctx: &Ctx<'_>, targets: &[String]) -> Result<FollowOutcome, ClientError> {
    let follows = enroll::read_follows(ctx.fs, &ctx.layout)?
        .ok_or_else(|| ClientError::Enrollment("not enrolled; nothing to approve".into()))?;
    let contexts = enroll::follow_contexts(&follows);
    let workspace_id = follows
        .follows
        .first()
        .map(|f| f.workspace_id.clone())
        .unwrap_or_default();

    let mut skills = Vec::new();
    let mut resumed = Vec::new();
    for target in targets {
        // Strip an optional `@<digest>` (the disclosed-offer reference) and resolve by skill name.
        let name = strip_digest(target);
        let (skill_id, lock) = super::resolve_skill(ctx, name)?;
        let mut was_resumed = false;
        if let Some((_, follow_ctx)) = contexts.iter().find(|(id, _)| id == skill_id.as_str()) {
            if follow_ctx.following {
                // The explicit accept IS the I-TOFU first-receive yes (places the bytes).
                sync_engine::sync_one(ctx, &skill_id, follow_ctx, Invocation::Accept)?;
            } else {
                // A retained-but-paused entry (what `unfollow` keeps): `--approve` RESUMES it — the
                // command every paused surface points at. Flip the durable flag first; then, if a
                // first-receive offer is still pending, place it as a normal approve. Otherwise nothing
                // is pulled here — the resume is disclosed and the next `pull` lands the team's current.
                enroll::set_following(ctx.fs, &ctx.layout, skill_id.as_str(), true)?;
                was_resumed = true;
                let sync: Option<SyncState> =
                    doc::read_doc(ctx.fs, &ctx.layout.published(&skill_id).sync)?;
                if sync.as_ref().is_some_and(sync_engine::is_never_received) {
                    let resumed_ctx = FollowContext {
                        following: true,
                        ..follow_ctx.clone()
                    };
                    sync_engine::sync_one(ctx, &skill_id, &resumed_ctx, Invocation::Accept)?;
                }
            }
        }
        // Re-read the lock to disclose what is now current locally.
        let updated =
            doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&skill_id).lock)?.unwrap_or(lock);
        if was_resumed {
            resumed.push(updated.name.clone());
        }
        skills.push(FollowOffer {
            skill_id: skill_id.into_string(),
            name: updated.name.clone(),
            offer: Offer {
                version_id: updated.base_commit.clone(),
                bundle_digest: updated.bundle_digest.clone(),
            },
        });
    }

    Ok(FollowOutcome {
        data: FollowData {
            workspace_id,
            enrolled: true,
            skills,
            deployment_mode: None,
            workspace_display_name: None,
            verified_domain: None,
            verified_domain_status: None,
            plane_base_url: None,
            pending: None,
            currency: None,
        },
        resumed,
    })
}

// =================================================================================================
// Small helpers.
// =================================================================================================

/// Parse `<base_url>/i/<token>` into `(base_url, token)`. A FULL URL splits on `/i/` (the token is the
/// first path segment after it). A bare token reuses the already-pinned plane's `base_url` (so a follow-up
/// `follow <token>` works once enrolled); without a prior enrollment a bare token is refused.
///
/// The base is validated as a well-formed absolute http(s) URL HERE — before the secret token is ever
/// spliced into a request URL — because a malformed base would otherwise surface downstream as a ureq
/// `BadUri` transport error whose message echoes the FULL URI (token included), and every transport error
/// detail is persisted to the `~/.topos/log.jsonl` diagnostics file.
fn parse_link(ctx: &Ctx<'_>, link: &str) -> Result<(String, String), ClientError> {
    let link = link.trim();
    if let Some(idx) = link.find("/i/") {
        let base = link[..idx].trim_end_matches('/');
        let rest = &link[idx + 3..];
        let token = rest.split(['/', '?', '#']).next().unwrap_or("");
        if base.is_empty() || token.is_empty() {
            return Err(ClientError::Enrollment("malformed invite link".into()));
        }
        validate_base_url(base)?;
        return Ok((base.to_owned(), token.to_owned()));
    }
    // A bare token: reuse the pinned plane (must already be enrolled). The persisted base gets the same
    // gate — a hand-edited `instance.json` must not smuggle the token into a URI-shaped error either.
    if let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)? {
        validate_base_url(&instance.base_url)?;
        return Ok((instance.base_url, link.to_owned()));
    }
    Err(ClientError::Enrollment(
        "a bare invite token needs a prior enrollment; pass the full /i/<token> link".into(),
    ))
}

/// Resolve the API base a follow re-roots onto: the bootstrap's declared `plane.base_url`, normalized
/// (trimmed of trailing slashes — the pin comparisons are exact string equality) and gated the same way
/// as the link base — plus the one extra rule the re-root introduces: an `https` link must never re-root
/// onto a plain-`http` plane (a transport downgrade the human who pasted the link could not see).
pub(super) fn resolve_api_base(link_base: &str, declared: &str) -> Result<String, ClientError> {
    let declared = declared.trim().trim_end_matches('/');
    if declared.is_empty() {
        return Err(ClientError::Enrollment(
            "the bootstrap declared no plane base URL; upgrade the plane".into(),
        ));
    }
    validate_base_url(declared)?;
    if link_base.starts_with("https://") && !declared.starts_with("https://") {
        return Err(ClientError::Enrollment(
            "refusing to enroll: the link is https but the plane declares a plain-http base URL"
                .into(),
        ));
    }
    Ok(declared.to_owned())
}

/// Refuse a plane base that is not a well-formed absolute `http(s)://…` URL (the transport's own `Uri`
/// grammar, so anything accepted here builds cleanly downstream). The error names the problem — never the
/// link's token, which the caller has not yet joined onto the base.
fn validate_base_url(base: &str) -> Result<(), ClientError> {
    let well_formed = base.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && authority_usable(&uri)
    });
    if well_formed {
        Ok(())
    } else {
        Err(ClientError::Enrollment(
            "malformed invite link: the plane base URL is not a valid http(s) URL".into(),
        ))
    }
}

/// The authority half of the base gate: a non-empty host, and a bracketed literal must be a REAL IPv6
/// address. `http::Uri` itself accepts RFC-3986 IPvFuture-shaped brackets (e.g. `[bad]`), which the
/// transport only rejects LATER — with a URI-echoing error, too late for a URL that carries the token.
fn authority_usable(uri: &ureq::http::Uri) -> bool {
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host_port = authority.as_str().rsplit('@').next().unwrap_or("");
    match host_port.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .is_some_and(|(v6, _port)| v6.parse::<std::net::Ipv6Addr>().is_ok()),
        None => !host_port.is_empty(),
    }
}

/// Build the pending FollowData (the agent surfaces the URL WITH the verified-domain provenance — the
/// relay-phishing guard — then runs `follow --resume`). `verification_uri_complete` is the SERVER-built
/// link when the plane provided one (used verbatim), else the caller's reconstruction.
fn pending_followdata(
    context: &enroll::EnrollContext,
    user_code: &str,
    verification_uri_complete: String,
    device_fingerprint: String,
) -> FollowData {
    FollowData {
        workspace_id: context.workspace_id.clone(),
        enrolled: false,
        skills: Vec::new(),
        deployment_mode: Some(context.deployment_mode),
        workspace_display_name: Some(context.workspace_display_name.clone()),
        verified_domain: context.verified_domain.clone(),
        verified_domain_status: Some(context.verified_domain_status),
        plane_base_url: Some(context.base_url.clone()),
        pending: Some(EnrollmentPending {
            verification_uri_complete,
            user_code: user_code.to_owned(),
            device_fingerprint,
            // No RFC-3339 formatter client-side; the WAL holds the absolute expiry for the recovery sweep.
            expires_at: None,
        }),
        currency: None,
    }
}

/// The verification URL with the `user_code` embedded (RFC-8628 `verification_uri_complete`) — the
/// CLIENT-side reconstruction, used only as the fallback when the plane did not provide the complete URI.
pub(super) fn complete_uri(verification_uri: &str, user_code: &str) -> String {
    let sep = if verification_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{verification_uri}{sep}user_code={user_code}")
}

/// A followed skill's display name from the bootstrap (else its id).
fn display_name(context: &enroll::EnrollContext, skill_id: &str) -> String {
    context
        .offered_skills
        .iter()
        .find(|s| s.skill_id == skill_id)
        .and_then(|s| s.name.clone())
        .unwrap_or_else(|| skill_id.to_owned())
}

/// Drop an `@<digest>` suffix from a `follow --approve` target when the part after the last `@` is a valid
/// version id, leaving the skill name (so a name containing `@` is still accepted).
fn strip_digest(target: &str) -> &str {
    if let Some((name, suffix)) = target.rsplit_once('@')
        && super::parse_hex32(suffix).is_ok()
    {
        return name;
    }
    target
}

/// A human-readable machine name for the verification page (a confused-deputy aid, not authority) — carries
/// the device key id so a human can cross-check the fingerprint shown on the page.
pub(super) fn machine_name(signer: &DeviceSigner) -> String {
    format!("topos CLI ({})", signer.device_key_id())
}

/// The 16-hex device fingerprint the plane shows on its verification page — the first 16 hex chars of
/// `sha256(device_public_key)`. The device key id is `dk_` + the 32 hex of that same digest, so the
/// fingerprint is exactly the leading half of the id's hex portion. Returned raw (no grouping); the TTY
/// renderer groups it for eyeball comparison. A human cross-checks it against the page before approving.
pub(super) fn device_fingerprint(signer: &DeviceSigner) -> String {
    let id = signer.device_key_id();
    id.strip_prefix("dk_")
        .unwrap_or(id)
        .chars()
        .take(16)
        .collect()
}

/// `now` as epoch-millis (saturating), via the injected clock.
fn now_millis(ctx: &Ctx<'_>) -> i64 {
    i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{device_fingerprint, resolve_api_base, validate_base_url};

    /// The device fingerprint is the leading 16 hex of the device key id's hex portion (`dk_` + 32 hex);
    /// grouped in fours by the TTY renderer, it is what a human cross-checks against the verification page.
    #[test]
    fn device_fingerprint_is_the_first_16_hex_of_the_key_id() {
        use crate::device_signer::DeviceSigner;
        use crate::fs_seam::RealFs;
        use crate::sidecar::Layout;

        let dir = std::env::temp_dir().join(format!(
            "topos-fp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let signer = DeviceSigner::load_or_generate(&RealFs, &Layout::new(&dir)).unwrap();

        let fp = device_fingerprint(&signer);
        // Exactly the first 16 hex of the id (id = `dk_` + 32 hex of sha256(pubkey)).
        let id_hex = signer.device_key_id().strip_prefix("dk_").unwrap();
        assert_eq!(fp, id_hex[..16]);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));

        // The grouped display (raw id → grouped): 4×4 hex separated by single spaces.
        let grouped = crate::render::group_fingerprint(&fp);
        assert_eq!(grouped.split(' ').count(), 4);
        assert!(grouped.split(' ').all(|chunk| chunk.len() == 4));
        assert_eq!(grouped.replace(' ', ""), fp);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The re-root resolver: normalizes trailing slashes (the pin compares are exact strings), applies
    /// the same URL gate as the link base, and refuses the one thing a re-root could newly smuggle in —
    /// an https→http transport downgrade the human who pasted the link could not see.
    #[test]
    fn api_base_resolver_normalizes_gates_and_refuses_downgrade() {
        assert_eq!(
            resolve_api_base("https://links.example", "https://api.plane.test/").unwrap(),
            "https://api.plane.test"
        );
        assert_eq!(
            resolve_api_base("http://localhost:1", "http://127.0.0.1:2").unwrap(),
            "http://127.0.0.1:2"
        );
        // An http link may upgrade to an https plane…
        assert_eq!(
            resolve_api_base("http://links.example", "https://api.plane.test").unwrap(),
            "https://api.plane.test"
        );
        // …but an https link never downgrades to plain http.
        assert!(resolve_api_base("https://links.example", "http://api.plane.test").is_err());
        // An empty / malformed declared base is refused typed (same gate as the link base).
        assert!(resolve_api_base("https://links.example", "").is_err());
        assert!(resolve_api_base("https://links.example", "not-a-url").is_err());
    }

    #[test]
    fn base_url_gate_accepts_the_legit_shapes_and_refuses_the_uri_hazards() {
        for ok in [
            "https://topos.sh",
            "https://api.topos.sh",
            "http://localhost:8787",
            "http://127.0.0.1:8080",
            "http://[::1]:8787",
            "http://[2001:db8::1]",
        ] {
            assert!(validate_base_url(ok).is_ok(), "must accept {ok}");
        }
        for bad in [
            "http://[bad]",     // IPvFuture-shaped garbage http::Uri itself accepts
            "http://[::1",      // unterminated bracket
            "ftp://plane.test", // not http(s)
            "http:",            // no authority
            "plane.test",       // no scheme
            "",
        ] {
            assert!(validate_base_url(bad).is_err(), "must refuse {bad:?}");
        }
    }
}
