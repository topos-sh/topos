//! Enrollment protocol GLUE — the concrete connectors the route handlers (landing next) drive.
//!
//! These modules hold **no durable state and make no issuance decision**: every identity /
//! roster / credential decision is `plane_store::Authority`'s, against a server-trusted row. This layer is
//! pure protocol plumbing — the passcode mailer seam, and (behind a default-off cargo feature) the OIDC
//! id-token connector. The mailer is built; the routes that send through it land in the next step.

// The passcode mailer seam. Production CONSTRUCTS the mailer (via `PlaneState::with_enroll_config`) but does
// not yet SEND through it — the verification routes that do land next — so the not-yet-called surface
// (`Passcode::new`, `MailContext`, …) is dead in a production lib build. Mirrors plane-store's `mod gc`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod mailer;

// The OIDC id-token connector — behind `enroll-oidc`, DEFAULT-OFF, so a default build never resolves
// oauth2/openidconnect. The id/access token is consumed SERVER-SIDE and never returned to the agent. The
// `start`/`callback` entry points are driven by the verification routes (landing next), so they are
// unreferenced in a non-test feature build today (mirrors `mod mailer`).
#[cfg(feature = "enroll-oidc")]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod oidc;
