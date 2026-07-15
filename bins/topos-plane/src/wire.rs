//! Small edge helpers shared by the handlers and the maintenance scheduler.

/// The server clock, epoch **milliseconds** — the one unit every authority op takes.
pub(crate) fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Flatten an error's full `source()` chain into one `": "`-joined line — the server-side diagnostic
/// the flat wire body deliberately omits. `AuthorityError::{Integrity, Internal}` `Display` a generic
/// line and carry the real fault as a boxed source, so without walking the chain a 500 is
/// undiagnosable. One line (not one event per level) keeps a JSON log entry self-contained.
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut line = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        line.push_str(": ");
        line.push_str(&cause.to_string());
        source = cause.source();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::error_chain;

    #[derive(Debug, thiserror::Error)]
    #[error("outer fault")]
    struct Outer(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("inner cause")]
    struct Inner(#[source] std::io::Error);

    /// The chain walk renders EVERY `source()` level, joined on `": "` — the diagnostic line the 500
    /// mapper and the maintenance scheduler log (the wire body never carries it).
    #[test]
    fn error_chain_renders_every_source_level() {
        let e = Outer(Inner(std::io::Error::other("disk on fire")));
        assert_eq!(error_chain(&e), "outer fault: inner cause: disk on fire");
    }
}
