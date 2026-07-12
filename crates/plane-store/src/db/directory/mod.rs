//! Directory — the raw-SQL twins for access / identity / policy: enrollment issuance, governance +
//! admin-claim, and the two web-session directory legs (read index, roster).

// The skill-lifecycle SQL (archive/unarchive/delete/purge — the guarded functions + custody un-rooting).
pub(crate) mod catalog;

// The channel/subscription/protection SQL (name resolution + the guarded topos_* function calls).
pub(crate) mod channels;

// The delivery + fleet SQL (the entitlement read + the applied-state report).
pub(crate) mod delivery;

pub(crate) mod enroll;
pub(crate) mod governance;

// The directory's implementation of custody's access-witness seam + the pool-level principal probes.
pub(crate) mod witness;

// The web-session read lane's SQL (the one skill-index query).
pub(crate) mod session_read;

// The web-session roster SQL (invite / remove / rotate / the roster read).
pub(crate) mod session_roster;
