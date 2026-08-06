//! `add --mcp` and the publish half of the `kind = "mcp"` bundle: the two doors a server comes in
//! through, the row each one records, and the gate that stands between a server document and the
//! workspace.
//!
//! What is under test here:
//!
//! - the FETCHED door applies immediately — the canonical document, the row, the converge — with
//!   an undo-led receipt whose `remove` is the verifiable FULL inverse (the folder the import
//!   wrote leaves with the row when its bytes still match; anything else is kept, disclosed);
//! - the LOCAL door applies immediately, and its row is `add`/`remove`'s exact file inverse even
//!   carrying `{ kind = "mcp" }` — an adopted folder is never deleted;
//! - a registry-shaped name resolves WORKSPACE-FIRST: a connected catalog EMBEDDING it
//!   subscribes to that bundle by its catalog name (source disclosed, registry never dialed),
//!   several embedding it refuse toward `--workspace`, and a miss falls through to the
//!   registry — whose 404 then names both consulted sources;
//! - the registry name goes to the versions/latest endpoint as ONE encoded path segment, and its
//!   `{server, _meta}` envelope is unwrapped before a byte is stored;
//! - a document carrying a credential is REFUSED at every door, with the shared typed code, before
//!   anything is written — including at `publish`, where the refusal must land before the op WAL;
//! - the bundle kind rides the WAL onto the wire, and the landed publish's governance rewrite
//!   drops the local `kind` field cleanly (the catalog is the authority from then on).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::ManifestEntry;
use topos_core::digest::{self, FileMode};
use topos_core::identity::Commit;
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};

use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::mcp_validate::McpRefusalCode;
use crate::ops::{self, McpDocSource};
use crate::plane::{InertFollow, InertPlane};
use crate::sessions::{self, SESSION_ACTIVE, Session};
use crate::sidecar::Layout;
// The directory + governance fakes are the manifest suite's — one pair of stand-ins for the whole
// test tree, so a trait's growth is felt in one place.
use super::manifest_reconcile::{FakeDirectory, NoGovernance};

const HOST: &str = "acme.test";
const WS: &str = "w_eng";
const WS_NAME: &str = "eng";

// =================================================================================================
// The rig
// =================================================================================================

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-mcpa-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical, so recorded and derived paths compare equal on macOS's symlinked $TMPDIR.
        let dir = dir.canonicalize().unwrap_or(dir);
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A no-op adapter: discovers nothing, touches no config.
#[derive(Debug)]
struct NoHarness;
impl HarnessAdapter for NoHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(
        &self,
        skill_id: &str,
        _n: topos_harness::PlacementNaming<'_>,
        _: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: PathBuf::from(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        report()
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        report()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
fn report() -> TriggerReport {
    TriggerReport {
        harness: HarnessId::ClaudeCode,
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "test:none".to_owned(),
        state: TriggerState::Inactive,
    }
}

struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NoHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: NoHarness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0.join(".topos"))
    }
    fn ctx_at<'a>(&'a self, cwd: Option<&Path>) -> Ctx<'a> {
        Ctx {
            progress: crate::progress::silent(),
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane: &InertPlane,
            follow: &InertFollow,
            roots: Some(crate::ctx::AgentRoots {
                home: self.home.0.clone(),
                cwd: cwd.map(Path::to_path_buf),
            }),
        }
    }
    fn manifest(&self) -> PathBuf {
        self.layout().home().join(crate::manifest::MANIFEST_FILE)
    }
    fn write_global(&self, body: &str) {
        let home = self.layout().home().to_path_buf();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(self.manifest(), body).unwrap();
    }
    fn global_text(&self) -> String {
        std::fs::read_to_string(self.manifest()).unwrap_or_default()
    }
    fn seed_session(&self) {
        sessions::upsert_session(
            &self.fs,
            &self.layout(),
            Session {
                host: HOST.into(),
                base_url: format!("https://{HOST}/api"),
                workspace_id: WS.into(),
                workspace_name: WS_NAME.into(),
                display_name: "Engineering".into(),
                session_id: "sn_1".into(),
                credential: "cred-1".into(),
                status: SESSION_ACTIVE.into(),
                logged_in_at: 1,
            },
        )
        .unwrap();
    }
    /// A SECOND live workspace on the same server — the ambiguity tests' other team.
    fn seed_session_in(&self, ws_id: &str, ws_name: &str) {
        sessions::upsert_session(
            &self.fs,
            &self.layout(),
            Session {
                host: HOST.into(),
                base_url: format!("https://{HOST}/api"),
                workspace_id: ws_id.into(),
                workspace_name: ws_name.into(),
                display_name: ws_name.into(),
                session_id: format!("sn_{ws_id}"),
                credential: format!("cred-{ws_id}"),
                status: SESSION_ACTIVE.into(),
                logged_in_at: 1,
            },
        )
        .unwrap();
    }
}

/// A document source that answers ONE canned body and records every URL it was asked for — no test
/// in this suite reaches the real registry.
#[derive(Clone)]
struct FakeDocs {
    body: Vec<u8>,
    asked: Arc<Mutex<Vec<String>>>,
}
impl FakeDocs {
    fn serving(body: &str) -> Self {
        Self {
            body: body.as_bytes().to_vec(),
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn urls(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}
impl McpDocSource for FakeDocs {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ClientError> {
        self.asked.lock().unwrap().push(url.to_owned());
        Ok(self.body.clone())
    }
}

/// A document source that answers the registry's own 404 for every ask — the miss shape the
/// both-sources-consulted message decorates.
struct Docs404;
impl McpDocSource for Docs404 {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ClientError> {
        Err(ClientError::RemoteFetch {
            msg: format!("{url} — no server document there (HTTP 404)"),
            fault: crate::error::FetchFault::Gone,
        })
    }
}

/// The session-connect the arms that never probe a workspace ride: an inert transport set (the
/// workspace-first tests build their own, serving a catalog).
fn empty_connect(_s: &Session) -> ops::SessionTransports {
    ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    }
}

/// A clean, shareable server document — one https streamable-http remote, one literal header.
fn good_server() -> String {
    r#"{"name":"io.github.acme/weather","description":"Conditions for a named place.",
        "version":"1.4.0","remotes":[{"type":"streamable-http",
        "url":"https://weather.acme.example/mcp","headers":[{"name":"X-Region","value":"eu-west-1"}]}]}"#
        .to_owned()
}

/// The same document with somebody's GitHub token smuggled into a header value.
fn server_with_secret() -> String {
    format!(
        r#"{{"name":"io.github.acme/weather","description":"Conditions for a named place.",
        "version":"1.4.0","remotes":[{{"type":"streamable-http",
        "url":"https://weather.acme.example/mcp","headers":[{{"name":"X-Token","value":"ghp_{}"}}]}}]}}"#,
        "A1b2C3d4E5".repeat(4)
    )
}

/// Write a local MCP bundle folder (the dir IS the bundle).
fn write_bundle(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("server.json"), body).unwrap();
}

// =================================================================================================
// The FETCHED door — applies immediately, undo-led
// =================================================================================================

/// A registry name APPLIES immediately: the canonical document, the row, the converge — and the
/// receipt LEADS with the undo (`topos remove -g <dir>`), beside the typed `mcp` block that
/// carries the server facts the old describe held.
#[test]
fn a_fetched_server_applies_immediately_with_an_undo_led_receipt() {
    let rig = Rig::new("applied");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    let bundle = rig.layout().home().join("mcp").join("weather");
    assert!(bundle.join("server.json").exists(), "the document landed");
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            bundle.display().to_string(),
        ],
        "the receipt leads with the full inverse"
    );
    let mcp = data
        .mcp
        .expect("the receipt carries the typed server block");
    assert_eq!(mcp.server, "io.github.acme/weather");
    assert_eq!(mcp.version, "1.4.0");
    assert_eq!(mcp.url, "https://weather.acme.example/mcp");
    assert_eq!(mcp.transport, "streamable-http");
    assert_eq!(mcp.headers, vec!["X-Region".to_owned()]);
    // The document declared no auth word, so the receipt claims none.
    assert_eq!(mcp.auth, None);
    assert!(
        mcp.bundle
            .as_deref()
            .is_some_and(|b| b.ends_with("weather")),
        "{:?}",
        mcp.bundle
    );
}

/// ITEM PAIR (add honors the narrowing): with `[defaults.mcp] harness = ["cursor"]` standing and
/// TWO MCP-capable agents set up, `add --mcp` places into cursor ALONE — the same narrowing the
/// sweep resolves, so the next sweep has nothing to claw back — and the receipt's breadth line
/// lists only the narrowed agent. Before the fix the inline converge carried an EMPTY filter,
/// which the engine reads as ALL harnesses.
#[test]
fn the_add_converge_honors_the_defaults_narrowing() {
    let rig = Rig::new("narrow");
    rig.write_global("[bundles]\n\n[defaults.mcp]\nharness = [\"cursor\"]\n");
    // Both hermetic agents are set up in the fake home; the narrowing admits one.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".openclaw")).unwrap();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let openclaw = rig.home.0.join(".openclaw/openclaw.json");
    assert!(
        cursor.exists(),
        "the narrowed harness gets the entry: {data:?}"
    );
    let placed = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        placed.contains("https://weather.acme.example/mcp"),
        "{placed}"
    );
    assert!(
        !openclaw.exists(),
        "a narrowing-excluded harness must not gain an entry on add"
    );
    // The breadth line is the narrowed one — never every engaged agent.
    let mcp = data.mcp.expect("the typed block");
    assert_eq!(mcp.agents, vec!["Cursor".to_owned()], "{:?}", mcp.agents);
    let note = data.note.clone().unwrap_or_default();
    assert!(!note.contains("openclaw"), "{note}");
}

/// ITEM PAIR (the add-time receipt's honesty): a fresh placement's line carries the harness's
/// reload note — how the change goes LIVE ("restart Cursor") — exactly as the update path's
/// receipt does; and when NO MCP-capable agent is set up, the receipt says the row waits for one
/// instead of closing with "each agent's own MCP config (named above)" over nothing named.
#[test]
fn the_add_receipt_carries_the_reload_note_and_says_when_nobody_is_reached() {
    // Arm 1: an agent is set up — its placement line names the reload step.
    let rig = Rig::new("reload-note");
    rig.write_global("[bundles]\n");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    let note = data.note.clone().unwrap_or_default();
    assert!(
        note.contains("~/.cursor/mcp.json: server entry — restart Cursor"),
        "the sub-line keys by the config file, `~`-abbreviated: {note}"
    );

    // Arm 2: NO agent is set up — the receipt (typed note AND the TTY closing line) says the row
    // waits, never "named above" with nothing named.
    let rig = Rig::new("nobody");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    assert!(
        data.mcp.as_ref().is_some_and(|m| m.agents.is_empty()),
        "{:?}",
        data.mcp
    );
    let note = data.note.clone().unwrap_or_default();
    assert!(
        note.contains("No MCP-capable agent is set up here yet"),
        "{note}"
    );
    let tty = crate::render::add_tty(&data);
    assert!(
        tty.contains("no MCP-capable agent is set up here yet"),
        "{tty}"
    );
    assert!(!tty.contains("named above"), "{tty}");
}

/// What lands: the canonical document (pretty-printed, trailing newline) plus ONE row whose
/// value is the inline table that records what the folder IS.
#[test]
fn the_fetched_document_lands_canonical_with_the_kind_row() {
    let rig = Rig::new("apply");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    assert_eq!(data.name, "weather");

    let bundle = rig.layout().home().join("mcp").join("weather");
    let stored = std::fs::read_to_string(bundle.join("server.json")).unwrap();
    assert!(stored.ends_with("}\n"), "trailing newline: {stored:?}");
    assert!(
        stored.contains("\n  \"name\""),
        "pretty-printed: {stored:?}"
    );
    // The stored bytes are a document the gate accepts — what is placed is what was disclosed.
    crate::mcp_validate::validate_server_json(stored.as_bytes()).expect("the stored bytes pass");

    let text = rig.global_text();
    assert!(
        text.contains(&format!("\"{}\" = {{ kind = \"mcp\" }}", bundle.display())),
        "the row records the kind: {text}"
    );
    // The receipt says what it points at, in the words a person reads.
    let note = data.note.clone().unwrap_or_default();
    assert!(note.contains("io.github.acme/weather"), "{note}");
    assert!(note.contains("https://weather.acme.example/mcp"), "{note}");
}

/// A registry NAME reads the versions/latest endpoint with its slash encoded as ONE segment, and
/// the `{server, _meta}` envelope never reaches the bundle.
#[test]
fn a_registry_name_reads_versions_latest_and_unwraps_the_envelope() {
    let rig = Rig::new("registry");
    rig.write_global("[bundles]\n");
    let enveloped = format!(
        r#"{{"server":{},"_meta":{{"io.registry":{{"x":1}}}}}}"#,
        good_server()
    );
    let docs = FakeDocs::serving(&enveloped);
    let ctx = rig.ctx_at(Some(&rig.work.0));

    ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    assert_eq!(
        docs.urls(),
        vec![
            "https://registry.modelcontextprotocol.io/v0.1/servers/io.github.acme%2Fweather/versions/latest"
                .to_owned()
        ]
    );
    let stored = std::fs::read_to_string(
        rig.layout()
            .home()
            .join("mcp")
            .join("weather")
            .join("server.json"),
    )
    .unwrap();
    assert!(
        !stored.contains("_meta"),
        "the envelope is not the document: {stored}"
    );
    assert!(stored.contains("io.github.acme/weather"), "{stored}");
}

/// An https URL is fetched verbatim — no registry path is invented around it.
#[test]
fn an_https_url_is_fetched_as_given() {
    let rig = Rig::new("url");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let url = "https://weather.acme.example/.well-known/server.json";

    ops::add_mcp(&ctx, &empty_connect, Some(&docs), url, true, None).unwrap();
    assert_eq!(docs.urls(), vec![url.to_owned()]);
}

/// A fetched document carrying a credential is refused with the SHARED typed code — and refusal is
/// the whole outcome: no bundle folder, no row, not even under `--yes`.
#[test]
fn a_fetched_secret_is_refused_before_anything_is_written() {
    let rig = Rig::new("secret-fetch");
    rig.write_global("[bundles]\n");
    let before = rig.global_text();
    let docs = FakeDocs::serving(&server_with_secret());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert!(matches!(
        err,
        ClientError::McpRefused {
            code: McpRefusalCode::SecretRefused,
            ..
        }
    ));
    // The refusal never echoes the credential back.
    assert!(!err.detail().contains("ghp_"), "{}", err.detail());
    assert_eq!(rig.global_text(), before);
    assert!(!rig.layout().home().join("mcp").exists());
}

/// A folder already standing where the bundle would go is never written through — the refusal
/// names it, so nothing is discovered as an overwrite afterwards.
#[test]
fn an_occupied_destination_refuses_by_name() {
    let rig = Rig::new("occupied");
    rig.write_global("[bundles]\n");
    let taken = rig.layout().home().join("mcp").join("weather");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("mine.txt"), b"keep me").unwrap();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("refused");
    assert!(
        err.detail().contains(&taken.display().to_string()),
        "{}",
        err.detail()
    );
    assert_eq!(std::fs::read(taken.join("mine.txt")).unwrap(), b"keep me");
}

/// The canonical bytes the fetched door stores for a document — pretty-printed, trailing newline
/// (what `unwrap_server_document` writes).
fn canonical(body: &str) -> Vec<u8> {
    let doc: serde_json::Value = serde_json::from_str(body).unwrap();
    format!("{}\n", serde_json::to_string_pretty(&doc).unwrap()).into_bytes()
}

/// ITEM PAIR (resumable import): an interrupted import's own leftovers — the dir landed, the row
/// did not — RESUME on retry: the destination is unregistered and its `server.json` bytes equal
/// the intended document, so the retry writes the row, converges, and reports. Before the fix the
/// retry refused forever on "already exists".
#[test]
fn an_interrupted_import_resumes_when_the_leftover_matches() {
    let rig = Rig::new("resume");
    rig.write_global("[bundles]\n");
    // The crash window's exact state: the canonical document on disk, no manifest row.
    let leftover = rig.layout().home().join("mcp").join("weather");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("server.json"), canonical(&good_server())).unwrap();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect("the retry resumes");
    assert_eq!(data.name, "weather");
    let text = rig.global_text();
    assert!(
        text.contains(&format!(
            "\"{}\" = {{ kind = \"mcp\" }}",
            leftover.display()
        )),
        "the row landed this time: {text}"
    );
}

/// The resume window is EXACTLY the leftover shape: different bytes are somebody's material and a
/// registered row is a live bundle — both still refuse by name.
#[test]
fn a_resume_refuses_foreign_bytes_and_a_registered_row() {
    let rig = Rig::new("resume-guard");
    rig.write_global("[bundles]\n");
    let dir = rig.layout().home().join("mcp").join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        b"{\"name\":\"io.github.other/x\"}\n",
    )
    .unwrap();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Foreign bytes: refused, untouched.
    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("refused");
    assert!(err.detail().contains("already exists"), "{}", err.detail());

    // A registered row: the full import lands once, then a second run refuses the same way.
    std::fs::write(dir.join("server.json"), canonical(&good_server())).unwrap();
    ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect("resumes");
    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("refused");
    assert!(err.detail().contains("already exists"), "{}", err.detail());
}

/// ITEM PAIR (resume runs the whole-dir gate, arm 1): a leftover whose `server.json` matches the
/// intended document but which carries a STRAY FILE beside it is not an interrupted import's own
/// state — it is somebody's folder wearing our document. The resume refuses through the same
/// candidate gate the adopt door runs, naming the file. Before the fix the resume checked
/// `server.json` alone and registered the foreign bytes.
#[test]
fn a_resume_with_a_stray_sibling_refuses_naming_the_file() {
    let rig = Rig::new("resume-stray");
    rig.write_global("[bundles]\n");
    let leftover = rig.layout().home().join("mcp").join("weather");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("server.json"), canonical(&good_server())).unwrap();
    std::fs::write(leftover.join("evil.sh"), b"#!/bin/sh\necho pwned\n").unwrap();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("refused");
    assert_eq!(err.code(), "MCP_INVALID");
    assert!(err.detail().contains("evil.sh"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "no row was registered");
    assert!(
        std::fs::read(leftover.join("evil.sh")).is_ok(),
        "the refusal touches nothing"
    );
}

/// ITEM PAIR (resume runs the whole-dir gate, arm 2): an ALLOWED sibling still runs the per-file
/// credential scan — a README carrying a token refuses `MCP_SECRET_REFUSED` naming the file,
/// exactly as the adopt door would. Before the fix the resume registered the credential-bearing
/// folder.
#[test]
fn a_resume_with_a_credential_readme_refuses() {
    let rig = Rig::new("resume-readme");
    rig.write_global("[bundles]\n");
    let leftover = rig.layout().home().join("mcp").join("weather");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("server.json"), canonical(&good_server())).unwrap();
    std::fs::write(
        leftover.join("README.md"),
        format!("Set GITHUB_TOKEN to ghp_{}.\n", "A1b2C3d4E5".repeat(4)),
    )
    .unwrap();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert!(err.detail().contains("README.md"), "{}", err.detail());
    assert!(!err.detail().contains("ghp_"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "no row was registered");
}

// =================================================================================================
// The WORKSPACE-FIRST resolution of a registry-shaped name
// =================================================================================================

/// One workspace-catalog entry an ACTIVE mcp bundle contributes, embedding `server` — what the
/// session lane's additive `mcp_server_name` field carries.
fn mcp_entry(name: &str, server: &str) -> topos_types::requests::WireSkillIndexEntry {
    topos_types::requests::WireSkillIndexEntry {
        skill_id: format!("s_{name}"),
        name: name.to_owned(),
        kind: "mcp".to_owned(),
        status: "active".to_owned(),
        version_id: "a".repeat(64),
        bundle_digest: "b".repeat(64),
        generation: 1,
        display_name: None,
        updated_at: 0,
        open_proposals: 0,
        upstream_host: None,
        upstream_repo: None,
        upstream_path: None,
        mcp_server_name: Some(server.to_owned()),
    }
}

/// A session-connect serving ONE catalog per workspace id (every other lane inert).
fn catalog_connect(
    per_ws: Vec<(String, Vec<topos_types::requests::WireSkillIndexEntry>)>,
) -> impl Fn(&Session) -> ops::SessionTransports {
    move |s: &Session| {
        let skills = per_ws
            .iter()
            .find(|(ws, _)| ws == &s.workspace_id)
            .map(|(_, entries)| entries.clone())
            .unwrap_or_default();
        ops::SessionTransports {
            plane: Box::new(NoDelivery),
            directory: Box::new(FakeDirectory::new(skills, Vec::new())),
            contribute: Box::new(RecordingPublish::default()),
            governance: Box::new(NoGovernance),
        }
    }
}

/// ITEM (workspace-first): a registry-shaped name a connected workspace's catalog EMBEDS
/// resolves to THAT bundle — subscribed by its catalog name through the ordinary reference arm,
/// the source disclosed at the head of the note — and the official registry is never dialed
/// after the hit.
#[test]
fn a_workspace_hit_wins_over_the_registry_and_discloses_its_source() {
    let rig = Rig::new("ws-first");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let connect = catalog_connect(vec![(
        WS.to_owned(),
        vec![mcp_entry("weather", "io.github.acme/weather")],
    )]);

    let data = ops::add_mcp(
        &ctx,
        &connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    assert!(
        docs.urls().is_empty(),
        "a workspace hit must never dial the registry: {:?}",
        docs.urls()
    );
    // The subscribe is the ordinary workspace-reference act: the canonical row, kind-less (the
    // catalog is the authority), with the catalog identity on the receipt.
    assert_eq!(data.name, "weather");
    assert_eq!(
        data.reference.as_deref(),
        Some(&*format!("{HOST}/{WS_NAME}/weather"))
    );
    assert_eq!(data.skill_id.as_deref(), Some("s_weather"));
    let text = rig.global_text();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/weather\" = \"*\"")),
        "{text}"
    );
    assert!(
        !text.contains("kind"),
        "a workspace row carries no kind: {text}"
    );
    let note = data.note.clone().unwrap_or_default();
    assert!(
        note.starts_with("from eng's catalog as 'weather'"),
        "the receipt leads with the source: {note}"
    );
}

/// ITEM (ambiguity refuses): SEVERAL connected workspaces embedding the same server name refuse
/// with the typed shape — naming every workspace — and the envelope's ways out are the
/// `--workspace`-narrowed re-runs of the user's own invocation. The registry is not consulted.
#[test]
fn several_workspaces_embedding_the_name_refuse_toward_workspace() {
    let rig = Rig::new("ws-ambig");
    rig.seed_session();
    rig.seed_session_in("w_ops", "ops");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let connect = catalog_connect(vec![
        (
            WS.to_owned(),
            vec![mcp_entry("weather", "io.github.acme/weather")],
        ),
        (
            "w_ops".to_owned(),
            vec![mcp_entry("wx", "io.github.acme/weather")],
        ),
    ]);

    let err = ops::add_mcp(
        &ctx,
        &connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("refused");
    assert!(docs.urls().is_empty(), "{:?}", docs.urls());
    assert_eq!(err.code(), "AMBIGUOUS_WORKSPACE");
    let ClientError::AmbiguousMcpWorkspace { ref workspaces, .. } = err else {
        panic!("the typed ambiguity shape: {err:?}");
    };
    assert_eq!(workspaces, &["eng".to_owned(), "ops".to_owned()]);
    assert!(err.detail().contains("--workspace"), "{}", err.detail());
    // The envelope's ways out: one `--workspace`-narrowed re-run per workspace, `-g` preserved.
    let envelope = crate::render::err_envelope(
        "add",
        &[
            "add".into(),
            "--mcp".into(),
            "io.github.acme/weather".into(),
            "-g".into(),
        ],
        &err,
    );
    let argvs: Vec<Vec<String>> = envelope
        .next_actions
        .iter()
        .map(|a| a.argv.clone())
        .collect();
    assert_eq!(
        argvs,
        vec![
            vec![
                "topos".to_owned(),
                "add".to_owned(),
                "--mcp".to_owned(),
                "io.github.acme/weather".to_owned(),
                "--workspace".to_owned(),
                "eng".to_owned(),
                "-g".to_owned(),
                "--json".to_owned(),
            ],
            vec![
                "topos".to_owned(),
                "add".to_owned(),
                "--mcp".to_owned(),
                "io.github.acme/weather".to_owned(),
                "--workspace".to_owned(),
                "ops".to_owned(),
                "-g".to_owned(),
                "--json".to_owned(),
            ],
        ]
    );
}

/// ITEM (`--workspace` narrows): the global selector picks WHICH workspace the probe means —
/// two teams embedding the name resolve cleanly to the selected one.
#[test]
fn the_workspace_selector_narrows_the_probe_to_one_team() {
    let rig = Rig::new("ws-narrow");
    rig.seed_session();
    rig.seed_session_in("w_ops", "ops");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let connect = catalog_connect(vec![
        (
            WS.to_owned(),
            vec![mcp_entry("weather", "io.github.acme/weather")],
        ),
        (
            "w_ops".to_owned(),
            vec![mcp_entry("wx", "io.github.acme/weather")],
        ),
    ]);

    let data = ops::add_mcp(
        &ctx,
        &connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        Some("w_ops"),
    )
    .unwrap();
    assert!(docs.urls().is_empty(), "{:?}", docs.urls());
    assert_eq!(data.name, "wx");
    assert_eq!(data.reference.as_deref(), Some(&*format!("{HOST}/ops/wx")));
}

/// ITEM (miss falls through): no connected workspace embeds the name — the registry answers
/// exactly as before, and a catalog that holds the name under a DIFFERENT embedded identity
/// (or as a `kind = "skill"` bundle) never matches.
#[test]
fn a_workspace_miss_falls_through_to_the_registry() {
    let rig = Rig::new("ws-miss");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // A different embedded name, and a SKILL wearing the exact token as its embedded field (a
    // shape a well-behaved server never emits) — neither participates.
    let mut skill_wearing_it = mcp_entry("weather-skill", "io.github.acme/weather");
    skill_wearing_it.kind = "skill".to_owned();
    let connect = catalog_connect(vec![(
        WS.to_owned(),
        vec![mcp_entry("wx", "io.github.acme/other"), skill_wearing_it],
    )]);

    ops::add_mcp(
        &ctx,
        &connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect("the registry arm lands it");
    assert_eq!(
        docs.urls(),
        vec![
            "https://registry.modelcontextprotocol.io/v0.1/servers/io.github.acme%2Fweather/versions/latest"
                .to_owned()
        ]
    );
    let text = rig.global_text();
    assert!(
        text.contains("kind = \"mcp\""),
        "the fetched row landed: {text}"
    );
}

/// ITEM (the miss message names both sources): the registry's 404 now says the workspace
/// catalogs were consulted too — one honest sentence — while every other fetch fault passes
/// through untouched.
#[test]
fn a_miss_everywhere_says_both_sources_were_consulted() {
    let rig = Rig::new("ws-404");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let connect = catalog_connect(vec![(WS.to_owned(), Vec::new())]);

    let err = ops::add_mcp(
        &ctx,
        &connect,
        Some(&Docs404),
        "io.github.acme/weather",
        true,
        None,
    )
    .expect_err("a miss everywhere refuses");
    assert_eq!(err.code(), "REMOTE_FETCH");
    let detail = err.detail();
    assert!(
        detail.contains("no server document there (HTTP 404)"),
        "{detail}"
    );
    assert!(
        detail.contains(
            "and no workspace you are connected to publishes a server named \
             io.github.acme/weather"
        ),
        "{detail}"
    );
}

// =================================================================================================
// The LOCAL door
// =================================================================================================

/// A folder on this machine is the person's own material: it applies immediately, adopts exactly
/// as a plain `add <path>` does, and the row it records carries the kind.
#[test]
fn a_local_folder_applies_immediately_with_a_kind_row() {
    let rig = Rig::new("local");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();
    // It went through the ordinary adopt: a real record with a real version identity.
    assert!(data.skill_id.is_some(), "the folder was adopted");
    assert!(data.version_id.is_some(), "the adopt minted a version");

    let text = rig.global_text();
    assert!(
        text.contains(&format!("\"{}\" = {{ kind = \"mcp\" }}", dir.display())),
        "{text}"
    );
}

/// `add --mcp` and `remove` stay EXACT FILE INVERSES even when the row carries a kind: the whole
/// row is dropped, and the file returns to the bytes it had.
#[test]
fn add_mcp_and_remove_are_exact_file_inverses() {
    let rig = Rig::new("inverse");
    rig.write_global("[bundles]\n# a comment that must survive\n\"topos.sh/eng/deploy\" = \"*\"\n");
    let before = rig.global_text();
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();
    assert_ne!(rig.global_text(), before, "the add changed the file");

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &[dir.display().to_string()],
        None,
        true,
    )
    .unwrap();
    assert!(
        matches!(outcome, ops::RemoveOutcome::Applied(_)),
        "the removal applies"
    );
    assert_eq!(
        rig.global_text(),
        before,
        "remove is add's exact inverse, kind and all"
    );
    // An ADOPTED folder is the person's own material — no import record, never deleted.
    assert!(
        dir.join("server.json").exists(),
        "remove never deletes an adopted folder"
    );
}

/// A `~/`-spelled row resolves like every other local-path key: `remove` of an adopted mcp
/// bundle whose row the person re-spelled home-rooted still finds the tracked identity the adopt
/// recorded, and the inline converge takes its config entries out with the row — they do not
/// linger undisclosed for the next sweep.
#[test]
fn a_tilde_spelled_adopted_row_converges_its_config_entries_out_on_remove() {
    let rig = Rig::new("tilde");
    rig.write_global("[bundles]\n");
    // One MCP-capable agent set up in the fake home, so the add's converge places an entry.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let dir = rig.home.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("weather.acme.example"),
        "the add placed the entry"
    );

    // The person re-spells the row by hand, `~/`-rooted.
    let respelled = rig
        .global_text()
        .replace(&dir.display().to_string(), "~/weather");
    assert!(respelled.contains("\"~/weather\""), "{respelled}");
    std::fs::write(rig.manifest(), &respelled).unwrap();

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &["~/weather".to_owned()],
        None,
        true,
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("the re-spelled row still removes");
    };
    // The inline converge answered on the receipt AND the entry actually left.
    let note = removed.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("server entry removed"),
        "the inline converge must reach a ~/ row's entries: {note}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .map(|t| !t.contains("weather.acme.example"))
            .unwrap_or(true),
        "the config entry left with the row"
    );
}

/// ITEM PAIR (the folder half of the fetched inverse): `remove` of a fetched import deletes the
/// bundle folder the import itself wrote — the row, the config entries, AND the folder go, so the
/// undo the add receipt led with restores the whole prior state. Verified, not assumed: the
/// folder still holds exactly the imported bytes.
#[test]
fn remove_deletes_the_folder_a_fetched_import_wrote() {
    let rig = Rig::new("undo-full");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    let bundle = rig.layout().home().join("mcp").join("weather");
    assert!(bundle.join("server.json").exists());

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    // The exact command the add receipt led with (minus the binary name + flag order).
    assert_eq!(data.undo[..3], ["topos", "remove", "-g"]);
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &[bundle.display().to_string()],
        None,
        false,
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("a clean fetched import removes without a gate");
    };
    assert!(!bundle.exists(), "the folder the import wrote is gone");
    let note = removed.items[0].note.clone().unwrap_or_default();
    assert!(note.contains("was removed with the row"), "{note}");
    assert_eq!(rig.global_text(), "[bundles]\n", "the row is gone");
}

/// A fetched import's receipt names WHAT LANDED with an identity a machine consumer can check:
/// the kernel digest of the bytes written, recomputable from the folder itself. The two fields a
/// pointer row cannot honestly fill — the sidecar id and the version — are ABSENT rather than
/// zeroed, so nobody reads a history that was never minted.
#[test]
fn a_fetched_import_reports_the_digest_of_what_it_wrote() {
    let rig = Rig::new("fetched-identity");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();

    assert!(
        data.skill_id.is_none() && data.version_id.is_none(),
        "a pointer row mints no record and no version: {data:?}"
    );
    let bundle = rig.layout().home().join("mcp").join("weather");
    let recomputed = topos_core::digest::to_hex(&crate::scan::scan(&bundle).unwrap().bundle_digest);
    assert_eq!(
        data.bundle_digest.as_deref(),
        Some(recomputed.as_str()),
        "the receipt's digest is the one the written folder hashes to"
    );
    assert!(
        !recomputed.bytes().all(|b| b == b'0'),
        "a real digest, not the all-zero sentinel"
    );
}

/// ITEM PAIR (the undo leaves NOTHING behind): an import into a home with no `topos.toml` at all
/// births that file and the sidecar's `mcp/` folder — and its `remove` takes both back, because a
/// birth this act caused is a birth its inverse owns. Everything the import touched is gone;
/// nothing it merely found is.
#[test]
fn removing_a_fetched_import_leaves_no_empty_husks() {
    let rig = Rig::new("undo-husks");
    // Deliberately NOT seeded: the add itself brings the manifest into existence.
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    let mcp_dir = rig.layout().home().join("mcp");
    assert!(mcp_dir.join("weather").join("server.json").exists());
    assert!(rig.manifest().exists(), "the add birthed the manifest");

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &[mcp_dir.join("weather").display().to_string()],
        None,
        false,
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("a clean fetched import removes without a gate");
    };
    assert!(!mcp_dir.exists(), "the emptied mcp/ folder is pruned too");
    assert!(
        !rig.manifest().exists(),
        "the manifest this import created goes back with it"
    );
    let note = removed.items[0].note.clone().unwrap_or_default();
    assert!(note.contains("this import created it"), "{note}");
}

/// A manifest the import merely FOUND is never taken away — not even when the removal empties it.
/// The birth record is what licenses the delete, and this file has no such record.
#[test]
fn a_manifest_the_import_found_survives_the_removal() {
    let rig = Rig::new("undo-found-manifest");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    ops::remove_global(
        &ctx,
        &session_connect,
        &[rig
            .layout()
            .home()
            .join("mcp")
            .join("weather")
            .display()
            .to_string()],
        None,
        false,
    )
    .unwrap();
    assert_eq!(rig.global_text(), "[bundles]\n", "the file stands, emptied");
}

/// ITEM PAIR (no undo beats a wrong one): a fetched import's folder whose bytes have moved since
/// the import — an edited document, a sibling file — is KEPT on remove, and the receipt says so.
#[test]
fn remove_keeps_an_edited_import_folder_and_says_so() {
    let rig = Rig::new("undo-kept");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        Some(&docs),
        "io.github.acme/weather",
        true,
        None,
    )
    .unwrap();
    let bundle = rig.layout().home().join("mcp").join("weather");
    // The person's own edit after the import: the folder is no longer provably ours.
    std::fs::write(bundle.join("NOTES.md"), b"my own notes\n").unwrap();

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &[bundle.display().to_string()],
        None,
        false,
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("the row still removes");
    };
    assert!(
        bundle.join("NOTES.md").exists() && bundle.join("server.json").exists(),
        "changed bytes are never deleted"
    );
    let note = removed.items[0].note.clone().unwrap_or_default();
    assert!(note.contains("kept"), "{note}");
}

/// ITEM PAIR (forgetting `--mcp`): a plain `topos add ./folder` — and publish's auto-add — on a
/// folder whose root holds `server.json` and no `SKILL.md` refuses with the `--mcp` hint instead
/// of silently adopting a SKILL that delivers raw JSON into skills dirs. The `--mcp` door itself
/// still adopts the same folder, and a folder carrying a SKILL.md stays an ordinary skill adopt.
#[test]
fn a_server_bundle_at_a_skill_door_refuses_toward_the_mcp_flag() {
    let rig = Rig::new("miskind");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The plain add door.
    let err = ops::add(&ctx, &dir).expect_err("refused");
    assert_eq!(err.code(), "MCP_FLAG_REQUIRED");
    assert!(err.detail().contains("--mcp"), "{}", err.detail());
    assert!(err.detail().contains("SKILL.md"), "{}", err.detail());
    assert!(
        !rig.layout().skills_dir().exists(),
        "nothing was adopted: {}",
        err.detail()
    );

    // The publish door's auto-add pre-step refuses the same way, before anything lands.
    let err =
        ops::ensure_tracked(&ctx, None, dir.to_str().unwrap()).expect_err("publish refuses too");
    assert_eq!(err.code(), "MCP_FLAG_REQUIRED");

    // The `--mcp` door still adopts that exact folder.
    ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .expect("the flagged door adopts");

    // A SKILL.md beside a server.json reads as a skill — the guard never fires on it.
    let skill = rig.work.0.join("notes");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), b"# notes\n").unwrap();
    std::fs::write(skill.join("server.json"), b"{}\n").unwrap();
    ops::add(&ctx, &skill).expect("a SKILL.md dir adopts as a skill");
}

/// A folder with no root `server.json` is not an MCP bundle — the refusal says exactly that
/// instead of adopting a skill under a flag that promised a server.
#[test]
fn a_folder_without_a_server_json_refuses() {
    let rig = Rig::new("noserver");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# not a server\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .expect_err("refused");
    assert!(err.detail().contains("server.json"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");
}

/// ITEM PAIR (sibling files, the adopt door): an MCP candidate is EXACTLY `server.json` +
/// `README.md` — a stray script beside the document refuses (naming the allowed set) before
/// anything is adopted, and an allowed README's bytes still run the credential scan. Before the
/// fix the adopt gate read `server.json` alone and let both through.
#[test]
fn a_stray_sibling_refuses_at_the_adopt_gate() {
    let rig = Rig::new("siblings");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    std::fs::write(dir.join("evil.sh"), b"#!/bin/sh\necho pwned\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .expect_err("refused");
    assert_eq!(err.code(), "MCP_INVALID");
    assert!(
        err.detail().contains("server.json, README.md"),
        "{}",
        err.detail()
    );
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");

    // A `topos-mcp.toml` is a stray like any other.
    std::fs::remove_file(dir.join("evil.sh")).unwrap();
    std::fs::write(dir.join("topos-mcp.toml"), b"# config\n").unwrap();
    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .expect_err("refused");
    assert_eq!(err.code(), "MCP_INVALID");
    assert!(err.detail().contains("topos-mcp.toml"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");

    // The allowed pair adopts — and the store gains the durable kind marker beside its docs.
    std::fs::remove_file(dir.join("topos-mcp.toml")).unwrap();
    std::fs::write(dir.join("README.md"), b"How to use this server.\n").unwrap();
    let data = ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();
    let sid = crate::id::SkillId::parse(data.skill_id.as_deref().unwrap()).unwrap();
    let marker = std::fs::read_to_string(rig.layout().published(&sid).kind).expect("kind.json");
    assert!(marker.contains("\"mcp\""), "{marker}");
}

/// The sibling scan is a CREDENTIAL gate too: a token in the README refuses exactly like one in
/// the document.
#[test]
fn an_allowed_readme_with_a_credential_refuses_at_the_adopt_gate() {
    let rig = Rig::new("readme-secret");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    std::fs::write(
        dir.join("README.md"),
        format!("Set GITHUB_TOKEN to ghp_{}.\n", "A1b2C3d4E5".repeat(4)),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert!(err.detail().contains("README.md"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");
}

/// A local document carrying a credential is refused at the same gate, before it is adopted.
#[test]
fn a_local_secret_is_refused_before_the_adopt() {
    let rig = Rig::new("secret-local");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &server_with_secret());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert_eq!(rig.global_text(), "[bundles]\n");
    assert!(
        !rig.layout().skills_dir().exists(),
        "no record was minted for a refused document"
    );
}

/// `--mcp` never re-labels a governed reference: a workspace bundle already carries its kind, so
/// the refusal points at the plain `add`.
#[test]
fn a_workspace_reference_refuses_toward_the_plain_add() {
    let rig = Rig::new("wsref");
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err =
        ops::add_mcp(&ctx, &empty_connect, None, "@eng/weather", true, None).expect_err("refused");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(
        err.detail().contains("topos add @eng/weather"),
        "{}",
        err.detail()
    );
}

// =================================================================================================
// The PUBLISH half — the gate, the WAL, and the wire
// =================================================================================================

/// A `ContributeSource` that records every publish body it is handed and answers OK.
#[derive(Clone, Default)]
struct RecordingPublish {
    seen: Arc<Mutex<Vec<topos_types::requests::PublishRequest>>>,
}
impl crate::plane::ContributeSource for RecordingPublish {
    fn publish(
        &self,
        b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        use base64::Engine as _;
        self.seen.lock().unwrap().push(b.clone());
        let entries: Vec<ManifestEntry> = b
            .candidate
            .files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.clone(),
                mode: match f.mode {
                    topos_types::requests::WireFileMode::Regular => FileMode::Regular,
                    topos_types::requests::WireFileMode::Executable => FileMode::Executable,
                },
                content_sha256: digest::sha256(
                    &base64::engine::general_purpose::STANDARD
                        .decode(&f.content_base64)
                        .unwrap(),
                ),
            })
            .collect();
        let tree = digest::bundle_digest(&entries).unwrap();
        let id = topos_core::identity::commit_id(&Commit {
            parents: &[],
            tree,
            author: &b.candidate.author,
            message: &b.candidate.message,
        })
        .unwrap();
        Ok(crate::plane::WriteReceipt {
            receipt: Some(topos_types::Receipt {
                schema_version: 1,
                op_id: b.op_id.clone(),
                command: "publish".to_owned(),
                outcome: topos_types::TerminalOutcome::Ok,
                workspace_id: b.workspace_id.clone(),
                skill_id: Some(b.skill_id.clone()),
                version_id: Some(topos_core::digest::to_hex(&id)),
                bundle_digest: Some(topos_core::digest::to_hex(&tree)),
                expected_generation: Some(b.expected),
                current_generation: Some(1),
                created_at: "2026-08-03T00:00:00.000Z".to_owned(),
                details: None,
            }),
            error: None,
            wire_record: Some(topos_types::WireCurrentRecord {
                schema_version: topos_types::WIRE_SCHEMA_VERSION,
                scope: topos_types::PointerScope {
                    workspace_id: b.workspace_id,
                    skill_id: b.skill_id,
                },
                record: topos_types::CurrentRecord {
                    version_id: topos_core::digest::to_hex(&id),
                    generation: 1,
                },
            }),
        })
    }
    fn propose(
        &self,
        _b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("this suite publishes directly")
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("this suite publishes directly")
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("this suite publishes directly")
    }
}

/// The session lane's READ transport: the inert plane plus an empty delivery, which is all these
/// publish flows ever ask of it.
#[derive(Debug, Clone, Copy)]
struct NoDelivery;
impl crate::plane::PlaneSource for NoDelivery {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<crate::plane::KnownCurrent>,
    ) -> Result<crate::plane::PointerFetch, crate::plane::PlaneError> {
        Err(crate::plane::PlaneError::NotFound)
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, crate::plane::PlaneError> {
        Err(crate::plane::PlaneError::NotFound)
    }
}
impl crate::plane::DeliverySource for NoDelivery {
    fn fetch_delivery(
        &self,
        _workspace_id: &str,
    ) -> Result<crate::plane::DeliverySnapshot, crate::plane::PlaneError> {
        Err(crate::plane::PlaneError::NotFound)
    }
    fn report_applied(
        &self,
        _workspace_id: &str,
        _applied: &[crate::plane::AppliedSkillReport],
    ) -> Result<(), crate::plane::PlaneError> {
        Ok(())
    }
}

/// Publish `name` through the session lane, capturing the wire bodies.
fn publish_through(
    ctx: &Ctx<'_>,
    recorder: &RecordingPublish,
    name: &str,
) -> Result<ops::PublishOutcome, ClientError> {
    let rec = recorder.clone();
    let session_connect = move |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(rec.clone()),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(RecordingPublish::default())
    };
    ops::publish(
        ctx,
        &cc,
        None,
        Some(&session_connect),
        None,
        name,
        false,
        None,
        None,
        None,
    )
}

/// The gate stands BEFORE the op WAL: a `kind = "mcp"` bundle whose document carries a credential
/// is refused, no op file is written, and nothing reaches the transport.
#[test]
fn a_secret_bearing_mcp_bundle_never_reaches_the_wal() {
    let rig = Rig::new("pub-secret");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    // Adopt it CLEAN, so the record and the row exist; then the draft gains the credential — the
    // author's own edit, which is exactly the moment the gate has to fire.
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();
    std::fs::write(dir.join("server.json"), server_with_secret()).unwrap();

    let recorder = RecordingPublish::default();
    let err = publish_through(&ctx, &recorder, "weather").expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert!(
        recorder.seen.lock().unwrap().is_empty(),
        "nothing was sent to the plane"
    );
    let ops_dir = rig.layout().ops_dir();
    let wal: Vec<_> = std::fs::read_dir(&ops_dir)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(wal.is_empty(), "no op record was written: {wal:?}");
}

/// ITEM PAIR (sibling files, the publish door): the preflight gates the WHOLE candidate — a stray
/// script a draft gained beside `server.json` refuses BEFORE the op WAL, naming the allowed set.
/// Before the fix the preflight validated `server.json` alone and shipped the stray file.
#[test]
fn a_stray_sibling_never_reaches_the_wal() {
    let rig = Rig::new("pub-siblings");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    // Adopt it CLEAN; then the draft gains the stray file — the author's own edit, exactly where
    // the gate has to fire.
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();
    std::fs::write(dir.join("evil.sh"), b"#!/bin/sh\necho pwned\n").unwrap();

    let recorder = RecordingPublish::default();
    let err = publish_through(&ctx, &recorder, "weather").expect_err("refused");
    assert_eq!(err.code(), "MCP_INVALID");
    assert!(err.detail().contains("evil.sh"), "{}", err.detail());
    assert!(
        recorder.seen.lock().unwrap().is_empty(),
        "nothing was sent to the plane"
    );
    let ops_dir = rig.layout().ops_dir();
    let wal: Vec<_> = std::fs::read_dir(&ops_dir)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(wal.is_empty(), "no op record was written: {wal:?}");
}

/// A [`RecordingPublish`] whose protocol card declares an OLD server — one that predates MCP
/// bundle kinds and would silently record a SKILL.
#[derive(Clone, Default)]
struct OldServerPublish {
    inner: RecordingPublish,
}
impl crate::plane::ContributeSource for OldServerPublish {
    fn publish(
        &self,
        b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.publish(b)
    }
    fn propose(
        &self,
        b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.propose(b)
    }
    fn revert(
        &self,
        b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.revert(b)
    }
    fn review(
        &self,
        b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.review(b)
    }
    fn protocol_card(&self) -> Option<topos_types::requests::WireProtocolCard> {
        Some(topos_types::requests::WireProtocolCard {
            schema_version: 1,
            card: "topos-protocol-card".to_owned(),
            api_base_url: format!("https://{HOST}/api"),
            server_version: Some("0.1.9".to_owned()),
            min_cli_version: None,
        })
    }
}

/// ITEM PAIR (pre-MCP server): publishing a `kind = "mcp"` bundle to a server whose card predates
/// MCP support refuses with the typed server-too-old shape BEFORE the op WAL — nothing is minted,
/// nothing reaches the wire — instead of the server silently recording a SKILL while the client
/// receipt claims an mcp bundle landed.
#[test]
fn an_mcp_publish_to_a_pre_mcp_server_refuses_before_the_wal() {
    let rig = Rig::new("pub-oldsrv");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();

    let recorder = OldServerPublish::default();
    let rec = recorder.clone();
    let session_connect = move |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(rec.clone()),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(OldServerPublish::default())
    };
    let err = ops::publish(
        &ctx,
        &cc,
        None,
        Some(&session_connect),
        None,
        "weather",
        false,
        None,
        None,
        None,
    )
    .expect_err("a pre-MCP server refuses the mcp publish");
    assert_eq!(err.code(), "SERVER_TOO_OLD");
    assert!(err.detail().contains("0.1.9"), "{}", err.detail());
    assert!(err.detail().contains("MCP"), "{}", err.detail());
    assert!(
        recorder.inner.seen.lock().unwrap().is_empty(),
        "nothing was sent to the plane"
    );
    let ops_dir = rig.layout().ops_dir();
    let wal: Vec<_> = std::fs::read_dir(&ops_dir)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(wal.is_empty(), "no op record was written: {wal:?}");
}

/// A clean mcp bundle publishes with its KIND: the op record carries it, the wire body carries it,
/// and the receipt names it — so the workspace records what the bundle IS.
#[test]
fn the_bundle_kind_rides_the_wal_onto_the_wire_and_the_receipt() {
    let rig = Rig::new("pub-kind");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(
        &ctx,
        &empty_connect,
        None,
        dir.to_str().unwrap(),
        true,
        None,
    )
    .unwrap();

    let recorder = RecordingPublish::default();
    let outcome = publish_through(&ctx, &recorder, "weather").unwrap();
    let ops::PublishOutcome::Published(data) = outcome else {
        panic!("the publish LANDED");
    };
    assert_eq!(
        data.kind.as_deref(),
        Some("mcp"),
        "the receipt names the kind"
    );
    let sent = recorder.seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].kind.as_deref(),
        Some("mcp"),
        "the wire carries the kind"
    );

    // The governance transfer rewrote the local path row to the workspace reference — and the
    // workspace row carries NO kind field: from here the catalog is the authority.
    let text = rig.global_text();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/weather\" = \"*\"")),
        "{text}"
    );
    assert!(
        !text.contains("kind"),
        "the local kind field is gone: {text}"
    );
    assert!(
        !text.contains(&dir.display().to_string()),
        "the path row is gone: {text}"
    );
}

/// An ordinary SKILL publish is untouched by any of this: no kind on the wire, no kind on the
/// receipt — an absent tag reads as `"skill"` by the request's own default.
#[test]
fn an_ordinary_skill_publish_carries_no_kind() {
    let rig = Rig::new("pub-skill");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let mut data = ops::add(&ctx, &dir).unwrap();
    let scope = crate::ops::add_scope(&ctx, true).unwrap();
    crate::ops::note_added_path_in(&ctx, &mut data, &scope.target, &dir).unwrap();

    let recorder = RecordingPublish::default();
    let outcome = publish_through(&ctx, &recorder, "deploy").unwrap();
    let ops::PublishOutcome::Published(pd) = outcome else {
        panic!("the publish LANDED");
    };
    assert_eq!(pd.kind, None);
    assert_eq!(recorder.seen.lock().unwrap()[0].kind, None);
}
