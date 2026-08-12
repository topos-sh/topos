//! The line-anchored TOML reading shared by the two instances that edit a TOML config surgically
//! (`codex`'s `[features]` switch, `kimi-code-cli`'s sentinel-led `[[hooks]]` block).
//!
//! There is deliberately NO TOML dependency behind these: they answer only what a line ANCHOR can
//! prove — the table a header line opens, the key an assignment line states (in every spelling
//! TOML reads as that same key), the basic string it assigns — so every line an edit does not touch
//! re-emits byte-for-byte, and a shape the anchors cannot stand on (non-UTF-8 bytes, a leading
//! byte-order mark, a multiline string, a key only escape rules can spell out) fails closed at the
//! call site with ZERO writes. It is the same discipline the Hermes YAML merge follows.
//!
//! Two of these answer the questions an APPENDING engine has to ask before it writes a header of
//! its own: [`shape_is_unprovable`] (is this file readable by anchors at all?) and
//! [`root_defines_key`] (would the header topos appends redefine a key the file already sets?).

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

/// What a table header states about `key` — matched on the HEAD segment of the header's dotted
/// key path, in every spelling TOML reads as that name: bare or quoted segments, whitespace
/// around them. `None` for a header naming some other key, for a non-header line, and for a head
/// these anchors cannot spell out (a quoted segment holding an escape — which also fails the
/// whole file via [`shape_is_unprovable`]).
///
/// An APPENDING engine needs the distinction both ways: a `[key]` table means an appended
/// `[[key]]` would REDEFINE the name (and vice versa), while `[key.sub]` merely creates the name
/// implicitly, after which TOML still allows the explicit super-table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeaderStated {
    /// `[key]` (`whole`) or `[key.sub]` — a plain table at, or created implicitly under, the name.
    Table { whole: bool },
    /// `[[key]]` (`whole`) or `[[key.sub]]` — an array of tables at or under the name.
    ArrayOfTables { whole: bool },
}

pub(super) fn header_stated(line: &str, key: &str) -> Option<HeaderStated> {
    let header = table_header(line)?;
    let (array, interior) = match header.strip_prefix("[[") {
        Some(inner) => (true, inner.strip_suffix("]]")?),
        None => (false, header.strip_prefix('[')?.strip_suffix(']')?),
    };
    let (head, rest) = key_segment(interior.trim())?;
    if head != key {
        return None;
    }
    let whole = match rest.trim_start().chars().next() {
        None => true,
        Some('.') => false,
        Some(_) => return None,
    };
    Some(if array {
        HeaderStated::ArrayOfTables { whole }
    } else {
        HeaderStated::Table { whole }
    })
}

/// What a line states about a key — the two shapes an anchored edit has to tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyStated {
    /// The line assigns the key itself: `hooks = true`, `"hooks" = false`.
    Whole,
    /// The key is only the HEAD of a dotted path: `features.hooks = false` states `features`,
    /// which is a definition of `features` without a `[features]` header to anchor on.
    DottedHead,
}

/// How a line states `key`, if it states it at all — a comment, a longer bare name
/// (`hooks_enabled`), or any other key states nothing.
///
/// **Quoted spellings are the same key.** TOML reads `hooks`, `"hooks"` and `'hooks'` as one and
/// the same name, so an engine that saw only the bare one would insert a second `hooks` beside the
/// quoted one already there and leave the config unparsable. The quotes are stripped and the
/// content compared; escapes are deliberately NOT resolved — a quoted key carrying a backslash
/// names something only TOML's escape rules can spell out, and [`shape_is_unprovable`] fails the
/// whole file closed on one rather than guessing here.
pub(super) fn key_stated(line: &str, key: &str) -> Option<KeyStated> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let (segment, rest) = key_segment(trimmed)?;
    if segment != key {
        return None;
    }
    match rest.trim_start().chars().next()? {
        '=' => Some(KeyStated::Whole),
        '.' => Some(KeyStated::DottedHead),
        _ => None,
    }
}

/// Whether a line assigns `key` (`hooks = …`, `"hooks" = …`), rather than mentioning it in a
/// comment, naming a longer key, or defining something under it.
pub(super) fn is_key_line(line: &str, key: &str) -> bool {
    key_stated(line, key) == Some(KeyStated::Whole)
}

/// Whether a config's ROOT context — the lines before its first table header — defines `key`.
///
/// This is the shape an engine that APPENDS a `[key]` / `[[key]]` table must refuse: `features =
/// { hooks = false }` and `features.hooks = false` are valid spellings of a table with no header
/// to insert into, and a header appended beside one REDEFINES the key, which costs the harness its
/// whole config rather than one setting. Only the lines before the first header count — after it
/// every key belongs to a table, so `features.hooks` under `[tui]` is `tui.features.hooks` and
/// collides with nothing.
///
/// A HEADER naming the key (`[features.sub]` with no `[features]` of its own) is deliberately not
/// this shape: TOML lets a super-table be defined after its sub-table, so an appended `[features]`
/// stays legal there.
pub(super) fn root_defines_key(lines: &[&str], key: &str) -> bool {
    for line in lines {
        if table_header(line).is_some() {
            return false; // root context ends at the first header
        }
        if key_stated(line, key).is_some() {
            return true;
        }
    }
    false
}

/// Whether a config's SHAPE is one no line anchor can stand on — the whole-file screen an engine
/// runs BEFORE planning any edit, so a file it cannot read exactly is a file it never writes.
///
/// Two shapes fail here, both because a line inside them would be read as structure it is not:
///
/// - **A multiline string** (`"""` or `'''`) anywhere in the file. Its content lines are ordinary
///   text to a line anchor, so somebody's `[features]` or `[[hooks]]` written INSIDE their own
///   prompt string would read as a real table header and an edit would land in the middle of their
///   sentence. Proving where such a string ends means lexing TOML, which these anchors deliberately
///   do not do — a degraded registration with an honest note beats a clever parse.
/// - **A quoted key carrying a backslash** — `"ho\u006Fks" = false` is a legal spelling of the
///   very key an edit is about to write, and only TOML's escape rules say so. A key that cannot be
///   spelled out cannot be ruled out. A table HEADER holding a backslash fails for the same
///   reason: [`header_stated`] refuses to spell its head out, and a header that cannot be
///   spelled out cannot be ruled out either.
pub(super) fn shape_is_unprovable(text: &str) -> bool {
    text.contains("\"\"\"")
        || text.contains("'''")
        || split_lines(text).iter().any(|line| {
            escaped_quoted_key(line)
                || table_header(line).is_some_and(|header| header.contains('\\'))
        })
}

/// The first key segment a line names, unquoted, plus the rest of the line after it. `None` where
/// no anchor can name one: an empty segment, or a quoted spelling holding an escape.
fn key_segment(trimmed: &str) -> Option<(&str, &str)> {
    if let Some((segment, rest)) = quoted_segment(trimmed) {
        return (!segment.contains('\\')).then_some((segment, rest));
    }
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(trimmed.len());
    (end > 0).then(|| trimmed.split_at(end))
}

/// The RAW content of the quoted token a line opens with (escapes unresolved), plus what follows
/// its closing quote — `None` for anything that does not open with a quote, or never closes one.
fn quoted_segment(trimmed: &str) -> Option<(&str, &str)> {
    let quote = match trimmed.chars().next()? {
        q @ ('"' | '\'') => q,
        _ => return None,
    };
    let rest = &trimmed[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some((&rest[..end], &rest[end + quote.len_utf8()..]))
}

/// Whether a line names a key in a quoted spelling holding an escape — a key position is proven by
/// what FOLLOWS the token (`=` or a dotted `.`), so a quoted array element carrying a Windows path
/// is left alone rather than read as an unprovable key.
fn escaped_quoted_key(line: &str) -> bool {
    let Some((segment, rest)) = quoted_segment(line.trim_start()) else {
        return false;
    };
    segment.contains('\\') && matches!(rest.trim_start().chars().next(), Some('=' | '.'))
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

    /// A header states its key in every spelling TOML reads as that name — and the distinction an
    /// appending engine needs: the whole name, vs a path merely created under it.
    #[test]
    fn a_header_states_its_key_in_every_spelling_toml_reads() {
        use HeaderStated::*;
        for (line, key, stated) in [
            ("[features]\n", "features", Table { whole: true }),
            ("[\"features\"]\n", "features", Table { whole: true }),
            ("[ features ]  # mine\n", "features", Table { whole: true }),
            ("['features'.sub]\n", "features", Table { whole: false }),
            ("[features . sub]\n", "features", Table { whole: false }),
            ("[[hooks]]\n", "hooks", ArrayOfTables { whole: true }),
            ("[['hooks']]\n", "hooks", ArrayOfTables { whole: true }),
            ("[[hooks.sub]]\n", "hooks", ArrayOfTables { whole: false }),
        ] {
            assert_eq!(header_stated(line, key), Some(stated), "{line:?}");
        }
        for (line, key) in [
            ("[featureset]\n", "features"),          // a longer name
            ("[mcp_servers.\"x.y\"]\n", "features"), // somebody else's table
            ("hooks = true\n", "hooks"),             // not a header at all
            ("# [[hooks]]\n", "hooks"),              // a comment
            ("[\"ho\\u006Fks\"]\n", "hooks"),        // an escape these anchors refuse to spell out
        ] {
            assert_eq!(header_stated(line, key), None, "{line:?} vs {key}");
        }
    }

    /// A quoted key is the SAME key as the bare one, so an engine matching keys sees both — the
    /// miss that would insert a duplicate `hooks` beside `"hooks"` and leave the config unparsable.
    #[test]
    fn a_quoted_key_is_the_same_key_as_its_bare_spelling() {
        for quoted in [
            "\"hooks\" = true\n",
            "'hooks' = false\n",
            "  \"hooks\"   =   true  # mine\n",
        ] {
            assert!(is_key_line(quoted, "hooks"), "{quoted:?}");
        }
        assert_eq!(
            key_stated("\"hooks\" = true\n", "hooks"),
            Some(KeyStated::Whole)
        );
        assert_eq!(
            key_stated("'features'.hooks = false\n", "features"),
            Some(KeyStated::DottedHead)
        );
        for not_the_key in [
            "\"hooks_enabled\" = true\n", // a longer name
            "\"hook\" = true\n",          // a shorter one
            "# \"hooks\" = true\n",       // a comment
            "\"hooks\"\n",                // no assignment at all
            "\"ho\\u006Fks\" = true\n",   // an escape these anchors refuse to spell out
        ] {
            assert!(!is_key_line(not_the_key, "hooks"), "{not_the_key:?}");
        }
    }

    /// A file holding a multiline string is a file whose lines cannot be read as structure — and
    /// so is one naming a key only TOML's escape rules could spell out.
    #[test]
    fn a_multiline_string_or_an_escaped_key_makes_the_shape_unprovable() {
        for unprovable in [
            "prompt = \"\"\"\n[features]\nhooks = false\n\"\"\"\n",
            "prompt = '''\n[[hooks]]\n'''\n",
            "[features]\n\"ho\\u006Fks\" = false\n",
            "\"features\\u002Ehooks\".x = 1\n",
            "[\"fea\\u0074ures\"]\nx = 1\n", // a header only escape rules can spell out
        ] {
            assert!(shape_is_unprovable(unprovable), "{unprovable:?}");
        }
        for provable in [
            "model = \"gpt-5-codex\"\n[features]\nhooks = true\n",
            "command = \"say \\\"hi\\\"\"\n", // escapes inside a VALUE are ordinary
            "roots = [\n  \"C:\\\\Users\\\\me\",\n]\n", // a quoted array element is not a key
        ] {
            assert!(!shape_is_unprovable(provable), "{provable:?}");
        }
    }

    /// The root-context definitions an appended table header would REDEFINE — and the shapes that
    /// only look like them: a dotted key inside somebody else's table, and a sub-table header,
    /// after which an explicit super-table is still legal TOML.
    #[test]
    fn a_root_context_key_is_the_one_an_appended_header_would_redefine() {
        for defines in [
            "features.hooks = false\n",
            "features = { hooks = false }\n",
            "\"features\" = {}\n",
            "model = \"gpt\"\n\nfeatures.hooks = false\n\n[tui]\nmouse = false\n",
        ] {
            assert!(
                root_defines_key(&split_lines(defines), "features"),
                "{defines:?}"
            );
        }
        for leaves_room in [
            "[tui]\nfeatures.hooks = false\n", // `tui.features`, somebody else's key entirely
            "[features.sub]\nx = 1\n",         // a super-table may still be defined afterward
            "[features]\nhooks = true\n",
            "# features.hooks = false\n",
            "featureset = 1\n",
            "",
        ] {
            assert!(
                !root_defines_key(&split_lines(leaves_room), "features"),
                "{leaves_room:?}"
            );
        }
        assert!(root_defines_key(&split_lines("hooks = []\n"), "hooks"));
        assert!(!root_defines_key(
            &split_lines("[[hooks]]\nevent = \"Stop\"\n"),
            "hooks"
        ));
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
