//! **One converge, run end to end, for every agent the breadth rows added** — and only tests: the
//! shapes themselves live in [`entry_value`](super::entry_value) and the surfaces in the registry
//! table.
//!
//! A driver test proves a mechanism; this proves a HARNESS. Each case starts from a file that
//! already holds somebody else's server (and, where the real file has one, a sibling key or a
//! comment that has nothing to do with MCP) and walks the whole life of a topos entry through it:
//!
//! 1. **add** — the entry lands, and the exact bytes it lands as are asserted, because a wrong key
//!    can brick the agent that reads them;
//! 2. **current** — the same desired set applied again leaves the file byte-identical;
//! 3. **update** — a changed address replaces the entry in place;
//! 4. **drift** — a hand edit is left exactly as the hand left it;
//! 5. **remove** — the desired set empties and the file comes back to what it was, with the
//!    foreign entry and every sibling byte still there.
//!
//! The foreign entry and the siblings are checked at EVERY step, not just the last one: the whole
//! promise of this layer is that a file topos edits is a file topos gives back.

use std::collections::BTreeMap;

use serde_json::Value;

use super::descriptor::EnvRef;
use super::testutil::{entry, local_entry_with};
use super::{ApplyOutcome, EditPlan, EntryState, EnvValue, McpDialect, apply, entry_value};

const KEY: &str = "topos-acme-linear";
const URL: &str = "https://mcp.example/linear";
// Deliberately NOT a suffix of `URL`: "replaced, not appended" has to be provable by
// absence, and a prefix would still be found in the file after the edit.
const MOVED: &str = "https://mcp.example/v2-linear";

fn ledger(out: &ApplyOutcome) -> BTreeMap<String, String> {
    out.fingerprints.iter().cloned().collect()
}

fn written(out: &ApplyOutcome) -> String {
    match &out.plan {
        EditPlan::Write(bytes) => String::from_utf8(bytes.clone()).expect("utf-8"),
        other => panic!("expected Write, got {other:?}"),
    }
}

fn state(out: &ApplyOutcome, key: &str) -> EntryState {
    out.states
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, s)| *s)
        .unwrap_or_else(|| panic!("no state for {key}: {:?}", out.states))
}

/// The entry as it stands in `text`, under `slot` — parsed back out of the file the driver wrote,
/// so the assertion is about bytes that landed and not about a value in memory.
fn placed(text: &str, slot: &str, key: &str) -> Value {
    let doc: Value = super::jsonc_edit::parsed_for_tests(text).expect("parses");
    doc.get(slot)
        .and_then(|s| s.get(key))
        .unwrap_or_else(|| panic!("{key} not under {slot} in:\n{text}"))
        .clone()
}

/// One harness's whole converge. `before` is the file as the agent's own tool left it; `slot` is
/// the container key; `foreign` is the name of the entry in `before` that is somebody else's;
/// `survives` are substrings of `before` that must be present, byte for byte, in every output —
/// the siblings, comments and formatting topos does not own.
fn converges(
    dialect: McpDialect,
    before: &str,
    slot: &str,
    foreign: &str,
    survives: &[&str],
    expected: Value,
) {
    let bytes = before.as_bytes();
    let keeps = |text: &str, step: &str| {
        for needle in survives {
            assert!(
                text.contains(needle),
                "{dialect:?} {step}: lost {needle:?} from the file:\n{text}"
            );
        }
        assert!(
            placed(text, slot, foreign).is_object(),
            "{dialect:?} {step}: lost the foreign entry"
        );
    };

    // 1. ADD.
    let want = entry(KEY, URL);
    let out = apply(
        dialect,
        Some(bytes),
        std::slice::from_ref(&want),
        &BTreeMap::new(),
    );
    let added = written(&out);
    assert_eq!(state(&out, KEY), EntryState::PlacedNew, "{dialect:?}");
    assert!(!out.created_file, "{dialect:?}: the file was already there");
    keeps(&added, "add");
    assert_eq!(
        placed(&added, slot, KEY),
        expected,
        "{dialect:?}: the entry's own bytes"
    );
    // The foreign entry is not merely PRESENT — it is untouched, and it is reported as foreign
    // only when it looks managed, which somebody else's name does not.
    assert!(
        !out.states.iter().any(|(k, _)| k == foreign),
        "{dialect:?}: somebody else's entry is not topos's business"
    );
    let after_add = ledger(&out);

    // 2. CURRENT — the same demand, applied to what it already produced.
    let again = apply(
        dialect,
        Some(added.as_bytes()),
        std::slice::from_ref(&want),
        &after_add,
    );
    assert_eq!(again.plan, EditPlan::Leave, "{dialect:?}: idempotent");
    assert_eq!(state(&again, KEY), EntryState::Current, "{dialect:?}");

    // 3. UPDATE — the server moved.
    let moved = entry(KEY, MOVED);
    let out = apply(
        dialect,
        Some(added.as_bytes()),
        std::slice::from_ref(&moved),
        &after_add,
    );
    let updated = written(&out);
    assert_eq!(state(&out, KEY), EntryState::Updated, "{dialect:?}");
    keeps(&updated, "update");
    assert!(
        !updated.contains(URL) && updated.contains(MOVED),
        "{dialect:?}: the address was replaced, not appended:\n{updated}"
    );
    let after_update = ledger(&out);

    // 4. DRIFT — a hand edit is a decision, and it stands.
    let edited = updated.replace(MOVED, "https://mcp.example/mine");
    let out = apply(
        dialect,
        Some(edited.as_bytes()),
        std::slice::from_ref(&moved),
        &after_update,
    );
    assert_eq!(state(&out, KEY), EntryState::Drifted, "{dialect:?}");
    assert_eq!(
        out.plan,
        EditPlan::Leave,
        "{dialect:?}: drift is never written over"
    );
    assert_eq!(
        ledger(&out).get(KEY),
        after_update.get(KEY),
        "{dialect:?}: the ledger keeps the OLD fingerprint, so the drift survives a re-run"
    );

    // 5. REMOVE — and the file comes back.
    let out = apply(dialect, Some(updated.as_bytes()), &[], &after_update);
    let removed = written(&out);
    assert_eq!(state(&out, KEY), EntryState::Removed, "{dialect:?}");
    keeps(&removed, "remove");
    assert!(
        !removed.contains(KEY),
        "{dialect:?}: the entry is gone:\n{removed}"
    );
    assert!(
        !removed.contains(MOVED),
        "{dialect:?}: and so is its address"
    );
}

/// `{"k": "v"}` as a [`Value`], for the expected-entry literals below.
fn obj(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    )
}

fn s(text: &str) -> Value {
    Value::String(text.to_owned())
}

#[test]
fn vs_code_converges_under_servers_and_keeps_its_inputs() {
    // The real file's two other sections, and a comment — VS Code's own JSON reads all of it.
    let before = r#"{
  // the key the extension prompts for
  "inputs": [
    { "type": "promptString", "id": "api-key", "password": true }
  ],
  "servers": {
    "playwright": { "type": "stdio", "command": "npx", "args": ["@playwright/mcp@latest"] }
  },
  "sandbox": { "enabled": false }
}
"#;
    converges(
        McpDialect::VscodeJson,
        before,
        "servers",
        "playwright",
        &[
            "// the key the extension prompts for",
            "\"inputs\"",
            "\"api-key\"",
            "\"sandbox\"",
        ],
        obj(&[("type", s("http")), ("url", s(URL))]),
    );
}

#[test]
fn copilot_cli_converges_and_states_the_tools_it_exposes() {
    let before = r#"{
  "mcpServers": {
    "github": { "type": "http", "url": "https://api.githubcopilot.com/mcp/", "tools": ["*"] }
  }
}
"#;
    converges(
        McpDialect::CopilotCliJson,
        before,
        "mcpServers",
        "github",
        &["api.githubcopilot.com"],
        obj(&[
            ("tools", Value::Array(vec![s("*")])),
            ("type", s("http")),
            ("url", s(URL)),
        ]),
    );
}

#[test]
fn gemini_cli_converges_inside_a_settings_file_that_is_mostly_not_mcp() {
    let before = r#"{
  "$schema": "https://gemini/settings.schema.json",
  "ui": { "theme": "dark" },
  "mcpServers": {
    "notes": { "command": "notes-server" }
  },
  "tools": { "sandbox": false }
}
"#;
    converges(
        McpDialect::GeminiJson,
        before,
        "mcpServers",
        "notes",
        &["\"$schema\"", "\"ui\"", "\"theme\": \"dark\"", "\"tools\""],
        obj(&[("type", s("http")), ("url", s(URL))]),
    );
}

#[test]
fn windsurf_converges_and_spells_the_address_its_own_way() {
    let before = r#"{
  "mcpServers": {
    "figma": { "serverUrl": "https://figma.example/mcp" }
  }
}
"#;
    converges(
        McpDialect::WindsurfJson,
        before,
        "mcpServers",
        "figma",
        &["figma.example"],
        obj(&[("serverUrl", s(URL))]),
    );
}

/// The two whose `type` word differs by one hyphen — and whose files look otherwise identical.
/// Leaving the key out means SSE in both, silently, which is why it is asserted here by value.
#[test]
fn cline_and_roo_each_state_their_own_spelling_of_the_same_transport() {
    let before = r#"{
  "mcpServers": {
    "sentry": { "type": "streamableHttp", "url": "https://sentry.example/mcp" }
  }
}
"#;
    converges(
        McpDialect::ClineJson,
        before,
        "mcpServers",
        "sentry",
        &["sentry.example"],
        obj(&[("type", s("streamableHttp")), ("url", s(URL))]),
    );
    converges(
        McpDialect::RooJson,
        before,
        "mcpServers",
        "sentry",
        &["sentry.example"],
        obj(&[("type", s("streamable-http")), ("url", s(URL))]),
    );
}

#[test]
fn zed_converges_without_touching_the_editor_configuration_around_it() {
    // What a real Zed settings file looks like: comments, and everything else the person uses.
    let before = r#"{
  // Zed settings — see zed.dev/docs
  "theme": "One Dark",
  "buffer_font_size": 15,
  "context_servers": {
    "postgres": { "command": "psql-mcp", "args": ["--tls"] }
  },
  "languages": {
    // rust
    "Rust": { "tab_size": 4 }
  }
}
"#;
    converges(
        McpDialect::ZedJson,
        before,
        "context_servers",
        "postgres",
        &[
            "// Zed settings — see zed.dev/docs",
            "\"theme\": \"One Dark\"",
            "\"buffer_font_size\": 15",
            "// rust",
            "\"tab_size\": 4",
        ],
        obj(&[("url", s(URL))]),
    );
}

#[test]
fn lm_studio_converges_with_an_address_and_no_type_beside_it() {
    let before = r#"{
  "mcpServers": {
    "hf-mcp-server": { "url": "https://huggingface.co/mcp" }
  }
}
"#;
    converges(
        McpDialect::LmStudioJson,
        before,
        "mcpServers",
        "hf-mcp-server",
        &["huggingface.co"],
        obj(&[("url", s(URL))]),
    );
}

/// Claude Desktop's converge is the one that runs on a PROGRAM: the file cannot dial an address,
/// so a shared server reaches it through the bridge, and that is what the entry holds.
#[test]
fn claude_desktop_converges_on_the_program_the_bridge_renders() {
    let before = r#"{
  "globalShortcut": "Alt+Space",
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/x/Desktop"]
    }
  }
}
"#;
    let bridged = local_entry_with(
        KEY,
        "npx",
        &["-y", "mcp-remote@0.1.38", URL],
        Vec::new(),
        EnvRef::DollarBrace,
    );
    let out = apply(
        McpDialect::ClaudeDesktopJson,
        Some(before.as_bytes()),
        std::slice::from_ref(&bridged),
        &BTreeMap::new(),
    );
    let text = written(&out);
    assert_eq!(state(&out, KEY), EntryState::PlacedNew);
    assert!(text.contains("\"globalShortcut\""), "{text}");
    assert!(placed(&text, "mcpServers", "filesystem").is_object());
    // The bare triple its own documentation writes — and NO `type`, which none of them carry.
    assert_eq!(
        placed(&text, "mcpServers", KEY),
        obj(&[
            (
                "args",
                Value::Array(vec![s("-y"), s("mcp-remote@0.1.38"), s(URL)])
            ),
            ("command", s("npx")),
        ])
    );

    // …and it converges back out, leaving the file it was given.
    let prior: BTreeMap<String, String> = ledger(&out);
    let out = apply(
        McpDialect::ClaudeDesktopJson,
        Some(text.as_bytes()),
        &[],
        &prior,
    );
    assert_eq!(state(&out, KEY), EntryState::Removed);
    assert_eq!(written(&out), before, "the file comes back byte for byte");
}

/// Goose's converge, over a config.yaml shaped the way goose's own tool writes one: its bundled
/// extensions are BLOCK mappings several lines deep, and they are the thing that must not move.
#[test]
fn goose_converges_among_its_own_bundled_extensions() {
    let before = "\
GOOSE_PROVIDER: anthropic
GOOSE_MODEL: claude-opus-4
extensions:
  developer:
    bundled: true
    display_name: Developer
    enabled: true
    name: developer
    timeout: 300
    type: builtin
  memory:
    bundled: true
    enabled: true
    name: memory
    type: builtin
";
    let want = entry(KEY, URL);
    let out = apply(
        McpDialect::GooseYaml,
        Some(before.as_bytes()),
        std::slice::from_ref(&want),
        &BTreeMap::new(),
    );
    let added = written(&out);
    assert_eq!(state(&out, KEY), EntryState::PlacedNew);
    // Goose reads an EXTENSION: all four of the fields its config enum requires, and the sentinel
    // that is the only thing making this line topos's rather than a person's.
    assert!(
        added.contains(&format!(
            "  {KEY}: {{enabled: true, name: {KEY}, type: streamable_http, uri: \"{URL}\"}}  # topos:mcp\n"
        )),
        "{added}"
    );
    // NOTHING of goose's own moved — every line of both builtins is still there, in order.
    for line in before.lines() {
        assert!(added.contains(line), "lost {line:?} from:\n{added}");
    }
    assert!(
        added.contains("    type: builtin\n"),
        "a builtin is never rewritten:\n{added}"
    );

    // Idempotent, then removed — and the file comes back byte for byte.
    let prior: BTreeMap<String, String> = ledger(&out);
    let again = apply(
        McpDialect::GooseYaml,
        Some(added.as_bytes()),
        std::slice::from_ref(&want),
        &prior,
    );
    assert_eq!(again.plan, EditPlan::Leave);
    assert_eq!(state(&again, KEY), EntryState::Current);

    let out = apply(McpDialect::GooseYaml, Some(added.as_bytes()), &[], &prior);
    assert_eq!(state(&out, KEY), EntryState::Removed);
    assert_eq!(written(&out), before, "goose's own file, back as it was");
}

/// The same converge for a program goose RUNS — its extension enum's other variant, where the
/// command line carries goose's own field names (`cmd`, `envs`) under the `type: stdio` tag. The
/// builtins beside it are block mappings and stay block mappings.
#[test]
fn goose_converges_a_program_among_its_own_bundled_extensions() {
    let before = "\
GOOSE_PROVIDER: anthropic
extensions:
  developer:
    bundled: true
    display_name: Developer
    enabled: true
    name: developer
    timeout: 300
    type: builtin
";
    let want = local_entry_with(
        KEY,
        "npx",
        &["-y", "some-server@1.0.0"],
        vec![("LINEAR_TOKEN".to_owned(), EnvValue::Inherited)],
        EnvRef::DollarBrace,
    );
    let out = apply(
        McpDialect::GooseYaml,
        Some(before.as_bytes()),
        std::slice::from_ref(&want),
        &BTreeMap::new(),
    );
    let added = written(&out);
    assert_eq!(state(&out, KEY), EntryState::PlacedNew);
    assert!(
        added.contains(&format!(
            "  {KEY}: {{enabled: true, name: {KEY}, type: stdio, cmd: \"npx\", args: [\"-y\", \"some-server@1.0.0\"], envs: {{LINEAR_TOKEN: \"${{LINEAR_TOKEN}}\"}}}}  # topos:mcp\n"
        )),
        "{added}"
    );
    // NOTHING of goose's own moved.
    for line in before.lines() {
        assert!(added.contains(line), "lost {line:?} from:\n{added}");
    }

    // The program topos runs is the address this entry names, read back off the line it wrote —
    // `cmd` is the same field as `command` under another name.
    let seen = super::observe_entries(McpDialect::GooseYaml, Some(added.as_bytes()), None)
        .expect("readable");
    assert_eq!(
        seen.iter()
            .find(|e| e.name == KEY)
            .and_then(|e| e.address.clone()),
        super::local_address("npx", &["-y".to_owned(), "some-server@1.0.0".to_owned()]),
    );

    // Idempotent, then removed — and the file comes back byte for byte.
    let prior: BTreeMap<String, String> = ledger(&out);
    let again = apply(
        McpDialect::GooseYaml,
        Some(added.as_bytes()),
        std::slice::from_ref(&want),
        &prior,
    );
    assert_eq!(again.plan, EditPlan::Leave);
    assert_eq!(state(&again, KEY), EntryState::Current);

    let out = apply(McpDialect::GooseYaml, Some(added.as_bytes()), &[], &prior);
    assert_eq!(state(&out, KEY), EntryState::Removed);
    assert_eq!(written(&out), before, "goose's own file, back as it was");
}

/// A builtin whose NAME topos would never mint is not topos's — and one written in the flow style
/// topos writes is still not topos's without the sentinel. Both are read, neither is touched.
#[test]
fn a_goose_extension_without_the_sentinel_is_never_read_as_topos_s() {
    let before = "\
extensions:
  topos-lookalike: {enabled: true, name: topos-lookalike, type: streamable_http, uri: \"https://x/mcp\"}
  developer: {bundled: true, enabled: true, name: developer, type: builtin}
";
    let want = entry(KEY, URL);
    let out = apply(
        McpDialect::GooseYaml,
        Some(before.as_bytes()),
        std::slice::from_ref(&want),
        &BTreeMap::new(),
    );
    let added = written(&out);
    // The lookalike is managed-LOOKING, so it is REPORTED — as somebody else's, and left alone.
    assert_eq!(state(&out, "topos-lookalike"), EntryState::Foreign);
    assert!(
        !out.fingerprints.iter().any(|(k, _)| k == "topos-lookalike"),
        "a foreign entry never enters the ledger"
    );
    for line in before.lines() {
        assert!(added.contains(line), "lost {line:?} from:\n{added}");
    }
    // And both of them are SEEN, with the address each names.
    let seen = super::observe_entries(McpDialect::GooseYaml, Some(before.as_bytes()), None)
        .expect("readable");
    assert_eq!(
        seen.iter()
            .find(|e| e.name == "topos-lookalike")
            .and_then(|e| e.address.clone()),
        Some("https://x/mcp".to_owned()),
        "goose spells an address `uri`, and it is read as one"
    );
    assert!(seen.iter().any(|e| e.name == "developer"));
}

/// A boolean read back off the line topos wrote is a BOOLEAN. Were it read as the text "true",
/// every goose entry would fingerprint differently from the value it was rendered from and read
/// as drifted forever, on the first sweep after it was placed.
#[test]
fn goose_entry_round_trips_to_the_value_it_was_rendered_from() {
    let want = entry(KEY, URL);
    let out = apply(
        McpDialect::GooseYaml,
        None,
        std::slice::from_ref(&want),
        &BTreeMap::new(),
    );
    assert!(out.created_file);
    let text = written(&out);
    let observed = super::observe(McpDialect::GooseYaml, Some(text.as_bytes()));
    assert!(observed.parseable);
    assert_eq!(
        observed.entries.get(KEY),
        Some(&super::fingerprint_value(
            &entry_value(McpDialect::GooseYaml, &want).expect("renders")
        )),
        "what was written reads back as what it was written from"
    );
}

/// The pairs no vendor evidence covers answer `None` — and every OTHER new dialect answers with a
/// shape, so a `None` is a statement about evidence and not a hole in the table.
#[test]
fn a_dialect_says_nothing_where_its_agent_documents_nothing() {
    let address = entry(KEY, URL);
    let program = local_entry_with(
        KEY,
        "npx",
        &["-y", "some-server@1.0.0"],
        vec![("TOKEN".to_owned(), EnvValue::Inherited)],
        EnvRef::DollarBraceEnv,
    );
    assert!(entry_value(McpDialect::LmStudioJson, &program).is_none());
    assert!(entry_value(McpDialect::ClaudeDesktopJson, &address).is_none());
    // Every other breadth dialect answers for BOTH shapes — goose included, since its program
    // grammar was verified against a real build.
    for dialect in [
        McpDialect::VscodeJson,
        McpDialect::CopilotCliJson,
        McpDialect::GeminiJson,
        McpDialect::WindsurfJson,
        McpDialect::ClineJson,
        McpDialect::RooJson,
        McpDialect::ZedJson,
        McpDialect::GooseYaml,
    ] {
        assert!(entry_value(dialect, &address).is_some(), "{dialect:?}");
        assert!(entry_value(dialect, &program).is_some(), "{dialect:?}");
    }
}

/// A program entry's environment slot the MACHINE fills, spelled the way each of these agents
/// reads one. The value never travels; the NAME does.
#[test]
fn an_inherited_slot_is_spelled_in_each_agents_own_reference_syntax() {
    let program = |env_ref| {
        local_entry_with(
            KEY,
            "npx",
            &["-y", "some-server@1.0.0"],
            vec![("LINEAR_TOKEN".to_owned(), EnvValue::Inherited)],
            env_ref,
        )
    };
    let env_of = |dialect, env_ref| {
        entry_value(dialect, &program(env_ref))
            .expect("renders")
            .get("env")
            .and_then(|e| e.get("LINEAR_TOKEN"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| panic!("{dialect:?} carries an env slot"))
    };
    for dialect in [
        McpDialect::VscodeJson,
        McpDialect::ClineJson,
        McpDialect::RooJson,
        McpDialect::WindsurfJson,
    ] {
        assert_eq!(
            env_of(dialect, EnvRef::DollarBraceEnv),
            "${env:LINEAR_TOKEN}",
            "{dialect:?}"
        );
    }
    for dialect in [McpDialect::GeminiJson, McpDialect::CopilotCliJson] {
        assert_eq!(
            env_of(dialect, EnvRef::DollarBrace),
            "${LINEAR_TOKEN}",
            "{dialect:?}"
        );
    }
}

/// A fresh file is a whole-file creation topos may later delete — and it is the MINIMAL document
/// in each dialect's own container key, never a template of somebody else's sections.
#[test]
fn a_fresh_file_is_the_minimal_document_under_the_right_key() {
    let want = entry(KEY, URL);
    let fresh = |dialect| {
        let out = apply(dialect, None, std::slice::from_ref(&want), &BTreeMap::new());
        assert!(out.created_file, "{dialect:?}");
        written(&out)
    };
    assert!(fresh(McpDialect::VscodeJson).contains("\"servers\""));
    assert!(fresh(McpDialect::ZedJson).contains("\"context_servers\""));
    for dialect in [
        McpDialect::CopilotCliJson,
        McpDialect::GeminiJson,
        McpDialect::WindsurfJson,
        McpDialect::ClineJson,
        McpDialect::RooJson,
        McpDialect::LmStudioJson,
    ] {
        assert!(fresh(dialect).contains("\"mcpServers\""), "{dialect:?}");
    }
}

/// A file this agent's OWN tool wrote with comments in it is still editable where the agent reads
/// comments, and refused untouched where it does not — the strictness column, stated as behaviour.
#[test]
fn a_commented_file_is_edited_only_where_that_agents_parser_reads_comments() {
    let commented = |slot: &str| format!("{{\n  // mine\n  \"{slot}\": {{}}\n}}\n");
    let want = entry(KEY, URL);
    let run = |dialect, text: &str| {
        apply(
            dialect,
            Some(text.as_bytes()),
            std::slice::from_ref(&want),
            &BTreeMap::new(),
        )
        .plan
    };
    // VS Code and Zed read their own JSON with comments in it, so the comment simply survives.
    for (dialect, slot) in [
        (McpDialect::VscodeJson, "servers"),
        (McpDialect::ZedJson, "context_servers"),
    ] {
        let text = commented(slot);
        match run(dialect, &text) {
            EditPlan::Write(bytes) => {
                let out = String::from_utf8(bytes).unwrap();
                assert!(out.contains("// mine"), "{dialect:?}: {out}");
            }
            other => panic!("{dialect:?}: expected a Write, got {other:?}"),
        }
    }
    // The rest are read by a plain JSON parser, so a comment is a file topos cannot prove it can
    // give back — refused, with nothing written.
    for dialect in [
        McpDialect::CopilotCliJson,
        McpDialect::GeminiJson,
        McpDialect::ClineJson,
        McpDialect::RooJson,
        McpDialect::LmStudioJson,
        McpDialect::ClaudeDesktopJson,
    ] {
        assert!(
            matches!(
                run(dialect, &commented("mcpServers")),
                EditPlan::Unprovable(_)
            ),
            "{dialect:?}"
        );
    }
}

/// Reading a surface topos did not write: every entry in it, whoever wrote it, with the address it
/// names — including the two agents that spell an address as something other than `url`.
#[test]
fn every_entry_is_seen_whatever_its_agent_calls_an_address() {
    let seen = |dialect, text: &str, slot: Option<&str>| {
        super::observe_entries(dialect, Some(text.as_bytes()), slot)
            .expect("readable")
            .into_iter()
            .map(|e| (e.name, e.address))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        seen(
            McpDialect::WindsurfJson,
            r#"{"mcpServers":{"figma":{"serverUrl":"https://Figma.example:443/mcp"}}}"#,
            None
        ),
        vec![(
            "figma".to_owned(),
            Some("https://figma.example/mcp".to_owned())
        )]
    );
    assert_eq!(
        seen(
            McpDialect::VscodeJson,
            r#"{"inputs":[],"servers":{"a":{"type":"http","url":"https://a.example/mcp/"}}}"#,
            None
        ),
        vec![("a".to_owned(), Some("https://a.example/mcp/".to_owned()))]
    );
    // Zed's slot is its own, and a program-run entry answers with its command line.
    assert_eq!(
        seen(
            McpDialect::ZedJson,
            r#"{"context_servers":{"pg":{"command":"psql-mcp","args":["--tls"]}}}"#,
            None
        ),
        vec![(
            "pg".to_owned(),
            Some("run:[\"psql-mcp\",\"--tls\"]".to_owned())
        )]
    );
}
