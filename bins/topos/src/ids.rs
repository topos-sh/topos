//! Injectable identity + time sources, so `add` minting and `log` timestamps are deterministic under
//! test and real in production.

/// Mints stable, controlled-ASCII skill ids. Injected so a fixed fixture yields a fixed id.
pub(crate) trait IdSource {
    /// A fresh skill id — the sidecar directory key, a `topos_<hex>` token (never written into a skill).
    fn new_skill_id(&self) -> String;
}

/// A monotonic-ish wall clock for local log events. Returns Unix milliseconds (the local `log.jsonl`
/// event shape is intentionally open, so a plain integer avoids a date dependency).
pub(crate) trait Clock {
    fn now_unix_millis(&self) -> u64;
}

/// Production id source — a 128-bit random UUIDv4 rendered as `topos_<32 hex>`.
#[derive(Debug, Default)]
pub(crate) struct RealIds;

impl IdSource for RealIds {
    fn new_skill_id(&self) -> String {
        format!("topos_{}", uuid::Uuid::new_v4().simple())
    }
}

/// Production clock.
#[derive(Debug, Default)]
pub(crate) struct RealClock;

impl Clock for RealClock {
    fn now_unix_millis(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

#[cfg(test)]
pub(crate) mod test_sources {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Clock, IdSource};

    /// A deterministic id source: `topos_<prefix><counter>`, so two adds get distinct, predictable ids.
    #[derive(Debug)]
    pub(crate) struct SeqIds {
        prefix: String,
        next: AtomicU64,
    }
    impl SeqIds {
        pub(crate) fn new(prefix: &str) -> Self {
            Self {
                prefix: prefix.to_owned(),
                next: AtomicU64::new(0),
            }
        }
    }
    impl IdSource for SeqIds {
        fn new_skill_id(&self) -> String {
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            format!("topos_{}{n:02}", self.prefix)
        }
    }

    /// A fixed clock.
    #[derive(Debug)]
    pub(crate) struct FixedClock(pub u64);
    impl Clock for FixedClock {
        fn now_unix_millis(&self) -> u64 {
            self.0
        }
    }
}
