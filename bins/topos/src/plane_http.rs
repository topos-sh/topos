//! The real plane transport: a blocking `ureq` (3, rustls+ring) [`PlaneSource`] that feeds the already-built
//! pull engine, plus the on-disk [`FollowSource`].
//!
//! [`UreqPlane`] is a **dumb transport** — it speaks the wire under the device's ONE **Bearer credential**
//! (`GET /v1/workspaces/{ws}/skills/{skill}/current` with the commit-sensitive conditional-GET headers;
//! `GET …/versions/{id}` + per-blob `GET …/bundles/{id}`) and verifies each blob's `sha256 == object_id`.
//! The `current` pointer is unsigned; the engine scope-checks it and re-verifies the fetched bytes against
//! the content-addressed `version_id` on apply. Status mapping ([`classify`]), version
//! assembly ([`build_fetched_version`]), and the envelope mappings are factored as pure functions so the
//! wire logic is unit-tested without a live server; the full loopback round-trips live in the `tests/`
//! member.
//!
//! **Ids are validated at this boundary.** Every skill/workspace id a response carries (the granted
//! poll's workspace id) is parsed through [`crate::id`] before it is returned — a server-chosen
//! `"../../x"` fails here as a malformed response, never reaching a path join or a URL splice.
//!
//! The client stays **sync + tokio-free**: `ureq` brings its own blocking TLS stack, so this adds no
//! `plane-store`/`sqlx`/`tokio` edge (`check-arch` holds the line).

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use topos_core::digest::{self, FileMode, to_hex};
use topos_types::requests::{
    DeviceAuthPollRequest, DeviceAuthPollResponse, DeviceAuthPollStatus, DeviceAuthStartRequest,
    DeviceAuthStartResponse, InvitationData, InvitationRequest, NoticeAckRequest, ProposeRequest,
    ProtectionSetRequest, PublishRequest, RevertRequest, ReviewRequest, WireChannelIndex,
    WireFileMode, WireMe, WireProposalIndex, WireProposalList, WireProtocolCard, WireReach,
    WireSkillIndex, WireSkillLog, WireVersionMeta,
};
use topos_types::{JsonEnvelope, TerminalOutcome, WireCurrentRecord};

use crate::error::ClientError;
use crate::plane::{
    CatalogSource, ContributeSource, DeviceAuthPoll, DeviceAuthStart, DirectorySource,
    EnrollSource, EnrolledGrant, EnrolledWorkspace, FetchedFile, FetchedVersion, GovernanceSource,
    KnownCurrent, LinkStatus, PlaneError, PlaneSource, PointerFetch, WriteReceipt,
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

/// The blocking `ureq` plane transport. Holds the base URL, the device's ONE Bearer credential, a
/// non-secret `skill_id → workspace_id` map (the URL-path scope each skill's reads splice), the
/// enrolled workspaces (the delivery lane's fan-out), and one configured agent (connection-pooled,
/// reused across requests).
pub(crate) struct UreqPlane {
    base_url: String,
    /// **SECRET** — the device's ONE Bearer credential (`None` = signed out; every read answers
    /// "not served"). Redacted from `Debug`.
    credential: Option<String>,
    /// skill_id → workspace_id (the URL-path scope; NO secret). Interior-mutable because the
    /// delivery-driven reconcile LEARNS a brand-new arrival's skill mid-sweep (`bind_skill`): the
    /// map is seeded from `follows.json`, which cannot name a skill this device has never held.
    /// Single-threaded by construction (a blocking CLI transport).
    skill_workspaces: RefCell<HashMap<String, String>>,
    /// The enrolled workspace ids (from `user.json`) — the delivery/report lane's fan-out set.
    workspaces: Vec<String>,
    agent: ureq::Agent,
}

impl std::fmt::Debug for UreqPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The agent is not Debug, and the credential is secret — print only the safe shape.
        f.debug_struct("UreqPlane")
            .field("base_url", &self.base_url)
            .field("skills", &self.skill_workspaces.borrow().len())
            .field("workspaces", &self.workspaces)
            .finish_non_exhaustive()
    }
}

impl UreqPlane {
    /// Build the transport: one blocking agent (rustls+ring, sane connect/recv/body timeouts,
    /// status-as-error OFF so a 304/404/5xx comes back as an inspectable status rather than an error
    /// variant) + the device credential and the skill → workspace map. `base_url`'s trailing slash is
    /// trimmed so URL joins never double up.
    pub(crate) fn new(
        base_url: String,
        credential: Option<String>,
        skill_workspaces: HashMap<String, String>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            credential,
            skill_workspaces: RefCell::new(skill_workspaces),
            workspaces: Vec::new(),
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }

    /// The `(workspace_id, credential)` a skill's reads use, if this transport knows the skill AND a
    /// credential is stored (a signed-out install reads nothing — never a request without a credential).
    fn skill_scope(&self, skill_id: &str) -> Option<(String, String)> {
        let ws = self.skill_workspaces.borrow().get(skill_id).cloned()?;
        let cred = self.credential.clone()?;
        Some((ws, cred))
    }

    /// Teach the read transport a skill's workspace (the shared body behind BOTH the [`PlaneSource`]
    /// and [`crate::plane::DeliverySource`] `bind_skill` — the read lane and the delivery lane on one
    /// object). Idempotent (`or_insert_with` never overwrites a follows-derived entry); the ONE device
    /// credential already authenticates every workspace the person's seats reach, so this is a pure
    /// scope lookup, never a new secret.
    fn bind(&self, workspace_id: &str, skill_id: &str) {
        self.skill_workspaces
            .borrow_mut()
            .entry(skill_id.to_owned())
            .or_insert_with(|| workspace_id.to_owned());
    }

    /// Attach the enrolled workspace ids (`user.json`'s memberships) — what arms the delivery-driven
    /// reconcile ([`crate::plane::DeliverySource`]). Without them the transport still serves the
    /// per-skill reads; the sweep just has no delivery lane to drive.
    pub(crate) fn with_workspaces(mut self, workspaces: Vec<String>) -> Self {
        self.workspaces = workspaces;
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
        let (workspace_id, credential) = self.skill_scope(skill_id).ok_or(PlaneError::NotFound)?;
        // The workspace + skill ids are spliced into the URL path; the credential rides the Bearer header,
        // so the URL carries no secret (safe in an error message). Refuse a non-URL-safe id defensively.
        ensure_url_safe_ids(skill_id, &workspace_id)?;
        let url = format!(
            "{}/v1/workspaces/{}/skills/{}/current",
            self.base_url, workspace_id, skill_id
        );
        let mut req = self
            .agent
            .get(&url)
            .header("authorization", format!("Bearer {credential}"));
        if let Some(k) = known {
            // Commit-sensitive conditional GET: the quoted ETag for the generation AND the known commit id.
            req = req
                .header("if-none-match", format!("\"{}\"", k.generation))
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
        let (workspace_id, credential) = self.skill_scope(skill_id).ok_or(PlaneError::NotFound)?;
        // Both ids are spliced into the URL path — refuse anything outside the validated id charset
        // (defense in depth; the enrollment loaders already validated what they persisted).
        ensure_url_safe_ids(skill_id, &workspace_id)?;
        let vid_hex = to_hex(&version_id);
        let meta_url = format!(
            "{}/v1/workspaces/{}/skills/{}/versions/{}",
            self.base_url, workspace_id, skill_id, vid_hex
        );
        let meta_bytes = self.bearer_get(&meta_url, &credential)?;
        let meta: WireVersionMeta = serde_json::from_slice(&meta_bytes)
            .map_err(|e| PlaneError::Malformed(format!("version metadata for {skill_id}: {e}")))?;
        build_fetched_version(&meta, |object_id_hex| {
            let url = format!(
                "{}/v1/workspaces/{}/skills/{}/bundles/{}",
                self.base_url, workspace_id, skill_id, object_id_hex
            );
            self.bearer_get(&url, &credential)
        })
    }

    fn list_open_proposals(&self, skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        // No known scope / no credential for this skill ⇒ none visible (best-effort; the count never
        // errors out a pull).
        let Some((workspace_id, credential)) = self.skill_scope(skill_id) else {
            return Ok(Vec::new());
        };
        ensure_url_safe_ids(skill_id, &workspace_id)?;
        let url = format!(
            "{}/v1/workspaces/{}/skills/{}/proposals",
            self.base_url, workspace_id, skill_id
        );
        match self.bearer_get(&url, &credential) {
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

    fn bind_skill(&self, workspace_id: &str, skill_id: &str) {
        // The read-lane twin of the delivery-lane bind (same map, same object): a catalog-resolved
        // read target (a review verdict on a skill with no follow entry) authenticates once bound.
        self.bind(workspace_id, skill_id);
    }
}

impl crate::plane::DeliverySource for UreqPlane {
    fn bind_skill(&self, workspace_id: &str, skill_id: &str) {
        // A brand-new arrival is absent from the `follows.json`-derived per-skill map; the device
        // credential already authenticates it (membership IS the authorization), so teach the read
        // transport the workspace scope before its first version fetch.
        self.bind(workspace_id, skill_id);
    }

    fn fetch_delivery(
        &self,
        workspace_id: &str,
    ) -> Result<crate::plane::DeliverySnapshot, PlaneError> {
        let cred = self.credential.clone().ok_or(PlaneError::NotFound)?;
        ensure_url_safe_ids("delivery", workspace_id)?;
        let url = format!("{}/v1/workspaces/{}/delivery", self.base_url, workspace_id);
        let body = self.bearer_get(&url, &cred)?;
        let wire: topos_types::requests::WireDelivery = serde_json::from_slice(&body)
            .map_err(|e| PlaneError::Malformed(format!("delivery body: {e}")))?;
        let link_status = LinkStatus::from_wire(wire.effective_status());
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
            });
        }
        Ok(crate::plane::DeliverySnapshot {
            skills,
            proposals_awaiting: wire.proposals_awaiting,
            notices: wire.notices,
            staleness_window_ms: wire.staleness_window_ms,
            link_status,
        })
    }

    fn ack_notices(&self, workspace_id: &str, ids: &[String]) -> Result<(), PlaneError> {
        let cred = self.credential.clone().ok_or(PlaneError::NotFound)?;
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
        let cred = self.credential.clone().ok_or(PlaneError::NotFound)?;
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

// =================================================================================================
// UreqDeviceClient — the real SESSION-LANE transport (sibling of the read-lane `UreqPlane`). One
// client speaks every route a session drives: the UNAUTHENTICATED login flow
// (`POST /v1/login/authorize` + `POST /v1/login/token`) and the CREDENTIALED routes — the
// governance invitation POST + the session self-end (`DELETE /v1/session`), the four contribute
// writes (publish / propose / revert / review), the workspace-catalog GET (`list --remote`), the
// profile row ops, and the member-scoped directory reads — each riding `Authorization: Bearer
// <session credential>` (the workspace-scoped credential; the server resolves credential →
// live session → person → seat). Every terminal protocol outcome of a write comes back as the
// all-outcome **200 envelope**. The flow code and the credential are sensitive — never logged or
// put in an error.
// =================================================================================================

/// The blocking `ureq` session-lane transport (`EnrollSource` + `GovernanceSource` +
/// `ContributeSource` + `CatalogSource` + `DirectorySource`). Holds the base URL, one configured
/// agent, and ONE session's workspace-scoped Bearer credential. The login-flow routes are
/// unauthenticated (they mint the credential the caller then stores); login-only callers pass
/// `None`.
pub(crate) struct UreqDeviceClient {
    base_url: String,
    /// **SECRET** — one session's workspace-scoped Bearer credential (`None` = signed out /
    /// login-only).
    credential: Option<String>,
    agent: ureq::Agent,
}

impl std::fmt::Debug for UreqDeviceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The agent is not Debug; the credential is secret — print only the safe shape.
        f.debug_struct("UreqDeviceClient")
            .field("base_url", &self.base_url)
            .field("credentialed", &self.credential.is_some())
            .finish_non_exhaustive()
    }
}

impl UreqDeviceClient {
    /// Build the transport against `base_url` (trailing slash trimmed) with the session credential,
    /// over the same agent configuration as [`UreqPlane`] (status-as-error OFF + the connect/recv/body
    /// timeouts). Login-only callers pass `None` (those routes are unauthenticated).
    pub(crate) fn new(base_url: String, credential: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            credential,
            agent: ureq::Agent::new_with_config(agent_config()),
        }
    }

    /// The device's Bearer credential, or a typed "not enrolled" error (mirroring the un-enrolled
    /// write refusal — a request without a credential must never reach the plane). `_workspace_id`
    /// documents the call site's scope; the ONE credential serves every workspace the person's seats
    /// reach (the server authorizes per request).
    fn credential_for(&self, _workspace_id: &str) -> Result<&str, ClientError> {
        self.credential.as_deref().ok_or_else(|| {
            ClientError::Enrollment(
                "not enrolled; run `topos login <workspace-address>` first".into(),
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

    /// POST a contribute write under the device's Bearer credential and map the all-outcome **200
    /// envelope** to a [`WriteReceipt`]. The four verbs differ only by `path` + body type; the op kind is
    /// derived from the route server-side, and the acting device is the credential's registry row (never a
    /// body field). A missing credential is refused BEFORE any send.
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
    fn fetch_card(&self, url: &str) -> Result<WireProtocolCard, ClientError> {
        // Ask for the machine contract EXPLICITLY: the route content-negotiates, and anything not
        // asking for JSON is served the human page instead (ureq's default Accept is `*/*`).
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

    fn device_auth_start(
        &self,
        workspace: &str,
        requested_name: &str,
        invite_token: Option<&str>,
    ) -> Result<DeviceAuthStart, ClientError> {
        let body = serde_json::to_value(DeviceAuthStartRequest {
            requested_name: requested_name.to_owned(),
            workspace: workspace.to_owned(),
            invite_token: invite_token.map(str::to_owned),
        })
        .map_err(|e| ClientError::Corrupt(format!("authorize body: {e}")))?;
        let url = format!("{}/v1/login/authorize", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, "device authorize")?;
        if classify(status) != HttpClass::Ok {
            return Err(ClientError::Plane(format!(
                "device authorize: HTTP {status}"
            )));
        }
        let resp: DeviceAuthStartResponse = serde_json::from_slice(&bytes).map_err(|e| {
            ClientError::WireInvalid(format!("authorize response is malformed: {e}"))
        })?;
        Ok(DeviceAuthStart {
            device_code: resp.device_code,
            user_code: resp.user_code,
            verification_uri: resp.verification_uri,
            expires_in_secs: resp.expires_in_secs,
            interval_secs: resp.interval_secs,
        })
    }

    fn device_auth_poll(&self, device_code: &str) -> Result<DeviceAuthPoll, ClientError> {
        // The body carries the SECRET device code — a serialize failure never echoes it, and the
        // response mapping is the pure `map_poll_response` (unit-tested without a socket).
        let body = serde_json::to_value(DeviceAuthPollRequest {
            device_code: device_code.to_owned(),
        })
        .map_err(|_| ClientError::Corrupt("poll body: could not serialize".to_owned()))?;
        let url = format!("{}/v1/login/token", self.base_url);
        let (status, bytes) = self.post_json(&url, &body, "device token poll")?;
        map_poll_response(status, &bytes)
    }
}

/// Map a `POST /v1/login/token` response to the typed [`DeviceAuthPoll`]. A `granted` poll must
/// carry the credential + session id + workspace (the promoted flow code IS the credential — there
/// is no second mint round-trip); the workspace id is validated at this wire boundary (it later
/// keys URL splices + the session row). **Pure** (status + bytes in), so every arm is unit-tested
/// without a socket.
fn map_poll_response(status: u16, bytes: &[u8]) -> Result<DeviceAuthPoll, ClientError> {
    if classify(status) != HttpClass::Ok {
        return Err(ClientError::Plane(format!(
            "device token poll: HTTP {status}"
        )));
    }
    let resp: DeviceAuthPollResponse = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("poll response is malformed: {e}")))?;
    Ok(match resp.status {
        DeviceAuthPollStatus::Pending => DeviceAuthPoll::Pending,
        DeviceAuthPollStatus::Denied => DeviceAuthPoll::Denied,
        DeviceAuthPollStatus::Expired => DeviceAuthPoll::Expired,
        DeviceAuthPollStatus::Granted => {
            // The SESSION wire grants `session_id`/`session_status`; the retired device wire
            // granted `device_id`/`link_status` — accept either pair (one id, one status).
            let id = resp.session_id.clone().or(resp.device_id);
            let (Some(credential), Some(device_id), Some(workspace)) =
                (resp.credential, id, resp.workspace)
            else {
                // `granted` without its grant halves is a malformed response, not a silent re-poll.
                return Err(ClientError::WireInvalid(
                    "a granted login poll carried no credential/session/workspace".into(),
                ));
            };
            // The wire boundary: the workspace id becomes a URL segment + a user.json key, so it must
            // be a safe path component (a traversal id is the corrupt family's WIRE flavor).
            crate::id::validate_workspace_id(&workspace.workspace_id)
                .map_err(crate::id::wire_flavor)?;
            DeviceAuthPoll::Granted(EnrolledGrant {
                credential,
                device_id,
                workspace: EnrolledWorkspace {
                    workspace_id: workspace.workspace_id,
                    name: workspace.name,
                    display_name: workspace.display_name,
                },
                session_id: resp.session_id,
                hint: resp.hint.map(|h| crate::plane::GrantHint {
                    kind: h.kind,
                    name: h.name,
                }),
                // The session's born status (the retired link spelling accepted as a fallback);
                // an older producer omits both (⇒ active).
                link_status: LinkStatus::from_wire(
                    resp.session_status
                        .as_deref()
                        .or(resp.link_status.as_deref()),
                ),
            })
        }
    })
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

    fn revoke_session(&self) -> Result<(), ClientError> {
        // The one-session sign-out — the Bearer credential itself names the session; the server
        // deletes the row (reported state cascading) and a retry answers the uniform 404.
        let credential = self.credential_for("")?;
        let url = format!("{}/v1/session", self.base_url);
        let resp = self
            .agent
            .delete(&url)
            .header("authorization", format!("Bearer {credential}"))
            .call()
            .map_err(|e| ClientError::Plane(format!("session revoke: {e}")))?;
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => {
                let bytes = read_body(resp).map_err(plane_err)?;
                map_row_envelope(&bytes)
            }
            // The uniform 404: already ended (or a dead credential) — the caller treats it as
            // already-signed-out and proceeds with the local delete.
            HttpClass::NotFound => Err(ClientError::TargetNotFound {
                target: "session".to_owned(),
            }),
            HttpClass::NotModified | HttpClass::Other => {
                Err(ClientError::Plane(format!("session revoke: HTTP {status}")))
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
        // No stored device credential ⇒ Unavailable (the caller degrades it to a warning), never a
        // request without a credential.
        let Some(credential) = self.credential.as_deref() else {
            return Err(PlaneError::Unavailable(format!(
                "not enrolled; no credential to read workspace {workspace_id}"
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

impl UreqDeviceClient {
    /// A row DELETE whose OK envelope's `data.status` names HOW the removal settled (the profile
    /// routes: `removed` / `excluded` / `not_in_profile`) — the receipt phrases the inverse from
    /// it. Everything else mirrors [`Self::row_op`].
    fn row_op_status(
        &self,
        method: RowMethod,
        workspace_id: &str,
        path: &str,
        what: &str,
        target: &str,
    ) -> Result<crate::plane::ProfileRemoval, ClientError> {
        let credential = self.credential_for(workspace_id)?;
        let url = format!("{}{path}", self.base_url);
        let auth = format!("Bearer {credential}");
        let resp = match method {
            RowMethod::Delete => self
                .agent
                .delete(&url)
                .header("authorization", &auth)
                .call()
                .map_err(|e| ClientError::Plane(format!("{what}: {e}")))?,
            RowMethod::Put | RowMethod::Post => {
                return Err(ClientError::Corrupt(format!(
                    "{what}: status row ops are DELETE-shaped"
                )));
            }
        };
        let status = resp.status().as_u16();
        match classify(status) {
            HttpClass::Ok => {
                let bytes = read_body(resp).map_err(plane_err)?;
                map_row_envelope(&bytes)?;
                let settled = serde_json::from_slice::<JsonEnvelope>(&bytes)
                    .ok()
                    .and_then(|env| {
                        env.data
                            .get("status")
                            .and_then(|s| s.as_str())
                            .map(str::to_owned)
                    });
                Ok(match settled.as_deref() {
                    Some("excluded") => crate::plane::ProfileRemoval::Excluded,
                    Some("not_in_profile") => crate::plane::ProfileRemoval::NotInProfile,
                    _ => crate::plane::ProfileRemoval::Removed,
                })
            }
            HttpClass::NotFound => Err(ClientError::TargetNotFound {
                target: target.to_owned(),
            }),
            HttpClass::NotModified | HttpClass::Other => {
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

    fn profile_include_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
        pin: Option<&str>,
    ) -> Result<(), ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        let body = pin.map(|p| serde_json::json!({ "pin": p }));
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/profile/skills/{skill_id}"),
            body.as_ref(),
            "profile include",
            skill_id,
        )
    }

    fn profile_remove_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<crate::plane::ProfileRemoval, ClientError> {
        ensure_safe_ids_client(skill_id, workspace_id)?;
        self.row_op_status(
            RowMethod::Delete,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/profile/skills/{skill_id}"),
            "profile remove",
            skill_id,
        )
    }

    fn profile_include_channel(
        &self,
        workspace_id: &str,
        channel: &str,
    ) -> Result<(), ClientError> {
        ensure_safe_ids_client("profile", workspace_id)?;
        ensure_url_safe_channel(channel)?;
        self.row_op(
            RowMethod::Put,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/profile/channels/{channel}"),
            None,
            "profile include",
            channel,
        )
    }

    fn profile_remove_channel(
        &self,
        workspace_id: &str,
        channel: &str,
    ) -> Result<crate::plane::ProfileRemoval, ClientError> {
        ensure_safe_ids_client("profile", workspace_id)?;
        ensure_url_safe_channel(channel)?;
        self.row_op_status(
            RowMethod::Delete,
            workspace_id,
            &format!("/v1/workspaces/{workspace_id}/profile/channels/{channel}"),
            "profile remove",
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

/// Parse an unauthenticated card-read body: the constant protocol card (its `card` discriminant is
/// checked, and the declared API base must be non-empty). **Pure**, unit-tested with canned bytes.
fn parse_card(bytes: &[u8]) -> Result<WireProtocolCard, ClientError> {
    if let Ok(card) = serde_json::from_slice::<WireProtocolCard>(bytes)
        && card.card == "topos-protocol-card"
    {
        if card.api_base_url.trim().is_empty() {
            return Err(ClientError::WireInvalid(
                "the protocol card declares no API base URL".into(),
            ));
        }
        return Ok(card);
    }
    Err(ClientError::WireInvalid(
        "the address did not answer the topos protocol card — is it a topos server?".into(),
    ))
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
    let JsonEnvelope {
        ok,
        data,
        receipt,
        error,
        ..
    } = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::WireInvalid(format!("write envelope is malformed: {e}")))?;
    // The pointer record is present ONLY when a pointer actually moved. NEEDS_REVIEW, an OK `review
    // --reject` (the plane returns OK with no record → data `{}`), and every failure carry `{}`; parse
    // leniently so a valid reject is never wrongly rejected as Corrupt.
    let wire_record = serde_json::from_value::<WireCurrentRecord>(data).ok();
    match receipt {
        Some(receipt) => Ok(WriteReceipt {
            receipt: Some(receipt),
            error,
            wire_record,
        }),
        // A receipt-LESS envelope is a valid TERMINAL answer only when it is an `ok:false` DENIED that
        // still parsed a flat `error` — an old server (or an already-stored wedged receipt a same-`op_id`
        // replay re-serves) that never attached a receipt to its DENIED. Mapping it to a settled receipt
        // is the typed way out of a wedged op-WAL (`run_write` deletes it). The escape is DENIED-only:
        // a receipt-less RETRYABLE_FAILURE / UNAVAILABLE / anything else stays `WireInvalid` (the WAL is
        // kept — the op may yet land, and settling it would silently drop a retryable write). An
        // `ok:true` success with no receipt is genuinely corrupt, and stays `WireInvalid` too.
        None => match (ok, error) {
            (false, Some(e)) if e.outcome == TerminalOutcome::Denied => Ok(WriteReceipt {
                receipt: None,
                error: Some(e),
                wire_record,
            }),
            _ => Err(ClientError::WireInvalid(
                "a write 200 carried no receipt".to_owned(),
            )),
        },
    }
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
// UreqVersionProbe — the passive version check's transport: ONE redirect-disabled GET of the public
// `releases/latest` URL on a deliberately short, hard timeout. The 302's `Location` header is the
// whole answer (no API, no auth, no JSON body); every failure is `None` — the nag is silent by
// contract, so this transport never errors. Its OWN agent (not `agent_config()`): the plane
// transports follow redirects and tolerate slow bodies; the probe must do neither.
// =================================================================================================

/// The URL whose 302 names the latest release (the same public coordinates `UreqReleases` speaks).
const VERSION_PROBE_URL: &str = "https://github.com/topos-sh/topos/releases/latest";

/// The hard ceiling on the probe's one request — a passive nag must never make a command feel slow.
const VERSION_PROBE_TIMEOUT_SECS: u64 = 2;

/// The blocking `ureq` version probe: redirects disabled, 2s global timeout.
pub(crate) struct UreqVersionProbe {
    agent: ureq::Agent,
}

impl UreqVersionProbe {
    pub(crate) fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // Redirects DISABLED — the redirect response itself comes back to the caller.
            .max_redirects(0)
            // ONE global ceiling over the whole request (DNS + connect + TLS + response), so the
            // probe can never hold a finished command hostage.
            .timeout_global(Some(Duration::from_secs(VERSION_PROBE_TIMEOUT_SECS)))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl crate::release::ReleaseProbe for UreqVersionProbe {
    fn latest_release_location(&self) -> Option<String> {
        let resp = self
            .agent
            .get(VERSION_PROBE_URL)
            .header("User-Agent", RELEASE_USER_AGENT)
            .call()
            .ok()?;
        if !resp.status().is_redirection() {
            return None;
        }
        resp.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
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
            expected_generation: Some(1),
            current_generation: Some(2),
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
                generation: 2,
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
            wr.receipt
                .as_ref()
                .expect("an OK move carries a receipt")
                .bundle_digest
                .as_deref(),
            Some("b".repeat(64).as_str())
        );
        assert!(wr.error.is_none());
    }

    #[test]
    fn map_write_envelope_receiptless_denied_is_a_terminal_receipt_not_wire_invalid() {
        // An `ok:false` DENIED envelope that carries a parseable flat `error` but NO receipt (an old server,
        // or an already-stored wedged receipt a same-op_id replay re-serves) is a valid TERMINAL answer —
        // NOT WireInvalid. Mapping it to a receipt-less `WriteReceipt` is what lets `run_write` settle
        // (delete) a wedged op-WAL instead of replaying the receipt-less body forever.
        let err = WireError {
            code: "FOUR_EYES_REQUIRED".to_owned(),
            outcome: TerminalOutcome::Denied,
            retryable: false,
            affected: topos_types::Affected::default(),
            expected_generation: None,
            current_generation: None,
            context: serde_json::json!({}),
            next_actions: Vec::new(),
        };
        let bytes = envelope_bytes(&JsonEnvelope {
            schema_version: 1,
            command: "reviews".to_owned(),
            ok: false,
            data: serde_json::json!({}),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            receipt: None,
            error: Some(err),
        });
        let wr =
            map_write_envelope(200, &bytes).expect("a receipt-less DENIED is a terminal receipt");
        assert!(wr.receipt.is_none(), "no receipt was attached");
        assert_eq!(
            wr.outcome(),
            TerminalOutcome::Denied,
            "outcome reads the flat error"
        );
        assert_eq!(
            wr.error.as_ref().map(|e| e.code.as_str()),
            Some("FOUR_EYES_REQUIRED")
        );
    }

    #[test]
    fn map_write_envelope_receiptless_non_denied_stays_wire_invalid() {
        // The receipt-less escape is DENIED-ONLY. A receipt-less RETRYABLE_FAILURE (or any other
        // outcome) must stay WireInvalid so `run_write` KEEPS the WAL — settling it would silently
        // drop a retryable write whose op may yet land on the same op_id replay.
        for outcome in [
            TerminalOutcome::RetryableFailure,
            TerminalOutcome::Unavailable,
            TerminalOutcome::Conflict,
            TerminalOutcome::PermanentFailure,
        ] {
            let err = WireError {
                code: "RETRYABLE_FAILURE".to_owned(),
                outcome,
                retryable: true,
                affected: topos_types::Affected::default(),
                expected_generation: None,
                current_generation: None,
                context: serde_json::json!({}),
                next_actions: Vec::new(),
            };
            let bytes = envelope_bytes(&JsonEnvelope {
                schema_version: 1,
                command: "reviews".to_owned(),
                ok: false,
                data: serde_json::json!({}),
                warnings: Vec::new(),
                next_actions: Vec::new(),
                receipt: None,
                error: Some(err),
            });
            let e = map_write_envelope(200, &bytes)
                .expect_err("a receipt-less non-DENIED outcome is not a settled answer");
            assert_eq!(e.code(), "CORRUPT_STATE", "{outcome:?}");
        }
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
            expected_generation: Some(1),
            current_generation: Some(5),
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
            Some(5),
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

    // ---- The device-auth poll mapping + the id boundary at the wire parse. ----

    fn poll_json(status: &str, workspace_id: Option<&str>) -> Vec<u8> {
        let mut body = serde_json::json!({ "status": status });
        if let Some(ws) = workspace_id {
            body["credential"] = serde_json::json!("devc_secret");
            body["device_id"] = serde_json::json!("dev_1");
            body["workspace"] = serde_json::json!({
                "workspace_id": ws,
                "name": "acme",
                "display_name": "Acme",
            });
        }
        serde_json::to_vec(&body).expect("serialize poll body")
    }

    #[test]
    fn map_poll_response_maps_every_arm() {
        assert!(matches!(
            map_poll_response(200, &poll_json("pending", None)).unwrap(),
            DeviceAuthPoll::Pending
        ));
        assert!(matches!(
            map_poll_response(200, &poll_json("denied", None)).unwrap(),
            DeviceAuthPoll::Denied
        ));
        assert!(matches!(
            map_poll_response(200, &poll_json("expired", None)).unwrap(),
            DeviceAuthPoll::Expired
        ));
        let granted = map_poll_response(200, &poll_json("granted", Some("w_acme"))).unwrap();
        let DeviceAuthPoll::Granted(grant) = granted else {
            panic!("expected a granted poll");
        };
        assert_eq!(grant.credential, "devc_secret");
        assert_eq!(grant.device_id, "dev_1");
        assert_eq!(grant.workspace.workspace_id, "w_acme");
        assert_eq!(grant.workspace.name, "acme");
        // The credential never surfaces in Debug.
        assert!(!format!("{grant:?}").contains("devc_secret"));
        // A non-200 is a transport-shaped error, never a silent pending.
        assert!(matches!(
            map_poll_response(500, b"{}").unwrap_err(),
            ClientError::Plane(_)
        ));
    }

    #[test]
    fn map_poll_response_refuses_a_bare_or_hostile_grant() {
        // `granted` without its credential/device/workspace halves is malformed, not a re-poll.
        let err = map_poll_response(200, &poll_json("granted", None)).unwrap_err();
        assert!(matches!(err, ClientError::WireInvalid(_)), "got {err:?}");
        // A hostile workspace id (it later keys URL splices + user.json) fails the WHOLE poll, as the
        // WIRE flavor of the corrupt family (same CORRUPT_STATE code; the safe message names the plane).
        for bad in ["../../x", "a/b", "A", "", ".", ".."] {
            let err = map_poll_response(200, &poll_json("granted", Some(bad))).unwrap_err();
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
    }

    #[test]
    fn parse_card_accepts_the_protocol_card_and_refuses_anything_else() {
        let ok = parse_card(
            br#"{"schema_version":1,"card":"topos-protocol-card","api_base_url":"https://topos.sh/api"}"#,
        )
        .expect("a clean card parses");
        assert_eq!(ok.api_base_url, "https://topos.sh/api");
        // A card with an empty base, a wrong discriminant, or a non-card body is refused typed.
        for bad in [
            br#"{"schema_version":1,"card":"topos-protocol-card","api_base_url":"  "}"#.as_slice(),
            br#"{"schema_version":1,"card":"something-else","api_base_url":"https://x"}"#
                .as_slice(),
            br#"{"hello":"world"}"#.as_slice(),
            b"not json".as_slice(),
        ] {
            assert!(matches!(
                parse_card(bad),
                Err(ClientError::WireInvalid(_)) | Err(ClientError::Plane(_))
            ));
        }
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
