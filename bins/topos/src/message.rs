//! The typed message channel — how a producer builds one, and how the legacy line is derived.
//!
//! ONE string, two renderings. A producer writes a [`Message`]: the machine-readable code it
//! already had, what kind of thing the line is, and the ONE sentence a person reads. The TTY
//! prints `text` alone (a code is a lookup key, never prose); the envelope's legacy `warnings`
//! array carries [`legacy_line`], which puts the code back on the front exactly where it always
//! sat. Neither surface re-derives the other's wording, so the two can no longer drift.

use topos_types::{Message, MessageKind};

/// A FAILURE with the stable code its consumers already match on.
pub(crate) fn failure(code: &str, text: String) -> Message {
    Message {
        code: Some(code.to_owned()),
        kind: MessageKind::Failure,
        text,
    }
}

/// A FAILURE the producer has no code for (the paging/size facts, the list's skipped workspace).
pub(crate) fn uncoded_failure(text: String) -> Message {
    Message {
        code: None,
        kind: MessageKind::Failure,
        text,
    }
}

/// An ADVISORY — a row that still delivered, annotated.
pub(crate) fn advisory(code: &str, text: String) -> Message {
    Message {
        code: Some(code.to_owned()),
        kind: MessageKind::Advisory,
        text,
    }
}

/// A DISCLOSURE — a fact about work that succeeded.
pub(crate) fn disclosure(code: &str, text: String) -> Message {
    Message {
        code: Some(code.to_owned()),
        kind: MessageKind::Disclosure,
        text,
    }
}

/// A DECISION — a bundle waiting on a person. No code by design: a code names a fault to look up,
/// and there is no fault here; what the reader needs is the commands, which ride `next_actions`.
pub(crate) fn decision(text: String) -> Message {
    Message {
        code: None,
        kind: MessageKind::Decision,
        text,
    }
}

/// The LEGACY `warnings` line for a message: `"{code} {text}"` when it carries a code, the bare
/// text when it does not. Membership and order are the message channel's — untouched — so every
/// count, exit status and `!warnings.is_empty()` gate reads exactly what it read before.
pub(crate) fn legacy_line(m: &Message) -> String {
    match &m.code {
        Some(code) => format!("{code} {}", m.text),
        None => m.text.clone(),
    }
}

/// [`legacy_line`] over a whole channel, in order.
pub(crate) fn legacy_lines(messages: &[Message]) -> Vec<String> {
    messages.iter().map(legacy_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The derivation is the WHOLE compatibility story: a coded message puts its code back on the
    /// front, an uncoded one is its own line, and nothing else is added.
    #[test]
    fn the_legacy_line_is_the_code_then_the_text() {
        assert_eq!(
            legacy_line(&failure("CATALOG_UNAVAILABLE", "acme: gone".to_owned())),
            "CATALOG_UNAVAILABLE acme: gone"
        );
        assert_eq!(
            legacy_line(&uncoded_failure("acme: gone".to_owned())),
            "acme: gone"
        );
        assert_eq!(
            legacy_line(&decision("docs: waiting on you".to_owned())),
            "docs: waiting on you"
        );
    }

    /// The kinds are what the four channels have always been, kept apart on the wire.
    #[test]
    fn each_constructor_carries_its_kind() {
        assert_eq!(failure("A", String::new()).kind, MessageKind::Failure);
        assert_eq!(advisory("A", String::new()).kind, MessageKind::Advisory);
        assert_eq!(disclosure("A", String::new()).kind, MessageKind::Disclosure);
        assert_eq!(decision(String::new()).kind, MessageKind::Decision);
        assert!(decision(String::new()).code.is_none());
        assert!(uncoded_failure(String::new()).code.is_none());
    }

    /// Order is preserved line for line — the array a consumer indexes into does not move.
    #[test]
    fn the_channel_maps_one_for_one_in_order() {
        let messages = vec![
            failure("A", "one".to_owned()),
            uncoded_failure("two".to_owned()),
            advisory("C", "three".to_owned()),
        ];
        assert_eq!(legacy_lines(&messages), vec!["A one", "two", "C three"]);
    }
}
