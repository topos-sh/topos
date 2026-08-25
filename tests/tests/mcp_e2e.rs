//! The composed CONNECTED-SERVER loop, over real HTTP against the real web app and the real
//! `topos` binary. An MCP server is not a folder anybody authors here: it is a CATALOG ROW a
//! workspace connects to, and its document rides the wire inline. So the arc starts with an owner
//! sharing one in a single command (`add --kind mcp <registry name>`) — which connects the
//! workspace to the install's catalog row and lands the entry in that owner's own six agent
//! configs at once — and then follows what a connection is worth to everybody else.
//!
//! What it pins: the connection REACHES NOBODY until a channel is chosen, so the delivery lane is
//! empty while the CATALOG INDEX carries the server, document and revision inline, and the
//! workspace's registry-shape lane serves it under its embedded name; a second member adds it by
//! that name and the entry lands in ALL SIX MCP-capable agents, each in its own exact dialect; the
//! applied report carries the CATALOG REVISION back as the thing this machine holds; a removal
//! converges every surface back to the bytes the person had; a hand-edited entry is DRIFTED —
//! never overwritten, never removed, and disclosed; a project-scoped row reaches the four project
//! surfaces alone; a curator puts the server in a channel and only then does the feed deliver it;
//! `publish` is refused, because a bundle of this kind has no files; and — the property only a
//! catalog kind has — a machine whose workspace is GONE still heals a deleted config file from the
//! document it was last given.
//!
//! A second arc follows the COMMITTED LOCK: the catalog moves on, and a fresh checkout still
//! receives the revision its `topos.lock` names — fetched by id over the same real HTTP — with
//! `topos update` the one thing that moves both halves.
//!
//! Like the conflict suite, this one drives the REAL CLI BINARY (`target/<profile>/topos`) as a
//! subprocess rather than the in-process fixture rig: `add --kind mcp`, the per-scope config
//! converge and the harness detection all resolve against `$HOME` / `$TOPOS_HOME`, so only a real
//! process with a fake home proves them. The two halves share one installation — the fixture rig
//! owns the browser login (the `/verify` approval), the binary owns every verb after it, both
//! over the same `~/.topos`.

mod common;

use std::path::{Path, PathBuf};

use common::{CliOut, OWNER_EMAIL, Session, Stack, cli_binary, run_cli, start_stack};
use serde_json::{Value, json};
use topos::test_support::SessionInstall;

// ── the shared scenario constants ───────────────────────────────────────────────────────────────

/// The server this suite shares: an install-catalog row every deployment is seeded with, so the
/// share needs no outbound network — the workspace already holds the document. Its shape is the
/// simplest one there is (one plain `streamable-http` remote, no headers, no credential), which
/// is what makes the six dialects comparable byte for byte.
const REGISTRY_NAME: &str = "com.amazon.aws/knowledge";
/// The name the workspace shares it as — the tail of the registry name. The catalog row, the
/// word every machine `topos add`s it by, and the key-mint half.
const BUNDLE: &str = "knowledge";
/// The one remote endpoint every dialect must carry, byte-identically.
const SERVER_URL: &str = "https://knowledge-mcp.global.api.aws";
/// The minted config key for the workspace bundle: `topos-<workspace slug>-<name>`.
const KEY: &str = "topos-acme-knowledge";
/// A url a hand-edit puts in place of [`SERVER_URL`] (the drift phase).
const DRIFTED_URL: &str = "https://hand-edited.example/mcp";

/// The six MCP-capable harnesses THIS end-to-end drives — one per editing driver and per entry
/// shape, in the harness table's row order (the order every surface reports them in — there is one
/// table). The table itself carries sixteen; the rest are covered by the converge round-trips in
/// the client's own suite, and this one proves the whole chain against the real binary.
const SIX: [&str; 6] = [
    "claude-code",
    "openclaw",
    "codex",
    "cursor",
    "hermes-agent",
    "opencode",
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

    /// The person's own pick FOR THAT CHECKOUT — personal and git-ignored, so a clone never
    /// carries one and a checkout without one places nothing.
    fn pick_in_project(&self, project_dir: &Path) {
        self.rig.pick_in_project(project_dir);
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

/// **Claude Code's own configuration** — one file holding two entries objects: top-level
/// `mcpServers`, which every project reads, and `projects.<a checkout's absolute path>.mcpServers`,
/// which only a session started in that directory reads. The suite unsets `$CLAUDE_CONFIG_DIR`, so
/// it sits at the default, beside the config dir.
fn claude_config(home: &Path) -> PathBuf {
    home.join(".claude.json")
}

/// The servers Claude Code sees in ONE checkout — its own slot in the file above.
fn claude_project_servers(home: &Path, project: &Path) -> Value {
    read_json(&claude_config(home))["projects"][canon(project)]["mcpServers"].clone()
}

/// The person's OWN content in each PATCHED surface (Claude Code's own configuration is asserted
/// separately: topos creates it here, so what a removal must do to it is DELETE it), written
/// before topos ever touches them — the
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

/// Mark every driven harness's trigger REGISTERED in the shared `~/.topos`, the state a real
/// machine reaches at login: the sweep offers a trigger once per agent ever, and this document is
/// that record. Written by arrangement because the fixture rig's ops-level login never runs the
/// composition root's breadth registration — without it, the first verb would settle the
/// question itself and write trigger artifacts into configs these tests pin byte-exactly.
fn settle_trigger_registration(install: &Install) {
    let agents: serde_json::Map<String, Value> = SIX
        .iter()
        .map(|slug| {
            (
                (*slug).to_owned(),
                json!({ "at_ms": 1, "registered": true, "retry_at_ms": 0 }),
            )
        })
        .collect();
    let state_dir = install.root().join(".topos").join("state");
    std::fs::create_dir_all(&state_dir).expect("create the state dir");
    std::fs::write(
        state_dir.join("trigger_registration.json"),
        serde_json::to_vec(&json!({ "schema_version": 1, "agents": agents }))
            .expect("serialize the registration record"),
    )
    .expect("write the registration record");
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

/// One row of a lane answer's `mcp_servers` list — the SECOND list a connected server rides. A
/// miss panics with the whole answer, because the list a bundle lands in is the assertion.
fn mcp_row<'a>(answer: &'a Value, name: &str) -> &'a Value {
    answer["mcp_servers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("`{name}` rides the connected-server list: {answer}"))
}

// ── the six dialects, asserted ──────────────────────────────────────────────────────────────────

/// The WHOLE per-dialect truth over one installation's fake home: the entry topos owns, spelled
/// the way each agent's own config parser demands, and the person's own content still standing
/// beside it. Asserted for every machine this suite gives the server to, because a dialect that
/// is right on one machine and wrong on the next is the failure this matrix exists to catch.
fn assert_the_six_dialects(home: &Path) {
    // Claude Code — its own configuration file, the slot every project reads, under the server's
    // own name. The FILE is the agent's whole configuration and holds whatever else it holds; the
    // entry is what topos owns, and its shape is what a wrong key would brick.
    assert_eq!(
        read_json(&claude_config(home))["mcpServers"][KEY],
        json!({ "type": "http", "url": SERVER_URL }),
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
    let openclaw_path = home.join(".openclaw").join("openclaw.json");
    let openclaw = read_jsonc(&openclaw_path);
    assert_eq!(
        openclaw["mcp"]["servers"][KEY],
        json!({ "transport": "streamable-http", "url": SERVER_URL }),
        "openclaw: {openclaw}"
    );
    assert!(
        read(&openclaw_path).contains("// the house server"),
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
}

// ── the arrangement halves ──────────────────────────────────────────────────────────────────────

/// Log an installation in through the REAL browser-approval ceremony (the fixture rig owns the
/// flow; the binary inherits the session it lands).
fn login(stack: &Stack, install: &Install, approver: &Session) {
    stack.login_begin_and_approve(&install.rig, approver);
    let granted = install.rig.login(None).expect("the login resume");
    assert_eq!(granted.session_status, "active");
}

/// The catalog revision THIS workspace's connection resolves to — a pin, else the server's own
/// current. The one expression the delivery lane and the machine's record both answer with, read
/// straight off the rows so the wire is compared against the catalog rather than against itself.
fn resolved_revision(stack: &Stack) -> String {
    stack
        .text_witness(&format!(
            "SELECT r.id FROM web.bundle b
             JOIN web.bundle_mcp bm ON bm.bundle_id = b.id
             JOIN web.mcp_server ms ON ms.id = bm.server_id
             JOIN web.mcp_server_revision r
               ON r.id = COALESCE(bm.pinned_revision_id, ms.current_revision_id)
             WHERE b.name = '{BUNDLE}'"
        ))
        .expect("the connection resolves to a revision")
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// The matrix — one composed stack, one arc.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn the_connected_server_loop_across_six_agents() {
    let stack = start_stack("mcp");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── 1. sharing: one command shares the server AND lands it on this machine ──────────────────
    let author = Install::new("mcp-owner");
    login(&stack, &author, &owner);
    settle_trigger_registration(&author);
    seed_user_configs(&author.home());
    let root = author.root().canonicalize().expect("canonical owner root");

    let added = author
        .run(
            &root,
            &["add", "--kind", "mcp", REGISTRY_NAME, "-g", "--json"],
        )
        .data("add --kind mcp");
    assert_eq!(
        added["name"], BUNDLE,
        "the workspace names the bundle off the registry name's tail: {added}"
    );
    assert_eq!(
        added["mcp"]["server"], REGISTRY_NAME,
        "the receipt states the server that was shared: {added}"
    );
    assert_eq!(added["mcp"]["url"], SERVER_URL, "{added}");
    assert_eq!(added["mcp"]["transport"], "streamable-http", "{added}");
    assert_eq!(
        added["mcp"]["auth"], "none",
        "the catalog's VERIFIED sign-in word rides the receipt: {added}"
    );
    assert_eq!(
        added["mcp"]["agents"].as_array().map(Vec::len),
        Some(SIX.len()),
        "the share reached every MCP-capable agent set up here: {added}"
    );

    // The workspace now holds a bundle whose bytes are a CATALOG ROW: the kind on the catalog
    // row, and the connection beside it naming the install's own global server.
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT kind FROM web.bundle WHERE name = '{BUNDLE}'"
        )),
        Some("mcp".to_owned()),
        "the catalog records what the bundle IS"
    );
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT ms.name || ' ' || coalesce(ms.workspace_id, 'global')
             FROM web.bundle b
             JOIN web.bundle_mcp bm ON bm.bundle_id = b.id
             JOIN web.mcp_server ms ON ms.id = bm.server_id
             WHERE b.name = '{BUNDLE}'"
        )),
        Some(format!("{REGISTRY_NAME} global")),
        "the connection names the INSTALL's catalog row — a name the catalog already offers is \
         connected, never copied into the workspace"
    );

    // …and the owner's own six agents already carry it, under the WORKSPACE key. There was no
    // second command: sharing a server and getting it are one act on the machine that shares it.
    assert_the_six_dialects(&author.home());

    // ── 2. the wire: a connection reaches nobody, and the catalog carries it anyway ─────────────
    let probe = stack.mint_session(&owner, "catalog-probe");
    let revision = resolved_revision(&stack);
    assert!(
        revision.starts_with("mcpr_"),
        "a connected server's version handle is the catalog's own: {revision}"
    );

    // The DELIVERY lane is the entitlement question, and nothing entitles anybody yet: the share
    // lane connects with NO channel, so the server reaches no one until a curator picks one.
    let delivery = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
    );
    assert_eq!(delivery.status, 200, "the delivery lane: {}", delivery.body);
    let delivery: Value = serde_json::from_str(&delivery.body).expect("delivery JSON");
    assert_eq!(
        delivery["mcp_servers"].as_array().map(Vec::len),
        Some(0),
        "a connection reaches nobody until a channel is chosen: {delivery}"
    );

    // The CATALOG INDEX is the other question — what this workspace holds — and it carries the
    // server in its OWN list, document inline. That is the lane a machine resolves a name
    // through, and the reason a bundle with no bytes needs no byte lane behind it.
    let index = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/skills", stack.workspace_id),
    );
    assert_eq!(index.status, 200, "the catalog index: {}", index.body);
    let index: Value = serde_json::from_str(&index.body).expect("catalog index JSON");
    assert!(
        index["skills"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|s| s["name"] != BUNDLE),
        "a connected server is not a file bundle, and never rides the file list: {index}"
    );
    let listed = mcp_row(&index, BUNDLE);
    assert_eq!(listed["kind"], "mcp", "{listed}");
    assert_eq!(
        listed["revision_id"], revision,
        "the index serves the revision the connection resolves to: {listed}"
    );
    assert_eq!(
        listed["document"]["name"], REGISTRY_NAME,
        "the document rides inline, verbatim: {listed}"
    );
    assert_eq!(listed["document"]["remotes"][0]["url"], SERVER_URL);

    // The workspace's registry-shape read API serves the same server under its EMBEDDED registry
    // name — the name the document itself carries, not the word the workspace shares it by.
    let registry = stack.origin_get_json(&probe.credential, "/registry/v0.1/servers");
    assert_eq!(registry.status, 200, "the registry lane: {}", registry.body);
    let registry: Value = serde_json::from_str(&registry.body).expect("registry JSON");
    assert_eq!(registry["metadata"]["count"], 1, "{registry}");
    assert_eq!(registry["servers"][0]["server"]["name"], REGISTRY_NAME);
    assert_eq!(
        registry["servers"][0]["server"]["remotes"][0]["url"],
        SERVER_URL
    );

    // ── 3. a second member adds it BY NAME: the entry lands in ALL SIX config surfaces ──────────
    let dev_browser = stack.add_member("dev@acme.test", "member");
    let dev = Install::new("mcp-dev");
    login(&stack, &dev, &dev_browser);
    let home = dev.home();
    // The fixture rig's login is ops-level and never runs the composition root's breadth
    // registration, so on this install the trigger question is still open — and the first real
    // verb would settle it by writing trigger artifacts into the very configs the seeds below
    // pin byte-exactly. A real machine settles registration at login; this rig settles it by
    // arrangement, so the assertions stay about MCP entry custody alone.
    settle_trigger_registration(&dev);
    let seeds = seed_user_configs(&home);

    // No feed carries the server, so the name is resolved against the workspace CATALOG — and a
    // bare word is all a person needs, because the catalog already records what the bundle is.
    let taken = dev
        .run(dev.root(), &["add", BUNDLE, "-g", "--json"])
        .data("add <name> -g");
    assert_eq!(taken["name"], BUNDLE, "{taken}");
    assert_eq!(
        taken["mcp"]["server"], REGISTRY_NAME,
        "the member's receipt names the same server the owner shared: {taken}"
    );
    assert_the_six_dialects(&home);

    // The add converged the entries inline, so this sweep finds every one of them in order.
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
        assert_eq!(state, "current", "{agent} holds the entry: {swept}");
    }

    // ── 4. the applied report: what a machine holds for this kind is a CATALOG REVISION ─────────
    let reported = stack
        .text_witness(&format!(
            "SELECT s.applied_version_id FROM web.session_bundle_state s
             JOIN web.bundle b ON b.id = s.bundle_id
             JOIN web.cli_session cs ON cs.id = s.session_id
             JOIN web.\"user\" u ON u.id = cs.user_id
             WHERE b.name = '{BUNDLE}' AND u.email = 'dev@acme.test'"
        ))
        .expect("the dev session reported the bundle");
    assert_eq!(
        reported, revision,
        "ONE field carries both vocabularies — a file bundle's commit, and a connected server's \
         catalog revision"
    );
    let harness_state = stack
        .text_witness(&format!(
            "SELECT s.harness_state::text FROM web.session_bundle_state s
             JOIN web.bundle b ON b.id = s.bundle_id
             JOIN web.cli_session cs ON cs.id = s.session_id
             JOIN web.\"user\" u ON u.id = cs.user_id
             WHERE b.name = '{BUNDLE}' AND u.email = 'dev@acme.test'"
        ))
        .expect("the report carries the per-harness half");
    let harness_state: Value = serde_json::from_str(&harness_state).expect("harness_state JSON");
    // The lane stores the harness under `slug` (the server's own spelling of the client's
    // `agent`), with the client's note kept verbatim.
    let reported_agents: Vec<&str> = harness_state
        .as_array()
        .expect("a per-harness list")
        .iter()
        .map(|h| h["slug"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        reported_agents.as_slice(),
        SIX.as_slice(),
        "the report carries every agent's state: {harness_state}"
    );
    assert!(
        harness_state
            .as_array()
            .expect("a per-harness list")
            .iter()
            .all(|h| h["state"] == "current"),
        "the fleet's STANDING picture — where the entries live, not what the last sweep did to \
         them: {harness_state}"
    );

    // ── 5. removal converges: every surface returns to the person's own bytes ───────────────────
    let removed = dev
        .run(dev.root(), &["remove", BUNDLE, "-g", "--yes", "--json"])
        .data("remove -g");
    assert_eq!(
        removed["items"][0]["kind"], "manifest-removed",
        "the machine's own line goes, and delivery to this scope ends with it: {removed}"
    );
    // The receipt names EVERY surface the entry left, keyed by the config FILE it lived in
    // (`~`-abbreviated) — destinations, never agents.
    let note = removed["items"][0]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("the removal discloses what it did: {removed}"));
    assert_eq!(
        note.matches(": the server's entry was removed.").count(),
        SIX.len(),
        "one line per config file the entry left: {note}"
    );
    assert!(
        note.contains("~/.cursor/mcp.json: the server's entry was removed."),
        "file-keyed, `~`-abbreviated: {note}"
    );
    let swept = dev
        .run(dev.root(), &["update", "--json"])
        .data("update after remove");
    assert!(
        !claude_config(&home).exists(),
        "a file topos created and still wholly owned goes with its last entry"
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

    // ── 6. the row goes back, and so does the entry ─────────────────────────────────────────────
    dev.run(
        dev.root(),
        &["add", &format!("@acme/{BUNDLE}"), "-g", "--json"],
    )
    .data("add -g (the workspace reference)");
    let swept = dev
        .run(dev.root(), &["update", "--json"])
        .data("update after the re-add");
    assert_eq!(
        state_of(&swept, BUNDLE, "cursor").as_deref(),
        Some("current"),
        "the entry is back — the `add` converged it inline, so this sweep found it in order: \
         {swept}"
    );
    let cursor_path = home.join(".cursor").join("mcp.json");
    assert_eq!(
        read_json(&cursor_path)["mcpServers"][KEY]["url"],
        SERVER_URL
    );

    // ── 7. DRIFT: a hand-edited entry is never overwritten ──────────────────────────────────────
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

    // ── 8. removal LEAVES the drifted entry, and says so ────────────────────────────────────────
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
            w.starts_with("MCP_DRIFTED ")
                && w.contains(&cursor_path.display().to_string())
                && w.contains("left in place for cursor")
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

    // ── 9. PROJECT scope: the four project surfaces, and no user config ─────────────────────────
    let proj = dev.root().join("proj");
    std::fs::create_dir_all(proj.join(".git")).expect("a git checkout");
    dev.pick_in_project(&proj);
    dev.run(&proj, &["init", "--json"]).data("init");
    dev.run(&proj, &["add", &format!("@acme/{BUNDLE}"), "--json"])
        .data("add (project scope)");
    let swept = dev
        .run(&proj, &["update", "--json"])
        .data("update (project)");

    let proj_claude = claude_project_servers(&home, &proj);
    assert_eq!(
        proj_claude[KEY],
        json!({ "type": "http", "url": SERVER_URL }),
        "this checkout's own slot in Claude Code's configuration: {proj_claude}"
    );
    assert!(
        !proj.join(".mcp.json").exists(),
        "and not a byte in the repository"
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
    // OpenCode's project config sits under its own folder, born with its `$schema`.
    let proj_opencode = proj.join(".opencode").join("opencode.json");
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
    assert!(
        read_json(&claude_config(&home)).get("mcpServers").is_none(),
        "a project row never writes the slot every project reads"
    );
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
            "{agent} took the project surface (the `add` placed it; this sweep found it in \
             order): {swept}"
        );
    }

    // The deep dive answers the same thing offline, per agent, with the file it lives in — for
    // the six PICKED agents alone (an agent outside the pick earns neither a file nor a line).
    let deep = dev
        .run(&proj, &["list", BUNDLE, "--json"])
        .data("list <bundle>");
    assert_eq!(deep["detail"]["kind"], "mcp", "{deep}");
    assert_eq!(deep["detail"]["scope"], "project", "{deep}");
    assert_eq!(
        deep["detail"]["harnesses"],
        json!([
            { "agent": "claude-code", "state": "current",
              "file": canon(&claude_config(&home)) },
            { "agent": "openclaw", "state": "not-supported", "note": "no project-level config" },
            { "agent": "codex", "state": "current",
              "file": canon(&proj.join(".codex").join("config.toml")) },
            { "agent": "cursor", "state": "current",
              "file": canon(&proj.join(".cursor").join("mcp.json")) },
            { "agent": "hermes-agent", "state": "not-supported", "note": "no project-level config" },
            { "agent": "opencode", "state": "current",
              "file": canon(&proj_opencode) },
        ]),
        "the deep dive answers per agent, with the file: {deep}"
    );

    // ── 10. the two KINDS coexist: a same-named local skill dir is never touched ────────────────
    // An agent folder that already holds a skill called `knowledge` is a different thing from the
    // workspace's MCP server of the same name: the kind rides the CATALOG, so the config
    // placement and the skill dir live side by side under the same `.claude/skills/` root.
    let local_skill = home.join(".claude").join("skills").join(BUNDLE);
    std::fs::create_dir_all(&local_skill).expect("create the same-named local skill");
    let local_bytes = "# knowledge\nSomeone's own, unmanaged notes.\n";
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
    assert_eq!(
        read_json(&claude_config(&home))["mcpServers"][KEY]["url"],
        SERVER_URL,
        "the config entry is a file beside the skills root, never inside it"
    );
    assert_eq!(
        read_json(&cursor_path)["mcpServers"][KEY]["url"],
        SERVER_URL,
        "…and the config entry is the MCP half"
    );
    // The workspace's registry-shape lane still publishes exactly the ONE connected server — a
    // skill folder of the same name on somebody's machine is not a catalog fact.
    let registry = stack.origin_get_json(&probe.credential, "/registry/v0.1/servers");
    let registry: Value = serde_json::from_str(&registry.body).expect("registry JSON");
    assert_eq!(registry["metadata"]["count"], 1, "{registry}");

    // ── 11. a curator gives the connection REACH, and the feed carries it ───────────────────────
    // The member drops their own row first, so what arrives next arrives for one reason only: a
    // curator put the server in a channel. Until this act the connection reached nobody — that is
    // the whole ruling the share lane rests on.
    dev.run(dev.root(), &["remove", BUNDLE, "-g", "--yes", "--json"])
        .data("remove -g (before the curation)");
    let everyone = stack
        .text_witness("SELECT id FROM web.channel WHERE name = 'everyone'")
        .expect("every workspace is born with its default channel");
    let bundle_id = taken["skill_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the add receipt names the bundle: {taken}"))
        .to_owned();
    let curated = owner.post_form(
        "/channels/everyone",
        &[
            ("intent", "add-skill"),
            ("channel_id", &everyone),
            ("skill_id", &bundle_id),
        ],
    );
    assert_eq!(curated.status, 200, "the curation lands: {}", curated.body);

    // The DELIVERY lane answers differently now — the connected server rides its own list, with
    // the catalog revision and the document inline, and says which channel earned it.
    let delivery = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
    );
    assert_eq!(delivery.status, 200, "the delivery lane: {}", delivery.body);
    let delivery: Value = serde_json::from_str(&delivery.body).expect("delivery JSON");
    assert!(
        delivery["skills"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|s| s["name"] != BUNDLE),
        "a connected server never rides the file list, delivered or not: {delivery}"
    );
    let served = mcp_row(&delivery, BUNDLE);
    assert_eq!(served["kind"], "mcp", "{served}");
    assert_eq!(served["revision_id"], revision, "{served}");
    assert_eq!(served["document"]["name"], REGISTRY_NAME, "{served}");
    assert_eq!(
        served["document"]["remotes"][0]["url"], SERVER_URL,
        "{served}"
    );
    assert_eq!(
        served["via"]["channels"],
        json!(["everyone"]),
        "the feed says WHY this device is entitled: {served}"
    );

    // …and the member's next sweep places it from the feed alone — no row of their own.
    let swept = dev
        .run(dev.root(), &["update", "--json"])
        .data("update (delivered by the channel)");
    for agent in SIX {
        assert_eq!(
            state_of(&swept, BUNDLE, agent).as_deref(),
            Some("created"),
            "{agent}: a FIRST placement, from the feed: {swept}"
        );
    }
    assert_the_six_dialects(&home);

    // ── 12. there are no files, so there is nothing to publish ──────────────────────────────────
    // REFUSAL-FIRST, and refused HERE: `publish` ships a bundle's placed files, the kind marker on
    // this machine already says there are none, and nothing is sent. The workspace would refuse it
    // too — but a round trip to be told what the record in front of the verb already says is one
    // trip too many, and a candidate built from a work tree that does not exist would fail as
    // "your own state is unreadable" long before it could ask.
    let refused = author
        .run(&root, &["publish", BUNDLE, "--yes", "--json"])
        .refusal("publish over a connected server");
    assert_eq!(
        refused["error"]["code"], "INVALID_ARGUMENT",
        "the verb refuses by the bundle's KIND: {refused}"
    );
    let said = refused["error"]["context"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        said.contains("is an MCP server bundle") && said.contains("`publish` ships"),
        "the refusal names what publish acts on and what this bundle is: {said}"
    );
    assert_eq!(
        resolved_revision(&stack),
        revision,
        "a refused publish moves no version pointer"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.mcp_server_revision r
             JOIN web.mcp_server ms ON ms.id = r.server_id
             JOIN web.bundle_mcp bm ON bm.server_id = ms.id
             JOIN web.bundle b ON b.id = bm.bundle_id
             WHERE b.name = '{BUNDLE}'"
        )),
        1,
        "…and mints no revision either"
    );

    // ── 13. OFFLINE HEALING: the machine's own record is the demand, online and off ─────────────
    // The whole workspace goes away — the app dies with the stack. A file bundle would have
    // nothing to fetch from; a connected server was DELIVERED WHOLE, and the document sits in
    // this installation's own record beside the revision it came from. Deleting an agent's whole
    // config file is exactly the accident that record exists for.
    let recorded = dev
        .root()
        .join(".topos")
        .join("skills")
        .join(&bundle_id)
        .join("server.json");
    let recorded: Value =
        serde_json::from_str(&read(&recorded)).expect("the recorded server document");
    assert_eq!(
        recorded["revision_id"], revision,
        "the record keeps the revision it was given: {recorded}"
    );
    assert_eq!(
        recorded["document"]["remotes"][0]["url"], SERVER_URL,
        "…and the document itself, verbatim: {recorded}"
    );

    drop(stack);
    std::fs::remove_file(&cursor_path).expect("delete an agent's whole config file");
    let healed = dev.run(dev.root(), &["update", "--json"]);
    assert_eq!(
        read_json(&cursor_path)["mcpServers"][KEY],
        json!({ "url": SERVER_URL }),
        "the entry is back from the machine's own record, with no workspace left to ask: {} {}",
        healed.stdout,
        healed.stderr
    );
    for (slug, path, _) in seeds.iter().filter(|(s, _, _)| *s != "cursor") {
        assert!(
            read(path).contains(SERVER_URL),
            "{slug}: an offline sweep leaves every standing entry alone ({})",
            path.display()
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// The LOCK arc — what a committed `topos.lock` is worth for a connection, over the same stack.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// The second address the catalog moves to mid-arc — the whole point of the assertions below is
/// which of the two a checkout receives.
const SERVER_URL_V2: &str = "https://knowledge-mcp.v2.api.aws.example";

/// Publish a SECOND revision of the shared catalog server and promote it — the catalog moving on
/// while a project's lock stays where it was. Written as rows because the promotion door is
/// staff's, and this suite is about what a MACHINE then receives.
fn promote_second_revision(stack: &Stack) -> String {
    let server_id = stack
        .text_witness(&format!(
            "SELECT id FROM web.mcp_server WHERE name = '{REGISTRY_NAME}' AND workspace_id IS NULL"
        ))
        .expect("the shared catalog server");
    let document = json!({
        "name": REGISTRY_NAME,
        "description": "AWS knowledge, at a second address.",
        "remotes": [{ "type": "streamable-http", "url": SERVER_URL_V2 }],
        "_meta": { "sh.topos/auth": "none" },
    });
    let revision = stack
        .text_witness(&format!(
            "INSERT INTO web.mcp_server_revision
               (id, server_id, seq, document, transport, url, published_at, published_by)
             VALUES ('mcpr_' || md5('knowledge-v2'), '{server_id}',
                     (SELECT max(seq) + 1 FROM web.mcp_server_revision WHERE server_id = '{server_id}'),
                     '{document}'::jsonb, 'streamable-http', '{SERVER_URL_V2}', now(), 'e2e-harness')
             RETURNING id"
        ))
        .expect("the second revision lands");
    stack
        .text_witness(&format!(
            "UPDATE web.mcp_server SET current_revision_id = '{revision}'
             WHERE id = '{server_id}' RETURNING id"
        ))
        .expect("the pointer moves");
    revision
}

/// A checkout: a git-shaped dir holding the two committed files, exactly as a clone would, plus
/// the person's own pick for it (personal and git-ignored, so a clone never carries one; a
/// checkout with no pick places nothing).
fn checkout(install: &Install, at: &Path, manifest: &str, lock: &str) -> PathBuf {
    std::fs::create_dir_all(at.join(".git")).expect("a git-shaped checkout");
    std::fs::write(at.join("topos.toml"), manifest).expect("the committed manifest");
    std::fs::write(at.join("topos.lock"), lock).expect("the committed lock");
    install.pick_in_project(at);
    at.to_path_buf()
}

/// **A COMMITTED LOCK NAMES THE REVISION A CHECKOUT RECEIVES.** The catalog moves on; the project
/// does not — until somebody runs `topos update` and commits the diff. The whole chain is real
/// here: the binary asks the web app for ONE STORED REVISION by id, and what lands in the agent's
/// config is the document that revision holds, not the one the catalog serves today.
#[test]
fn a_committed_lock_installs_the_mcp_revision_it_names() {
    let stack = start_stack("mcp-lock");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── the author: share the server from inside a project, and commit what it resolved to ──────
    let author = Install::new("mcp-lock-author");
    login(&stack, &author, &owner);
    settle_trigger_registration(&author);
    let proj = author
        .root()
        .canonicalize()
        .expect("canonical root")
        .join("api");
    std::fs::create_dir_all(proj.join(".git")).expect("a git-shaped project");
    author.pick_in_project(&proj);
    author.run(&proj, &["init", "--json"]).data("init");
    let added = author
        .run(&proj, &["add", "--kind", "mcp", REGISTRY_NAME, "--json"])
        .data("add --kind mcp");
    assert_eq!(added["name"], BUNDLE, "{added}");
    author.run(&proj, &["install", "--json"]).data("install");

    let rev1 = resolved_revision(&stack);
    let manifest = read(&proj.join("topos.toml"));
    let lock = read(&proj.join("topos.lock"));
    assert!(
        lock.contains(&format!("[mcp.{BUNDLE}]")) && lock.contains(&rev1),
        "the lock records the revision the connection resolved to:\n{lock}"
    );

    // ── the catalog moves on ────────────────────────────────────────────────────────────────────
    let rev2 = promote_second_revision(&stack);
    assert_ne!(rev1, rev2);
    assert_eq!(resolved_revision(&stack), rev2, "the connection follows it");

    // ── the teammate: a FRESH checkout of the committed pair, installed frozen ───────────────────
    let dev = Install::new("mcp-lock-dev");
    login(&stack, &dev, &owner);
    settle_trigger_registration(&dev);
    let clone = checkout(
        &dev,
        &dev.root()
            .canonicalize()
            .expect("canonical root")
            .join("api"),
        &manifest,
        &lock,
    );
    let frozen = dev.run(&clone, &["install", "--frozen", "--json"]);
    assert_eq!(
        frozen.code, 0,
        "the locked revision is served, so CI installs it: {} {}",
        frozen.stdout, frozen.stderr
    );
    let placed = claude_project_servers(&dev.root().join("home"), &clone).to_string();
    assert!(
        placed.contains(SERVER_URL) && !placed.contains(SERVER_URL_V2),
        "the checkout receives the document its lock names, not today's:\n{placed}"
    );
    assert_eq!(
        read(&clone.join("topos.lock")),
        lock,
        "an install never moves a lock entry"
    );

    // ── `topos update` is what moves it — both halves, together ─────────────────────────────────
    dev.run(&clone, &["update", "--json"]).data("update");
    let placed = claude_project_servers(&dev.root().join("home"), &clone).to_string();
    assert!(
        placed.contains(SERVER_URL_V2) && !placed.contains(SERVER_URL),
        "update takes the served revision:\n{placed}"
    );
    let moved = read(&clone.join("topos.lock"));
    assert!(
        moved.contains(&rev2) && !moved.contains(&rev1),
        "…and rewrites the lock to it:\n{moved}"
    );
}
