//! The real plane transport: a blocking `ureq` (3, rustls+ring) [`PlaneSource`] that feeds the already-built
//! pull engine, plus the on-disk [`FollowSource`].
//!
//! [`UreqPlane`] is a **dumb transport** — it speaks the B3 wire (`GET /v1/current/{read_token}` with the
//! commit-sensitive conditional-GET headers; `GET …/versions/{id}` + per-blob `GET …/bundles/{id}` under a
//! Bearer token) and verifies each blob's `sha256 == object_id`, but it does **not** verify the pointer
//! signature — the engine does that against `ctx.plane_key`. Status mapping ([`classify`]), version
//! assembly ([`build_fetched_version`]), and the envelope mappings are factored as pure functions so the
//! wire logic is unit-tested without a live server; the full loopback round-trips live in the `tests/`
//! member.
//!
//! **Ids are validated at this boundary.** Every skill/workspace id a response carries (the redeem's
//! read creds, the bootstrap's offered skills) is parsed through [`crate::id`] before it is returned —
//! a plane-chosen `"../../x"` fails here as a malformed response, never reaching a path join or a URL
//! splice.
//!
//! The client stays **sync + tokio-free**: `ureq` brings its own blocking TLS stack, so this adds no
//! `plane-store`/`sqlx`/`tokio` edge (`check-arch` holds the line).

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use topos_core::digest::{self, FileMode, to_hex};
use topos_types::requests::{
    DeviceAuthorizeRequest, DeviceAuthorizeResponse, DeviceTokenRequest, DeviceTokenResponse,
    DeviceTokenStatus, InviteRequest, ProposeRequest, PublishRequest, RedeemRequest,
    RedeemResponse, RevertRequest, ReviewRequest, WireFileMode, WireProposalList, WireVersionMeta,
};
use topos_types::results::InviteData;
use topos_types::{BootstrapData, JsonEnvelope, SignedCurrentRecord};

use crate::error::ClientError;
use crate::plane::{
    ContributeSource, DeviceAuthorize, EnrollSource, FetchedFile, FetchedVersion, FollowContext,
    FollowSource, GovernanceSource, Grant, KnownCurrent, PlaneError, PlaneSource, PointerFetch,
    Redeem, RedeemedCred, TokenPoll, WriteReceipt,
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

/// One skill's transport credential — its workspace + its **secret** read token. (Distinct from the
/// engine's [`FollowContext`] consent state: creds live in the transport, consent in the follow seam.)
pub(crate) struct SkillCred {
    pub(crate) workspace_id: String,
    /// The per-follower read token (Bearer on versions/bundles, path segment on current). **SECRET.**
    pub(crate) read_token: String,
}

impl SkillCred {
    pub(crate) fn new(workspace_id: String, read_token: String) -> Self {
        Self {
            workspace_id,
            read_token,
        }
    }
}

// Redact the secret token — it must never reach a log / panic message / Debug dump.
impl std::fmt::Debug for SkillCred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillCred")
            .field("workspace_id", &self.workspace_id)
            .field("read_token", &"<redacted>")
            .finish()
    }
}

/// The blocking `ureq` plane transport. Holds the base URL, a per-skill credential map, and one configured
/// agent (connection-pooled, reused across requests).
pub(crate) struct UreqPlane {
    base_url: String,
    creds: HashMap<String, SkillCred>,
    agent: ureq::Agent,
}

impl std::fmt::Debug for UreqPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The agent is not Debug, and the creds carry secrets — print only the safe shape.
        f.debug_struct("UreqPlane")
            .field("base_url", &self.base_url)
            .field("skills", &self.creds.len())
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
            creds,
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }

    /// A `GET` carrying `Authorization: Bearer <read_token>` (versions + bundles). Returns the raw body on
    /// 2xx, [`PlaneError::NotFound`] on 404, [`PlaneError::Unreachable`] on a connect-level fault, and
    /// [`PlaneError::Unavailable`] on any other status.
    /// `url` here never contains the secret (the token is in the header), so it is safe in the error text.
    fn bearer_get(&self, url: &str, read_token: &str) -> Result<Vec<u8>, PlaneError> {
        let resp = self
            .agent
            .get(url)
            .header("authorization", format!("Bearer {read_token}"))
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
        let cred = self.creds.get(skill_id).ok_or(PlaneError::NotFound)?;
        // The read token is the PATH segment on this route — so its URL is SECRET and must never appear in
        // an error message (unlike the Bearer routes). Error text names only the skill.
        let url = format!("{}/v1/current/{}", self.base_url, cred.read_token);
        let mut req = self.agent.get(&url);
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
                // Transport only deserializes — the engine authenticates the signature against the plane key.
                let rec = serde_json::from_slice::<SignedCurrentRecord>(&bytes).map_err(|e| {
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
        let cred = self.creds.get(skill_id).ok_or(PlaneError::NotFound)?;
        // Both ids are spliced into the URL path — refuse anything outside the validated id charset
        // (defense in depth; the enrollment loaders already validated what they persisted).
        ensure_url_safe_ids(skill_id, &cred.workspace_id)?;
        let vid_hex = to_hex(&version_id);
        let meta_url = format!(
            "{}/v1/workspaces/{}/skills/{}/versions/{}",
            self.base_url, cred.workspace_id, skill_id, vid_hex
        );
        let meta_bytes = self.bearer_get(&meta_url, &cred.read_token)?;
        let meta: WireVersionMeta = serde_json::from_slice(&meta_bytes)
            .map_err(|e| PlaneError::Malformed(format!("version metadata for {skill_id}: {e}")))?;
        build_fetched_version(&meta, |object_id_hex| {
            let url = format!(
                "{}/v1/workspaces/{}/skills/{}/bundles/{}",
                self.base_url, cred.workspace_id, skill_id, object_id_hex
            );
            self.bearer_get(&url, &cred.read_token)
        })
    }

    fn list_open_proposals(&self, skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        // No credential for this skill ⇒ none visible (best-effort; the count never errors out a pull).
        let Some(cred) = self.creds.get(skill_id) else {
            return Ok(Vec::new());
        };
        ensure_url_safe_ids(skill_id, &cred.workspace_id)?;
        let url = format!(
            "{}/v1/workspaces/{}/skills/{}/proposals",
            self.base_url, cred.workspace_id, skill_id
        );
        match self.bearer_get(&url, &cred.read_token) {
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
// UreqDeviceClient — the real creds-free DEVICE-SIGNED transport (sibling of the read-credentialed
// `UreqPlane`). One client speaks every route a device key (not a read token) authenticates: the
// enrollment flow (`GET /i/{token}`, `POST /v1/device/authorize`, `POST /v1/device/token`, the redeem
// `POST /v1/workspaces/{ws}/devices`), the governance Invite POST, and the four contribute writes
// (publish / propose / revert / review) — the signature rides the `Topos-Device-Signature` header, and
// every terminal protocol outcome of a write comes back as the all-outcome **200 envelope**. The
// `/i/{token}` URL, the device code, and the grant are sensitive — never logged or put in an error.
// =================================================================================================

/// The blocking `ureq` device-signed transport (`EnrollSource` + `GovernanceSource` +
/// `ContributeSource`). Holds the base URL + one configured agent and NO credential map — it is
/// creds-free: authentication is the per-request device-op signature in the `Topos-Device-Signature`
/// header (enrollment starts unauthenticated; the redeem mints the read tokens `UreqPlane` then holds).
pub(crate) struct UreqDeviceClient {
    base_url: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for UreqDeviceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The agent is not Debug; the base_url alone is safe (the secret /i/ token is never stored here).
        f.debug_struct("UreqDeviceClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl UreqDeviceClient {
    /// Build the transport against `base_url` (trailing slash trimmed), over the same agent
    /// configuration as [`UreqPlane`] (status-as-error OFF + the connect/recv/body timeouts).
    pub(crate) fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }

    /// POST a JSON body (optionally with the device-signature header). Returns `(status, body bytes)`.
    /// `what` names the step for a transport-fault message; the body is NEVER echoed (it may hold a secret).
    fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        sig_b64: Option<&str>,
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
        if let Some(sig) = sig_b64 {
            req = req.header("topos-device-signature", sig);
        }
        let resp = req
            .send(payload.as_slice())
            .map_err(|e| ClientError::Plane(format!("{what}: {e}")))?;
        let status = resp.status().as_u16();
        let bytes = read_body(resp).map_err(plane_err)?;
        Ok((status, bytes))
    }

    /// POST a device-signed contribute write (the 64-byte device-op signature in the
    /// `Topos-Device-Signature` header) and map the all-outcome **200 envelope** to a [`WriteReceipt`].
    /// The four verbs differ only by `path` + body type; the signing rode the body's bound identity.
    fn post_write<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
        device_sig: [u8; 64],
        what: &str,
    ) -> Result<WriteReceipt, ClientError> {
        let value = serde_json::to_value(body)
            .map_err(|e| ClientError::Corrupt(format!("{what} body: {e}")))?;
        let url = format!("{}{path}", self.base_url);
        let sig = b64(&device_sig); // 64 bytes → 86 base64url-unpadded chars (the frozen header codec).
        let (status, bytes) = self.post_json(&url, &value, Some(&sig), what)?;
        map_write_envelope(status, &bytes)
    }
}

impl EnrollSource for UreqDeviceClient {
    fn fetch_bootstrap(&self, token: &str) -> Result<BootstrapData, ClientError> {
        // The `/i/{token}` URL is SECRET (the token grants the bootstrap read) — error text names no URL.
        let url = format!("{}/i/{}", self.base_url, token);
        let resp = self
            .agent
            .get(&url)
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
        token: &str,
        device_public_key: [u8; 32],
        machine_name: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        let body = serde_json::to_value(DeviceAuthorizeRequest {
            invite_token: token.to_owned(),
            device_public_key: b64(&device_public_key),
            machine_name: machine_name.to_owned(),
        })
        .map_err(|e| ClientError::Corrupt(format!("authorize body: {e}")))?;
        let url = format!("{}/v1/device/authorize", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, None, "device authorize")?;
        if classify(status) != HttpClass::Ok {
            return Err(ClientError::Plane(format!(
                "device authorize: HTTP {status}"
            )));
        }
        let resp: DeviceAuthorizeResponse = serde_json::from_slice(&bytes).map_err(|e| {
            ClientError::WireInvalid(format!("authorize response is malformed: {e}"))
        })?;
        Ok(DeviceAuthorize {
            device_code: resp.device_code,
            user_code: resp.user_code,
            verification_uri: resp.verification_uri,
            expires_in: resp.expires_in,
            interval: resp.interval,
        })
    }

    fn poll_token(&self, device_code: &str) -> Result<TokenPoll, ClientError> {
        let body = serde_json::to_value(DeviceTokenRequest {
            device_code: device_code.to_owned(),
        })
        .map_err(|e| ClientError::Corrupt(format!("token body: {e}")))?;
        let url = format!("{}/v1/device/token", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, None, "device token poll")?;
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
                Some(g) => TokenPoll::Granted(Grant::new(g)),
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
        enroll_sig: [u8; 64],
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
        let sig = b64(&enroll_sig);
        let (status, bytes) = self.post_json(&url, &body, Some(&sig), "redeem")?;
        map_redeem_envelope(status, &bytes)
    }
}

/// Map a redeem response — the all-outcome **200 envelope** — to the typed [`Redeem`], validating every
/// id the plane minted (the per-cred skill ids become path components under `~/.topos/` and the harness
/// skills dir; the workspace id becomes a URL segment) — a traversal-shaped id fails the whole redeem as
/// malformed. **Pure** (status + bytes in), so the ok / denied / hostile-id arms are unit-tested without
/// a socket (mirrors [`map_invite_envelope`]).
fn map_redeem_envelope(status: u16, bytes: &[u8]) -> Result<Redeem, ClientError> {
    // The redeem is an all-outcome 200 envelope; a non-2xx is a transport/auth/integrity fault.
    if classify(status) != HttpClass::Ok {
        return Err(ClientError::Plane(format!("redeem: HTTP {status}")));
    }
    let env: JsonEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("redeem envelope is malformed: {e}")))?;
    if !env.ok {
        // A DENIED redeem (e.g. a device-key mismatch) — surface the code, never any secret.
        let code = env
            .error
            .map(|e| e.code)
            .unwrap_or_else(|| "DENIED".to_owned());
        return Err(ClientError::Plane(format!("redeem refused ({code})")));
    }
    let resp: RedeemResponse = serde_json::from_value(env.data)
        .map_err(|e| ClientError::WireInvalid(format!("redeem data is malformed: {e}")))?;
    crate::id::validate_workspace_id(&resp.workspace_id).map_err(crate::id::wire_flavor)?;
    let mut read_creds = Vec::with_capacity(resp.read_creds.len());
    for c in resp.read_creds {
        // The wire boundary: the minted skill id must be a safe path component (it keys the sidecar +
        // the harness placement). Parse-don't-validate — the failure is the corrupt family's WIRE
        // flavor (same code; the safe message names the plane, not a sidecar).
        crate::id::SkillId::parse(&c.skill_id).map_err(crate::id::wire_flavor)?;
        read_creds.push(RedeemedCred {
            skill_id: c.skill_id,
            read_token: c.read_token,
            expires_at: c.expires_at,
        });
    }
    Ok(Redeem {
        workspace_id: resp.workspace_id,
        device_key_id: resp.device_key_id,
        read_creds,
    })
}

/// Parse + validate the `/i/` bootstrap body: the serde decode (a non-Ed25519 `alg` fails the CLOSED
/// enum), then the id boundary — the workspace id and every offered skill id must be safe path/URL
/// segments (they persist into the WAL and later key path joins). **Pure**, unit-tested with canned JSON.
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
// The governance-write side of `UreqDeviceClient` — the owner's signed Invite POST. Creds-free (the 64-byte
// governance signature is the auth, riding the `Topos-Device-Signature` header); mirrors `redeem`'s
// all-outcome 200 envelope mapping.
// =================================================================================================

impl GovernanceSource for UreqDeviceClient {
    fn create_invite(
        &self,
        body: InviteRequest,
        governance_sig: [u8; 64],
    ) -> Result<InviteData, ClientError> {
        let value = serde_json::to_value(&body)
            .map_err(|e| ClientError::Corrupt(format!("invite body: {e}")))?;
        let url = format!("{}/v1/invites", self.base_url);
        let sig = b64(&governance_sig);
        let (status, bytes) = self.post_json(&url, &value, Some(&sig), "create invite")?;
        map_invite_envelope(status, &bytes)
    }
}

/// Map a create-invite response — the all-outcome **200 envelope** — to the typed result. A non-200 is a
/// transport/auth/integrity fault; `ok` carries the [`InviteData`]; `!ok` is a typed DENIED error carrying
/// the wire error's code (never a secret). **Pure** (status + bytes in), so the ok / denied / non-200 /
/// malformed arms are all unit-tested without a socket (mirrors [`build_fetched_version`]).
fn map_invite_envelope(status: u16, bytes: &[u8]) -> Result<InviteData, ClientError> {
    if classify(status) != HttpClass::Ok {
        return Err(ClientError::Plane(format!("create invite: HTTP {status}")));
    }
    let env: JsonEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("invite envelope is malformed: {e}")))?;
    if !env.ok {
        // A DENIED invite (e.g. the signer is not an owner) — surface the code, never any secret.
        let code = env
            .error
            .map(|e| e.code)
            .unwrap_or_else(|| "DENIED".to_owned());
        return Err(ClientError::Plane(format!("invite refused ({code})")));
    }
    serde_json::from_value(env.data)
        .map_err(|e| ClientError::WireInvalid(format!("invite data is malformed: {e}")))
}

// =================================================================================================
// The contribute-write side of `UreqDeviceClient` — the device-signed publish / propose / revert / review
// POSTs. Creds-free (the 64-byte device-op signature rides the `Topos-Device-Signature` header). UNLIKE
// `map_invite_envelope`, a `!ok` body is NOT an error: CONFLICT / APPROVAL_REQUIRED / DENIED are terminal
// protocol outcomes the verb branches on (carrying `current_generation` + `next_actions`).
// =================================================================================================

impl ContributeSource for UreqDeviceClient {
    fn publish(
        &self,
        body: PublishRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/publish", &body, device_sig, "publish")
    }
    fn propose(
        &self,
        body: ProposeRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/proposals", &body, device_sig, "propose")
    }
    fn revert(
        &self,
        body: RevertRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/reverts", &body, device_sig, "revert")
    }
    fn review(
        &self,
        body: ReviewRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError> {
        self.post_write("/v1/reviews", &body, device_sig, "review")
    }
}

/// Map a contribute-write response — the all-outcome **200 envelope** — to a typed [`WriteReceipt`]. EVERY
/// parsed 200 is an `Ok(WriteReceipt)` (OK / NEEDS_REVIEW / CONFLICT / APPROVAL_REQUIRED / DENIED are all
/// terminal protocol outcomes the verb acts on); only a non-200 (transport/auth/integrity) or an
/// unparseable envelope is a [`ClientError`]. The signed `current` pointer rides `data` ONLY when a pointer
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
    // The signed pointer is present ONLY when a pointer actually moved. NEEDS_REVIEW, an OK `review
    // --reject` (the plane returns OK with no signed record → data `{}`), and every failure carry `{}`;
    // parse leniently so a valid reject is never wrongly rejected as Corrupt.
    let signed_record = serde_json::from_value::<SignedCurrentRecord>(env.data).ok();
    Ok(WriteReceipt {
        receipt,
        error: env.error,
        signed_record,
    })
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

    // ---- The create-invite all-outcome 200 envelope mapping. ----

    fn envelope_bytes(env: &JsonEnvelope) -> Vec<u8> {
        serde_json::to_vec(env).expect("serialize envelope")
    }

    #[test]
    fn map_invite_envelope_ok_yields_invite_data() {
        let env = JsonEnvelope {
            schema_version: 1,
            command: "invite".to_owned(),
            ok: true,
            data: serde_json::to_value(InviteData {
                invite_link: "https://acme.topos.test/i/tok_abc".to_owned(),
                roster_added: vec!["alice@acme.com".to_owned()],
                skills: vec!["s_deploy".to_owned()],
            })
            .unwrap(),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: None,
        };
        let data = map_invite_envelope(200, &envelope_bytes(&env)).expect("ok maps to InviteData");
        assert_eq!(data.invite_link, "https://acme.topos.test/i/tok_abc");
        assert_eq!(data.roster_added, vec!["alice@acme.com".to_owned()]);
        assert_eq!(data.skills, vec!["s_deploy".to_owned()]);
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
            key_id: Some("pk_plane".to_owned()),
            details: None,
        }
    }

    fn signed_record_value() -> serde_json::Value {
        serde_json::to_value(SignedCurrentRecord {
            schema_version: 1,
            scope: topos_types::PointerScope {
                workspace_id: "w_demo".to_owned(),
                skill_id: "s_demo".to_owned(),
            },
            record: topos_types::CurrentRecord {
                version_id: "a".repeat(64),
                generation: topos_types::Generation { epoch: 1, seq: 2 },
            },
            signature: topos_types::Signature {
                alg: topos_types::SignatureAlg::Ed25519,
                key_id: "pk_plane".to_owned(),
                value: "A".repeat(86),
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
    fn map_write_envelope_ok_carries_the_signed_record_and_digest() {
        let bytes = write_env(
            true,
            signed_record_value(),
            receipt(TerminalOutcome::Ok),
            None,
        );
        let wr = map_write_envelope(200, &bytes).expect("ok maps to a receipt");
        assert_eq!(wr.outcome(), TerminalOutcome::Ok);
        assert!(
            wr.signed_record.is_some(),
            "an OK move carries the signed pointer"
        );
        assert_eq!(
            wr.receipt.bundle_digest.as_deref(),
            Some("b".repeat(64).as_str())
        );
        assert!(wr.error.is_none());
    }

    #[test]
    fn map_write_envelope_needs_review_is_ok_with_no_record() {
        // NEEDS_REVIEW: the proposal opened, nothing moved → data `{}`, no signed record, no error.
        let bytes = write_env(
            true,
            serde_json::json!({}),
            receipt(TerminalOutcome::NeedsReview),
            None,
        );
        let wr = map_write_envelope(200, &bytes).expect("needs_review is a 200 receipt");
        assert_eq!(wr.outcome(), TerminalOutcome::NeedsReview);
        assert!(wr.signed_record.is_none());
        assert!(wr.error.is_none());
    }

    #[test]
    fn map_write_envelope_reject_ok_with_empty_data_is_not_corrupt() {
        // THE regression guard: an OK `review --reject` returns outcome Ok with data `{}` (it signs
        // nothing). A strict `from_value` would wrongly fail it; the lenient `.ok()` keeps it valid.
        let bytes = write_env(
            true,
            serde_json::json!({}),
            receipt(TerminalOutcome::Ok),
            None,
        );
        let wr = map_write_envelope(200, &bytes).expect("an OK reject is not Corrupt");
        assert_eq!(wr.outcome(), TerminalOutcome::Ok);
        assert!(wr.signed_record.is_none(), "no pointer moved on a reject");
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

    fn redeem_env(skill_id: &str, workspace_id: &str) -> Vec<u8> {
        envelope_bytes(&JsonEnvelope {
            schema_version: 1,
            command: "redeem".to_owned(),
            ok: true,
            data: serde_json::json!({
                "workspace_id": workspace_id,
                "device_key_id": "dk_abc",
                "read_creds": [
                    { "skill_id": skill_id, "read_token": "rt_secret" }
                ]
            }),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: None,
        })
    }

    #[test]
    fn map_redeem_envelope_refuses_a_hostile_skill_id() {
        // The redeem's skill ids become path components (~/.topos/skills/<id>, the harness skills dir),
        // so every traversal/separator/case shape must fail the WHOLE redeem — as the WIRE flavor of the
        // corrupt family (same CORRUPT_STATE code; the safe message names the plane, not a sidecar).
        for bad in ["../../x", "a/b", "A", "", ".", ".."] {
            let err = map_redeem_envelope(200, &redeem_env(bad, "w_acme")).unwrap_err();
            assert!(
                matches!(err, ClientError::WireInvalid(_)),
                "skill id {bad:?} must be refused as WireInvalid, got {err:?}"
            );
            assert_eq!(err.code(), "CORRUPT_STATE", "no new wire code");
            assert_eq!(
                crate::render::safe_message(&err),
                "the plane's response failed validation"
            );
        }
        // A clean redeem still parses.
        let ok = map_redeem_envelope(200, &redeem_env("s_deploy", "w_acme")).unwrap();
        assert_eq!(ok.read_creds.len(), 1);
    }

    #[test]
    fn map_redeem_envelope_refuses_a_hostile_workspace_id() {
        // The workspace id is spliced into request URL paths — same charset rule.
        for bad in ["../../x", "a/b", "A", ""] {
            let err = map_redeem_envelope(200, &redeem_env("s_deploy", bad)).unwrap_err();
            assert!(matches!(err, ClientError::WireInvalid(_)), "got {err:?}");
        }
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
                "enrollment_method": "device_code",
                "signing_key": { "alg": "Ed25519", "key_id": "pk_1", "value": "A".repeat(43) }
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
