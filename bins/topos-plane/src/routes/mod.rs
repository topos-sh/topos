//! The route handlers: the bearer-gated internal custody lane ([`internal`]) and the contract-only
//! stubs pinning the product's PUBLIC device-lane wire ([`door`] — served by the composing app,
//! never mounted here).

pub(crate) mod door;
pub(crate) mod internal;
