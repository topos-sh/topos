//! Directory — access / identity / policy (the orchestration half, outside the transaction). The
//! raw-SQL twins live under `db/directory/` (except `session_review`, whose write terminates in the
//! shared custody pointer-move transaction and so has no db twin).

pub(crate) mod catalog;
pub(crate) mod channels;
pub(crate) mod delivery;
pub(crate) mod describe;
pub(crate) mod enroll;
pub(crate) mod governance;
pub(crate) mod session_read;
pub(crate) mod session_review;
pub(crate) mod session_roster;
