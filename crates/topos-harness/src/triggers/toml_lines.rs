//! The line-anchored TOML reading shared by the two instances that edit a TOML config surgically
//! (`codex`'s `[features]` switch, `kimi-code-cli`'s sentinel-led `[[hooks]]` block).
//!
//! There is deliberately NO TOML dependency behind these: they answer only what a line ANCHOR can
//! prove — the table a header line opens, the key an assignment line states, the basic string it
//! assigns — so every line an edit does not touch re-emits byte-for-byte, and a shape the anchors
//! cannot stand on (non-UTF-8 bytes, a leading byte-order mark) fails closed at the call site with
//! ZERO writes. It is the same discipline the Hermes YAML merge follows.

/// Split preserving each line's bytes (terminators included), so untouched lines re-emit verbatim.
pub(super) fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A line's own terminator, so an inserted line matches the file's line endings instead of
/// imposing ours on a CRLF config.
pub(super) fn terminator(line: &str) -> &'static str {
    if line.ends_with("\r\n") { "\r\n" } else { "\n" }
}

/// The TOML table header a line states (`[features]`, `[[hooks]]`), if it states one — a comment,
/// an assignment, or a header with anything but a comment trailing it is not one.
pub(super) fn table_header(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || !trimmed.starts_with('[') {
        return None;
    }
    let end = if trimmed.starts_with("[[") {
        trimmed.find("]]").map(|i| i + 2)?
    } else {
        trimmed.find(']').map(|i| i + 1)?
    };
    let rest = trimmed[end..].trim_start();
    if !rest.is_empty() && !rest.starts_with('#') {
        return None;
    }
    Some(&trimmed[..end])
}

/// Whether a line assigns `key` (`hooks = …`), rather than mentioning it in a comment or as part
/// of a longer name.
pub(super) fn is_key_line(line: &str, key: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') || !trimmed.starts_with(key) {
        return false;
    }
    trimmed[key.len()..].trim_start().starts_with('=')
}

/// The basic string an assignment line states (`command = "…"` → the command), read to the closing
/// quote with `\"` kept inside it. `None` for a line that assigns something else — a number, a
/// literal (single-quoted) string, a multi-line one — because a value this cannot delimit exactly
/// is a value it must not report.
///
/// The escape walk is what keeps `command = "say \"hi\" then run"` from ending at the second
/// quote; the content is returned RAW (escapes unresolved), which is all a substring probe like the
/// hand-rolled-sweep predicate needs.
pub(super) fn basic_string_value(line: &str) -> Option<&str> {
    let rest = line.split_once('=')?.1.trim_start().strip_prefix('"')?;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(&rest[..i]),
            _ => escaped = false,
        }
    }
    None // an unterminated string is not a value these anchors can prove
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_line_is_read_only_where_a_header_is_all_it_states() {
        assert_eq!(table_header("[features]\n"), Some("[features]"));
        assert_eq!(table_header("  [[hooks]]  # mine\n"), Some("[[hooks]]"));
        assert_eq!(
            table_header("[mcp_servers.topos]\n"),
            Some("[mcp_servers.topos]")
        );
        for not_a_header in [
            "# [features]\n",          // a comment
            "model = \"gpt\"\n",       // an assignment
            "[features] hooks = true", // a header with content trailing it
            "[unterminated\n",
        ] {
            assert_eq!(table_header(not_a_header), None, "{not_a_header:?}");
        }
    }

    #[test]
    fn a_key_line_is_the_assignment_and_never_a_mention() {
        assert!(is_key_line("hooks = true\n", "hooks"));
        assert!(is_key_line("  hooks   =   false\n", "hooks"));
        for not_the_key in ["# hooks = true\n", "hooks_enabled = true\n", "hooks\n"] {
            assert!(!is_key_line(not_the_key, "hooks"), "{not_the_key:?}");
        }
    }

    #[test]
    fn a_basic_string_value_ends_at_its_own_closing_quote() {
        assert_eq!(
            basic_string_value("command = \"topos update --quiet\"\n"),
            Some("topos update --quiet")
        );
        assert_eq!(
            basic_string_value("  command = \"say \\\"hi\\\" then run\"\n"),
            Some("say \\\"hi\\\" then run"),
            "an escaped quote does not end the value"
        );
        for not_a_basic_string in [
            "timeout = 60\n",             // a number
            "command = 'literal'\n",      // a literal string
            "command = \"unterminated\n", // no closing quote
            "command\n",                  // no assignment at all
        ] {
            assert_eq!(
                basic_string_value(not_a_basic_string),
                None,
                "{not_a_basic_string:?}"
            );
        }
    }

    #[test]
    fn a_split_line_keeps_its_own_terminator() {
        assert_eq!(split_lines("a\r\nb\n"), vec!["a\r\n", "b\n"]);
        assert_eq!(split_lines("no terminator"), vec!["no terminator"]);
        assert_eq!(terminator("a\r\n"), "\r\n");
        assert_eq!(terminator("a\n"), "\n");
        assert_eq!(terminator("a"), "\n");
    }
}
