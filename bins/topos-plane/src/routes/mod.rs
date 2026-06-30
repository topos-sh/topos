//! The seven thin route handlers: 4 device-signed writes (publish / propose / revert / review) + 3
//! token-scoped reads (current / bundles / versions).
//!
//! Each handler is a flat **parse → call the authority → serialize** — it never makes a trust decision,
//! never reads a raw object, and never `Principal::parse`s a client-asserted id (a read resolves the token
//! to the opaque `ReadScope` and passes the PATH's `(ws, skill)` straight in as `req_ws`/`req_skill`, so the
//! authority does the scope-vs-path check). The wire mapping lives in [`crate::wire`], never in a body.

pub(crate) mod bundles;
pub(crate) mod current;
pub(crate) mod proposals;
pub(crate) mod publish;
pub(crate) mod reverts;
pub(crate) mod reviews;
pub(crate) mod versions;
