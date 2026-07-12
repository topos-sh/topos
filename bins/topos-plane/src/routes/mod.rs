//! The thin route handlers: the device-credential writes (publish / propose / revert / review), the
//! token-scoped reads (current / bundles / versions), the member-lane DESCRIBE reads (me / channels /
//! proposals / log / reach) + row-op writes (follows / channels / exclusions / protection / notices ack /
//! invitations), the unauthenticated claim bootstrap + the constant protocol-card fallback, the enrollment
//! flow (device-auth / passcode / redeem / admin-claim / login), and the governance mutations (roster /
//! revoke).
//!
//! Each handler is a flat **parse → call the authority → serialize** — it never makes a trust decision,
//! never reads a raw object, and never `Principal::parse`s a client-asserted *identity* (a read resolves the
//! token to the opaque `ReadScope` and passes the PATH's `(ws, skill)` straight in; a confirmed identity is
//! resolved from a server-trusted row inside the authority). The wire mapping lives in [`crate::wire`].

pub(crate) mod bootstrap;
pub(crate) mod bootstrap_doc;
pub(crate) mod bundles;
pub(crate) mod card;
pub(crate) mod channels;
pub(crate) mod current;
pub(crate) mod delivery;
pub(crate) mod describe;
pub(crate) mod enroll;
pub(crate) mod governance;
pub(crate) mod internal;
pub(crate) mod invitations;
pub(crate) mod login;
pub(crate) mod notices;
pub(crate) mod policy;
pub(crate) mod proposals;
pub(crate) mod protection;
pub(crate) mod publish;
pub(crate) mod reverts;
pub(crate) mod reviews;
pub(crate) mod skills_index;
pub(crate) mod subscriptions;
pub(crate) mod versions;

// The OIDC routes — behind `enroll-oidc` (default-off), so a default build resolves none of the connector.
#[cfg(feature = "enroll-oidc")]
pub(crate) mod oidc;
