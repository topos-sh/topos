//! The edge: the request/response wire helpers (parse → domain, domain → wire) the thin handlers share.
//!
//! Everything that turns an HTTP request into the authority's domain inputs (and a domain receipt back into
//! the canonical [`JsonEnvelope`](topos_types::JsonEnvelope)) lives here, never in a handler body — so the
//! handlers stay a flat "parse → call the authority → serialize" with no trust decision and no string-format
//! drift. [`error`] owns the non-2xx mapping; [`map`] owns the receipt/version/candidate mappers.

pub(crate) mod error;
pub(crate) mod map;

use axum::Json;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, header};
use base64::Engine as _;
use plane_store::{FileMode, OpId};
use topos_types::requests::WireFileMode;

use error::PlaneHttpError;

/// A JSON body extractor that fails into a [`PlaneHttpError`] (so a malformed body is the SAME envelope-
/// shaped 400 as every other error), unlike the bare `axum::Json` whose rejection is plain text. Used as the
/// LAST handler argument (it consumes the request body).
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = PlaneHttpError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(PlaneHttpError::BadBody(rejection.body_text())),
        }
    }
}

/// Parse the WRITE credential — the `Topos-Device-Signature` header (base64url-unpadded, 86 chars) → the raw
/// 64-byte Ed25519 signature. Missing or malformed → a 400 (`BadDeviceSignature`); verification itself is the
/// authority's, server-side.
pub(crate) fn device_signature(headers: &HeaderMap) -> Result<[u8; 64], PlaneHttpError> {
    let raw = headers
        .get("topos-device-signature")
        .ok_or(PlaneHttpError::BadDeviceSignature)?;
    let text = raw
        .to_str()
        .map_err(|_| PlaneHttpError::BadDeviceSignature)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .map_err(|_| PlaneHttpError::BadDeviceSignature)?;
    bytes
        .try_into()
        .map_err(|_| PlaneHttpError::BadDeviceSignature)
}

/// Parse + validate a WRITE `op_id` at the edge. The authority binds `op_id` as 16 bytes in the device-op
/// preimage and, on the write paths, **ingests + leases the candidate BEFORE** that bind — so a path-safe but
/// non-canonical-UUID `op_id` accepted here would let an unauthenticated malformed request pin the uploaded
/// objects when the later parse fails (the failure path does not release the lease). Reject anything but the
/// canonical lowercase-hyphenated UUID with a 400 here (exactly matching plane-store's own `op_id` parse), so
/// a bad `op_id` never reaches ingest.
pub(crate) fn parse_op_id(s: &str) -> Result<OpId, PlaneHttpError> {
    let canonical_uuid = uuid::Uuid::parse_str(s).is_ok_and(|u| u.as_hyphenated().to_string() == s);
    if !canonical_uuid {
        return Err(PlaneHttpError::BadId(
            "op_id must be a canonical hyphenated UUID".to_owned(),
        ));
    }
    OpId::parse(s).map_err(|e| PlaneHttpError::BadId(e.to_string()))
}

/// Parse the READ credential — `Authorization: Bearer <token>` → the token string. A missing/blank/no-scheme
/// credential is the single indistinguishable `MissingReadCredential` (→ 404, never 401/403): the plane never
/// reveals whether a token, workspace, or skill exists.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Result<String, PlaneHttpError> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .ok_or(PlaneHttpError::MissingReadCredential)?;
    let text = raw
        .to_str()
        .map_err(|_| PlaneHttpError::MissingReadCredential)?;
    let token = text
        .strip_prefix("Bearer ")
        .or_else(|| text.strip_prefix("bearer "))
        .ok_or(PlaneHttpError::MissingReadCredential)?
        .trim();
    if token.is_empty() {
        return Err(PlaneHttpError::MissingReadCredential);
    }
    Ok(token.to_owned())
}

/// Parse the reading device's key id — the `Topos-Device-Key-Id` header. A missing/blank/non-ASCII value is
/// the single indistinguishable `MissingReadCredential` (→ 404, never 400/401/403): a device-signed READ
/// never reveals whether a device, workspace, or membership exists (the same posture as [`bearer_token`]).
pub(crate) fn device_key_id_header(headers: &HeaderMap) -> Result<String, PlaneHttpError> {
    let raw = headers
        .get("topos-device-key-id")
        .ok_or(PlaneHttpError::MissingReadCredential)?;
    let text = raw
        .to_str()
        .map_err(|_| PlaneHttpError::MissingReadCredential)?
        .trim();
    if text.is_empty() {
        return Err(PlaneHttpError::MissingReadCredential);
    }
    Ok(text.to_owned())
}

/// Parse the catalog-READ credential — the `Topos-Device-Signature` header decoded EXACTLY as the write
/// routes decode it ([`device_signature`]: base64url-unpadded → the raw 64-byte Ed25519 signature). Unlike
/// the write path (a malformed signature there is an honest 400), a missing/malformed header on this READ is
/// the single indistinguishable `MissingReadCredential` (→ 404): the read must not distinguish a missing
/// credential from an unknown device / workspace / non-membership.
pub(crate) fn read_signature(headers: &HeaderMap) -> Result<[u8; 64], PlaneHttpError> {
    let raw = headers
        .get("topos-device-signature")
        .ok_or(PlaneHttpError::MissingReadCredential)?;
    let text = raw
        .to_str()
        .map_err(|_| PlaneHttpError::MissingReadCredential)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .map_err(|_| PlaneHttpError::MissingReadCredential)?;
    bytes
        .try_into()
        .map_err(|_: Vec<u8>| PlaneHttpError::MissingReadCredential)
}

/// Decode a base64url-unpadded raw 32-byte key (a device public key in an enrollment body) → `[u8; 32]`.
/// A bad alphabet or the wrong length is a 400 (`BadId`); the server then re-derives the device key id from
/// these bytes itself (a client-asserted id is never trusted).
pub(crate) fn base64url_key(s: &str) -> Result<[u8; 32], PlaneHttpError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .map_err(|_| PlaneHttpError::BadId("device_public_key must be base64url".to_owned()))?;
    bytes.try_into().map_err(|_: Vec<u8>| {
        PlaneHttpError::BadId("device_public_key must be 32 bytes".to_owned())
    })
}

/// The wire governance role → the kernel/authority role (1:1).
pub(crate) fn domain_role(role: topos_types::requests::WorkspaceRole) -> plane_store::Role {
    use topos_types::requests::WorkspaceRole;
    match role {
        WorkspaceRole::Owner => plane_store::Role::Owner,
        WorkspaceRole::Reviewer => plane_store::Role::Reviewer,
        WorkspaceRole::Member => plane_store::Role::Member,
    }
}

/// Decode exactly 64 hex characters into a 32-byte id (a commit/object id field in a request body). `None` on
/// any other length or a non-hex byte.
pub(crate) fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

/// The wire file mode → the kernel file mode (1:1).
pub(crate) fn domain_mode(mode: WireFileMode) -> FileMode {
    match mode {
        WireFileMode::Regular => FileMode::Regular,
        WireFileMode::Executable => FileMode::Executable,
    }
}

/// The kernel file mode → the wire file mode (1:1).
pub(crate) fn wire_mode(mode: FileMode) -> WireFileMode {
    match mode {
        FileMode::Regular => WireFileMode::Regular,
        FileMode::Executable => WireFileMode::Executable,
    }
}

/// The server clock: the RFC-3339 `created_at` string + the `now` in epoch milliseconds the handler stamps
/// onto every write (the client never supplies a wall clock). A retry replays the STORED `created_at`, so a
/// drifting clock between attempts never breaks byte-faithful idempotency.
pub(crate) fn now_utc() -> (String, i64) {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (rfc3339(dur.as_secs() as i64), dur.as_millis() as i64)
}

/// Format a UTC RFC-3339 timestamp (seconds precision, `Z`) from a Unix-epoch second count — no date crate.
fn rfc3339(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let sod = unix_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Howard Hinnant's `civil_from_days`: a Unix-epoch day count → `(year, month, day)`, proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month as u32, day as u32)
}
