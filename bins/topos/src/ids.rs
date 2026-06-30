//! Injectable identity + time sources, so `add` minting and `log` timestamps are deterministic under
//! test and real in production.

/// Mints stable, controlled-ASCII skill ids. Injected so a fixed fixture yields a fixed id.
pub(crate) trait IdSource {
    /// A fresh skill id — the sidecar directory key, a `topos_<hex>` token (never written into a skill).
    fn new_skill_id(&self) -> String;

    /// A fresh op id — the raw 16 bytes of a UUIDv4, the client-minted idempotency key a governance/device
    /// op signs. The signing frame binds these bytes; the wire carries their canonical hyphenated form (the
    /// plane parses it back to the SAME 16 bytes), so a lost-ack retry replays the deterministic receipt.
    fn new_op_id(&self) -> [u8; 16];
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
    fn new_op_id(&self) -> [u8; 16] {
        uuid::Uuid::new_v4().into_bytes()
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
        next_op: AtomicU64,
    }
    impl SeqIds {
        pub(crate) fn new(prefix: &str) -> Self {
            Self {
                prefix: prefix.to_owned(),
                next: AtomicU64::new(0),
                next_op: AtomicU64::new(0),
            }
        }
    }
    impl IdSource for SeqIds {
        fn new_skill_id(&self) -> String {
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            format!("topos_{}{n:02}", self.prefix)
        }
        fn new_op_id(&self) -> [u8; 16] {
            // A deterministic, distinct 16-byte op id per call (its own counter, so skill-id numbering is
            // never perturbed). Any 16 bytes round-trip through `Uuid::from_bytes(..).as_hyphenated()`.
            let n = self.next_op.fetch_add(1, Ordering::Relaxed);
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&n.to_be_bytes());
            bytes
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
