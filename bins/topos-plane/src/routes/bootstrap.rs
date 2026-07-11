//! `GET /i/{token}` — the UNAUTHENTICATED bootstrap read. An `/i/` link resolves to EITHER an invite
//! (the workspace identity + offered skills + the plane API base to dial, **no bytes, no role, no trust root**)
//! OR a one-time admin CLAIM (the workspace-to-be's name, no skills, `enrollment_method = "admin_claim"`). The
//! two live in disjoint tables and are probed in sequence — a claim token can never resolve as an invite
//! nor vice versa — and every dead/unknown token of either kind is the single indistinguishable 404.
//!
//! ONE resource, TWO representations, negotiated on `Accept`:
//! - a request that asks for JSON (`application/json` / `application/*` / a `+json` type — or sends no
//!   `Accept` at all) gets the versioned [`BootstrapData`] payload — the machine contract the `topos`
//!   client drives (it sends `Accept: application/json` explicitly);
//! - anything else (`*/*` from curl and agent web-fetch tools, `text/html` from a browser) gets a
//!   **markdown agent-instruction document** ([`bootstrap_doc`]) — the paste-a-link-to-your-agent door:
//!   the human hand-off first, then install `topos` if missing, redeem the link, surface the
//!   verification URL to the human, land the offers per-digest. Served as `text/plain` — browsers
//!   DISPLAY that inline while `text/markdown` triggers a download (the GitHub-raw precedent), and the
//!   document is the browser face too: there is no separate HTML page, here or on a hosted front.
//!   Errors (404/429/500) stay the uniform JSON envelope on every representation.
//!
//! Both 200s carry `Cache-Control: no-store` + `Vary: Accept` (a token-bearing URL must never be cached)
//! and `X-Robots-Tag: noindex`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use plane_store::AuthorityError;

use super::bootstrap_doc;
use crate::state::PlaneState;
use crate::wire::{self, error::PlaneHttpError, map};

#[utoipa::path(
    get,
    path = "/i/{token}",
    tag = "enrollment",
    params(("token" = String, Path, description = "The opaque `/i/<token>` invite or admin-claim token.")),
    responses(
        (status = 200, description = "The bootstrap (workspace + plane API base; no bytes, no role, no trust root). A claim link carries enrollment_method \"admin_claim\" and no skills. Content-negotiated: an Accept asking for JSON (or absent) gets this payload; anything else (curl `*/*`, a browser) gets a markdown agent-instruction document rendered from the same data, served as `text/plain` so browsers display it inline.", body = topos_types::bootstrap::BootstrapData),
        (status = 404, description = "No such invite or claim, or it is revoked/consumed/expired (always the JSON envelope, any Accept).", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn read_invite_bootstrap(
    State(state): State<PlaneState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    // The server clock (epoch-ms) enforces the invite/claim expiry inside the authority.
    let now = wire::now_utc().1;
    // Try the invite table first, then the claim table — sequential probes over DISJOINT stores (an
    // invite resolver never touches claims and vice versa), folding both misses into one uniform 404.
    // The echoed `invite.token_id` differs by door: an INVITE echoes the token the client used (the
    // shareable link's own tail, non-secret by design), but a CLAIM token is the LIVE one-time bearer
    // owner capability — repeating it in the response body would mint a second custody surface (a
    // body-logging proxy captures it), and the claim client only ever uses the token it parsed from the
    // link, never the echo. The claim branch therefore emits an empty placeholder — and the markdown
    // representation holds the same line (no token, no link echo).
    let (bootstrap, echoed_token_id) =
        match state.authority().read_invite_bootstrap(&token, now).await {
            Ok(bootstrap) => (bootstrap, token.as_str()),
            Err(AuthorityError::NotFound) => (
                state.authority().read_claim_bootstrap(&token, now).await?,
                "",
            ),
            Err(other) => return Err(other.into()),
        };
    // The AUTHORITY's config is the one source for the link base (a `PlaneState::new` composition never
    // fills the state-side copy) — pulled off the domain struct before it maps to the wire payload.
    let link_base = bootstrap.link_base.clone();
    let data = map::bootstrap_to_wire(echoed_token_id, bootstrap);

    let mut response = if wants_json(&headers) {
        Json(&data).into_response()
    } else {
        // The share link, rebuilt on the PUBLIC link base — echoed for an invite only (claim custody rule
        // above; an empty echoed token id IS the claim discriminator).
        let link = (!echoed_token_id.is_empty()).then(|| format!("{link_base}/i/{token}"));
        let doc = bootstrap_doc::agent_instructions(&data, link.as_deref());
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
/// RFC 9110 negotiation — the two consumers are cleanly disjoint in practice).
fn wants_json(headers: &HeaderMap) -> bool {
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
