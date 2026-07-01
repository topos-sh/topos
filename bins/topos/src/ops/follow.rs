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
//!   the named, already-disclosed first-receive bytes (the I-TOFU "one --approve").
//!
//! **I-NO-USER-TOKEN.** The agent only ever holds the opaque grant + the minted read creds — never a user
//! token; enrollment completes by POLLING. **Secrets** (the device code, the grant, the read tokens) live
//! only in the `0600` WAL / `follows.json`, are redacted in `Debug`, and never reach a URL / log / error.

use std::collections::HashMap;

use topos_core::digest::{self, ManifestEntry, to_hex};
use topos_core::sign::EnrollFields;
use topos_gitstore::Store;
use topos_types::persisted::{Lock, PlacementMap, SwapCapability, SyncState};
use topos_types::results::{EnrollmentPending, FollowData, FollowOffer, Offer};
use topos_types::{Generation, SCHEMA_VERSION};

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::error::ClientError;
use crate::identity::{self, DeviceKeyRef};
use crate::plane::{EnrollSource, PlaneSource, PointerFetch, TokenPoll};
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
) -> Result<FollowData, ClientError> {
    if !opts.approve.is_empty() {
        return approve(ctx, &opts.approve);
    }
    if opts.resume {
        return resume(ctx, connectors);
    }
    let link = link.ok_or_else(|| {
        ClientError::Enrollment("follow needs an /i/ link (or --resume / --approve)".into())
    })?;
    begin(ctx, connectors, &link, opts.manual)
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
    let (base_url, token) = parse_link(ctx, link)?;
    let enroll_src = (connectors.enroll)(&base_url);

    let bootstrap = enroll_src.fetch_bootstrap(&token)?;
    // I-TOFU: pin the plane key over the unauthenticated `/i/` channel (or refuse a cross-plane / re-pin).
    let pinned_plane_key = tofu_decide(ctx, &base_url, &bootstrap)?;

    // Load (or, on first use, mint) the device signer — its public key starts the device authorization.
    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
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
    };

    // The 0600 WAL — written BEFORE returning, so a `--resume` can pick up exactly this session.
    let expires_at_millis = now_millis(ctx).saturating_add(
        i64::try_from(auth.expires_in)
            .unwrap_or(0)
            .saturating_mul(1000),
    );
    let wal = enroll::PendingEnrollment {
        schema_version: SCHEMA_VERSION,
        state: enroll::EnrollPhase::Authorizing {
            context: context.clone(),
            device_code: auth.device_code,
            user_code: auth.user_code.clone(),
            interval: auth.interval,
            expires_at_millis,
        },
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;

    Ok(pending_followdata(
        &context,
        &auth.user_code,
        &auth.verification_uri,
    ))
}

/// The TOFU decision (I-TOFU). Decode the bootstrap's plane signing key (`alg` is the CLOSED `SignatureAlg`
/// enum — a non-Ed25519 alg already failed the bootstrap deserialize; the value is raw-32B base64url →
/// 64-hex). Then: ABSENT `instance.json` ⇒ first-ever pin; PRESENT with a different `base_url` ⇒ refuse a
/// cross-plane follow (v0 is one plane per install); PRESENT, same `base_url`, different key ⇒
/// `KEY_REPIN_REQUIRED`; same key ⇒ OK. Returns the pinned key as 64-char lowercase hex.
fn tofu_decide(
    ctx: &Ctx<'_>,
    base_url: &str,
    bootstrap: &topos_types::BootstrapData,
) -> Result<String, ClientError> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(bootstrap.plane.signing_key.value.as_bytes())
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
            enrolled_at_millis,
        } => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            promote(
                ctx,
                connectors,
                &context,
                &read_creds,
                &device_key_id,
                enrolled_at_millis,
                &signer,
            )
        }
        enroll::EnrollPhase::Authorizing {
            context,
            device_code,
            user_code,
            ..
        } => {
            let enroll_src = (connectors.enroll)(&context.base_url);
            match enroll_src.poll_token(&device_code)? {
                // Still pending — re-surface the URL; the WAL stays put for the next `--resume`.
                TokenPoll::Pending | TokenPoll::SlowDown => Ok(pending_followdata(
                    &context,
                    &user_code,
                    &verification_uri(&context),
                )),
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
                TokenPoll::Granted(grant) => {
                    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
                    // Sign the enroll possession proof over the SERVER-trusted framed fields (the server
                    // re-derives each from the grant, so they must match) + redeem.
                    let redeem =
                        redeem_grant(&*enroll_src, &context, &user_code, grant.as_str(), &signer)?;
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
                            schema_version: SCHEMA_VERSION,
                            state: enroll::EnrollPhase::Redeemed {
                                context: context.clone(),
                                read_creds: read_creds.clone(),
                                device_key_id: redeem.device_key_id.clone(),
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
                        enrolled_at,
                        &signer,
                    )
                }
            }
        }
    }
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

fn promote(
    ctx: &Ctx<'_>,
    connectors: &FollowConnectors<'_>,
    context: &enroll::EnrollContext,
    read_creds: &[enroll::RedeemedCredDoc],
    device_key_id: &str,
    enrolled_at: i64,
    signer: &DeviceSigner,
) -> Result<FollowData, ClientError> {
    // 1) instance.json — PUBLIC (the plane key is a public key) → ordinary perms.
    enroll::write_instance(
        ctx.fs,
        &ctx.layout,
        &enroll::Instance {
            schema_version: SCHEMA_VERSION,
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

    // 3) user.json — metadata only (no secret) → ordinary perms.
    enroll::write_user(
        ctx.fs,
        &ctx.layout,
        &enroll::UserDoc {
            schema_version: SCHEMA_VERSION,
            workspace_id: context.workspace_id.clone(),
            deployment_mode: context.deployment_mode,
            email: None,
            roles: Vec::new(),
            invite_rooted: true,
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
        lay_first_receive_baseline(ctx, &skill_id, display_name(context, &cred.skill_id))?;
    }

    // 6) Delete the WAL — enrollment is complete.
    enroll::delete_wal(ctx.fs, &ctx.layout)?;

    // 7) Arm session-start currency for this follower — best-effort + idempotent, mirroring `add`. A pure
    // follower never runs `add`, so this is the one place their hook gets installed; it edits the harness
    // CONFIG (never a skill dir), and the sweep no-ops until the first bytes land. Infallible (a
    // TriggerReport, degraded on a config hiccup), so it can never roll back the completed enrollment;
    // the outcome is disclosed on the result.
    let currency = ctx.harness.install_currency_trigger();

    // 8) Disclose the batched offers (a READ-ONLY metadata fetch — places NOTHING, never mutates the
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
        pending: None,
        currency: Some(currency),
    })
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
    // An empty embedded-git store the first received version is later written into.
    let store = Store::init(&sp.store)?;
    let batch = store.durability_set()?;
    for f in &batch.files {
        ctx.fs.fsync_file(f)?;
    }
    for d in &batch.dirs {
        ctx.fs.fsync_dir(d)?;
    }

    // The adapter keeps a `&str` seam; the id here is the validated newtype, honoring its "callers pass
    // an already-validated id" contract.
    let placement = ctx.harness.placement_for(skill_id.as_str(), None).dir;
    doc::write_doc(
        ctx.fs,
        &sp.sync,
        &SyncState {
            schema_version: SCHEMA_VERSION,
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
            schema_version: SCHEMA_VERSION,
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
            schema_version: SCHEMA_VERSION,
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

fn approve(ctx: &Ctx<'_>, targets: &[String]) -> Result<FollowData, ClientError> {
    let follows = enroll::read_follows(ctx.fs, &ctx.layout)?
        .ok_or_else(|| ClientError::Enrollment("not enrolled; nothing to approve".into()))?;
    let contexts = enroll::follow_contexts(&follows);
    let workspace_id = follows
        .follows
        .first()
        .map(|f| f.workspace_id.clone())
        .unwrap_or_default();

    let mut skills = Vec::new();
    for target in targets {
        // Strip an optional `@<digest>` (the disclosed-offer reference) and resolve by skill name.
        let name = strip_digest(target);
        let (skill_id, lock) = super::resolve_skill(ctx, name)?;
        if let Some((_, follow_ctx)) = contexts.iter().find(|(id, _)| id == skill_id.as_str())
            && follow_ctx.following
        {
            // The explicit accept IS the I-TOFU first-receive yes (places the bytes).
            sync_engine::sync_one(ctx, &skill_id, follow_ctx, Invocation::Accept)?;
        }
        // Re-read the lock to disclose what is now current locally.
        let updated =
            doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&skill_id).lock)?.unwrap_or(lock);
        skills.push(FollowOffer {
            skill_id: skill_id.into_string(),
            name: updated.name.clone(),
            offer: Offer {
                version_id: updated.base_commit.clone(),
                bundle_digest: updated.bundle_digest.clone(),
            },
        });
    }

    Ok(FollowData {
        workspace_id,
        enrolled: true,
        skills,
        deployment_mode: None,
        workspace_display_name: None,
        verified_domain: None,
        verified_domain_status: None,
        pending: None,
        currency: None,
    })
}

// =================================================================================================
// Small helpers.
// =================================================================================================

/// Parse `<base_url>/i/<token>` into `(base_url, token)`. A FULL URL splits on `/i/` (the token is the
/// first path segment after it). A bare token reuses the already-pinned plane's `base_url` (so a follow-up
/// `follow <token>` works once enrolled); without a prior enrollment a bare token is refused.
fn parse_link(ctx: &Ctx<'_>, link: &str) -> Result<(String, String), ClientError> {
    let link = link.trim();
    if let Some(idx) = link.find("/i/") {
        let base = link[..idx].trim_end_matches('/');
        let rest = &link[idx + 3..];
        let token = rest.split(['/', '?', '#']).next().unwrap_or("");
        if base.is_empty() || token.is_empty() {
            return Err(ClientError::Enrollment("malformed invite link".into()));
        }
        return Ok((base.to_owned(), token.to_owned()));
    }
    // A bare token: reuse the pinned plane (must already be enrolled).
    if let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)? {
        return Ok((instance.base_url, link.to_owned()));
    }
    Err(ClientError::Enrollment(
        "a bare invite token needs a prior enrollment; pass the full /i/<token> link".into(),
    ))
}

/// Build the pending FollowData (the agent surfaces the URL WITH the verified-domain provenance — the
/// relay-phishing guard — then runs `follow --resume`).
fn pending_followdata(
    context: &enroll::EnrollContext,
    user_code: &str,
    verification_uri: &str,
) -> FollowData {
    FollowData {
        workspace_id: context.workspace_id.clone(),
        enrolled: false,
        skills: Vec::new(),
        deployment_mode: Some(context.deployment_mode),
        workspace_display_name: Some(context.workspace_display_name.clone()),
        verified_domain: context.verified_domain.clone(),
        verified_domain_status: Some(context.verified_domain_status),
        pending: Some(EnrollmentPending {
            verification_uri_complete: complete_uri(verification_uri, user_code),
            user_code: user_code.to_owned(),
            // No RFC-3339 formatter client-side; the WAL holds the absolute expiry for the recovery sweep.
            expires_at: None,
        }),
        currency: None,
    }
}

/// The verification URL with the `user_code` embedded (RFC-8628 `verification_uri_complete`).
fn complete_uri(verification_uri: &str, user_code: &str) -> String {
    let sep = if verification_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{verification_uri}{sep}user_code={user_code}")
}

/// The plane's device-verification URL, rebuilt from the pinned base (used when a `--resume` is still
/// pending — the `Authorizing` WAL stores only the user code, not the full URL).
fn verification_uri(context: &enroll::EnrollContext) -> String {
    format!("{}/device", context.base_url)
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
fn machine_name(signer: &DeviceSigner) -> String {
    format!("topos CLI ({})", signer.device_key_id())
}

/// `now` as epoch-millis (saturating), via the injected clock.
fn now_millis(ctx: &Ctx<'_>) -> i64 {
    i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX)
}
