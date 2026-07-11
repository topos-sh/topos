//! Custody — byte custody: bytes / versions / pointers / GC (the orchestration half, outside the
//! transaction). The raw-SQL twins live under `db/custody/`.

pub(crate) mod gc;
pub(crate) mod lifecycle;
pub(crate) mod lineage;
pub(crate) mod read;
pub(crate) mod restore;
pub(crate) mod set_current;
pub(crate) mod upload;
