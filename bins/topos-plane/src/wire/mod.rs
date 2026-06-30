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
use plane_store::FileMode;
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
