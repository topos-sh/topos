//! Enrollment protocol GLUE — the concrete connectors the route handlers drive.
//!
//! These modules hold **no durable state and make no issuance decision**: every identity /
//! roster / credential decision is `plane_store::Authority`'s, against a server-trusted row. This layer is
//! pure protocol plumbing — (behind a default-off cargo feature) the OIDC id-token connector. (The passcode
//! MAIL seam left this tier with the mail unification: the internal lane mints the code once —
//! `routes::internal::mint_passcode` — and the composing surface's mail seam delivers it; the vault holds
//! no mail transport.)

// The OIDC id-token connector — behind `enroll-oidc`, DEFAULT-OFF, so a default build never resolves
// oauth2/openidconnect. The id/access token is consumed SERVER-SIDE and never returned to the agent.
#[cfg(feature = "enroll-oidc")]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod oidc;
