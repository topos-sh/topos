//! `add --mcp` and the publish half of the `kind = "mcp"` bundle: the two doors a server comes in
//! through, the row each one records, and the gate that stands between a server document and the
//! workspace.
//!
//! What is under test here:
//!
//! - the FETCHED door is two-phase — a bare run writes NOTHING and describes; `--yes` writes the
//!   canonical document, records the row, and reports;
//! - the LOCAL door applies immediately, and its row is `add`/`remove`'s exact file inverse even
//!   carrying `{ kind = "mcp" }`;
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
use crate::ops::{self, AddMcpOutcome, McpDocSource};
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
// The FETCHED door — two-phase
// =================================================================================================

/// A server this machine has never read is a NEW-SOURCE TRUST moment: the bare run describes it
/// completely and writes nothing at all — no document, no row, no manifest file birthed.
#[test]
fn a_fetched_server_describes_and_writes_nothing() {
    let rig = Rig::new("describe");
    rig.write_global("[bundles]\n");
    let before = rig.global_text();
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let outcome = ops::add_mcp(&ctx, Some(&docs), "io.github.acme/weather", true, false).unwrap();
    let AddMcpOutcome::Described { data, yes_argv } = outcome else {
        panic!("a never-read source DESCRIBES before it writes");
    };
    let mcp = data.mcp.expect("the describe names the server");
    assert_eq!(mcp.server, "io.github.acme/weather");
    assert_eq!(mcp.version, "1.4.0");
    assert_eq!(mcp.url, "https://weather.acme.example/mcp");
    assert_eq!(mcp.transport, "streamable-http");
    assert_eq!(mcp.headers, vec!["X-Region".to_owned()]);
    // The document declared no auth word, so the describe claims none.
    assert_eq!(mcp.auth, None);
    assert!(mcp.bundle.ends_with("weather"), "{}", mcp.bundle);
    assert_eq!(data.value, "{ kind = \"mcp\" }");
    assert_eq!(
        yes_argv,
        vec![
            "topos".to_owned(),
            "add".to_owned(),
            "--mcp".to_owned(),
            "io.github.acme/weather".to_owned(),
            "-g".to_owned(),
            "--yes".to_owned()
        ]
    );

    // NOTHING moved.
    assert_eq!(rig.global_text(), before, "the describe edits no manifest");
    assert!(
        !rig.layout().home().join("mcp").exists(),
        "the describe writes no bundle"
    );
}

/// `--yes` lands it: the canonical document (pretty-printed, trailing newline) plus ONE row whose
/// value is the inline table that records what the folder IS.
#[test]
fn yes_writes_the_canonical_document_and_the_kind_row() {
    let rig = Rig::new("apply");
    rig.write_global("[bundles]\n");
    let docs = FakeDocs::serving(&good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let outcome = ops::add_mcp(&ctx, Some(&docs), "io.github.acme/weather", true, true).unwrap();
    let AddMcpOutcome::Applied(data) = outcome else {
        panic!("--yes applies");
    };
    assert_eq!(data.name, "weather");

    let bundle = rig.layout().home().join("mcp").join("weather");
    let stored = std::fs::read_to_string(bundle.join("server.json")).unwrap();
    assert!(stored.ends_with("}\n"), "trailing newline: {stored:?}");
    assert!(
        stored.contains("\n  \"name\""),
        "pretty-printed: {stored:?}"
    );
    // The stored bytes are a document the gate accepts — what is placed is what was consented to.
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

    ops::add_mcp(&ctx, Some(&docs), "io.github.acme/weather", true, true).unwrap();
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

    ops::add_mcp(&ctx, Some(&docs), url, true, false).unwrap();
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

    let err =
        ops::add_mcp(&ctx, Some(&docs), "io.github.acme/weather", true, true).expect_err("refused");
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

    let err =
        ops::add_mcp(&ctx, Some(&docs), "io.github.acme/weather", true, true).expect_err("refused");
    assert!(
        err.detail().contains(&taken.display().to_string()),
        "{}",
        err.detail()
    );
    assert_eq!(std::fs::read(taken.join("mine.txt")).unwrap(), b"keep me");
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

    let outcome = ops::add_mcp(&ctx, None, dir.to_str().unwrap(), true, false).unwrap();
    let AddMcpOutcome::Applied(data) = outcome else {
        panic!("an on-disk folder needs no consent ceremony");
    };
    // It went through the ordinary adopt: a real record with a real version identity.
    assert!(!data.skill_id.is_empty(), "the folder was adopted");
    assert_ne!(data.version_id, "0".repeat(64));

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

    ops::add_mcp(&ctx, None, dir.to_str().unwrap(), true, false).unwrap();
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

    let err = ops::add_mcp(&ctx, None, dir.to_str().unwrap(), true, false).expect_err("refused");
    assert!(err.detail().contains("server.json"), "{}", err.detail());
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

    let err = ops::add_mcp(&ctx, None, dir.to_str().unwrap(), true, false).expect_err("refused");
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
    let err = ops::add_mcp(&ctx, None, "@eng/weather", true, false).expect_err("refused");
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
    ops::add_mcp(&ctx, None, dir.to_str().unwrap(), true, false).unwrap();
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
    ops::add_mcp(&ctx, None, dir.to_str().unwrap(), true, false).unwrap();

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
