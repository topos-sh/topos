//! The thin route handlers: the device-credential writes (publish / propose / revert / review), the
//! token-scoped reads (current / bundles / versions), the two byte-decorated describe reads the vault
//! keeps serving (the review inbox + the skill log — git commit messages ride both), the
//! unauthenticated claim bootstrap + the constant protocol-card fallback, the enrollment flow
//! (device-auth / passcode / redeem / admin-claim / login), and the governance mutations (roster /
//! revoke). The member-lane ROW OPS — subscriptions, curation, exclusions, protection, notices ack,
//! invitations, delivery, the fleet report, me/channels/reach — are served by the composing web app
//! since the door cutover; their wire contract lives on as the [`door`] stubs.
//!
//! Each handler is a flat **parse → call the authority → serialize** — it never makes a trust decision,
//! never reads a raw object, and never `Principal::parse`s a client-asserted *identity* (a read resolves the
//! token to the opaque `ReadScope` and passes the PATH's `(ws, skill)` straight in; a confirmed identity is
//! resolved from a server-trusted row inside the authority). The wire mapping lives in [`crate::wire`].

pub(crate) mod bootstrap;
pub(crate) mod bootstrap_doc;
pub(crate) mod bundles;
pub(crate) mod card;
pub(crate) mod current;
pub(crate) mod describe;
pub(crate) mod door;
pub(crate) mod enroll;
pub(crate) mod governance;
pub(crate) mod internal;
pub(crate) mod login;
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
