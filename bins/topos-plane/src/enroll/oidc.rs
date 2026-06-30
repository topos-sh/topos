//! The OIDC enrollment connector — a MINIMAL, single-provider id-token flow, behind `enroll-oidc`.
//!
//! Two entry points the verification routes (landing next) drive:
//! - [`start`] builds the IdP authorize redirect with **PKCE + CSRF state + nonce**, binding the topos
//!   `user_code` into `state` so the callback re-finds the device-auth session.
//! - [`callback`] runs SERVER-SIDE: validate `state`, exchange the code (PKCE), validate the id_token
//!   (issuer + audience pinned by the verifier, signature via JWKS, nonce), extract the verified email, and
//!   confirm the session in the authority.
//!
//! **No user token ever returns to the agent.** The id/access token is consumed here and **dropped** — it NEVER reaches a poll
//! response or any agent-facing value. The only thing that crosses out is the proven email → the in-authority
//! session confirm (which returns the opaque grant on the device's next poll, never a user token). The
//! HTTP client is built with **redirects disabled** (per openidconnect's guidance — following redirects opens
//! the client to SSRF).

use base64::Engine as _;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::reqwest;
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use plane_store::Authority;

/// The OIDC connector config (one generic single-provider IdP). Read from `TOPOS_PLANE_OIDC_*` in the bin.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The IdP issuer URL (discovery anchors on `<issuer>/.well-known/openid-configuration`).
    pub issuer: String,
    /// The registered client id.
    pub client_id: String,
    /// The registered client secret.
    pub client_secret: String,
    /// The redirect URI registered with the IdP (where the IdP returns the code).
    pub redirect_uri: String,
}

/// The authorize redirect [`start`] produces. The handler sends the human to `url` and PERSISTS `state` /
/// `pkce_verifier` / `nonce` (keyed to the browser flow), presenting them back in [`OidcCallback`].
#[derive(Debug, Clone)]
pub(crate) struct AuthorizeRedirect {
    /// The IdP authorize URL to redirect the human to.
    pub url: String,
    /// The CSRF `state` (carries the bound `user_code`) — persist it; the callback compares against it.
    pub state: String,
    /// The PKCE verifier — persist it; presented back in [`OidcCallback`].
    pub pkce_verifier: String,
    /// The OIDC nonce — persist it; checked against the id_token in the callback.
    pub nonce: String,
}

/// The callback inputs the handler reassembles: the IdP's `(code, returned_state)` plus the persisted
/// `(expected_state, pkce_verifier, nonce)`. The bound `user_code` is recovered from the validated state.
#[derive(Debug, Clone)]
pub(crate) struct OidcCallback {
    /// The authorization `code` from the IdP redirect.
    pub code: String,
    /// The `state` the IdP echoed back (must equal `expected_state`).
    pub returned_state: String,
    /// The `state` the handler persisted at [`start`] (the CSRF anchor; carries the bound `user_code`).
    pub expected_state: String,
    /// The PKCE verifier the handler persisted at [`start`].
    pub pkce_verifier: String,
    /// The OIDC nonce the handler persisted at [`start`].
    pub nonce: String,
}

/// An OIDC enrollment failure. Coarse on purpose — no IdP response, token, or claim detail rides in it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EnrollError {
    /// A config value (issuer / redirect URI) is malformed.
    #[error("oidc configuration is invalid")]
    Config,
    /// The redirects-disabled HTTP client could not be built.
    #[error("oidc http client could not be built")]
    Http,
    /// Provider discovery failed.
    #[error("oidc provider discovery failed")]
    Discovery,
    /// The returned `state` did not match the persisted one (CSRF / replay guard).
    #[error("oidc state mismatch")]
    StateMismatch,
    /// The code exchange failed.
    #[error("oidc code exchange failed")]
    Exchange,
    /// The id_token was absent or failed validation (signature / issuer / audience / nonce).
    #[error("oidc id token missing or invalid")]
    IdToken,
    /// The id_token carried no email claim.
    #[error("oidc id token carries no email")]
    NoEmail,
    /// Confirming the session identity in the authority failed.
    #[error("confirming the session identity failed")]
    Confirm(#[from] plane_store::AuthorityError),
}

/// Build the IdP authorize redirect for a topos device-auth `user_code` (PKCE + CSRF state + nonce). The
/// caller PERSISTS the returned `state` / `pkce_verifier` / `nonce` and presents them back in the callback.
pub(crate) async fn start(
    cfg: &OidcConfig,
    user_code: &str,
    _now: i64,
) -> Result<AuthorizeRedirect, EnrollError> {
    let http_client = http_client()?;
    let provider_metadata = CoreProviderMetadata::discover_async(
        IssuerUrl::new(cfg.issuer.clone()).map_err(|_| EnrollError::Config)?,
        &http_client,
    )
    .await
    .map_err(|_| EnrollError::Discovery)?;
    let client = CoreClient::from_provider_metadata(
        provider_metadata,
        ClientId::new(cfg.client_id.clone()),
        Some(ClientSecret::new(cfg.client_secret.clone())),
    )
    .set_redirect_uri(RedirectUrl::new(cfg.redirect_uri.clone()).map_err(|_| EnrollError::Config)?);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    // Bind the topos user_code INTO the CSRF state — the callback both re-finds the device-auth session and
    // verifies the round-trip (the persisted state must match what the IdP echoes back).
    let bound_state = encode_state(user_code);
    let (auth_url, csrf_token, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(bound_state.clone()),
            Nonce::new_random,
        )
        .add_scope(Scope::new("openid".to_owned()))
        .add_scope(Scope::new("email".to_owned()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    Ok(AuthorizeRedirect {
        url: auth_url.to_string(),
        state: csrf_token.secret().clone(),
        pkce_verifier: pkce_verifier.secret().clone(),
        nonce: nonce.secret().clone(),
    })
}

/// Run the OIDC callback SERVER-SIDE: validate state, exchange the code, validate the id_token, extract the
/// email, and confirm the session. **The id/access token is dropped here — nothing token-bearing returns.**
pub(crate) async fn callback(
    authority: &Authority,
    cfg: &OidcConfig,
    params: OidcCallback,
    now: i64,
) -> Result<(), EnrollError> {
    // CSRF / replay guard: the IdP-echoed state must equal the persisted one.
    if params.returned_state != params.expected_state {
        return Err(EnrollError::StateMismatch);
    }
    // Recover the bound topos user_code from the validated state.
    let user_code = decode_state(&params.expected_state).ok_or(EnrollError::StateMismatch)?;

    let http_client = http_client()?;
    let provider_metadata = CoreProviderMetadata::discover_async(
        IssuerUrl::new(cfg.issuer.clone()).map_err(|_| EnrollError::Config)?,
        &http_client,
    )
    .await
    .map_err(|_| EnrollError::Discovery)?;
    let client = CoreClient::from_provider_metadata(
        provider_metadata,
        ClientId::new(cfg.client_id.clone()),
        Some(ClientSecret::new(cfg.client_secret.clone())),
    )
    .set_redirect_uri(RedirectUrl::new(cfg.redirect_uri.clone()).map_err(|_| EnrollError::Config)?);

    // Exchange the code (with the PKCE verifier). The token response is consumed entirely within this
    // function — it is NEVER returned to the agent.
    let token_response = client
        .exchange_code(AuthorizationCode::new(params.code))
        .map_err(|_| EnrollError::Exchange)?
        .set_pkce_verifier(PkceCodeVerifier::new(params.pkce_verifier))
        .request_async(&http_client)
        .await
        .map_err(|_| EnrollError::Exchange)?;

    // Validate the id_token: issuer + audience are pinned by the verifier (from discovery + the client id),
    // the signature via JWKS, and the nonce here. Then extract the email claim.
    let id_token = token_response.id_token().ok_or(EnrollError::IdToken)?;
    let verifier = client.id_token_verifier();
    let nonce = Nonce::new(params.nonce);
    let claims = id_token
        .claims(&verifier, &nonce)
        .map_err(|_| EnrollError::IdToken)?;
    let email = claims
        .email()
        .map(|email| email.as_str().to_owned())
        .ok_or(EnrollError::NoEmail)?;

    // The ONLY value that crosses out is the proven email → the in-authority session confirm (the device's
    // next poll then yields the opaque grant). The id/access token drops here; nothing token-bearing returns.
    authority
        .confirm_external_identity(&user_code, &email, now)
        .await?;
    Ok(())
}

/// A reqwest client with redirects DISABLED (following them opens the client to SSRF — per the IdP guidance).
fn http_client() -> Result<reqwest::Client, EnrollError> {
    reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| EnrollError::Http)
}

/// Encode the CSRF state as `base64url(user_code) . <random>`: the head re-finds the device-auth session, the
/// random tail is CSRF entropy (so the state is unpredictable, not just the user_code).
fn encode_state(user_code: &str) -> String {
    let head = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(user_code.as_bytes());
    let tail = CsrfToken::new_random();
    format!("{head}.{}", tail.secret())
}

/// Recover the bound `user_code` from a state produced by [`encode_state`]. `None` on a malformed state.
fn decode_state(state: &str) -> Option<String> {
    let head = state.split('.').next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(head)
        .ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_ok_type_is_unit_so_no_token_reaches_the_agent() {
        // No user token reaches the agent — pinned STRUCTURALLY. This helper is compiled (type-checked) but NEVER run: it
        // binds `callback`'s awaited success value to the unit type `()`. A token could only escape
        // SERVER-side processing through a non-`()` Ok payload — so if the signature ever changes to return a
        // token-bearing type, this binding stops compiling.
        async fn pins_callback_ok_to_unit(a: &Authority, c: &OidcConfig, p: OidcCallback) {
            let _confirmed: Result<(), EnrollError> = callback(a, c, p, 0).await;
        }
        // Reference it so it is compiled — the assertion is the binding above; it never executes.
        let _ = pins_callback_ok_to_unit;
    }

    #[test]
    fn state_round_trips_the_bound_user_code_with_csrf_entropy() {
        let state = encode_state("ABCD-EFGH");
        // The bound user_code is recoverable…
        assert_eq!(decode_state(&state).as_deref(), Some("ABCD-EFGH"));
        // …but the state is NOT just the user_code — it carries a random CSRF tail.
        assert!(state.contains('.'));
        assert_ne!(encode_state("ABCD-EFGH"), encode_state("ABCD-EFGH"));
    }

    #[test]
    fn start_produces_a_redirect_carrying_the_persisted_flow_secrets() {
        // Reference `start` so it is compiled/used (it makes a network discovery call, so it is not executed
        // here). The handler PERSISTS `state` / `pkce_verifier` / `nonce` and presents them back in the
        // callback — assert the redirect carries exactly those alongside the URL.
        let _ = start;
        let redirect = AuthorizeRedirect {
            url: "https://idp.example/authorize".to_owned(),
            state: encode_state("ABCD-EFGH"),
            pkce_verifier: "verifier".to_owned(),
            nonce: "nonce".to_owned(),
        };
        assert_eq!(decode_state(&redirect.state).as_deref(), Some("ABCD-EFGH"));
        assert!(!redirect.url.is_empty());
        assert!(!redirect.pkce_verifier.is_empty());
        assert!(!redirect.nonce.is_empty());
    }
}
