//! Custody — the raw-SQL twins for byte custody: the pointer-move transaction, the object-lifecycle
//! fence, the contribute-table SQL, the receipt machinery, and the restore epoch bump.

pub(crate) mod set_current;

// The object-lifecycle transitions (the fenced CAS state machine, leases, quarantine, tombstones). A few
// helpers (e.g. `release_lease`) are exercised only by tests, so the dead-code waiver stays on the module.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod lifecycle;

pub(crate) mod proposals;
pub(crate) mod receipts;
pub(crate) mod restore;
