//! The thin route handlers: 4 device-credential writes (publish / propose / revert / review), 3 token-scoped
//! reads (current / bundles / versions), the unauthenticated invite bootstrap, the enrollment flow
//! (device-auth / passcode / redeem / admin-claim), and the governance mutations (invite / roster / revoke).
//!
//! Each handler is a flat **parse → call the authority → serialize** — it never makes a trust decision,
//! never reads a raw object, and never `Principal::parse`s a client-asserted *identity* (a read resolves the
//! token to the opaque `ReadScope` and passes the PATH's `(ws, skill)` straight in; a confirmed identity is
//! resolved from a server-trusted row inside the authority). The wire mapping lives in [`crate::wire`].

pub(crate) mod bootstrap;
pub(crate) mod bootstrap_doc;
pub(crate) mod bundles;
pub(crate) mod current;
pub(crate) mod delivery;
pub(crate) mod enroll;
pub(crate) mod governance;
pub(crate) mod policy;
pub(crate) mod proposals;
pub(crate) mod publish;
pub(crate) mod reverts;
pub(crate) mod reviews;
pub(crate) mod skills_index;
pub(crate) mod versions;

// The OIDC routes — behind `enroll-oidc` (default-off), so a default build resolves none of the connector.
#[cfg(feature = "enroll-oidc")]
pub(crate) mod oidc;
