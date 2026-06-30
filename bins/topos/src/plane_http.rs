//! The real plane transport: a blocking `ureq` (3, rustls+ring) [`PlaneSource`] that feeds the already-built
//! pull engine, plus the on-disk [`FollowSource`].
//!
//! [`UreqPlane`] is a **dumb transport** — it speaks the B3 wire (`GET /v1/current/{read_token}` with the
//! commit-sensitive conditional-GET headers; `GET …/versions/{id}` + per-blob `GET …/bundles/{id}` under a
//! Bearer token) and verifies each blob's `sha256 == object_id`, but it does **not** verify the pointer
//! signature — the engine does that against `ctx.plane_key`. Status mapping ([`classify`]) and version
//! assembly ([`build_fetched_version`]) are factored as pure functions so the wire logic is unit-tested
//! without a live server (the full loopback round-trip is the next leaf).
//!
//! The client stays **sync + tokio-free**: `ureq` brings its own blocking TLS stack, so this adds no
//! `plane-store`/`sqlx`/`tokio` edge (`check-arch` holds the line).

use std::collections::HashMap;
use std::time::Duration;

use topos_core::digest::{self, FileMode, to_hex};
use topos_types::SignedCurrentRecord;
use topos_types::requests::{WireFileMode, WireVersionMeta};

use crate::plane::{
    FetchedFile, FetchedVersion, FollowContext, FollowSource, KnownCurrent, PlaneError,
    PlaneSource, PointerFetch,
};

/// Fail fast establishing a connection (a dead plane must not hang the session-start sweep).
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Fail fast waiting for the response head (body streaming is uncapped so a large legit blob isn't cut off).
const RECV_RESPONSE_TIMEOUT_SECS: u64 = 30;
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
    /// Build the transport: one blocking agent (rustls+ring, sane connect/recv timeouts, status-as-error
    /// OFF so a 304/404/5xx comes back as an inspectable status rather than an error variant) + the cred map.
    /// `base_url`'s trailing slash is trimmed so URL joins never double up.
    pub(crate) fn new(base_url: String, creds: HashMap<String, SkillCred>) -> Self {
        let config = ureq::Agent::config_builder()
            // Treat EVERY status (incl. 304 / 404 / 5xx) as a returned response, not an `Err`, so the status
            // mapping is uniform; only a genuine transport/timeout/TLS fault surfaces as `Err`.
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(CONNECT_TIMEOUT_SECS)))
            .timeout_recv_response(Some(Duration::from_secs(RECV_RESPONSE_TIMEOUT_SECS)))
            .build();
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            creds,
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// A `GET` carrying `Authorization: Bearer <read_token>` (versions + bundles). Returns the raw body on
    /// 2xx, [`PlaneError::NotFound`] on 404, [`PlaneError::Unavailable`] on transport / any other status.
    /// `url` here never contains the secret (the token is in the header), so it is safe in the error text.
    fn bearer_get(&self, url: &str, read_token: &str) -> Result<Vec<u8>, PlaneError> {
        let resp = self
            .agent
            .get(url)
            .header("authorization", format!("Bearer {read_token}"))
            .call()
            .map_err(|e| PlaneError::Unavailable(format!("GET {url}: {e}")))?;
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
            .map_err(|e| PlaneError::Unavailable(format!("get_current {skill_id}: {e}")))?;
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
/// [`crate::enroll::follow_contexts`]). `proposals_awaiting` is `0` until proposals/review land client-side.
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
    fn proposals_awaiting(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topos_types::requests::WireVersionFile;

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
}
