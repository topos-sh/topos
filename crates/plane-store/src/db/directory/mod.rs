//! Directory — the raw-SQL twins for access / identity / policy: enrollment issuance, governance +
//! admin-claim, and the two web-session directory legs (read index, roster).

pub(crate) mod enroll;
pub(crate) mod governance;

// The web-session read lane's SQL (the one skill-index query).
pub(crate) mod session_read;

// The web-session roster SQL (invite / remove / rotate / the roster read).
pub(crate) mod session_roster;
