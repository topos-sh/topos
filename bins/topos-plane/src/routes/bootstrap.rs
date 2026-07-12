//! `GET /i/{token}` — the UNAUTHENTICATED bootstrap read. An `/i/` link now resolves to ONE kind of token:
//! a one-time admin CLAIM (the workspace-to-be's name, no skills, `enrollment_method = "admin_claim"`) — the
//! self-host first-boot + cloud break-glass door. (The tokened INVITE door was interred: invitations became
//! roster writes with no `/i/` link, and enrollment is by workspace ADDRESS.) A dead/unknown/consumed token
//! is the single indistinguishable 404.
//!
//! ONE resource, TWO representations, negotiated on `Accept`:
//! - a request that asks for JSON (`application/json` / `application/*` / a `+json` type — or sends no
//!   `Accept` at all) gets the versioned [`BootstrapData`] payload — the machine contract the `topos`
//!   client drives (it sends `Accept: application/json` explicitly);
//! - anything else (`*/*` from curl and agent web-fetch tools, `text/html` from a browser) gets a
//!   **markdown agent-instruction document** ([`bootstrap_doc`]) — the paste-a-link-to-your-agent door:
//!   the human hand-off first, then install `topos` if missing, redeem the claim. Served as `text/plain` —
//!   browsers DISPLAY that inline while `text/markdown` triggers a download (the GitHub-raw precedent), and
//!   the document is the browser face too: there is no separate HTML page, here or on a hosted front.
//!   Errors (404/429/500) stay the uniform JSON envelope on every representation.
//!
//! Both 200s carry `Cache-Control: no-store` + `Vary: Accept` (a token-bearing URL must never be cached)
//! and `X-Robots-Tag: noindex`. The claim token is the LIVE one-time bearer owner capability, so the JSON
//! `token_id` is an empty placeholder and the markdown never echoes the token or a link (a body-logging
//! proxy learns nothing; the fetcher already holds the URL it fetched).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};

use super::bootstrap_doc;
use crate::state::PlaneState;
use crate::wire::{self, error::PlaneHttpError, map};

#[utoipa::path(
    get,
    path = "/i/{token}",
    tag = "enrollment",
    params(("token" = String, Path, description = "The opaque `/i/<token>` one-time admin-claim token.")),
    responses(
        (status = 200, description = "The claim bootstrap (workspace-to-be + plane API base; no bytes, no role, no trust root; enrollment_method \"admin_claim\", no skills). Content-negotiated: an Accept asking for JSON (or absent) gets this payload; anything else (curl `*/*`, a browser) gets a markdown agent-instruction document rendered from the same data, served as `text/plain` so browsers display it inline.", body = topos_types::bootstrap::BootstrapData),
        (status = 404, description = "No such claim, or it is revoked/consumed/expired (always the JSON envelope, any Accept).", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn read_bootstrap(
    State(state): State<PlaneState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    // The server clock (epoch-ms) enforces the claim expiry inside the authority. Only the CLAIM table is
    // probed now (the invite table is gone); a consumed/expired/unknown token is the uniform 404.
    let now = wire::now_utc().1;
    let bootstrap = state.authority().read_claim_bootstrap(&token, now).await?;
    // The claim token is the LIVE one-time bearer owner capability — the response body must never echo it,
    // so the JSON `token_id` is the empty placeholder and the markdown points at the URL the fetcher holds.
    let data = map::bootstrap_to_wire("", bootstrap);

    let mut response = if wants_json(&headers) {
        Json(&data).into_response()
    } else {
        let doc = bootstrap_doc::agent_instructions(&data);
        // text/plain, deliberately: browsers display it inline (the whole point of the browser face
        // being this document), where text/markdown would trigger a download.
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            doc,
        )
            .into_response()
    };
    // A token-bearing URL's 200 must never sit in a shared cache, and the representation varies on Accept.
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::VARY, HeaderValue::from_static("accept"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    Ok(response)
}

/// Whether the request asks for the JSON representation: an `Accept` naming `application/json`,
/// `application/*`, or any `+json` suffix type — or NO `Accept` header at all (the conservative
/// machine-contract default for bare HTTP libraries). `*/*` alone (curl, agent web-fetch) and browser
/// accepts get the markdown door. q-values are deliberately ignored (a substring dispatch, not a full
/// RFC 9110 negotiation — the two consumers are cleanly disjoint in practice). Shared with the protocol
/// card fallback ([`super::card`]) so both content-negotiated GETs dispatch identically.
pub(crate) fn wants_json(headers: &HeaderMap) -> bool {
    // Consider EVERY Accept header (a client may send several; `get` would read only the first).
    let mut any = false;
    for value in headers.get_all(header::ACCEPT) {
        any = true;
        if let Ok(accept) = value.to_str() {
            let accept = accept.to_ascii_lowercase();
            if accept.contains("application/json")
                || accept.contains("application/*")
                || accept.contains("+json")
            {
                return true;
            }
        }
    }
    !any
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::wants_json;

    /// The dispatch table: explicit JSON (the topos client), `application/*`, `+json` suffixes, and an
    /// ABSENT Accept all take the JSON door; curl's bare `*/*`, browsers, and markdown/plain accepts take
    /// the agent-instruction door.
    #[test]
    fn accept_dispatch_matches_the_two_consumers() {
        let json_cases = [
            "application/json",
            "application/json; charset=utf-8",
            "application/*",
            "application/vnd.topos+json",
            "application/json, text/plain, */*",
        ];
        for accept in json_cases {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT, HeaderValue::from_static(accept));
            assert!(wants_json(&h), "expected JSON for {accept:?}");
        }
        let markdown_cases = [
            "*/*",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "text/markdown",
            "text/plain",
        ];
        for accept in markdown_cases {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT, HeaderValue::from_static(accept));
            assert!(!wants_json(&h), "expected markdown for {accept:?}");
        }
        // No Accept header at all ⇒ the machine-contract default.
        assert!(wants_json(&HeaderMap::new()));
    }
}
