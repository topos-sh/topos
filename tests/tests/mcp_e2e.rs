//! The composed MCP-BUNDLE loop, over real HTTP against the real web app and the real `topos`
//! binary: an author adopts a local `server.json` folder (`add --mcp`) and publishes it as a
//! `kind = "mcp"` catalog bundle; a second member's session sweeps and the server lands as a
//! config entry in ALL SIX MCP-capable agents, each in its own exact dialect; the applied report
//! carries the per-agent states back to the workspace; a removal converges every surface back to
//! the bytes the person had; a hand-edited entry is DRIFTED — never overwritten, never removed,
//! and disclosed; and a project-scoped row reaches the four project surfaces alone.
//!
//! Like the conflict suite, this one drives the REAL CLI BINARY (`target/<profile>/topos`) as a
//! subprocess rather than the in-process fixture rig: `add --mcp`, the per-scope config converge
//! and the harness detection all resolve against `$HOME` / `$TOPOS_HOME`, so only a real process
//! with a fake home proves them. The two halves share one installation — the fixture rig owns the
//! browser login (the `/verify` approval), the binary owns every verb after it, both over the same
//! `~/.topos`.

mod common;

use std::path::{Path, PathBuf};

use common::{CliOut, OWNER_EMAIL, Session, Stack, cli_binary, run_cli, start_stack};
use serde_json::{Value, json};
use topos::test_support::SessionInstall;

// ── the shared scenario constants ───────────────────────────────────────────────────────────────

/// The published bundle's name — the catalog row, the `publish` target, and the key-mint half.
const BUNDLE: &str = "team-weather";
/// The document's embedded registry name (what the registry-shape lane serves it under).
const SERVER_NAME: &str = "io.github.acme/team-weather";
/// The one remote endpoint every dialect must carry, byte-identically.
const SERVER_URL: &str = "https://team-weather.acme.example/mcp";
/// The minted config key for the workspace bundle: `topos-<workspace slug>-<name>`.
const KEY: &str = "topos-acme-team-weather";
/// A url a hand-edit puts in place of [`SERVER_URL`] (the drift phase).
const DRIFTED_URL: &str = "https://hand-edited.example/mcp";

/// The six MCP-capable harnesses, in descriptor order.
const SIX: [&str; 6] = [
    "claude-code",
    "codex",
    "cursor",
    "opencode",
    "openclaw",
    "hermes-agent",
];

// ── the real binary ─────────────────────────────────────────────────────────────────────────────

/// One installation the REAL binary drives: a fake `$HOME` (where the six harnesses are detected
/// and their configs live) over the fixture rig's `~/.topos` (where the login it performed lives).
struct Install {
    rig: SessionInstall,
    bin: PathBuf,
}

impl Install {
    /// A fresh installation whose fake home holds every one of the six MCP-capable harnesses'
    /// detect dirs — so detection, and therefore engagement, is uniform across the matrix.
    fn new(tag: &str) -> Self {
        let rig = SessionInstall::new(tag);
        let home = rig.root().join("home");
        for dir in [
            ".claude",
            ".codex",
            ".cursor",
            // OpenCode lives wholly under the XDG config root — the dir it is DETECTED by is the
            // same dir its MCP config file sits in.
            ".config/opencode",
            ".openclaw",
            ".hermes",
        ] {
            std::fs::create_dir_all(home.join(dir)).expect("seed a harness detect dir");
        }
        Self {
            rig,
            bin: cli_binary(),
        }
    }

    fn root(&self) -> &Path {
        self.rig.root()
    }

    fn home(&self) -> PathBuf {
        self.rig.root().join("home")
    }

    /// Run the real binary from `cwd` with this installation's roots.
    fn run(&self, cwd: &Path, args: &[&str]) -> CliOut {
        run_cli(
            &self.bin,
            &self.home(),
            &self.root().join(".topos"),
            cwd,
            args,
        )
    }
}

// ── the six user-scope config surfaces ──────────────────────────────────────────────────────────

/// The Claude Code plugin dir topos owns whole.
fn claude_plugin_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("skills").join("topos-mcp")
}

/// The person's OWN content in each PATCHED surface (the Claude plugin dir is rendered whole and
/// asserted separately), written before topos ever touches them — the
/// byte-exact baseline a removal must restore.
fn seed_user_configs(home: &Path) -> Vec<(&'static str, PathBuf, String)> {
    let seeds: Vec<(&'static str, PathBuf, String)> = vec![
        (
            "codex",
            home.join(".codex").join("config.toml"),
            "model = \"gpt-5\"\n\n[mcp_servers.house]\nurl = \"https://house.example/mcp\"\n"
                .to_owned(),
        ),
        (
            "cursor",
            home.join(".cursor").join("mcp.json"),
            "{\n  \"mcpServers\": {\n    \"house\": {\n      \"url\": \"https://house.example/mcp\"\n    }\n  }\n}\n"
                .to_owned(),
        ),
        (
            "opencode",
            home.join(".config").join("opencode").join("opencode.json"),
            "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {\n    \"house\": {\n      \"type\": \"remote\",\n      \"url\": \"https://house.example/mcp\",\n      \"enabled\": true\n    }\n  }\n}\n"
                .to_owned(),
        ),
        (
            "openclaw",
            home.join(".openclaw").join("openclaw.json"),
            "{\n  // the house server, hand-kept\n  \"mcp\": {\n    \"servers\": {\n      \"house\": {\n        \"transport\": \"streamable-http\",\n        \"url\": \"https://house.example/mcp\"\n      }\n    }\n  }\n}\n"
                .to_owned(),
        ),
        (
            "hermes-agent",
            home.join(".hermes").join("config.yaml"),
            "mcp_servers:\n  house: {url: \"https://house.example/mcp\"}\n".to_owned(),
        ),
    ];
    for (_, path, bytes) in &seeds {
        std::fs::create_dir_all(path.parent().expect("a surface has a parent"))
            .expect("create the surface dir");
        std::fs::write(path, bytes).expect("seed the surface");
    }
    seeds
}

/// A path as the CLIENT reports it — canonicalized (on macOS `/var` is a symlink to
/// `/private/var`, and the receipts carry the resolved spelling).
fn canon(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Read a config surface as text (panicking with the path — a missing surface is the assertion).
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} ({e})", path.display()))
}

/// Parse a surface as strict JSON.
fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read(path))
        .unwrap_or_else(|e| panic!("{} is not strict JSON ({e})", path.display()))
}

/// Parse OpenClaw's JSONC by stripping its `//` line comments — enough for the assertions here
/// (the seeded comment is the only one, and its survival is asserted on the raw text).
fn read_jsonc(path: &Path) -> Value {
    let text = read(path);
    let stripped: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&stripped)
        .unwrap_or_else(|e| panic!("{} is not JSONC we can read ({e}): {text}", path.display()))
}

/// The body lines of one `[table.path]` block in a TOML file (empty when the table is absent).
fn toml_table_body(text: &str, header: &str) -> Option<Vec<String>> {
    let mut lines = text.lines();
    lines.find(|l| l.trim() == header)?;
    Some(
        lines
            .take_while(|l| !l.trim_start().starts_with('['))
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// Every `agent → state` pair one update/add receipt row carries for `bundle`.
fn harness_states(data: &Value, bundle: &str) -> Vec<(String, String)> {
    data["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["skill"] == bundle)
        .map(|row| {
            row["harnesses"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|h| {
                    (
                        h["agent"].as_str().unwrap_or_default().to_owned(),
                        h["state"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The state one agent reported for `bundle` on this run.
fn state_of(data: &Value, bundle: &str, agent: &str) -> Option<String> {
    harness_states(data, bundle)
        .into_iter()
        .find(|(a, _)| a == agent)
        .map(|(_, s)| s)
}

// ── the arrangement halves ──────────────────────────────────────────────────────────────────────

/// Write the author's MCP bundle folder: ONE `server.json`, the shared vector's no-auth shape with
/// this suite's own name and endpoint.
fn write_server_bundle(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create the mcp bundle dir");
    let document = json!({
        "$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
        "name": SERVER_NAME,
        "description": "The team's shared weather endpoint.",
        "version": "1.0.0",
        "remotes": [ { "type": "streamable-http", "url": SERVER_URL } ],
    });
    let mut text = serde_json::to_string_pretty(&document).expect("render server.json");
    text.push('\n');
    std::fs::write(dir.join("server.json"), text).expect("write server.json");
}

/// Log an installation in through the REAL browser-approval ceremony (the fixture rig owns the
/// flow; the binary inherits the session it lands).
fn login(stack: &Stack, install: &Install, approver: &Session) {
    stack.login_begin_and_approve(&install.rig, approver);
    let granted = install.rig.login(None).expect("the login resume");
    assert_eq!(granted.session_status, "active");
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// The matrix — one composed stack, one arc.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn the_mcp_bundle_loop_across_six_agents() {
    let stack = start_stack("mcp");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── 1. the author: adopt a local server.json folder, then publish it ────────────────────────
    let author = Install::new("mcp-author");
    login(&stack, &author, &owner);
    seed_user_configs(&author.home());

    let root = author.root().canonicalize().expect("canonical author root");
    let src = root.join(BUNDLE);
    write_server_bundle(&src);

    let added = author
        .run(&root, &["add", "--mcp", "./team-weather", "-g", "--json"])
        .data("add --mcp");
    assert_eq!(
        added["name"], BUNDLE,
        "the folder name IS the bundle: {added}"
    );
    // The local-adopt arm converges the author's OWN surfaces immediately, under the LOCAL key.
    let author_cursor = read_json(&author.home().join(".cursor").join("mcp.json"));
    assert_eq!(
        author_cursor["mcpServers"]["topos-local-team-weather"],
        json!({ "url": SERVER_URL }),
        "the local adopt places under the local key: {author_cursor}"
    );

    let published = author
        .run(
            &root,
            &[
                "publish",
                BUNDLE,
                "--yes",
                "-m",
                "the team endpoint",
                "--json",
            ],
        )
        .data("publish");
    assert!(
        published["version_id"].is_string(),
        "the genesis publish lands a version: {published}"
    );

    // The catalog row's KIND, three ways: the row itself, the delivery lane, and the workspace's
    // own registry-shape read API (which serves `kind = 'mcp'` bundles and nothing else).
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT kind FROM web.bundle WHERE name = '{BUNDLE}'"
        )),
        Some("mcp".to_owned()),
        "the catalog records what the bundle IS"
    );
    let probe = stack.mint_session(&owner, "registry-probe");
    let registry = stack.origin_get_json(&probe.credential, "/registry/v0.1/servers");
    assert_eq!(registry.status, 200, "the registry lane: {}", registry.body);
    let registry: Value = serde_json::from_str(&registry.body).expect("registry JSON");
    assert_eq!(registry["metadata"]["count"], 1, "{registry}");
    assert_eq!(registry["servers"][0]["server"]["name"], SERVER_NAME);
    assert_eq!(
        registry["servers"][0]["server"]["remotes"][0]["url"],
        SERVER_URL
    );
    let delivery = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
    );
    assert_eq!(delivery.status, 200, "the delivery lane: {}", delivery.body);
    let delivery: Value = serde_json::from_str(&delivery.body).expect("delivery JSON");
    let item = delivery["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == BUNDLE)
        .unwrap_or_else(|| panic!("the delivery lane carries the bundle: {delivery}"));
    assert_eq!(item["kind"], "mcp", "the kind rides the wire: {item}");

    // ── 2. a second member sweeps: the entry lands in ALL SIX config surfaces ───────────────────
    let dev_browser = stack.add_member("dev@acme.test", "member");
    let dev = Install::new("mcp-dev");
    login(&stack, &dev, &dev_browser);
    let home = dev.home();
    let seeds = seed_user_configs(&home);

    let swept = dev.run(dev.root(), &["update", "--json"]).data("update");
    let states = harness_states(&swept, BUNDLE);
    assert_eq!(
        states
            .iter()
            .map(|(a, _)| a.as_str())
            .collect::<Vec<_>>()
            .as_slice(),
        SIX.as_slice(),
        "every MCP-capable agent reports: {swept}"
    );
    for (agent, state) in &states {
        assert_eq!(state, "current", "{agent} placed the entry: {swept}");
    }

    // Claude Code — a wholly topos-owned plugin DIR, both files rendered whole.
    let plugin = claude_plugin_dir(&home);
    assert_eq!(
        read(&plugin.join(".claude-plugin").join("plugin.json")),
        "{\n  \"description\": \"Team-shared MCP servers delivered by Topos\",\n  \"displayName\": \"Topos MCP\",\n  \"name\": \"topos-mcp\",\n  \"version\": \"0.1.0\"\n}\n",
        "the plugin manifest is the constant"
    );
    assert_eq!(
        read(&plugin.join(".mcp.json")),
        format!(
            "{{\n  \"mcpServers\": {{\n    \"{KEY}\": {{\n      \"type\": \"http\",\n      \"url\": \"{SERVER_URL}\"\n    }}\n  }}\n}}\n"
        ),
        "claude: `type: http` is mandatory"
    );

    // Codex — `[mcp_servers.<key>]` with the url and NOTHING else (a wrong key hard-fails its
    // whole config load).
    let codex = read(&home.join(".codex").join("config.toml"));
    assert_eq!(
        toml_table_body(&codex, &format!("[mcp_servers.{KEY}]")),
        Some(vec![format!("url = \"{SERVER_URL}\"")]),
        "codex: url-only: {codex}"
    );
    assert!(
        codex.contains("model = \"gpt-5\"") && codex.contains("[mcp_servers.house]"),
        "the person's own codex content survives: {codex}"
    );

    // Cursor — NEVER a `type` key (it makes cursor-agent drop the whole file).
    let cursor = read_json(&home.join(".cursor").join("mcp.json"));
    assert_eq!(
        cursor["mcpServers"][KEY],
        json!({ "url": SERVER_URL }),
        "cursor: url only: {cursor}"
    );

    // OpenCode — the remote type plus the explicit enable.
    let opencode = read_json(&home.join(".config").join("opencode").join("opencode.json"));
    assert_eq!(
        opencode["mcp"][KEY],
        json!({ "enabled": true, "type": "remote", "url": SERVER_URL }),
        "opencode: {opencode}"
    );

    // OpenClaw — nested `mcp.servers`, transport EXPLICIT, nothing else.
    let openclaw = read_jsonc(&home.join(".openclaw").join("openclaw.json"));
    assert_eq!(
        openclaw["mcp"]["servers"][KEY],
        json!({ "transport": "streamable-http", "url": SERVER_URL }),
        "openclaw: {openclaw}"
    );
    assert!(
        read(&home.join(".openclaw").join("openclaw.json")).contains("// the house server"),
        "the person's JSONC comment survives"
    );

    // Hermes — ONE flow-mapping line at the block's child indent, sentinel-suffixed.
    let hermes = read(&home.join(".hermes").join("config.yaml"));
    assert!(
        hermes.contains(&format!(
            "  {KEY}: {{url: \"{SERVER_URL}\"}}  # topos:mcp\n"
        )),
        "hermes: the one-line sentinel entry: {hermes}"
    );

    // The APPLIED REPORT reached the workspace: the dev session's row carries the six states.
    let reported = stack
        .text_witness(&format!(
            "SELECT s.harness_state::text FROM web.session_bundle_state s
             JOIN web.bundle b ON b.id = s.bundle_id
             JOIN web.cli_session cs ON cs.id = s.session_id
             JOIN web.\"user\" u ON u.id = cs.user_id
             WHERE b.name = '{BUNDLE}' AND u.email = 'dev@acme.test'"
        ))
        .expect("the dev session reported the bundle");
    let reported: Value = serde_json::from_str(&reported).expect("harness_state JSON");
    // The lane stores the harness under `slug` (the server's own spelling of the client's
    // `agent`), with the client's note kept verbatim.
    let reported_agents: Vec<&str> = reported
        .as_array()
        .expect("a per-harness list")
        .iter()
        .map(|h| h["slug"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        reported_agents.as_slice(),
        SIX.as_slice(),
        "the report carries every agent's state: {reported}"
    );
    assert!(
        reported
            .as_array()
            .expect("a per-harness list")
            .iter()
            .all(|h| h["state"] == "current"),
        "every reported state is current: {reported}"
    );

    // ── 3. removal converges: every surface returns to the person's own bytes ───────────────────
    let removed = dev
        .run(dev.root(), &["remove", BUNDLE, "-g", "--yes", "--json"])
        .data("remove -g");
    assert_eq!(
        removed["items"][0]["kind"], "manifest-excluded",
        "the off row is the one negative: {removed}"
    );
    // The receipt names EVERY surface the entry left, keyed by the config FILE it lived in
    // (`~`-abbreviated) — destinations, never agents.
    let note = removed["items"][0]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("the removal discloses what it did: {removed}"));
    assert_eq!(
        note.matches(": server entry removed").count(),
        SIX.len(),
        "one line per config file the entry left: {note}"
    );
    assert!(
        note.contains("~/.cursor/mcp.json: server entry removed"),
        "file-keyed, `~`-abbreviated: {note}"
    );
    let swept = dev
        .run(dev.root(), &["update", "--json"])
        .data("update after remove");
    assert!(
        !plugin.exists(),
        "the wholly-owned plugin dir goes with its last entry"
    );
    for (slug, path, seeded) in &seeds {
        assert_eq!(
            &read(path),
            seeded,
            "{slug}: the surface is byte-identical to what the person had ({})",
            path.display()
        );
    }
    assert_eq!(
        swept["skills"].as_array().map(Vec::len),
        Some(0),
        "nothing is delivered here any more: {swept}"
    );

    // ── 4. the off switch lifts and the feed flows again ───────────────────────────────────────
    dev.run(
        dev.root(),
        &["add", &format!("@acme/{BUNDLE}"), "-g", "--json"],
    )
    .data("add -g (the off lift)");
    let swept = dev
        .run(dev.root(), &["update", "--json"])
        .data("update after the lift");
    assert_eq!(
        state_of(&swept, BUNDLE, "cursor").as_deref(),
        Some("current"),
        "the entry is back: {swept}"
    );
    let cursor_path = home.join(".cursor").join("mcp.json");
    assert_eq!(
        read_json(&cursor_path)["mcpServers"][KEY]["url"],
        SERVER_URL
    );

    // ── 5. DRIFT: a hand-edited entry is never overwritten ─────────────────────────────────────
    let mut hand_edited = read_json(&cursor_path);
    hand_edited["mcpServers"][KEY]["url"] = json!(DRIFTED_URL);
    let drifted_bytes = format!(
        "{}\n",
        serde_json::to_string_pretty(&hand_edited).expect("render the hand edit")
    );
    std::fs::write(&cursor_path, &drifted_bytes).expect("hand-edit the cursor entry");

    let swept = dev
        .run(dev.root(), &["update", "--json"])
        .data("update over the drift");
    assert_eq!(
        state_of(&swept, BUNDLE, "cursor").as_deref(),
        Some("drifted"),
        "the sweep names the drift: {swept}"
    );
    assert_eq!(
        read_json(&cursor_path)["mcpServers"][KEY]["url"],
        DRIFTED_URL,
        "the hand edit stands"
    );
    // Every OTHER agent converged normally over the same run.
    for agent in SIX.iter().filter(|a| **a != "cursor") {
        assert_eq!(
            state_of(&swept, BUNDLE, agent).as_deref(),
            Some("current"),
            "{agent} is unaffected by another surface's drift: {swept}"
        );
    }

    // ── 6. removal LEAVES the drifted entry, and says so ───────────────────────────────────────
    dev.run(dev.root(), &["remove", BUNDLE, "-g", "--yes", "--json"])
        .data("remove -g (with drift standing)");
    let out = dev.run(dev.root(), &["update", "--json"]);
    let envelope = out.envelope("update after the drifted removal");
    assert_eq!(
        read_json(&cursor_path)["mcpServers"][KEY]["url"],
        DRIFTED_URL,
        "a removal never destroys a hand-edited entry"
    );
    let warnings: Vec<&str> = envelope["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        warnings.iter().any(|w| {
            w.starts_with("MCP_DRIFTED cursor:")
                && w.contains(&cursor_path.display().to_string())
                && w.contains("left in place")
        }),
        "the drift survivor is disclosed by file: {warnings:?}"
    );
    // The five surfaces that were NOT hand-edited are back to the person's own bytes.
    for (slug, path, seeded) in seeds.iter().filter(|(s, _, _)| *s != "cursor") {
        assert_eq!(
            &read(path),
            seeded,
            "{slug}: converged back after the drifted removal"
        );
    }
    // Put cursor back to its seed so the project phase reads a clean user surface.
    std::fs::write(&cursor_path, &seeds[1].2).expect("restore the cursor seed");

    // ── 7. PROJECT scope: the four project surfaces, and no user config ─────────────────────────
    let proj = dev.root().join("proj");
    std::fs::create_dir_all(proj.join(".git")).expect("a git checkout");
    dev.run(&proj, &["init", "--json"]).data("init");
    dev.run(&proj, &["add", &format!("@acme/{BUNDLE}"), "--json"])
        .data("add (project scope)");
    let swept = dev
        .run(&proj, &["update", "--json"])
        .data("update (project)");

    let proj_claude = read_json(&proj.join(".mcp.json"));
    assert_eq!(
        proj_claude["mcpServers"][KEY],
        json!({ "type": "http", "url": SERVER_URL }),
        "the project `.mcp.json`: {proj_claude}"
    );
    let proj_codex = read(&proj.join(".codex").join("config.toml"));
    assert_eq!(
        toml_table_body(&proj_codex, &format!("[mcp_servers.{KEY}]")),
        Some(vec![format!("url = \"{SERVER_URL}\"")]),
        "the project codex config: {proj_codex}"
    );
    assert_eq!(
        read_json(&proj.join(".cursor").join("mcp.json"))["mcpServers"][KEY],
        json!({ "url": SERVER_URL })
    );
    // OpenCode's project config is the checkout ROOT's `opencode.json`, born with its `$schema`.
    let proj_opencode = proj.join("opencode.json");
    let proj_oc_text = read(&proj_opencode);
    assert_eq!(
        read_json(&proj_opencode)["mcp"][KEY],
        json!({ "enabled": true, "type": "remote", "url": SERVER_URL })
    );
    assert!(
        proj_oc_text.starts_with("{\n  \"$schema\": \"https://opencode.ai/config.json\","),
        "a fresh opencode.json leads with its schema: {proj_oc_text}"
    );
    // …and NOTHING reached the user surfaces (they still hold exactly the person's own bytes).
    assert!(!plugin.exists(), "no user-scope plugin dir was written");
    for (slug, path, seeded) in &seeds {
        assert_eq!(
            &read(path),
            seeded,
            "{slug}: a project row never edits a user surface"
        );
    }
    // The two harnesses with no project surface withhold, and say why.
    for agent in ["openclaw", "hermes-agent"] {
        assert_eq!(
            state_of(&swept, BUNDLE, agent).as_deref(),
            Some("not-supported"),
            "{agent} has no project-level config: {swept}"
        );
    }
    for agent in ["claude-code", "codex", "cursor", "opencode"] {
        assert_eq!(
            state_of(&swept, BUNDLE, agent).as_deref(),
            Some("current"),
            "{agent} took the project surface: {swept}"
        );
    }

    // The deep dive answers the same thing offline, per agent, with the file it lives in.
    let deep = dev
        .run(&proj, &["list", BUNDLE, "--json"])
        .data("list <bundle>");
    assert_eq!(deep["detail"]["kind"], "mcp", "{deep}");
    assert_eq!(deep["detail"]["scope"], "project", "{deep}");
    assert_eq!(
        deep["detail"]["harnesses"],
        json!([
            { "agent": "claude-code", "state": "current",
              "file": canon(&proj.join(".mcp.json")) },
            { "agent": "codex", "state": "current",
              "file": canon(&proj.join(".codex").join("config.toml")) },
            { "agent": "cursor", "state": "current",
              "file": canon(&proj.join(".cursor").join("mcp.json")) },
            { "agent": "opencode", "state": "current",
              "file": canon(&proj_opencode) },
            { "agent": "openclaw", "state": "not-supported", "note": "no project-level config" },
            { "agent": "hermes-agent", "state": "not-supported", "note": "no project-level config" },
        ]),
        "the deep dive answers per agent, with the file: {deep}"
    );

    // ── 8. the two KINDS coexist: a same-named local skill dir is never touched ─────────────────
    // An agent folder that already holds a skill called `team-weather` is a different thing from
    // the workspace's MCP server of the same name: the kind rides the CATALOG, so the config
    // placement and the skill dir live side by side under the same `.claude/skills/` root.
    let local_skill = home.join(".claude").join("skills").join(BUNDLE);
    std::fs::create_dir_all(&local_skill).expect("create the same-named local skill");
    let local_bytes = "# team-weather\nSomeone's own, unmanaged notes.\n";
    std::fs::write(local_skill.join("SKILL.md"), local_bytes).expect("write the local skill");

    dev.run(
        dev.root(),
        &["add", &format!("@acme/{BUNDLE}"), "-g", "--json"],
    )
    .data("add -g (alongside a same-named local skill)");
    let swept = dev
        .run(dev.root(), &["update", "--json"])
        .data("update (alongside a same-named local skill)");
    assert_eq!(
        state_of(&swept, BUNDLE, "claude-code").as_deref(),
        Some("current"),
        "the MCP entry lands beside the same-named skill dir: {swept}"
    );
    assert_eq!(
        read(&local_skill.join("SKILL.md")),
        local_bytes,
        "the person's same-named skill dir is untouched"
    );
    assert!(
        plugin.join(".mcp.json").is_file(),
        "the plugin dir is its own sibling under the same skills root"
    );
    assert_eq!(
        read_json(&cursor_path)["mcpServers"][KEY]["url"],
        SERVER_URL,
        "…and the config entry is the MCP half"
    );
    // The workspace's registry-shape lane still publishes exactly the ONE mcp bundle — a skill
    // folder of the same name on somebody's machine is not a catalog fact.
    let registry = stack.origin_get_json(&probe.credential, "/registry/v0.1/servers");
    let registry: Value = serde_json::from_str(&registry.body).expect("registry JSON");
    assert_eq!(registry["metadata"]["count"], 1, "{registry}");
}
