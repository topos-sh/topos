//! The real plane transport: a blocking `ureq` (3, rustls+ring) [`PlaneSource`] that feeds the already-built
//! pull engine, plus the on-disk [`FollowSource`].
//!
//! [`UreqPlane`] is a **dumb transport** — it speaks the wire under the workspace **Bearer credential**
//! (`GET /v1/workspaces/{ws}/skills/{skill}/current` with the commit-sensitive conditional-GET headers;
//! `GET …/versions/{id}` + per-blob `GET …/bundles/{id}`) and verifies each blob's `sha256 == object_id`.
//! The `current` pointer is unsigned; the engine scope-checks it and re-verifies the fetched bytes against
//! the content-addressed `version_id` on apply. Status mapping ([`classify`]), version
//! assembly ([`build_fetched_version`]), and the envelope mappings are factored as pure functions so the
//! wire logic is unit-tested without a live server; the full loopback round-trips live in the `tests/`
//! member.
//!
//! **Ids are validated at this boundary.** Every skill/workspace id a response carries (the redeem's
//! workspace id, the bootstrap's offered skills) is parsed through [`crate::id`] before it is returned —
//! a plane-chosen `"../../x"` fails here as a malformed response, never reaching a path join or a URL
//! splice.
//!
//! The client stays **sync + tokio-free**: `ureq` brings its own blocking TLS stack, so this adds no
//! `plane-store`/`sqlx`/`tokio` edge (`check-arch` holds the line).

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use topos_core::digest::{self, FileMode, to_hex};
use topos_types::requests::{
    AdminClaimRequest, DeviceAuthorizeRequest, DeviceAuthorizeResponse, DeviceTokenRequest,
    DeviceTokenResponse, DeviceTokenStatus, InvitationData, InvitationRequest, LoginData,
    LoginRedeemRequest, NoticeAckRequest, ProposeRequest, ProtectionSetRequest, PublishRequest,
    RedeemRequest, RedeemResponse, RevertRequest, ReviewRequest, SessionIntent, WireChannelIndex,
    WireFileMode, WireMe, WireProposalIndex, WireProposalList, WireProtocolCard, WireReach,
    WireSkillIndex, WireSkillLog, WireVersionMeta,
};
use topos_types::{BootstrapData, JsonEnvelope, TerminalOutcome, WireCurrentRecord};

use crate::error::ClientError;
use crate::plane::{
    Card, CatalogSource, ContributeSource, DeviceAuthorize, DirectorySource, EnrollSource,
    FetchedFile, FetchedVersion, FollowContext, FollowSource, GovernanceSource, Grant,
    GrantedToken, GrantedWorkspace, KnownCurrent, LoginRedeem, LoginSeat, PlaneError, PlaneSource,
    PointerFetch, Redeem, StandupAuthorize, TokenPoll, WriteReceipt,
};

/// Fail fast establishing a connection (a dead plane must not hang the session-start sweep).
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Fail fast waiting for the response head.
const RECV_RESPONSE_TIMEOUT_SECS: u64 = 30;
/// Bound the whole body read, so a stalled or byte-trickling plane cannot hang the session-start hook
/// indefinitely. Generous: a legitimate blob near the plane's ~100 MiB cap fits at ~350 KiB/s.
const RECV_BODY_TIMEOUT_SECS: u64 = 300;
/// The read cap for any single response body — comfortably above the plane's ~100 MiB per-blob reject cap,
/// with headroom for the metadata/record JSON. `ureq`'s default `read_to_vec` caps at 10 MiB, too small.
const MAX_FETCH_BYTES: u64 = 128 * 1024 * 1024;

/// One skill's transport credential — its workspace + the **secret** WORKSPACE credential (the Bearer that
/// authenticates every read in that workspace). Keyed by skill id in [`UreqPlane`]'s map; distinct from the
/// engine's [`FollowContext`] consent state (creds live in the transport, consent in the follow seam).
#[derive(Clone)]
pub(crate) struct SkillCred {
    pub(crate) workspace_id: String,
    /// The workspace Bearer credential (shared by every skill in the workspace). **SECRET.**
    pub(crate) credential: String,
}

impl SkillCred {
    pub(crate) fn new(workspace_id: String, credential: String) -> Self {
        Self {
            workspace_id,
            credential,
        }
    }
}

// Redact the secret credential — it must never reach a log / panic message / Debug dump.
impl std::fmt::Debug for SkillCred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillCred")
            .field("workspace_id", &self.workspace_id)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// The blocking `ureq` plane transport. Holds the base URL, a per-skill credential map, and one configured
/// agent (connection-pooled, reused across requests).
pub(crate) struct UreqPlane {
    base_url: String,
    /// skill_id → the credential + workspace its reads present. Interior-mutable because the
    /// delivery-driven reconcile LEARNS a brand-new arrival's skill mid-sweep (`bind_skill`): the
    /// map is seeded from `follows.json`, which cannot name a skill this device has never held.
    /// Single-threaded by construction (a blocking CLI transport).
    creds: RefCell<HashMap<String, SkillCred>>,
    /// workspace_id → the workspace Bearer credential (the delivery/report lane's key — one call
    /// per WORKSPACE, unlike the per-skill read creds above). **SECRET values.**
    ws_creds: HashMap<String, String>,
    agent: ureq::Agent,
}

impl std::fmt::Debug for UreqPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The agent is not Debug, and the creds carry secrets — print only the safe shape.
        f.debug_struct("UreqPlane")
            .field("base_url", &self.base_url)
            .field("skills", &self.creds.borrow().len())
            .finish_non_exhaustive()
    }
}

impl UreqPlane {
    /// Build the transport: one blocking agent (rustls+ring, sane connect/recv/body timeouts,
    /// status-as-error OFF so a 304/404/5xx comes back as an inspectable status rather than an error
    /// variant) + the cred map. `base_url`'s trailing slash is trimmed so URL joins never double up.
    pub(crate) fn new(base_url: String, creds: HashMap<String, SkillCred>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            creds: RefCell::new(creds),
            ws_creds: HashMap::new(),
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }

    /// The credential a skill's reads present, if this transport knows the skill.
    fn skill_cred(&self, skill_id: &str) -> Option<SkillCred> {
        self.creds.borrow().get(skill_id).cloned()
    }

    /// Attach the per-WORKSPACE credential map (`credentials.json`) — what arms the delivery-driven
    /// reconcile ([`crate::plane::DeliverySource`]). Without it the transport still serves the
    /// per-skill reads; the sweep just has no delivery lane to drive.
    pub(crate) fn with_workspace_credentials(mut self, ws_creds: HashMap<String, String>) -> Self {
        self.ws_creds = ws_creds;
        self
    }

    /// A `GET` carrying `Authorization: Bearer <credential>` (current + versions + bundles). Returns the
    /// raw body on 2xx, [`PlaneError::NotFound`] on 404, [`PlaneError::Unreachable`] on a connect-level
    /// fault, and [`PlaneError::Unavailable`] on any other status. `url` never contains the secret (the
    /// credential is in the header), so it is safe in the error text.
    fn bearer_get(&self, url: &str, credential: &str) -> Result<Vec<u8>, PlaneError> {
        let resp = self
            .agent
            .get(url)
            .header("authorization", format!("Bearer {credential}"))
            .call()
            // A `.call()` Err is connect-level (dial/TLS/timeout before any status): the plane itself is
            // unreachable, so the sweep's circuit breaker may trip on it.
            .map_err(|e| PlaneError::Unreachable(format!("GET {url}: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => read_body(resp),
            HttpClass::NotFound => Err(PlaneError::NotFound),
            // No conditional headers are sent here, so 304 cannot occur; fold it in with other statuses.
            HttpClass::NotModified | HttpClass::Other => {
                Err(PlaneError::Unavailable(format!("GET {url}: HTTP {status}")))
            }
        }
    }
}

impl PlaneSource for UreqPlane {
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        let cred = self.skill_cred(skill_id).ok_or(PlaneError::NotFound)?;
        // The workspace + skill ids are spliced into the URL path; the credential rides the Bearer header,
        // so the URL carries no secret (safe in an error message). Refuse a non-URL-safe id defensively.
        ensure_url_safe_ids(skill_id, &cred.workspace_id)?;
        let url = format!(
            "{}/v1/workspaces/{}/skills/{}/current",
            self.base_url, cred.workspace_id, skill_id
        );
        let mut req = self
            .agent
            .get(&url)
            .header("authorization", format!("Bearer {}", cred.credential));
        if let Some(k) = known {
            // Commit-sensitive conditional GET: the quoted ETag for the generation AND the known commit id.
            req = req
                .header(
                    "if-none-match",
                    format!("\"{}.{}\"", k.generation.epoch, k.generation.seq),
                )
                .header("topos-known-version-id", to_hex(&k.version_id));
        }
        let resp = req
            .call()
            // Connect-level (see `bearer_get`) — distinguishable so the sweep breaker can trip.
            .map_err(|e| PlaneError::Unreachable(format!("get_current {skill_id}: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::NotModified => Ok(PointerFetch::NotModified),
            HttpClass::NotFound => Err(PlaneError::NotFound),
            HttpClass::Other => Err(PlaneError::Unavailable(format!(
                "get_current {skill_id}: HTTP {status}"
            ))),
            HttpClass::Ok => {
                let bytes = read_body(resp)?;
                // Transport only deserializes — the engine scope-checks the record + re-verifies the bytes.
                let rec = serde_json::from_slice::<WireCurrentRecord>(&bytes).map_err(|e| {
                    PlaneError::Malformed(format!("current record for {skill_id}: {e}"))
                })?;
                Ok(PointerFetch::Record(rec))
            }
        }
    }

    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        let cred = self.skill_cred(skill_id).ok_or(PlaneError::NotFound)?;
        // Both ids are spliced into the URL path — refuse anything outside the validated id charset
        // (defense in depth; the enrollment loaders already validated what they persisted).
        ensure_url_safe_ids(skill_id, &cred.workspace_id)?;
        let vid_hex = to_hex(&version_id);
        let meta_url = format!(
            "{}/v1/workspaces/{}/skills/{}/versions/{}",
            self.base_url, cred.workspace_id, skill_id, vid_hex
        );
        let meta_bytes = self.bearer_get(&meta_url, &cred.credential)?;
        let meta: WireVersionMeta = serde_json::from_slice(&meta_bytes)
            .map_err(|e| PlaneError::Malformed(format!("version metadata for {skill_id}: {e}")))?;
        build_fetched_version(&meta, |object_id_hex| {
            let url = format!(
                "{}/v1/workspaces/{}/skills/{}/bundles/{}",
                self.base_url, cred.workspace_id, skill_id, object_id_hex
            );
            self.bearer_get(&url, &cred.credential)
        })
    }

    fn list_open_proposals(&self, skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        // No credential for this skill ⇒ none visible (best-effort; the count never errors out a pull).
        let Some(cred) = self.skill_cred(skill_id) else {
            return Ok(Vec::new());
        };
        ensure_url_safe_ids(skill_id, &cred.workspace_id)?;
        let url = format!(
            "{}/v1/workspaces/{}/skills/{}/proposals",
            self.base_url, cred.workspace_id, skill_id
        );
        match self.bearer_get(&url, &cred.credential) {
            Ok(bytes) => {
                let list: WireProposalList = serde_json::from_slice(&bytes).map_err(|e| {
                    PlaneError::Malformed(format!("proposals list for {skill_id}: {e}"))
                })?;
                list.proposals
                    .iter()
                    .map(|p| parse_id(&p.version_id))
                    .collect()
            }
            // A 404 is the indistinguishable not-served (unknown/expired token, scope mismatch) ⇒ none visible.
            Err(PlaneError::NotFound) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

impl crate::plane::DeliverySource for UreqPlane {
    fn bind_skill(&self, workspace_id: &str, skill_id: &str) {
        // A brand-new arrival is absent from the `follows.json`-derived per-skill map; its
        // workspace credential already authenticates it (membership IS the authorization), so
        // teach the read transport the pairing before its first version fetch.
        let Some(credential) = self.ws_creds.get(workspace_id) else {
            return;
        };
        self.creds
            .borrow_mut()
            .entry(skill_id.to_owned())
            .or_insert_with(|| SkillCred::new(workspace_id.to_owned(), credential.clone()));
    }

    fn workspaces(&self) -> Vec<String> {
        let mut ws: Vec<String> = self.ws_creds.keys().cloned().collect();
        ws.sort();
        ws
    }

    fn fetch_delivery(
        &self,
        workspace_id: &str,
    ) -> Result<crate::plane::DeliverySnapshot, PlaneError> {
        let cred = self
            .ws_creds
            .get(workspace_id)
            .ok_or(PlaneError::NotFound)?;
        ensure_url_safe_ids("delivery", workspace_id)?;
        let url = format!("{}/v1/workspaces/{}/delivery", self.base_url, workspace_id);
        let body = self.bearer_get(&url, cred)?;
        let wire: topos_types::requests::WireDelivery = serde_json::from_slice(&body)
            .map_err(|e| PlaneError::Malformed(format!("delivery body: {e}")))?;
        let mut skills = Vec::with_capacity(wire.skills.len());
        for ds in wire.skills {
            skills.push(crate::plane::DeliverySkill {
                version_id: parse_id(&ds.version_id)?,
                bundle_digest: parse_id(&ds.bundle_digest)?,
                review_required: ds.protection == "reviewed",
                skill_id: ds.skill_id,
                name: ds.name,
                generation: ds.generation,
                via_channels: ds.via.channels,
                via_direct: ds.via.direct,
            });
        }
        Ok(crate::plane::DeliverySnapshot {
            skills,
            detached: wire.detached,
            excluded: wire.excluded,
            proposals_awaiting: wire.proposals_awaiting,
            notices: wire.notices,
            staleness_window_ms: wire.staleness_window_ms,
        })
    }

    fn ack_notices(&self, workspace_id: &str, ids: &[String]) -> Result<(), PlaneError> {
        let cred = self
            .ws_creds
            .get(workspace_id)
            .ok_or(PlaneError::NotFound)?;
        ensure_url_safe_ids("ack", workspace_id)?;
        let url = format!(
            "{}/v1/workspaces/{}/notices/ack",
            self.base_url, workspace_id
        );
        let body =
            serde_json::to_vec(&topos_types::requests::NoticeAckRequest { ids: ids.to_vec() })
                .map_err(|e| PlaneError::Malformed(format!("ack body: {e}")))?;
        let resp = self
            .agent
            .post(&url)
            .header("authorization", format!("Bearer {cred}"))
            .header("content-type", "application/json")
            .send(&body[..])
            .map_err(|e| PlaneError::Unreachable(format!("POST {url}: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => Ok(()),
            HttpClass::NotFound => Err(PlaneError::NotFound),
            HttpClass::NotModified | HttpClass::Other => Err(PlaneError::Unavailable(format!(
                "POST {url}: HTTP {status}"
            ))),
        }
    }

    fn report_applied(
        &self,
        workspace_id: &str,
        applied: &[(String, [u8; 32])],
    ) -> Result<(), PlaneError> {
        let cred = self
            .ws_creds
            .get(workspace_id)
            .ok_or(PlaneError::NotFound)?;
        ensure_url_safe_ids("report", workspace_id)?;
        let url = format!("{}/v1/workspaces/{}/report", self.base_url, workspace_id);
        let report = topos_types::requests::WireAppliedReport {
            schema_version: topos_types::WIRE_SCHEMA_VERSION,
            applied: applied
                .iter()
                .map(
                    |(skill_id, commit)| topos_types::requests::WireAppliedSkill {
                        skill_id: skill_id.clone(),
                        version_id: topos_core::digest::to_hex(commit),
                    },
                )
                .collect(),
        };
        let body = serde_json::to_vec(&report)
            .map_err(|e| PlaneError::Malformed(format!("report body: {e}")))?;
        let resp = self
            .agent
            .put(&url)
            .header("authorization", format!("Bearer {cred}"))
            .header("content-type", "application/json")
            .send(&body[..])
            .map_err(|e| PlaneError::Unreachable(format!("PUT {url}: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => Ok(()),
            HttpClass::NotFound => Err(PlaneError::NotFound),
            HttpClass::NotModified | HttpClass::Other => {
                Err(PlaneError::Unavailable(format!("PUT {url}: HTTP {status}")))
            }
        }
    }
}

/// The one agent configuration both transports share: status-as-error OFF (every status comes back as an
/// inspectable response; only a genuine transport/timeout/TLS fault surfaces as `Err`) + the three
/// timeouts, so neither a dead plane (connect), a silent head (recv-response), nor a stalled/trickling
/// body (recv-body) can hang the session-start hook.
fn agent_config() -> ureq::config::Config {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(CONNECT_TIMEOUT_SECS)))
        .timeout_recv_response(Some(Duration::from_secs(RECV_RESPONSE_TIMEOUT_SECS)))
        .timeout_recv_body(Some(Duration::from_secs(RECV_BODY_TIMEOUT_SECS)))
        .build()
}

/// Refuse a skill/workspace id that is not URL-path-safe before splicing it into a request path (the
/// same lowercase charset [`crate::id`] enforces at the load boundaries — this is the last-line guard).
/// The fixed message never echoes the hostile bytes.
fn ensure_url_safe_ids(skill_id: &str, workspace_id: &str) -> Result<(), PlaneError> {
    if crate::id::is_valid_id(skill_id) && crate::id::is_valid_id(workspace_id) {
        Ok(())
    } else {
        Err(PlaneError::Malformed(
            "a skill/workspace id is not a safe path segment".into(),
        ))
    }
}

/// The transport-level classification of a response status — before any body read. Factored out so the
/// 304 / 404 / 5xx / 2xx mapping is unit-tested without a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpClass {
    /// 2xx — read the body.
    Ok,
    /// 304 Not Modified (the conditional GET matched).
    NotModified,
    /// 404 — not served here (unauthorized/unknown/scope-mismatch are all the indistinguishable 404).
    NotFound,
    /// Any other status (5xx, an unexpected 3xx/4xx) — treat as transiently unavailable.
    Other,
}

/// Map an HTTP status code to its transport class (the one place the wire status set is interpreted).
fn classify(status: u16) -> HttpClass {
    match status {
        200..=299 => HttpClass::Ok,
        304 => HttpClass::NotModified,
        404 => HttpClass::NotFound,
        _ => HttpClass::Other,
    }
}

/// Read a response body up to [`MAX_FETCH_BYTES`] (the default 10 MiB cap is too small for a large blob). A
/// read fault (truncation / over-limit) is transient → [`PlaneError::Unavailable`].
fn read_body(resp: ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, PlaneError> {
    resp.into_body()
        .into_with_config()
        .limit(MAX_FETCH_BYTES)
        .read_to_vec()
        .map_err(|e| PlaneError::Unavailable(format!("read body: {e}")))
}

/// Assemble a [`FetchedVersion`] from a version's metadata + a blob-fetching closure, re-verifying every
/// blob's `sha256 == object_id`. **Pure** (the closure abstracts the transport), so the happy path, the
/// sha256-mismatch → `Malformed`, and the bad-hex → `Malformed` are all unit-testable with canned bytes.
///
/// The engine re-derives `version_id` from `(parents, tree, author, message)` and the bundle digest on top
/// of this, so a lying metadata frame still fails there; this gate catches a blob whose bytes don't match
/// the id the metadata named.
fn build_fetched_version(
    meta: &WireVersionMeta,
    mut fetch_blob: impl FnMut(&str) -> Result<Vec<u8>, PlaneError>,
) -> Result<FetchedVersion, PlaneError> {
    let mut parents = Vec::with_capacity(meta.parents.len());
    for parent in &meta.parents {
        parents.push(parse_id(parent)?);
    }
    let mut files = Vec::with_capacity(meta.files.len());
    for f in &meta.files {
        let want = parse_id(&f.object_id)?;
        let bytes = fetch_blob(&f.object_id)?;
        if digest::sha256(&bytes) != want {
            return Err(PlaneError::Malformed(format!(
                "blob {} does not match its content id (sha256 mismatch)",
                f.object_id
            )));
        }
        files.push(FetchedFile {
            path: f.path.clone(),
            mode: domain_mode(f.mode),
            bytes,
        });
    }
    Ok(FetchedVersion {
        parents,
        author: meta.author.clone(),
        message: meta.message.clone(),
        files,
    })
}

/// A 64-char lowercase-hex id → 32 bytes, via the shared lowercase-strict codec; any other shape is a
/// [`PlaneError::Malformed`] (a forged/garbled metadata field, not a transient fault).
fn parse_id(hex: &str) -> Result<[u8; 32], PlaneError> {
    crate::ops::parse_hex32(hex)
        .map_err(|_| PlaneError::Malformed(format!("malformed 32-byte id: {hex}")))
}

/// The wire file mode → the kernel file mode (1:1), client-side.
fn domain_mode(mode: WireFileMode) -> FileMode {
    match mode {
        WireFileMode::Regular => FileMode::Regular,
        WireFileMode::Executable => FileMode::Executable,
    }
}

/// The on-disk [`FollowSource`]: the follow-state read from `follows.json` (mapped to consent contexts by
/// [`crate::enroll::follow_contexts`]). The open-proposal count is sourced from the plane (the proposals
/// read route), not the follow-state, so this carries only the followed set.
#[derive(Debug)]
pub(crate) struct FileFollow {
    entries: Vec<(String, FollowContext)>,
}

impl FileFollow {
    pub(crate) fn new(entries: Vec<(String, FollowContext)>) -> Self {
        Self { entries }
    }
}

impl FollowSource for FileFollow {
    fn followed(&self) -> Vec<(String, FollowContext)> {
        self.entries.clone()
    }
}

// =================================================================================================
// UreqDeviceClient — the real DEVICE transport (sibling of the read-credentialed `UreqPlane`). One client
// speaks every route a device drives: the UNAUTHENTICATED enrollment flow (`GET /i/{token}`,
// `POST /v1/device/authorize`, `POST /v1/device/token`, the redeem `POST /v1/workspaces/{ws}/devices`,
// `POST /v1/admin-claim`), and the WORKSPACE-CREDENTIAL routes — the governance Invite POST, the four
// contribute writes (publish / propose / revert / review), and the workspace-catalog GET (`list --remote`)
// — each of which rides `Authorization: Bearer <workspace credential>` looked up by the request's
// `workspace_id` in the client's credential map; every terminal protocol outcome of a write comes back as
// the all-outcome **200 envelope**. The `/i/{token}` URL, the device code, the grant, and the credentials
// are sensitive — never logged or put in an error.
// =================================================================================================

/// The blocking `ureq` device transport (`EnrollSource` + `GovernanceSource` + `ContributeSource` +
/// `CatalogSource`). Holds the base URL, one configured agent, and a per-workspace credential map
/// (`workspace_id → credential`) the write/governance/catalog routes present as Bearer. Enrollment starts
/// unauthenticated (its routes never touch the map); the redeem mints the credential the caller then stores.
pub(crate) struct UreqDeviceClient {
    base_url: String,
    /// `workspace_id → workspace credential` — the Bearer the write/governance/catalog routes present.
    /// **Each value is a SECRET** (redacted in `Debug`); empty for an enrollment-only client.
    creds: HashMap<String, String>,
    agent: ureq::Agent,
}

impl std::fmt::Debug for UreqDeviceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The agent is not Debug; the credentials are secret — print only the safe shape (count, not keys).
        f.debug_struct("UreqDeviceClient")
            .field("base_url", &self.base_url)
            .field("workspaces", &self.creds.len())
            .finish_non_exhaustive()
    }
}

impl UreqDeviceClient {
    /// Build the transport against `base_url` (trailing slash trimmed) with a per-workspace credential map,
    /// over the same agent configuration as [`UreqPlane`] (status-as-error OFF + the connect/recv/body
    /// timeouts). Enrollment-only callers pass an empty map (those routes are unauthenticated).
    pub(crate) fn new(base_url: String, creds: HashMap<String, String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            creds,
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }

    /// The workspace's Bearer credential, or a typed "not enrolled in this workspace" error (mirroring the
    /// un-enrolled write refusal — a request without a credential must never reach the plane).
    fn credential_for(&self, workspace_id: &str) -> Result<&str, ClientError> {
        self.creds
            .get(workspace_id)
            .map(String::as_str)
            .ok_or_else(|| {
                ClientError::Enrollment(
                    "not enrolled in this workspace; run `topos follow <link>` first".into(),
                )
            })
    }

    /// POST a JSON body UNAUTHENTICATED (the enrollment routes). See [`Self::post_json_auth`] for the
    /// Bearer-credentialed variant the write/governance routes use.
    fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        what: &str,
    ) -> Result<(u16, Vec<u8>), ClientError> {
        self.post_json_inner(url, None, body, what)
    }

    /// POST a JSON body carrying `Authorization: Bearer <credential>` (the write/governance routes).
    fn post_json_auth(
        &self,
        url: &str,
        credential: &str,
        body: &serde_json::Value,
        what: &str,
    ) -> Result<(u16, Vec<u8>), ClientError> {
        self.post_json_inner(url, Some(credential), body, what)
    }

    /// POST a JSON body, optionally Bearer-credentialed. Returns `(status, body bytes)`. `what` names the
    /// step for a transport-fault message; the body + credential are NEVER echoed (they may hold a secret).
    fn post_json_inner(
        &self,
        url: &str,
        credential: Option<&str>,
        body: &serde_json::Value,
        what: &str,
    ) -> Result<(u16, Vec<u8>), ClientError> {
        // Serialize ourselves + `send` the bytes (the `ureq` `json` feature is not enabled — this keeps the
        // dependency surface unchanged). The body may carry a secret (the device code / grant), so a serde
        // failure message never echoes it.
        let payload = serde_json::to_vec(body).map_err(|_| {
            ClientError::Corrupt(format!("{what}: could not serialize the request"))
        })?;
        let mut req = self
            .agent
            .post(url)
            .header("content-type", "application/json");
        if let Some(cred) = credential {
            req = req.header("authorization", format!("Bearer {cred}"));
        }
        let resp = req
            .send(payload.as_slice())
            .map_err(|e| ClientError::Plane(format!("{what}: {e}")))?;
        let status = resp.status().as_u16();
        let bytes = read_body(resp).map_err(plane_err)?;
        Ok((status, bytes))
    }

    /// POST `/v1/device/authorize` (enroll or standup — the body decides) and parse the typed response.
    /// `what` names the step for the transport-fault message.
    fn post_device_authorize(
        &self,
        req: DeviceAuthorizeRequest,
        what: &str,
    ) -> Result<DeviceAuthorizeResponse, ClientError> {
        let body = serde_json::to_value(req)
            .map_err(|e| ClientError::Corrupt(format!("authorize body: {e}")))?;
        let url = format!("{}/v1/device/authorize", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, what)?;
        if classify(status) != HttpClass::Ok {
            return Err(ClientError::Plane(format!("{what}: HTTP {status}")));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| ClientError::WireInvalid(format!("authorize response is malformed: {e}")))
    }

    /// POST a contribute write under the workspace's Bearer credential and map the all-outcome **200
    /// envelope** to a [`WriteReceipt`]. The four verbs differ only by `path` + body type; the op kind is
    /// derived from the route server-side, and the acting device is the credential's registry row (never a
    /// body field). A missing credential for `workspace_id` is refused BEFORE any send.
    fn post_write<T: serde::Serialize>(
        &self,
        path: &str,
        workspace_id: &str,
        body: &T,
        what: &str,
    ) -> Result<WriteReceipt, ClientError> {
        let value = serde_json::to_value(body)
            .map_err(|e| ClientError::Corrupt(format!("{what} body: {e}")))?;
        let credential = self.credential_for(workspace_id)?;
        let url = format!("{}{path}", self.base_url);
        let (status, bytes) = self.post_json_auth(&url, credential, &value, what)?;
        map_write_envelope(status, &bytes)
    }
}

impl EnrollSource for UreqDeviceClient {
    fn fetch_bootstrap(&self, token: &str) -> Result<BootstrapData, ClientError> {
        // The `/i/{token}` URL is SECRET (the token grants the bootstrap read) — error text names no URL.
        let url = format!("{}/i/{}", self.base_url, token);
        // Ask for the machine contract EXPLICITLY: the route content-negotiates, and anything not asking
        // for JSON is served the agent-instruction markdown instead (ureq's default Accept is `*/*`).
        let resp = self
            .agent
            .get(&url)
            .header("Accept", "application/json")
            .call()
            .map_err(|e| ClientError::Plane(format!("fetch invite bootstrap: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => {
                let bytes = read_body(resp).map_err(plane_err)?;
                parse_bootstrap(&bytes)
            }
            HttpClass::NotFound => Err(ClientError::Plane(
                "the invite link is invalid or has expired".into(),
            )),
            HttpClass::NotModified | HttpClass::Other => Err(ClientError::Plane(format!(
                "fetch invite bootstrap: HTTP {status}"
            ))),
        }
    }

    fn device_authorize(
        &self,
        workspace: &str,
        device_public_key: [u8; 32],
        machine_name: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        // The enroll-intent start names the workspace by its ADDRESS; the address-follow dispatch is a
        // marked seam in `ops::follow`, so this reshaped body compiles ahead of that leg wiring it.
        let resp = self.post_device_authorize(
            DeviceAuthorizeRequest {
                workspace: Some(workspace.to_owned()),
                intent: Some(SessionIntent::Enroll),
                device_public_key: b64(&device_public_key),
                machine_name: machine_name.to_owned(),
            },
            "device authorize",
        )?;
        Ok(device_authorize_from_wire(resp))
    }

    fn device_authorize_standup(
        &self,
        device_public_key: [u8; 32],
        machine_name: &str,
    ) -> Result<StandupAuthorize, ClientError> {
        let resp = self.post_device_authorize(
            DeviceAuthorizeRequest {
                workspace: None,
                intent: Some(SessionIntent::Standup),
                device_public_key: b64(&device_public_key),
                machine_name: machine_name.to_owned(),
            },
            "standup authorize",
        )?;
        // The plane block declares the API base a standup device dials; a response without it is unusable.
        let plane = resp.plane.clone().ok_or_else(|| {
            ClientError::WireInvalid("a standup authorize carried no plane block".into())
        })?;
        Ok(StandupAuthorize {
            auth: device_authorize_from_wire(resp),
            plane,
        })
    }

    fn fetch_card(&self, url: &str) -> Result<Card, ClientError> {
        // The URL is a pasted address, but it MAY be an `/i/` claim link (whose token is a bearer
        // secret) — so no error on this path ever echoes the URL.
        let resp = self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .call()
            .map_err(|e| ClientError::Plane(format!("fetch protocol card: {e}")))?;
        let status = resp.status().as_u16();
        if classify(status) != HttpClass::Ok {
            return Err(ClientError::Plane(format!(
                "fetch protocol card: HTTP {status} — the address did not answer the topos \
                 protocol card"
            )));
        }
        let bytes = read_body(resp).map_err(plane_err)?;
        parse_card(&bytes)
    }

    fn device_authorize_login(
        &self,
        device_public_key: [u8; 32],
        machine_name: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        let resp = self.post_device_authorize(
            DeviceAuthorizeRequest {
                workspace: None,
                intent: Some(SessionIntent::Login),
                device_public_key: b64(&device_public_key),
                machine_name: machine_name.to_owned(),
            },
            "login authorize",
        )?;
        Ok(device_authorize_from_wire(resp))
    }

    fn login_redeem(
        &self,
        grant: &str,
        device_public_key: [u8; 32],
    ) -> Result<LoginRedeem, ClientError> {
        let body = serde_json::to_value(LoginRedeemRequest {
            grant: grant.to_owned(),
            device_public_key: b64(&device_public_key),
        })
        .map_err(|_| ClientError::Corrupt("login body: could not serialize".to_owned()))?;
        let url = format!("{}/v1/login", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, "login")?;
        map_login_envelope(status, &bytes)
    }

    fn poll_token(&self, device_code: &str) -> Result<TokenPoll, ClientError> {
        let body = serde_json::to_value(DeviceTokenRequest {
            device_code: device_code.to_owned(),
        })
        .map_err(|e| ClientError::Corrupt(format!("token body: {e}")))?;
        let url = format!("{}/v1/device/token", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, "device token poll")?;
        if classify(status) != HttpClass::Ok {
            return Err(ClientError::Plane(format!(
                "device token poll: HTTP {status}"
            )));
        }
        let resp: DeviceTokenResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ClientError::WireInvalid(format!("token response is malformed: {e}")))?;
        Ok(match resp.status {
            DeviceTokenStatus::Pending => TokenPoll::Pending,
            DeviceTokenStatus::SlowDown => TokenPoll::SlowDown,
            DeviceTokenStatus::Denied => TokenPoll::Denied,
            DeviceTokenStatus::Expired => TokenPoll::Expired,
            DeviceTokenStatus::Granted => match resp.grant {
                Some(g) => {
                    // The workspace context (a standup grant's disclosure) rides alongside; its id later
                    // keys the redeem URL + user.json, so it is validated at this wire boundary.
                    let workspace = match resp.workspace {
                        Some(w) => {
                            crate::id::validate_workspace_id(&w.workspace_id)
                                .map_err(crate::id::wire_flavor)?;
                            Some(GrantedWorkspace {
                                workspace_id: w.workspace_id,
                                display_name: w.display_name,
                                address: w.address,
                            })
                        }
                        None => None,
                    };
                    TokenPoll::Granted(GrantedToken {
                        grant: Grant::new(g),
                        workspace,
                    })
                }
                // `granted` without a grant is a malformed response, not a silent re-poll.
                None => {
                    return Err(ClientError::WireInvalid(
                        "a granted device-token poll carried no grant".into(),
                    ));
                }
            },
        })
    }

    fn redeem(
        &self,
        workspace_id: &str,
        grant: &str,
        device_public_key: [u8; 32],
    ) -> Result<Redeem, ClientError> {
        // The workspace id is spliced into the URL path below — validate before building the request
        // (it entered via the bootstrap, which already validated; this is the last-line guard).
        crate::id::validate_workspace_id(workspace_id).map_err(crate::id::wire_flavor)?;
        let body = serde_json::to_value(RedeemRequest {
            workspace_id: workspace_id.to_owned(),
            grant: grant.to_owned(),
            device_public_key: b64(&device_public_key),
        })
        .map_err(|e| ClientError::Corrupt(format!("redeem body: {e}")))?;
        let url = format!("{}/v1/workspaces/{}/devices", self.base_url, workspace_id);
        let (status, bytes) = self.post_json(&url, &body, "redeem")?;
        map_redeem_envelope(RedeemKind::Grant, status, &bytes)
    }

    fn admin_claim(
        &self,
        claim_token: &str,
        device_public_key: [u8; 32],
        display_name: &str,
    ) -> Result<Redeem, ClientError> {
        // The claim token is the bearer capability; the display name is disclosure-only (the seated name
        // comes from the mint-time claim row). The token is a SECRET — it rides the body, never a URL or an
        // error message.
        let body = serde_json::to_value(AdminClaimRequest {
            claim_token: claim_token.to_owned(),
            device_public_key: b64(&device_public_key),
            display_name: display_name.to_owned(),
        })
        .map_err(|_| ClientError::Corrupt("claim body: could not serialize".to_owned()))?;
        let url = format!("{}/v1/admin-claim", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, "admin claim")?;
        map_redeem_envelope(RedeemKind::Claim, status, &bytes)
    }
}

/// Which redeem-shaped envelope is being mapped — the two doors carry the SAME wire shape but deserve
/// different typed denials: a grant redeem's DENIED is authenticated-but-uninvited (ask an owner), a claim
/// redeem's DENIED means the one-time claim is dead (consumed elsewhere / expired / workspace exists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedeemKind {
    Grant,
    Claim,
}

/// Map a redeem / admin-claim response — the all-outcome **200 envelope** — to the typed [`Redeem`],
/// validating the workspace id the plane echoed (it becomes a URL segment) — a traversal-shaped id fails
/// the whole redeem as malformed. The plaintext workspace `credential` rides in `data`. **Pure** (status +
/// bytes in), so the ok / denied / hostile-id arms are unit-tested without a socket (mirrors
/// [`map_invite_envelope`]).
fn map_redeem_envelope(kind: RedeemKind, status: u16, bytes: &[u8]) -> Result<Redeem, ClientError> {
    // The redeem is an all-outcome 200 envelope; a non-2xx is a transport/auth/integrity fault.
    if classify(status) != HttpClass::Ok {
        return Err(ClientError::Plane(format!("redeem: HTTP {status}")));
    }
    let env: JsonEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("redeem envelope is malformed: {e}")))?;
    if !env.ok {
        // A DENIED redeem — surface a typed error, never any secret. The plane's denial is deliberately
        // uniform, so the guidance comes from WHICH door was knocked on, not from the (static) code.
        let code = env
            .error
            .map(|e| e.code)
            .unwrap_or_else(|| "DENIED".to_owned());
        return Err(match kind {
            // Authenticated-but-uninvited on a hosted plane: the ask-an-owner guidance + REQUEST_ACCESS.
            RedeemKind::Grant => ClientError::RedeemDenied { code },
            // A dead one-time claim: consumed by ANOTHER device (a same-device retry replays Redeemed),
            // expired, or the workspace already exists — ask the operator for a fresh claim link.
            RedeemKind::Claim => ClientError::Enrollment(
                "the claim link was refused — it may be consumed, expired, or the workspace may \
                 already exist; ask the plane operator for a fresh claim link"
                    .into(),
            ),
        });
    }
    let resp: RedeemResponse = serde_json::from_value(env.data)
        .map_err(|e| ClientError::WireInvalid(format!("redeem data is malformed: {e}")))?;
    // The wire boundary: the echoed workspace id becomes a URL segment + the credential lookup key, so it
    // must be a safe path component (a traversal id is the corrupt family's WIRE flavor).
    crate::id::validate_workspace_id(&resp.workspace_id).map_err(crate::id::wire_flavor)?;
    Ok(Redeem {
        workspace_id: resp.workspace_id,
        device_key_id: resp.device_key_id,
        principal: resp.principal,
        credential: resp.credential,
    })
}

/// Map the wire [`DeviceAuthorizeResponse`] to the transport-level [`DeviceAuthorize`] (drops the standup
/// plane block, which [`UreqDeviceClient::device_authorize_standup`] extracts separately).
fn device_authorize_from_wire(resp: DeviceAuthorizeResponse) -> DeviceAuthorize {
    DeviceAuthorize {
        device_code: resp.device_code,
        user_code: resp.user_code,
        verification_uri: resp.verification_uri,
        verification_uri_complete: resp.verification_uri_complete,
        expires_in: resp.expires_in,
        interval: resp.interval,
    }
}

/// Parse + validate the `/i/` bootstrap body: the serde decode, then the id boundary — the workspace id
/// and every offered skill id must be safe path/URL segments (they persist into the WAL and later key path
/// joins). **Pure**, unit-tested with canned JSON.
fn parse_bootstrap(bytes: &[u8]) -> Result<BootstrapData, ClientError> {
    let bootstrap: BootstrapData = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("invite bootstrap is malformed: {e}")))?;
    crate::id::validate_workspace_id(&bootstrap.workspace.workspace_id)
        .map_err(crate::id::wire_flavor)?;
    for s in &bootstrap.offered_skills {
        crate::id::SkillId::parse(&s.skill_id).map_err(crate::id::wire_flavor)?;
    }
    Ok(bootstrap)
}

// =================================================================================================
// The governance-write side of `UreqDeviceClient` — the invitation roster-write under the workspace Bearer
// credential (the acting device is the credential's registry row); mirrors the write 200 envelope mapping.
// The workspace id rides the URL path; the body carries only the emails + channel pre-placements.
// =================================================================================================

impl GovernanceSource for UreqDeviceClient {
    fn invite(
        &self,
        workspace_id: &str,
        body: InvitationRequest,
    ) -> Result<InvitationData, ClientError> {
        // The workspace id is spliced into the URL path — refuse anything outside the validated charset.
        crate::id::validate_workspace_id(workspace_id).map_err(crate::id::wire_flavor)?;
        let value = serde_json::to_value(&body)
            .map_err(|e| ClientError::Corrupt(format!("invite body: {e}")))?;
        let credential = self.credential_for(workspace_id)?;
        let url = format!(
            "{}/v1/workspaces/{}/invitations",
            self.base_url, workspace_id
        );
        let (status, bytes) = self.post_json_auth(&url, credential, &value, "create invitation")?;
        map_invite_envelope(status, &bytes)
    }

    fn revoke_device(
        &self,
        workspace_id: &str,
        target_device_key_id: &str,
        op_id: &str,
    ) -> Result<(), ClientError> {
        crate::id::validate_workspace_id(workspace_id).map_err(crate::id::wire_flavor)?;
        let credential = self.credential_for(workspace_id)?;
        let body = serde_json::to_vec(&topos_types::requests::DeviceRevokeRequest {
            workspace_id: workspace_id.to_owned(),
            op_id: op_id.to_owned(),
            target_device_key_id: target_device_key_id.to_owned(),
        })
        .map_err(|e| ClientError::Corrupt(format!("revoke body: {e}")))?;
        let url = format!("{}/v1/workspaces/{}/devices", self.base_url, workspace_id);
        // A DELETE with a JSON body (the governance op names its target there) — `force_send_body`
        // converts the bodyless builder.
        let resp = self
            .agent
            .delete(&url)
            .header("authorization", format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .force_send_body()
            .send(&body[..])
            .map_err(|e| ClientError::Plane(format!("device revoke: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => {
                let bytes = read_body(resp).map_err(plane_err)?;
                map_row_envelope(&bytes)
            }
            HttpClass::NotFound => Err(ClientError::TargetNotFound {
                target: workspace_id.to_owned(),
            }),
            HttpClass::NotModified | HttpClass::Other => {
                Err(ClientError::Plane(format!("device revoke: HTTP {status}")))
            }
        }
    }
}

/// Map an invitation response — the all-outcome **200 envelope** — to the typed result. A non-200 is a
/// transport/auth/integrity fault; `ok` carries the [`InvitationData`]; `!ok` is a typed DENIED error
/// carrying the wire error's code (never a secret). **Pure** (status + bytes in), so the ok / denied /
/// non-200 / malformed arms are all unit-tested without a socket (mirrors [`build_fetched_version`]).
fn map_invite_envelope(status: u16, bytes: &[u8]) -> Result<InvitationData, ClientError> {
    if classify(status) != HttpClass::Ok {
        return Err(ClientError::Plane(format!(
            "create invitation: HTTP {status}"
        )));
    }
    let env: JsonEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("invitation envelope is malformed: {e}")))?;
    if !env.ok {
        // A DENIED invitation (e.g. the workspace restricts inviting to owners) — surface the code.
        let code = env
            .error
            .map(|e| e.code)
            .unwrap_or_else(|| "DENIED".to_owned());
        return Err(ClientError::Plane(format!("invite refused ({code})")));
    }
    serde_json::from_value(env.data)
        .map_err(|e| ClientError::WireInvalid(format!("invitation data is malformed: {e}")))
}

// =================================================================================================
// The contribute-write side of `UreqDeviceClient` — the publish / propose / revert / review POSTs (each
// under the workspace Bearer credential; the acting device is the credential's registry row). UNLIKE
// `map_invite_envelope`, a `!ok` body is NOT an error: CONFLICT / APPROVAL_REQUIRED / DENIED are terminal
// protocol outcomes the verb branches on (carrying `current_generation` + `next_actions`).
// =================================================================================================

impl ContributeSource for UreqDeviceClient {
    fn publish(&self, body: PublishRequest) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/publish", &body.workspace_id, &body, "publish")
    }
    fn propose(&self, body: ProposeRequest) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/proposals", &body.workspace_id, &body, "propose")
    }
    fn revert(&self, body: RevertRequest) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/reverts", &body.workspace_id, &body, "revert")
    }
    fn review(&self, body: ReviewRequest) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/reviews", &body.workspace_id, &body, "review")
    }
}

// =================================================================================================
// The catalog-read side of `UreqDeviceClient` — the workspace-catalog GET (`list --remote`). Authorized by
// the workspace's Bearer credential (catalog visibility == workspace membership); metadata only, no bytes.
// A workspace with no stored credential ⇒ Unavailable (degraded to a per-workspace warning); a 404 (not a
// member / no such workspace) is the indistinguishable "no catalog" ⇒ mapped to an EMPTY index, so a caller
// sweeping several workspaces degrades cleanly rather than erroring.
// =================================================================================================

impl CatalogSource for UreqDeviceClient {
    fn fetch_catalog(&self, workspace_id: &str) -> Result<WireSkillIndex, PlaneError> {
        // The workspace id is spliced into the URL path — refuse anything outside the validated charset
        // (defense in depth; the enrollment loaders already validated what they persisted). The fixed
        // message never echoes the hostile bytes.
        if !crate::id::is_valid_id(workspace_id) {
            return Err(PlaneError::Malformed(
                "a workspace id is not a safe path segment".into(),
            ));
        }
        // No stored credential for this workspace ⇒ Unavailable (the caller degrades it to a warning), never
        // a request without a credential.
        let Some(credential) = self.creds.get(workspace_id) else {
            return Err(PlaneError::Unavailable(format!(
                "not enrolled in workspace {workspace_id}"
            )));
        };
        let url = format!("{}/v1/workspaces/{}/skills", self.base_url, workspace_id);
        // The read is authorized by the workspace Bearer credential (resolved to a confirmed-member row);
        // the credential rides the header, so the URL carries no secret — safe in an error message.
        let resp = self
            .agent
            .get(&url)
            .header("authorization", format!("Bearer {credential}"))
            .call()
            // A `.call()` Err is connect-level (dial/TLS/timeout before any status): the plane itself is
            // unreachable — surfaced distinctly (the caller degrades it to a per-workspace warning).
            .map_err(|e| PlaneError::Unreachable(format!("fetch catalog {workspace_id}: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => {
                let bytes = read_body(resp)?;
                serde_json::from_slice::<WireSkillIndex>(&bytes)
                    .map_err(|e| PlaneError::Malformed(format!("catalog for {workspace_id}: {e}")))
            }
            // 404 = not a member / no such workspace (the indistinguishable "no catalog") ⇒ an empty index.
            HttpClass::NotFound => Ok(WireSkillIndex { skills: Vec::new() }),
            // No conditional headers are sent, so 304 cannot occur; fold it in with the other statuses.
            HttpClass::NotModified | HttpClass::Other => Err(PlaneError::Unavailable(format!(
                "fetch catalog {workspace_id}: HTTP {status}"
            ))),
        }
    }
}

// =================================================================================================
// The directory side of `UreqDeviceClient` — the member-scoped describe reads + the person/device
// row ops (subscription / curation / protection / notices), each under the workspace Bearer
// credential looked up by `workspace_id`. Reads map the uniform 404 to the ONE not-found error;
// row ops map the all-outcome 200 envelope LENIENTLY (`ok: true` — or a non-envelope 2xx body — is
// success; `ok: false` is the typed refusal carrying the wire error's code/outcome verbatim).
// =================================================================================================

/// The HTTP method a directory row op rides (the routes are REST-shaped: PUT creates/asserts the
/// row, DELETE removes it, POST carries a batch body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowMethod {
    Put,
    Delete,
    Post,
}

impl UreqDeviceClient {
    /// A member-scoped typed GET under the workspace Bearer credential. `what` names the step for a
    /// transport-fault message; `target` is the user-facing token the uniform not-found echoes.
    fn get_typed<T: serde::de::DeserializeOwned>(
        &self,
        workspace_id: &str,
        path: &str,
        what: &str,
        target: &str,
    ) -> Result<T, ClientError> {
        let credential = self.credential_for(workspace_id)?;
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .agent
            .get(&url)
            .header("authorization", format!("Bearer {credential}"))
            .call()
            .map_err(|e| ClientError::Plane(format!("{what}: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => {
                let bytes = read_body(resp).map_err(plane_err)?;
                serde_json::from_slice(&bytes)
                    .map_err(|e| ClientError::WireInvalid(format!("{what} body is malformed: {e}")))
            }
            // The plane's pre-gate miss is deliberately uniform (no existence signal); mirror it.
            HttpClass::NotFound => Err(ClientError::TargetNotFound {
                target: target.to_owned(),
            }),
            HttpClass::NotModified | HttpClass::Other => {
                Err(ClientError::Plane(format!("{what}: HTTP {status}")))
            }
        }
    }

    /// One directory ROW OP: send under the workspace Bearer credential, map the response. A 404 is
    /// the uniform not-found (echoing `target`); a definitive 4xx drops through as
    /// [`ClientError::PlaneRejected`]; a 2xx maps through [`map_row_envelope`].
    fn row_op(
        &self,
        method: RowMethod,
        workspace_id: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        what: &str,
        target: &str,
    ) -> Result<(), ClientError> {
        let credential = self.credential_for(workspace_id)?;
        let url = format!("{}{path}", self.base_url);
        let auth = format!("Bearer {credential}");
        let send = |req: ureq::RequestBuilder<ureq::typestate::WithBody>| match body {
            Some(v) => {
                // The body carries no secret (row ops name resources, never credentials), but a
                // serialize failure message still never echoes it.
                let payload = serde_json::to_vec(v).map_err(|_| {
                    ClientError::Corrupt(format!("{what}: could not serialize the request"))
                })?;
                req.header("content-type", "application/json")
                    .send(payload.as_slice())
                    .map_err(|e| ClientError::Plane(format!("{what}: {e}")))
            }
            None => req
                .send_empty()
                .map_err(|e| ClientError::Plane(format!("{what}: {e}"))),
        };
        let resp = match method {
            RowMethod::Put => send(self.agent.put(&url).header("authorization", &auth))?,
            RowMethod::Post => send(self.agent.post(&url).header("authorization", &auth))?,
            // A row DELETE carries no body (the row IS the path).
            RowMethod::Delete => self
                .agent
                .delete(&url)
                .header("authorization", &auth)
                .call()
                .map_err(|e| ClientError::Plane(format!("{what}: {e}")))?,
        };
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => {
                let bytes = read_body(resp).map_err(plane_err)?;
                map_row_envelope(&bytes)
            }
            HttpClass::NotFound => Err(ClientError::TargetNotFound {
                target: target.to_owned(),
            }),
            HttpClass::NotModified | HttpClass::Other => {
                // Mirror the contribute writes: a definitive 4xx (≠429) provably did not land.
                Err(if (400..500).contains(&status) && status != 429 {
                    ClientError::PlaneRejected(status)
                } else {
                    ClientError::Plane(format!("{what}: HTTP {status}"))
                })
            }
        }
    }
}

/// Map a directory row-op 2xx body — the standard all-outcome **200 envelope** — LENIENTLY: a body
/// that is not an envelope (or is empty) still counts as success (the status said the row landed;
/// the envelope is the richer shape, not a requirement), `ok: true` is success, and `ok: false` is
/// the typed refusal carrying the wire error's `code`/`outcome`/`retryable` verbatim so the verb
/// (and the agent) branch on the TRUE refusal. **Pure**, unit-tested with canned bytes.
fn map_row_envelope(bytes: &[u8]) -> Result<(), ClientError> {
    let Ok(env) = serde_json::from_slice::<JsonEnvelope>(bytes) else {
        return Ok(());
    };
    if env.ok {
        return Ok(());
    }
    Err(match env.error {
        Some(e) => ClientError::PlaneTerminal {
            outcome: e.outcome,
            code: e.code,
            retryable: e.retryable,
        },
        // An ok:false envelope without an error block still refuses — closed, with the generic code.
        None => ClientError::PlaneTerminal {
            outcome: TerminalOutcome::Denied,
            code: "DENIED".to_owned(),
            retryable: false,
        },
    })
}

/// Refuse a channel name that is not URL-path-safe before splicing it into a request path (channels
/// are addressed by NAME — user-chosen, so this boundary is stricter than trusting the argv). The
/// fixed message never echoes the hostile bytes.
fn ensure_url_safe_channel(name: &str) -> Result<(), ClientError> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(ClientError::InvalidArgument(
            "the channel name is not a safe address segment (letters, digits, '-', '_', '.')"
                .into(),
        ))
    }
}

/// The skill/workspace id gate in the [`ClientError`] flavor (the [`PlaneError`] twin is
/// [`ensure_url_safe_ids`]) — the directory routes splice both into paths.
fn ensure_safe_ids_client(skill_id: &str, workspace_id: &str) -> Result<(), ClientError> {
    ensure_url_safe_ids(skill_id, workspace_id).map_err(plane_err)
}

impl DirectorySource for UreqDeviceClient {
    fn me(&self, workspace_id: &str) -> Result<WireMe, ClientError> {
        ensure_safe_ids_client("me", workspace_id)?;
        self.get_typed(
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/me"),
            "membership describe",
            workspace_id,
        )
    }

    fn channels_index(&self, workspace_id: &str) -> Result<WireChannelIndex, ClientError> {
        ensure_safe_ids_client("channels", workspace_id)?;
        self.get_typed(
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/channels"),
            "channel index",
            workspace_id,
        )
    }

    fn skills_index(&self, workspace_id: &str) -> Result<WireSkillIndex, ClientError> {
        ensure_safe_ids_client("skills", workspace_id)?;
        self.get_typed(
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/skills"),
            "skill catalog",
            workspace_id,
        )
    }

    fn proposals_index(&self, workspace_id: &str) -> Result<WireProposalIndex, ClientError> {
        ensure_safe_ids_client("proposals", workspace_id)?;
        self.get_typed(
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/proposals"),
            "proposal index",
            workspace_id,
        )
    }

    fn skill_log(&self, workspace_id: &str, skill_id: &str) -> Result<WireSkillLog, ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        self.get_typed(
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/skills/{skill_id}/log"),
            "skill log",
            skill_id,
        )
    }

    fn reach(&self, workspace_id: &str, skill_id: &str) -> Result<WireReach, ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        self.get_typed(
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/skills/{skill_id}/reach"),
            "reach",
            skill_id,
        )
    }

    fn follow_skill(&self, workspace_id: &str, skill_id: &str) -> Result<(), ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/follows/{skill_id}"),
            None,
            "follow",
            skill_id,
        )
    }

    fn unfollow_skill(&self, workspace_id: &str, skill_id: &str) -> Result<(), ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        self.row_op(
            RowMethod::Delete,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/follows/{skill_id}"),
            None,
            "unfollow",
            skill_id,
        )
    }

    fn channel_join(&self, workspace_id: &str, channel: &str) -> Result<(), ClientError> {
        ensure_safe_ids_client("join", workspace_id)?;
        ensure_url_safe_channel(channel)?;
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/channels/{channel}/membership"),
            None,
            "channel join",
            channel,
        )
    }

    fn channel_leave(&self, workspace_id: &str, channel: &str) -> Result<(), ClientError> {
        ensure_safe_ids_client("leave", workspace_id)?;
        ensure_url_safe_channel(channel)?;
        self.row_op(
            RowMethod::Delete,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/channels/{channel}/membership"),
            None,
            "channel leave",
            channel,
        )
    }

    fn channel_place(
        &self,
        workspace_id: &str,
        channel: &str,
        skill_id: &str,
    ) -> Result<(), ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        ensure_url_safe_channel(channel)?;
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/channels/{channel}/skills/{skill_id}"),
            None,
            "channel place",
            skill_id,
        )
    }

    fn channel_unplace(
        &self,
        workspace_id: &str,
        channel: &str,
        skill_id: &str,
    ) -> Result<(), ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        ensure_url_safe_channel(channel)?;
        self.row_op(
            RowMethod::Delete,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/channels/{channel}/skills/{skill_id}"),
            None,
            "channel unplace",
            skill_id,
        )
    }

    fn exclude_device(&self, workspace_id: &str, skill_id: &str) -> Result<(), ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/exclusions/{skill_id}"),
            None,
            "exclude",
            skill_id,
        )
    }

    fn protect_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
        level: &str,
    ) -> Result<(), ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        let body = serde_json::to_value(ProtectionSetRequest {
            level: level.to_owned(),
        })
        .map_err(|e| ClientError::Corrupt(format!("protection body: {e}")))?;
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/skills/{skill_id}/protection"),
            Some(&body),
            "protect",
            skill_id,
        )
    }

    fn protect_channel(
        &self,
        workspace_id: &str,
        channel: &str,
        level: &str,
    ) -> Result<(), ClientError> {
        ensure_safe_ids_client("protect", workspace_id)?;
        ensure_url_safe_channel(channel)?;
        let body = serde_json::to_value(ProtectionSetRequest {
            level: level.to_owned(),
        })
        .map_err(|e| ClientError::Corrupt(format!("protection body: {e}")))?;
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/channels/{channel}/protection"),
            Some(&body),
            "protect",
            channel,
        )
    }

    fn ack_notices(&self, workspace_id: &str, ids: &[String]) -> Result<(), ClientError> {
        ensure_safe_ids_client("ack", workspace_id)?;
        let body = serde_json::to_value(NoticeAckRequest { ids: ids.to_vec() })
            .map_err(|e| ClientError::Corrupt(format!("ack body: {e}")))?;
        self.row_op(
            RowMethod::Post,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/notices/ack"),
            Some(&body),
            "ack notices",
            workspace_id,
        )
    }
}

/// Dispatch an unauthenticated card-read body: the constant protocol card (its `card` discriminant
/// decides), else an `/i/` claim link's bootstrap. **Pure**, unit-tested with canned bytes.
fn parse_card(bytes: &[u8]) -> Result<Card, ClientError> {
    if let Ok(card) = serde_json::from_slice::<WireProtocolCard>(bytes)
        && card.card == "topos-protocol-card"
    {
        if card.api_base_url.trim().is_empty() {
            return Err(ClientError::WireInvalid(
                "the protocol card declares no API base URL".into(),
            ));
        }
        return Ok(Card::Protocol(card));
    }
    parse_bootstrap(bytes)
        .map(|b| Card::Claim(Box::new(b)))
        .map_err(|_| {
            ClientError::WireInvalid(
                "the address answered neither a protocol card nor a claim bootstrap — is it a \
                 topos plane?"
                    .into(),
            )
        })
}

/// Map a `POST /v1/login` response to the typed [`LoginRedeem`]: the all-outcome **200 envelope**
/// (mirroring the redeem), parsed leniently — a bare `LoginData` body also lands. Every seat's
/// workspace id is validated at this wire boundary (it later keys credential lookups + URL splices).
/// **Pure** (status + bytes in), unit-tested without a socket.
fn map_login_envelope(status: u16, bytes: &[u8]) -> Result<LoginRedeem, ClientError> {
    if classify(status) != HttpClass::Ok {
        return Err(ClientError::Plane(format!("login: HTTP {status}")));
    }
    let data: LoginData = match serde_json::from_slice::<JsonEnvelope>(bytes) {
        Ok(env) => {
            if !env.ok {
                let code = env
                    .error
                    .map(|e| e.code)
                    .unwrap_or_else(|| "DENIED".to_owned());
                return Err(ClientError::Enrollment(format!(
                    "the login was refused ({code}) — complete the sign-in at the verification \
                     page, then re-run `topos auth login`"
                )));
            }
            serde_json::from_value(env.data)
                .map_err(|e| ClientError::WireInvalid(format!("login data is malformed: {e}")))?
        }
        Err(_) => serde_json::from_slice(bytes)
            .map_err(|e| ClientError::WireInvalid(format!("login response is malformed: {e}")))?,
    };
    let mut seats = Vec::with_capacity(data.memberships.len());
    for m in data.memberships {
        crate::id::validate_workspace_id(&m.workspace_id).map_err(crate::id::wire_flavor)?;
        seats.push(LoginSeat {
            workspace_id: m.workspace_id,
            name: m.name,
            display_name: m.display_name,
            role: m.role,
            device_key_id: m.device_key_id,
            credential: m.credential,
            blocked: m.blocked,
        });
    }
    Ok(LoginRedeem {
        principal: data.principal,
        seats,
    })
}

/// Map a contribute-write response — the all-outcome **200 envelope** — to a typed [`WriteReceipt`]. EVERY
/// parsed 200 is an `Ok(WriteReceipt)` (OK / NEEDS_REVIEW / CONFLICT / APPROVAL_REQUIRED / DENIED are all
/// terminal protocol outcomes the verb acts on); only a non-200 (transport/auth/integrity) or an
/// unparseable envelope is a [`ClientError`]. The `current` pointer record (unsigned — nothing signs) rides
/// `data` ONLY when a pointer
/// moved — NEEDS_REVIEW, an OK `review --reject`, and every failure carry `{}`, so it is parsed leniently
/// (`.ok()`), never assuming `outcome == Ok ⟹ data is a record`. **Pure** (status + bytes), so every arm is
/// unit-tested without a socket (mirrors [`map_invite_envelope`]).
fn map_write_envelope(status: u16, bytes: &[u8]) -> Result<WriteReceipt, ClientError> {
    if classify(status) != HttpClass::Ok {
        // A 4xx other than 429 is a DEFINITIVE rejection — the op provably did NOT land (a bad request /
        // payload-too-large), so the caller drops the op-WAL instead of replaying it forever. A 5xx / 429 /
        // timeout is AMBIGUOUS (the op may have landed) — keep the WAL for a safe same-op_id replay.
        return Err(if (400..500).contains(&status) && status != 429 {
            ClientError::PlaneRejected(status)
        } else {
            ClientError::Plane(format!("contribute write: HTTP {status}"))
        });
    }
    let env: JsonEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("write envelope is malformed: {e}")))?;
    let receipt = env
        .receipt
        .ok_or_else(|| ClientError::WireInvalid("a write 200 carried no receipt".to_owned()))?;
    // The pointer record is present ONLY when a pointer actually moved. NEEDS_REVIEW, an OK `review
    // --reject` (the plane returns OK with no record → data `{}`), and every failure carry `{}`; parse
    // leniently so a valid reject is never wrongly rejected as Corrupt.
    let wire_record = serde_json::from_value::<WireCurrentRecord>(env.data).ok();
    Ok(WriteReceipt {
        receipt,
        error: env.error,
        wire_record,
    })
}

// =================================================================================================
// UreqReleases — the real release source for `topos upgrade` (the native self-updater). Speaks the
// GitHub REST API for latest-tag resolution + raw asset GETs over the same blocking agent as the plane
// transports (its own rustls+ring stack — no new dependency edge). GitHub 403s a request without a
// `User-Agent`, so every request carries this build's. The checksum verify + atomic replace stay in the
// op; this is a dumb byte fetcher.
// =================================================================================================

/// The user-agent GitHub requires (it 403s a request without one). Carries this build's version.
const RELEASE_USER_AGENT: &str = concat!("topos/", env!("CARGO_PKG_VERSION"));

/// The blocking `ureq` release source: the GitHub API for latest-tag resolution + raw asset GETs.
pub(crate) struct UreqReleases {
    agent: ureq::Agent,
}

impl UreqReleases {
    pub(crate) fn new() -> Self {
        Self {
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }
}

impl crate::release::ReleaseSource for UreqReleases {
    fn latest_tag(&self) -> Result<String, ClientError> {
        let url = "https://api.github.com/repos/topos-sh/topos/releases/latest";
        let resp = self
            .agent
            .get(url)
            .header("User-Agent", RELEASE_USER_AGENT)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| ClientError::Plane(format!("release check: {e}")))?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(ClientError::Plane(format!(
                "release check: HTTP {status} from {url}"
            )));
        }
        let body = read_body(resp).map_err(plane_err)?;
        #[derive(serde::Deserialize)]
        struct Rel {
            tag_name: String,
        }
        let rel: Rel = serde_json::from_slice(&body)
            .map_err(|e| ClientError::Plane(format!("release check: malformed response: {e}")))?;
        Ok(rel.tag_name)
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, ClientError> {
        let resp = self
            .agent
            .get(url)
            .header("User-Agent", RELEASE_USER_AGENT)
            .call()
            .map_err(|e| ClientError::Plane(format!("download {url}: {e}")))?;
        let status = resp.status().as_u16();
        if !(200..=299).contains(&status) {
            return Err(ClientError::Plane(format!("download {url}: HTTP {status}")));
        }
        read_body(resp).map_err(plane_err)
    }
}

// =================================================================================================
// UreqGitSource — the real remote-source fetcher for `add <owner/repo>`. Downloads a repo as a `.tar.gz`
// from GitHub's API tarball endpoint (which 302-redirects to codeload; the agent follows it) over the same
// blocking agent as the other transports. GitHub 403s a UA-less request, so every call carries this
// build's. Extraction + selection + the byte-exact digest stay in the op; this is a dumb byte fetcher.
// =================================================================================================

/// The blocking `ureq` remote-source: a public repo tarball over the GitHub API.
pub(crate) struct UreqGitSource {
    agent: ureq::Agent,
}

impl UreqGitSource {
    pub(crate) fn new() -> Self {
        Self {
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }
}

impl crate::git_source::GitTarballSource for UreqGitSource {
    fn fetch(&self, spec: &crate::source::RemoteSpec) -> Result<Vec<u8>, ClientError> {
        // Only GitHub is wired today; `crate::source::classify` guarantees it, and the closed `GitHost`
        // enum makes a non-GitHub host unrepresentable — so no host branch is needed.
        let _ = spec.host.domain();
        if !is_repo_seg(&spec.owner) || !is_repo_seg(&spec.repo) {
            return Err(ClientError::RemoteFetch(format!(
                "{}: invalid owner/repo",
                spec.label()
            )));
        }
        // GitHub's `/tarball/{ref}` (ref optional → the default branch) redirects to codeload.
        let url = match &spec.git_ref {
            Some(r) => {
                let r = sanitize_ref(r).ok_or_else(|| {
                    ClientError::RemoteFetch(format!("{}: invalid ref", spec.label()))
                })?;
                format!(
                    "https://api.github.com/repos/{}/{}/tarball/{r}",
                    spec.owner, spec.repo
                )
            }
            None => format!(
                "https://api.github.com/repos/{}/{}/tarball",
                spec.owner, spec.repo
            ),
        };
        let resp = self
            .agent
            .get(&url)
            .header("User-Agent", RELEASE_USER_AGENT)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| ClientError::RemoteFetch(format!("{}: {e}", spec.label())))?;
        match resp.status().as_u16() {
            200..=299 => read_body(resp).map_err(|_| {
                ClientError::RemoteFetch(format!(
                    "{}: response body could not be read",
                    spec.label()
                ))
            }),
            404 => Err(ClientError::RemoteFetch(format!(
                "{} — repo or ref not found (only public repos are supported today)",
                spec.label()
            ))),
            s => Err(ClientError::RemoteFetch(format!(
                "{}: HTTP {s}",
                spec.label()
            ))),
        }
    }
}

/// A GitHub owner/repo path segment: ASCII alphanumerics + `.`, `_`, `-`, non-empty — the last-line guard
/// before splicing into the request path (mirrors [`ensure_url_safe_ids`]).
fn is_repo_seg(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Sanitize a user-supplied git ref before it rides the URL path: a branch/tag/sha may contain `/`, but
/// never an empty/`.`/`..` segment (traversal) or a control / `%?#`/space char. `None` rejects it.
fn sanitize_ref(r: &str) -> Option<String> {
    if r.is_empty() || r.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return None;
    }
    r.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        .then(|| r.to_owned())
}

/// base64url-unpadded encode raw bytes (the device public key on the wire; the enroll signature header).
fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Map a transport-level [`PlaneError`] from a shared body read into the client error family.
fn plane_err(e: PlaneError) -> ClientError {
    match e {
        PlaneError::NotFound => ClientError::Plane("not found".into()),
        PlaneError::Unavailable(m) | PlaneError::Unreachable(m) | PlaneError::Malformed(m) => {
            ClientError::Plane(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topos_types::requests::WireVersionFile;
    use topos_types::{TerminalOutcome, WireError};

    #[test]
    fn classify_maps_the_wire_status_set() {
        assert_eq!(classify(200), HttpClass::Ok);
        assert_eq!(classify(201), HttpClass::Ok);
        assert_eq!(classify(304), HttpClass::NotModified);
        assert_eq!(classify(404), HttpClass::NotFound);
        // 5xx / unexpected statuses are transiently unavailable, never silently OK.
        assert_eq!(classify(500), HttpClass::Other);
        assert_eq!(classify(503), HttpClass::Other);
        assert_eq!(classify(403), HttpClass::Other);
        assert_eq!(classify(302), HttpClass::Other);
    }

    /// A meta frame whose `object_id`s are the real sha256 of the canned bytes (so verify passes).
    fn meta_for(files: &[(&str, WireFileMode, &[u8])], parents: Vec<String>) -> WireVersionMeta {
        WireVersionMeta {
            version_id: "a".repeat(64),
            parents,
            author: "d_test".to_owned(),
            message: "topos: publish".to_owned(),
            bundle_digest: "b".repeat(64),
            files: files
                .iter()
                .map(|(path, mode, bytes)| WireVersionFile {
                    path: (*path).to_owned(),
                    mode: *mode,
                    object_id: to_hex(&digest::sha256(bytes)),
                })
                .collect(),
        }
    }

    #[test]
    fn build_fetched_version_assembles_and_maps_modes() {
        let files: &[(&str, WireFileMode, &[u8])] = &[
            ("SKILL.md", WireFileMode::Regular, b"hello\n"),
            ("run.sh", WireFileMode::Executable, b"#!/bin/sh\n"),
        ];
        let meta = meta_for(files, vec!["c".repeat(64)]);
        // The closure serves each blob by its content id from an in-memory map (no HTTP).
        let blobs: HashMap<String, Vec<u8>> = files
            .iter()
            .map(|(_, _, bytes)| (to_hex(&digest::sha256(bytes)), bytes.to_vec()))
            .collect();

        let fetched = build_fetched_version(&meta, |id| {
            blobs.get(id).cloned().ok_or(PlaneError::NotFound)
        })
        .expect("assembly succeeds");

        assert_eq!(fetched.parents, vec![[0xcc; 32]]);
        assert_eq!(fetched.author, "d_test");
        assert_eq!(fetched.files.len(), 2);
        assert_eq!(fetched.files[0].path, "SKILL.md");
        assert_eq!(fetched.files[0].mode, FileMode::Regular);
        assert_eq!(fetched.files[0].bytes, b"hello\n");
        assert_eq!(fetched.files[1].mode, FileMode::Executable);
    }

    #[test]
    fn build_fetched_version_rejects_a_sha256_mismatch() {
        let files: &[(&str, WireFileMode, &[u8])] =
            &[("SKILL.md", WireFileMode::Regular, b"hello\n")];
        let meta = meta_for(files, vec![]);
        // Serve the WRONG bytes for the requested id → the verify catches it as Malformed (forged blob).
        let err = build_fetched_version(&meta, |_id| Ok(b"tampered\n".to_vec())).unwrap_err();
        assert!(matches!(err, PlaneError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn build_fetched_version_rejects_a_bad_parent_hex() {
        let meta = meta_for(&[], vec!["not-hex".to_owned()]);
        let err = build_fetched_version(&meta, |_id| Ok(Vec::new())).unwrap_err();
        assert!(matches!(err, PlaneError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn build_fetched_version_rejects_an_uppercase_object_id() {
        // The schema pins lowercase hex; an uppercase object id is malformed, not silently accepted.
        let mut meta = meta_for(&[("a", WireFileMode::Regular, b"x")], vec![]);
        meta.files[0].object_id = meta.files[0].object_id.to_uppercase();
        let err = build_fetched_version(&meta, |_id| Ok(b"x".to_vec())).unwrap_err();
        assert!(matches!(err, PlaneError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn build_fetched_version_propagates_a_blob_fetch_error() {
        let files: &[(&str, WireFileMode, &[u8])] = &[("a", WireFileMode::Regular, b"x")];
        let meta = meta_for(files, vec![]);
        let err = build_fetched_version(&meta, |_id| Err(PlaneError::NotFound)).unwrap_err();
        assert!(matches!(err, PlaneError::NotFound), "got {err:?}");
    }

    // ---- The invitation all-outcome 200 envelope mapping. ----

    fn envelope_bytes(env: &JsonEnvelope) -> Vec<u8> {
        serde_json::to_vec(env).expect("serialize envelope")
    }

    #[test]
    fn map_invite_envelope_ok_yields_invitation_data() {
        let env = JsonEnvelope {
            schema_version: 1,
            command: "invite".to_owned(),
            ok: true,
            data: serde_json::to_value(InvitationData {
                address: "https://acme.topos.test/acme".to_owned(),
                invited: vec!["alice@acme.com".to_owned()],
                mailed: false,
            })
            .unwrap(),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: None,
        };
        let data =
            map_invite_envelope(200, &envelope_bytes(&env)).expect("ok maps to InvitationData");
        assert_eq!(data.address, "https://acme.topos.test/acme");
        assert_eq!(data.invited, vec!["alice@acme.com".to_owned()]);
        assert!(!data.mailed);
    }

    #[test]
    fn map_invite_envelope_denied_is_a_typed_error_carrying_the_code() {
        use topos_types::{Affected, TerminalOutcome, WireError};
        let env = JsonEnvelope {
            schema_version: 1,
            command: "invite".to_owned(),
            ok: false,
            data: serde_json::json!({}),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: Some(WireError {
                code: "NOT_AUTHORIZED".to_owned(),
                outcome: TerminalOutcome::Denied,
                retryable: false,
                affected: Affected::default(),
                expected_generation: None,
                current_generation: None,
                context: serde_json::json!({}),
                next_actions: Vec::new(),
            }),
        };
        let err = map_invite_envelope(200, &envelope_bytes(&env)).unwrap_err();
        match err {
            ClientError::Plane(m) => assert!(m.contains("NOT_AUTHORIZED"), "got {m}"),
            other => panic!("expected a typed Plane error, got {other:?}"),
        }
    }

    #[test]
    fn map_invite_envelope_non_200_is_a_typed_error() {
        // A non-200 (transport/auth/integrity) never reaches the envelope decode.
        let err = map_invite_envelope(500, b"{}").unwrap_err();
        assert!(matches!(err, ClientError::Plane(_)), "got {err:?}");
    }

    // ---- The contribute-write all-outcome 200 envelope mapping. ----

    fn receipt(outcome: TerminalOutcome) -> topos_types::Receipt {
        topos_types::Receipt {
            schema_version: 1,
            op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            command: "publish-direct".to_owned(),
            outcome,
            workspace_id: "w_demo".to_owned(),
            skill_id: Some("s_demo".to_owned()),
            version_id: Some("a".repeat(64)),
            bundle_digest: Some("b".repeat(64)),
            expected_generation: Some(topos_types::Generation { epoch: 1, seq: 1 }),
            current_generation: Some(topos_types::Generation { epoch: 1, seq: 2 }),
            created_at: "2026-06-30T00:00:00Z".to_owned(),
            details: None,
        }
    }

    fn wire_record_value() -> serde_json::Value {
        serde_json::to_value(WireCurrentRecord {
            schema_version: 1,
            scope: topos_types::PointerScope {
                workspace_id: "w_demo".to_owned(),
                skill_id: "s_demo".to_owned(),
            },
            record: topos_types::CurrentRecord {
                version_id: "a".repeat(64),
                generation: topos_types::Generation { epoch: 1, seq: 2 },
            },
        })
        .unwrap()
    }

    fn write_env(
        ok: bool,
        data: serde_json::Value,
        r: topos_types::Receipt,
        error: Option<topos_types::WireError>,
    ) -> Vec<u8> {
        envelope_bytes(&JsonEnvelope {
            schema_version: 1,
            command: "publish".to_owned(),
            ok,
            data,
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: Some(r),
            error,
        })
    }

    #[test]
    fn map_write_envelope_ok_carries_the_wire_record_and_digest() {
        let bytes = write_env(
            true,
            wire_record_value(),
            receipt(TerminalOutcome::Ok),
            None,
        );
        let wr = map_write_envelope(200, &bytes).expect("ok maps to a receipt");
        assert_eq!(wr.outcome(), TerminalOutcome::Ok);
        assert!(
            wr.wire_record.is_some(),
            "an OK move carries the current pointer"
        );
        assert_eq!(
            wr.receipt.bundle_digest.as_deref(),
            Some("b".repeat(64).as_str())
        );
        assert!(wr.error.is_none());
    }

    #[test]
    fn map_write_envelope_needs_review_is_ok_with_no_record() {
        // NEEDS_REVIEW: the proposal opened, nothing moved → data `{}`, no record, no error.
        let bytes = write_env(
            true,
            serde_json::json!({}),
            receipt(TerminalOutcome::NeedsReview),
            None,
        );
        let wr = map_write_envelope(200, &bytes).expect("needs_review is a 200 receipt");
        assert_eq!(wr.outcome(), TerminalOutcome::NeedsReview);
        assert!(wr.wire_record.is_none());
        assert!(wr.error.is_none());
    }

    #[test]
    fn map_write_envelope_reject_ok_with_empty_data_is_not_corrupt() {
        // THE regression guard: an OK `review --reject` returns outcome Ok with data `{}` (it moves
        // nothing). A strict `from_value` would wrongly fail it; the lenient `.ok()` keeps it valid.
        let bytes = write_env(
            true,
            serde_json::json!({}),
            receipt(TerminalOutcome::Ok),
            None,
        );
        let wr = map_write_envelope(200, &bytes).expect("an OK reject is not Corrupt");
        assert_eq!(wr.outcome(), TerminalOutcome::Ok);
        assert!(wr.wire_record.is_none(), "no pointer moved on a reject");
    }

    #[test]
    fn map_write_envelope_conflict_is_ok_not_err() {
        // CONFLICT is a terminal protocol outcome the verb branches on (NOT collapsed into an Err) — it
        // carries the live current_generation (the rebase target).
        let err = WireError {
            code: "CONFLICT".to_owned(),
            outcome: TerminalOutcome::Conflict,
            retryable: true,
            affected: topos_types::Affected::default(),
            expected_generation: Some(topos_types::Generation { epoch: 1, seq: 1 }),
            current_generation: Some(topos_types::Generation { epoch: 1, seq: 5 }),
            context: serde_json::json!({}),
            next_actions: Vec::new(),
        };
        let bytes = write_env(
            false,
            serde_json::json!({}),
            receipt(TerminalOutcome::Conflict),
            Some(err),
        );
        let wr = map_write_envelope(200, &bytes).expect("conflict is a 200 receipt, not an Err");
        assert_eq!(wr.outcome(), TerminalOutcome::Conflict);
        assert_eq!(
            wr.error.and_then(|e| e.current_generation),
            Some(topos_types::Generation { epoch: 1, seq: 5 }),
            "the live generation (rebase target) survives the mapping"
        );
    }

    #[test]
    fn map_write_envelope_non_200_is_a_plane_error() {
        let err = map_write_envelope(500, b"{}").unwrap_err();
        assert!(matches!(err, ClientError::Plane(_)), "got {err:?}");
    }

    #[test]
    fn map_write_envelope_missing_receipt_is_wire_invalid() {
        let bytes = envelope_bytes(&JsonEnvelope {
            schema_version: 1,
            command: "publish".to_owned(),
            ok: true,
            data: serde_json::json!({}),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: None,
        });
        let err = map_write_envelope(200, &bytes).unwrap_err();
        assert!(matches!(err, ClientError::WireInvalid(_)), "got {err:?}");
        assert_eq!(
            crate::render::safe_message(&err),
            "the plane's response failed validation",
            "a wire fault never blames a local sidecar"
        );
    }

    // ---- The id boundary: hostile plane-supplied ids are refused at the wire parse. ----

    fn redeem_env(workspace_id: &str) -> Vec<u8> {
        envelope_bytes(&JsonEnvelope {
            schema_version: 1,
            command: "redeem".to_owned(),
            ok: true,
            data: serde_json::json!({
                "workspace_id": workspace_id,
                "device_key_id": "dk_abc",
                "principal": "alice@acme.com",
                "credential": "wsc_secret"
            }),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: None,
        })
    }

    #[test]
    fn map_redeem_envelope_refuses_a_hostile_workspace_id() {
        // The workspace id is the only id the redeem carries now, and it becomes a URL path segment + the
        // credential lookup key — every traversal/separator/case shape must fail the WHOLE redeem, as the
        // WIRE flavor of the corrupt family (same CORRUPT_STATE code; the safe message names the plane).
        for bad in ["../../x", "a/b", "A", "", ".", ".."] {
            let err = map_redeem_envelope(RedeemKind::Grant, 200, &redeem_env(bad)).unwrap_err();
            assert!(
                matches!(err, ClientError::WireInvalid(_)),
                "workspace id {bad:?} must be refused as WireInvalid, got {err:?}"
            );
            assert_eq!(err.code(), "CORRUPT_STATE", "no new wire code");
            assert_eq!(
                crate::render::safe_message(&err),
                "the plane's response failed validation"
            );
        }
        // A clean redeem still parses (and carries the workspace credential + the seated principal).
        let ok = map_redeem_envelope(RedeemKind::Grant, 200, &redeem_env("w_acme")).unwrap();
        assert_eq!(ok.credential, "wsc_secret");
        assert_eq!(ok.principal.as_deref(), Some("alice@acme.com"));
        // The credential never surfaces in Debug.
        assert!(!format!("{ok:?}").contains("wsc_secret"));
    }

    #[test]
    fn a_denied_redeem_is_typed_by_its_door() {
        // The plane's DENIED envelope is uniform — the guidance comes from which door was knocked on.
        let denied = envelope_bytes(&JsonEnvelope {
            schema_version: 1,
            command: "redeem".to_owned(),
            ok: false,
            data: serde_json::json!({}),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: Some(WireError {
                code: "DENIED".to_owned(),
                outcome: TerminalOutcome::Denied,
                retryable: false,
                affected: topos_types::Affected::default(),
                expected_generation: None,
                current_generation: None,
                context: serde_json::json!({}),
                next_actions: Vec::new(),
            }),
        });
        // A grant redeem's denial → the ask-an-owner guidance (REQUEST_ACCESS rides the envelope).
        let grant = map_redeem_envelope(RedeemKind::Grant, 200, &denied).unwrap_err();
        assert!(
            matches!(&grant, ClientError::RedeemDenied { code } if code == "DENIED"),
            "got {grant:?}"
        );
        assert_eq!(grant.code(), "DENIED");
        assert_eq!(grant.outcome(), TerminalOutcome::Denied);
        assert!(
            crate::render::safe_message(&grant).contains("topos invite <your-email>"),
            "the ask-an-owner guidance names the exact command: {grant}"
        );
        // A claim redeem's denial → the dead-claim guidance (ask the operator for a fresh link).
        let claim = map_redeem_envelope(RedeemKind::Claim, 200, &denied).unwrap_err();
        assert!(matches!(claim, ClientError::Enrollment(_)), "got {claim:?}");
        assert!(claim.to_string().contains("fresh claim link"), "{claim}");
    }

    fn bootstrap_json(skill_id: &str, workspace_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "invite": {
                "token_id": "tok_1",
                "consent": "direct_human_first_receive",
                "first_receive_auto_land": false
            },
            "plane": {
                "base_url": "https://acme.topos.test",
                "deployment_mode": "self_host",
                "enrollment_method": "device_code"
            },
            "workspace": {
                "workspace_id": workspace_id,
                "display_name": "Acme",
                "verified_domain_status": "unverified"
            },
            "offered_skills": [ { "skill_id": skill_id } ]
        }))
        .expect("serialize bootstrap")
    }

    #[test]
    fn parse_bootstrap_refuses_hostile_ids_but_accepts_clean_ones() {
        // The bootstrap's ids persist into the enrollment WAL and later key path joins / URL splices.
        for bad in ["../../x", "a/b", "A", "", ".", ".."] {
            let err = parse_bootstrap(&bootstrap_json(bad, "w_acme")).unwrap_err();
            assert!(
                matches!(err, ClientError::WireInvalid(_)),
                "offered skill id {bad:?} must be refused, got {err:?}"
            );
            let err = parse_bootstrap(&bootstrap_json("s_deploy", bad)).unwrap_err();
            assert!(
                matches!(err, ClientError::WireInvalid(_)),
                "workspace id {bad:?} must be refused, got {err:?}"
            );
        }
        let ok = parse_bootstrap(&bootstrap_json("s_deploy", "w_acme")).expect("clean bootstrap");
        assert_eq!(ok.workspace.workspace_id, "w_acme");
        assert_eq!(ok.offered_skills[0].skill_id, "s_deploy");
    }

    #[test]
    fn url_splice_guard_refuses_an_invalid_id() {
        // The last-line guard before an id reaches a URL path.
        assert!(ensure_url_safe_ids("s_deploy", "w_acme").is_ok());
        for (skill, ws) in [("../x", "w_acme"), ("s_deploy", "../w"), ("A", "w_acme")] {
            let err = ensure_url_safe_ids(skill, ws).unwrap_err();
            assert!(matches!(err, PlaneError::Malformed(_)), "got {err:?}");
        }
    }
}
