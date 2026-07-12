//! The constant PROTOCOL CARD — the router FALLBACK for any unmatched path.
//!
//! Every resource address in the product (`topos.sh/<workspace>`, a channel/skill path) is a URL a human
//! might paste to their agent or open in a browser. Those user-visible paths are served by a web front, but
//! a fetch that reaches the plane on an UNMATCHED path must still teach a client what to do — WITHOUT leaking
//! whether the path names anything. So the fallback answers ONE constant card for every unmatched GET (no
//! path echo, no existence signal): a machine face (JSON [`WireProtocolCard`] — the discriminant + the API
//! base to re-root onto) for a client asking for JSON, and a human/agent markdown card otherwise.
//!
//! Non-GET unmatched keeps the uniform JSON 404 envelope (there is nothing to teach a mutation on a path
//! that routes nowhere). The card is rate-limited and traced like every other route (it rides under the same
//! `router()` layers). Both 200s are `Cache-Control: no-store` + `Vary: accept` + `X-Robots-Tag: noindex`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};

use super::bootstrap::wants_json;
use super::bootstrap_doc::INSTALL_LINE;
use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::map;

/// The constant markdown card — the human/agent representation. No path echo (an unmatched path must not
/// become an existence oracle): it names the product, the human hand-off, and the one agent command, plus
/// the install-if-missing block. The agent fetches the resource address it already holds; the card points
/// back at that, never at a path this handler was given.
fn card_markdown() -> String {
    format!(
        "# A Topos resource address\n\n\
         Topos keeps a team's agent skills — bundles of instructions, scripts, and reference docs — \
         current on every machine: publish once, every subscribed agent picks the update up at its next \
         session start.\n\n\
         **If you are a human reading this:** paste this URL to your agent and ask it to follow — the \
         step below does the rest.\n\n\
         **If you are an AI agent:** follow this resource with your shell tool, non-interactively. Nothing \
         installs silently — every skill lands only after its content digest is disclosed and the human \
         says yes.\n\n\
         ```sh\ntopos follow '<the URL you just fetched>' --json\n```\n\n\
         If `topos` is not installed (checksum-verified; no sudo; lands in `~/.local/bin`):\n\n\
         ```sh\n{INSTALL_LINE}\n```\n\n\
         Machine note: fetch any resource address with `Accept: application/json` to get the API base to \
         dial.\n"
    )
}

/// The router fallback. A GET returns the constant card (content-negotiated); any other method is the
/// uniform JSON 404 envelope.
pub(crate) async fn protocol_card(
    State(state): State<PlaneState>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    // A mutation on a path that routes nowhere has nothing to teach — the uniform JSON 404.
    if method != Method::GET {
        return Err(plane_store::AuthorityError::NotFound.into());
    }
    // The API base the client re-roots onto — the AUTHORITY's authoritative copy (one source, the same the
    // bootstrap + standup plane block read).
    let api_base_url = state.authority().enrollment_disclosure()?.base_url;
    let mut response = if wants_json(&headers) {
        Json(map::protocol_card(api_base_url)).into_response()
    } else {
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            card_markdown(),
        )
            .into_response()
    };
    // The card is the same for every path and every caller, but it must not sit in a shared cache keyed on
    // path (a proxy could then serve it for a real resource), and it varies on Accept.
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::VARY, HeaderValue::from_static("accept"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    Ok(response)
}
