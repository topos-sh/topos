//! `add --kind mcp` — the ONE door a server comes in through, the rows it records, and the verbs a
//! connected server cannot serve.
//!
//! What is under test here:
//!
//! - the door is THIN. A registry name and an https link go to the workspace VERBATIM, the
//!   workspace rules on them, and its refusal reaches the person in the workspace's own words —
//!   there is no second gate here to drift from the first;
//! - a FOLDER is refused at that door, teaching the hand-written line a machine-local server is —
//!   and that line converges its config entries on the sweep and takes them out again with the row;
//! - which workspace a share goes to is never guessed;
//! - a connected server holds no placed files and no version history HERE (its versions are the
//!   catalog's), so every file verb and every version verb refuses over one, in the two shared
//!   sentences — and every one of them still serves a skill;
//! - the plain skill doors still refuse a `server.json`-rooted folder toward the kind it is, and a
//!   record's kind is fixed when it is written;
//! - the bundle kind rides the op WAL onto the wire: what may be published is the workspace's
//!   ruling, and no client-side gate stands in front of it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::ManifestEntry;
use topos_core::digest::{self, FileMode};
use topos_core::identity::Commit;
use topos_types::requests::{
    McpAddRequest, McpAddedData, WireChannelIndex, WireMcpIndexEntry, WireMe, WireProposalIndex,
    WireSkillIndex, WireSkillLog,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops::{self};
use crate::plane::{
    AppliedSkillReport, DeliverySnapshot, DeliverySource, DirectorySource, FetchedVersion,
    InertFollow, InertPlane, KnownCurrent, LinkStatus, PlaneError, PlaneSource, PointerFetch,
};
use crate::sessions::{self, SESSION_ACTIVE, Session};
use crate::sidecar::Layout;
use crate::test_support::MockHarness;
// The governance fake is the manifest suite's — one stand-in for the whole test tree, so a trait's
// growth is felt in one place.
use super::manifest_reconcile::NoGovernance;

const HOST: &str = "acme.test";
const WS: &str = "w_eng";
const WS_NAME: &str = "eng";
/// A catalog revision as the wire spells one — an opaque handle, never a digest.
const REVISION: &str = "mcpr_0123456789abcdef0123456789abcdef";

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

/// A no-op placement adapter: discovers nothing, touches no skills folder. What an MCP converge
/// engages is the harness table's config surfaces joined onto detection under the fake `$HOME`,
/// which is what these fixtures seed.
fn no_harness() -> MockHarness {
    MockHarness::joining("")
}

struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: MockHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: no_harness(),
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
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
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
        self.seed_session_named(WS, WS_NAME, "sn_1");
    }
    fn seed_session_named(&self, workspace_id: &str, workspace_name: &str, session_id: &str) {
        sessions::upsert_session(
            &self.fs,
            &self.layout(),
            Session {
                host: HOST.into(),
                base_url: format!("https://{HOST}/api"),
                workspace_id: workspace_id.into(),
                workspace_name: workspace_name.into(),
                display_name: "Engineering".into(),
                session_id: session_id.into(),
                credential: "cred-1".into(),
                status: SESSION_ACTIVE.into(),
                logged_in_at: 1,
            },
        )
        .unwrap();
    }
}

/// A clean server document — one https streamable-http remote, one literal header.
fn good_server() -> String {
    r#"{"name":"io.github.acme/weather","description":"Conditions for a named place.",
        "version":"1.4.0","remotes":[{"type":"streamable-http",
        "url":"https://weather.acme.example/mcp","headers":[{"name":"X-Region","value":"eu-west-1"}]}]}"#
        .to_owned()
}

/// Write a folder whose root holds a server document — what a machine-local server IS.
fn write_server_folder(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("server.json"), body).unwrap();
}

/// Record a CONNECTED SERVER in this scope's store, exactly as a delivery does: the document, the
/// catalog revision it came from, and the durable kind marker every later classification reads
/// first. A workspace server has no folder here and no door adopts one, so the fixtures write the
/// record the sweep writes.
fn record_connected(ctx: &Ctx<'_>, id: &str, name: &str, document: &str) -> crate::id::SkillId {
    let sid = crate::id::SkillId::parse(id).unwrap();
    crate::mcp_engine::record_server(ctx, &sid, name, REVISION, document.as_bytes(), false, false)
        .unwrap();
    crate::bundle_kind::write_kind_marker(ctx, &sid, crate::bundle_kind::BundleKind::Mcp);
    sid
}

// -------------------------------------------------------------------------------------------------
// The session lane's fakes
// -------------------------------------------------------------------------------------------------

/// The read half: a workspace that delivers nothing of its own, so what a sweep converges is
/// exactly what the manifest asks for.
#[derive(Debug, Clone, Copy)]
struct EmptyPlane;
impl PlaneSource for EmptyPlane {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::NotFound)
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        Err(PlaneError::NotFound)
    }
}
impl DeliverySource for EmptyPlane {
    fn fetch_delivery(&self, _workspace_id: &str) -> Result<DeliverySnapshot, PlaneError> {
        Ok(DeliverySnapshot {
            skills: Vec::new(),
            mcp_servers: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            staleness_window_ms: 604_800_000,
            link_status: LinkStatus::Active,
            declined: Vec::new(),
        })
    }
    fn report_applied(
        &self,
        _workspace_id: &str,
        _applied: &[AppliedSkillReport],
    ) -> Result<(), PlaneError> {
        Ok(())
    }
}

/// The SHARE LANE, faked: it records every request the door hands it, answers with what the
/// workspace shares the server as (or refuses in the workspace's own words), and serves the
/// catalog that answer is then resolved against.
#[derive(Clone, Default)]
struct ShareLane {
    seen: Arc<Mutex<Vec<McpAddRequest>>>,
    /// What the workspace answers — `Err` is its own refusal SENTENCE, rendered as it wrote it.
    answer: Arc<Mutex<Option<Result<McpAddedData, String>>>>,
    /// The connected servers this workspace's catalog holds.
    servers: Arc<Mutex<Vec<WireMcpIndexEntry>>>,
}
impl ShareLane {
    /// A lane that shares `name` under `skill_id` and then carries it in its catalog — the ordinary
    /// answer, and the state it leaves the workspace in.
    fn sharing(skill_id: &str, name: &str, document: &str) -> Self {
        let lane = ShareLane::default();
        *lane.answer.lock().unwrap() = Some(Ok(McpAddedData {
            skill_id: skill_id.to_owned(),
            name: name.to_owned(),
            created: true,
        }));
        lane.servers.lock().unwrap().push(WireMcpIndexEntry {
            skill_id: skill_id.to_owned(),
            name: name.to_owned(),
            kind: "mcp".to_owned(),
            status: "active".to_owned(),
            display_name: None,
            revision_id: REVISION.to_owned(),
            document: serde_json::from_str(document).unwrap(),
            pinned: None,
            revoked: None,
            updated_at: 0,
        });
        lane
    }
    /// A lane that REFUSES, in the workspace's own words.
    fn refusing(message: &str) -> Self {
        let lane = ShareLane::default();
        *lane.answer.lock().unwrap() = Some(Err(message.to_owned()));
        lane
    }
    fn requests(&self) -> Vec<McpAddRequest> {
        self.seen.lock().unwrap().clone()
    }
}
impl DirectorySource for ShareLane {
    fn me(&self, _ws: &str) -> Result<WireMe, ClientError> {
        Err(ClientError::Plane("no me in this fake".into()))
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        Ok(WireChannelIndex {
            channels: Vec::new(),
        })
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        Ok(WireSkillIndex {
            skills: Vec::new(),
            mcp_servers: self.servers.lock().unwrap().clone(),
        })
    }
    fn mcp_revision(
        &self,
        _ws: &str,
        _s: &str,
        _r: &str,
    ) -> Result<Option<WireMcpIndexEntry>, ClientError> {
        unreachable!("no by-revision read in these flows")
    }
    fn proposals_index(&self, _ws: &str) -> Result<WireProposalIndex, ClientError> {
        unreachable!("no proposal read in these flows")
    }
    fn skill_log(&self, _ws: &str, _s: &str) -> Result<WireSkillLog, ClientError> {
        unreachable!("no history read in these flows")
    }
    fn protect_skill(&self, _ws: &str, _s: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!("no protection write in these flows")
    }
    fn protect_channel(&self, _ws: &str, _c: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!("no protection write in these flows")
    }
    fn add_mcp_server(&self, _ws: &str, body: McpAddRequest) -> Result<McpAddedData, ClientError> {
        self.seen.lock().unwrap().push(body);
        match self.answer.lock().unwrap().clone() {
            Some(Ok(data)) => Ok(data),
            // The transport maps a refused envelope's own message onto this variant, which is
            // shown VERBATIM — so the fake answers in the shape a real refusal arrives in.
            Some(Err(message)) => Err(ClientError::InvalidArgument(message)),
            None => unreachable!("this flow shares no server"),
        }
    }
}

/// The per-session transports these flows ride: the empty plane, the share lane, and a recorder on
/// the contribute lane (only the publish flows ever send anything through it).
fn connect<'a>(
    lane: &'a ShareLane,
    publishes: &'a RecordingPublish,
) -> impl Fn(&Session) -> ops::SessionTransports + 'a {
    move |_s: &Session| ops::SessionTransports {
        plane: Box::new(EmptyPlane),
        directory: Box::new(lane.clone()),
        contribute: Box::new(publishes.clone()),
        governance: Box::new(NoGovernance),
    }
}

// =================================================================================================
// The door: what it takes, and what it refuses
// =================================================================================================

/// **A FOLDER IS NOT A SHARE.** A server only this machine runs has nobody to ask and no ruling to
/// make, so it is a line in the file rather than a command — and the refusal spells that line,
/// because a person who typed a path wants it. The shared path is named beside it, so neither
/// answer has to be guessed at.
#[test]
fn a_folder_refuses_and_spells_the_line_a_machine_local_server_is() {
    let rig = Rig::new("folder");
    rig.write_global("schema = 1\n");
    let dir = rig.work.0.join("weather");
    write_server_folder(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();

    let err = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        dir.to_str().unwrap(),
        None,
        true,
        &Default::default(),
    )
    .expect_err("a folder is not something to share");
    let detail = err.detail();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(detail.contains("is a folder on this machine"), "{detail}");
    // The LINE, spelled with the person's own folder in it, and how to make it take effect.
    assert!(
        detail.contains(&format!(
            "`<name> = {{ path = \"{}\", kind = \"mcp\" }}`",
            dir.display()
        )),
        "{detail}"
    );
    assert!(detail.contains("under `[mcp]`"), "{detail}");
    assert!(detail.contains("'topos install'"), "{detail}");
    // …and the other half: what sharing this server with the team actually takes.
    assert!(
        detail.contains("topos add --kind mcp <its registry name or the https link"),
        "{detail}"
    );
    // Nothing was asked of the workspace, and nothing was written.
    assert!(lane.requests().is_empty(), "{:?}", lane.requests());
    assert_eq!(rig.global_text(), "schema = 1\n");
}

/// `--kind mcp` never re-labels a governed reference: a workspace already records what each bundle
/// is, so the refusal points at the plain `add` — which gets it with its kind intact.
#[test]
fn a_workspace_reference_refuses_toward_the_plain_add() {
    let rig = Rig::new("wsref");
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();

    let err = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "@eng/weather",
        None,
        true,
        &Default::default(),
    )
    .expect_err("a workspace bundle carries its own kind");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(
        err.detail().contains("topos add @eng/weather"),
        "{}",
        err.detail()
    );
    assert!(
        err.detail().contains("no `--kind` needed"),
        "{}",
        err.detail()
    );
    assert!(lane.requests().is_empty(), "nothing was shared");
}

/// A token that is neither spelling names nothing this door can share — so the refusal states the
/// two it CAN, one of them with an example.
#[test]
fn a_source_this_door_cannot_read_names_the_two_it_can() {
    let rig = Rig::new("unreadable");
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();

    let err = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "weather",
        None,
        true,
        &Default::default(),
    )
    .expect_err("a bare word is neither a server name nor a link");
    let detail = err.detail();
    assert!(detail.contains("io.github.acme/weather"), "{detail}");
    assert!(detail.contains("https link to a server.json"), "{detail}");
    assert!(lane.requests().is_empty(), "nothing was shared");
    assert_eq!(rig.global_text(), "schema = 1\n");
}

// =================================================================================================
// The share lane — the workspace rules, the client carries
// =================================================================================================

/// **THE SPELLING A PERSON TYPED GOES TO THE WORKSPACE, AND NOTHING ELSE DOES.** The two shared
/// spellings ride two different fields, because they are two different acts: a registry name is a
/// CONNECTION to a server the catalog can resolve, and a link is a document the workspace writes
/// down as its own. The client reads neither and fetches neither.
#[test]
fn the_two_shared_spellings_reach_the_workspace_verbatim() {
    let rig = Rig::new("spellings");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let publishes = RecordingPublish::default();

    let lane = ShareLane::sharing("s_weather", "weather", &good_server());
    ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        None,
        true,
        &Default::default(),
    )
    .expect("the workspace shares it");
    let sent = lane.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].registry_name.as_deref(),
        Some("io.github.acme/weather")
    );
    assert_eq!(sent[0].document_url, None);
    assert_eq!(sent[0].schema_version, topos_types::WIRE_SCHEMA_VERSION);

    let link = ShareLane::sharing(
        "s_tides",
        "tides",
        &good_server().replace("weather", "tides"),
    );
    ops::add_mcp(
        &ctx,
        &connect(&link, &publishes),
        "https://tides.acme.example/server.json",
        None,
        true,
        &Default::default(),
    )
    .expect("the workspace holds the document as its own");
    let sent = link.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].registry_name, None);
    assert_eq!(
        sent[0].document_url.as_deref(),
        Some("https://tides.acme.example/server.json")
    );
}

/// **THE WORKSPACE NAMES THE BUNDLE, AND THE ROW FOLLOWS THAT NAME.** What a person typed is a
/// registry identity; what every machine adds the server by is the name the workspace shares it as
/// — so the row this add writes is the workspace reference, and the same invocation converges the
/// scope's MCP configs from the document the catalog carries.
#[test]
fn the_workspace_names_the_bundle_and_the_row_and_the_entries_follow() {
    let rig = Rig::new("shared");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    // One MCP-capable agent set up in the fake home, so the inline converge has a surface.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let publishes = RecordingPublish::default();
    // The person typed the registry identity; the workspace shares it as `weather`.
    let lane = ShareLane::sharing("s_weather", "weather", &good_server());

    let added = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        None,
        true,
        &Default::default(),
    )
    .expect("the share lands");
    assert_eq!(added.data.name, "weather");
    assert!(
        added.messages.is_empty(),
        "every planned surface was written: {:?}",
        added.messages
    );

    let text = rig.global_text();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/weather\" = \"latest\"")),
        "the row is the workspace reference, under the name the workspace gave: {text}"
    );
    let cursor = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json"))
        .expect("the converge wrote the agent's config");
    assert!(
        cursor.contains("https://weather.acme.example/mcp"),
        "{cursor}"
    );

    // The receipt states WHICH server this row now points at — the one thing the per-surface lines
    // beside it cannot say — read back from the record the delivery landed, never from the
    // spelling that was typed. No `bundle` folder is named: a shared server has none.
    let mcp = added.data.mcp.as_ref().expect("the typed block");
    assert_eq!(mcp.server, "io.github.acme/weather");
    assert_eq!(mcp.url, "https://weather.acme.example/mcp");
    assert_eq!(mcp.transport, "streamable-http");
    assert_eq!(mcp.agents, vec!["Cursor".to_owned()]);
    assert_eq!(mcp.bundle, None);
    assert!(
        added.data.note.as_deref().unwrap_or_default().contains(
            "MCP server io.github.acme/weather v1.4.0 — https://weather.acme.example/mcp over \
                 streamable-http"
        ),
        "{:?}",
        added.data.note
    );
}

/// **THE AUTH WORD THAT IS A TASK GETS A LINE OF ITS OWN.** `oauth` and `none` describe something
/// that happens by itself and ride the summary line as a clause; `manual` describes something a
/// person has to go and do, and the entry stands there doing nothing until they do — so the
/// receipt says it in words, once, where the row is reported.
#[test]
fn a_manual_sign_in_says_what_the_person_has_to_do() {
    const SETUP: &str = "Signing in to this server is a one-time manual step on this machine — a \
                         token you create, or an app an administrator registers; no agent can \
                         complete it";
    let rig = Rig::new("manual-auth");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let publishes = RecordingPublish::default();

    let lane = ShareLane::sharing(
        "s_rota",
        "rota",
        r#"{"name":"io.github.acme/rota","description":"Read and edit the on-call rota.",
            "version":"2.1.0","remotes":[{"type":"streamable-http",
            "url":"https://rota.acme.example/mcp"}],"_meta":{"sh.topos/auth":"manual"}}"#,
    );
    let added = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/rota",
        None,
        true,
        &Default::default(),
    )
    .expect("the share lands");
    // The word travels TYPED too — an agent reading `--json` gets it without parsing prose.
    assert_eq!(
        added
            .data
            .mcp
            .as_ref()
            .expect("typed block")
            .auth
            .as_deref(),
        Some("manual")
    );
    let note = added.data.note.clone().unwrap_or_default();
    assert!(note.contains(", auth manual"), "{note}");
    assert!(note.contains(SETUP), "{note}");

    // An OAUTH server is unchanged: the clause, and no second line — nobody has to do anything.
    let rig = Rig::new("oauth-auth");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let lane = ShareLane::sharing(
        "s_tickets",
        "tickets",
        r#"{"name":"io.github.acme/tickets","description":"Search and comment on tickets.",
            "version":"0.9.0","remotes":[{"type":"streamable-http",
            "url":"https://tickets.acme.example/mcp"}],"_meta":{"sh.topos/auth":"oauth"}}"#,
    );
    let added = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/tickets",
        None,
        true,
        &Default::default(),
    )
    .expect("the share lands");
    let note = added.data.note.clone().unwrap_or_default();
    assert!(note.contains(", auth oauth"), "{note}");
    assert!(!note.contains(SETUP), "{note}");
}

/// A `--kind` WORD THAT CONTRADICTS THE CATALOG IS REFUSED. The catalog is the authority on what a
/// workspace bundle is, which is exactly why the flag can only agree with it or be wrong — and
/// being wrong mattered: `--kind skill` on a shared server used to deliver a tool endpoint into
/// the person's agents without a syllable about it.
#[test]
fn a_kind_word_contradicting_the_catalog_refuses_at_the_workspace_door() {
    let rig = Rig::new("kind-contradict");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let lane = ShareLane::sharing("s_weather", "weather", &good_server());
    let publishes = RecordingPublish::default();

    let add = |declared: Option<crate::bundle_kind::BundleKind>| {
        ops::add_reference(
            &ctx,
            &connect(&lane, &publishes),
            None,
            &format!("{HOST}/{WS_NAME}/weather"),
            true,
            true,
            &Default::default(),
            declared,
        )
    };

    // The contradiction refuses — naming both kinds, and the flag as the thing to drop.
    let err = add(Some(crate::bundle_kind::BundleKind::Skill))
        .expect_err("`--kind skill` on a shared server refuses");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("is an MCP server in the catalog, not a skill")
            && msg.contains("needs no `--kind` at all"),
        "{msg}"
    );
    assert_eq!(
        rig.global_text(),
        "schema = 1\n",
        "nothing was written: {msg}"
    );

    // The word that AGREES passes this gate — the flag is redundant here, never wrong.
    add(Some(crate::bundle_kind::BundleKind::Mcp)).expect("the matching word is never the refusal");
    assert!(
        rig.global_text()
            .contains(&format!("\"{HOST}/{WS_NAME}/weather\"")),
        "{}",
        rig.global_text()
    );
}

/// **A REFUSAL IS THE WORKSPACE'S, WORD FOR WORD.** Whether a document may be shared, whether the
/// catalog holds that name, and whether the caller may write a server down at all are answered
/// where the workspace lives — for every surface that asks. A client that re-worded any of it
/// would be a second gate with its own vocabulary to drift, so the sentence is passed through and
/// nothing is written.
#[test]
fn the_workspaces_refusal_arrives_in_its_own_words() {
    const SAID: &str = "this server's document carries a credential in a header value, so it \
                        cannot be shared — publish it with the value left empty";
    let rig = Rig::new("refused");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let lane = ShareLane::refusing(SAID);
    let publishes = RecordingPublish::default();

    let err = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        None,
        true,
        &Default::default(),
    )
    .expect_err("the workspace refused");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert_eq!(err.detail(), SAID);
    assert_eq!(crate::render::safe_message(&err), SAID);
    assert_eq!(rig.global_text(), "schema = 1\n", "nothing was recorded");
}

/// **A SHARE IS NEVER AIMED BY GUESS.** The server reaches everybody in the workspace it lands in,
/// so a machine logged into two of them is asked which one — by name — instead of one being
/// picked; `--workspace` answers that, and a machine logged into none is told what login it needs.
#[test]
fn the_workspace_a_share_lands_in_is_named_never_guessed() {
    let rig = Rig::new("which-ws");
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let publishes = RecordingPublish::default();

    // No session at all: the refusal is the typed one, and it names the verb that fixes it.
    let lane = ShareLane::sharing("s_weather", "weather", &good_server());
    let err = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        None,
        true,
        &Default::default(),
    )
    .expect_err("sharing needs a workspace to share with");
    assert_eq!(err.code(), "SESSION_REQUIRED");
    assert!(err.detail().contains("'topos login'"), "{}", err.detail());
    assert!(lane.requests().is_empty(), "nothing was shared");

    // TWO live sessions and no `--workspace`: both names are stated, and nothing is sent.
    rig.seed_session_named(WS, WS_NAME, "sn_1");
    rig.seed_session_named("w_ops", "ops", "sn_2");
    let err = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        None,
        true,
        &Default::default(),
    )
    .expect_err("two workspaces are not one");
    let detail = err.detail();
    assert!(detail.contains("more than one workspace"), "{detail}");
    assert!(
        detail.contains(WS_NAME) && detail.contains("ops"),
        "{detail}"
    );
    assert!(detail.contains("--workspace"), "{detail}");
    assert!(lane.requests().is_empty(), "nothing was shared");

    // `--workspace` names it, and the share goes to exactly that one.
    ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        Some("ops"),
        true,
        &Default::default(),
    )
    .expect("the named workspace takes it");
    assert_eq!(lane.requests().len(), 1);
    assert!(
        rig.global_text()
            .contains(&format!("\"{HOST}/ops/weather\"")),
        "the row names the workspace that was asked: {}",
        rig.global_text()
    );

    // A workspace this machine is not logged into is a refusal that spells the login.
    let err = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        Some("design"),
        true,
        &Default::default(),
    )
    .expect_err("no session for that workspace");
    assert!(
        err.detail().contains("'topos login design'"),
        "{}",
        err.detail()
    );
}

/// `-a` narrows a shared server's row to the named agents' config FILES, and `remove <name> -a`
/// SUBTRACTS one: its entry leaves that config in the same invocation, the other agent's entry
/// stays, and the row keeps the rest — receipts counting config files, never folders.
#[test]
fn a_selection_narrows_a_shared_server_by_config_file() {
    let rig = Rig::new("mcp-dest");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    // Two MCP-capable agents set up in the fake home.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let lane = ShareLane::sharing("s_weather", "weather", &good_server());
    let publishes = RecordingPublish::default();

    let sel = ops::Selection {
        agents: vec!["cursor".to_owned(), "codex".to_owned()],
        dests: Vec::new(),
    };
    let data = ops::add_mcp(
        &ctx,
        &connect(&lane, &publishes),
        "io.github.acme/weather",
        None,
        true,
        &sel,
    )
    .expect("the share lands")
    .data;
    assert_eq!(
        data.dest,
        vec![
            "~/.cursor/mcp.json".to_owned(),
            "~/.codex/config.toml".to_owned()
        ]
    );
    let text = rig.global_text();
    assert!(
        text.contains(r#"dest = ["~/.cursor/mcp.json", "~/.codex/config.toml"]"#),
        "{text}"
    );
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let codex = rig.home.0.join(".codex/config.toml");
    for f in [&cursor, &codex] {
        assert!(
            std::fs::read_to_string(f)
                .unwrap()
                .contains("weather.acme.example"),
            "{} got the entry",
            f.display()
        );
    }

    // Narrow remove: subtract codex's config file — its entry leaves NOW, cursor's stays, and the
    // row keeps the remaining file.
    let narrow = ops::Selection {
        agents: vec!["codex".to_owned()],
        dests: Vec::new(),
    };
    let outcome = ops::remove_global(
        &ctx,
        &connect(&lane, &publishes),
        &["weather".to_owned()],
        None,
        false,
        &narrow,
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("a narrow applies immediately");
    };
    let text = rig.global_text();
    assert!(
        text.contains(r#"dest = ["~/.cursor/mcp.json"]"#),
        "the row keeps the remaining file: {text}"
    );
    assert!(
        std::fs::read_to_string(&codex)
            .map(|t| !t.contains("weather.acme.example"))
            .unwrap_or(true),
        "the codex entry left with the subtraction"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("weather.acme.example"),
        "cursor's entry stays"
    );
    // The receipt counts CONFIG FILES and names what remains.
    let u = &removed.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/.codex/config.toml".to_owned()]);
    assert_eq!(u.kind.as_deref(), Some("mcp"));
    assert_eq!(u.remaining, Some(1));
    let tty = crate::render::remove_applied_tty(&removed);
    assert!(
        tty.contains("removed (~/.codex/config.toml) — 1 config file remains"),
        "{tty}"
    );
}

// =================================================================================================
// The LOCAL row — a hand-written line, read every run
// =================================================================================================

/// **THE LINE THE FOLDER REFUSAL TEACHES IS A LINE THAT WORKS.** A `kind = "mcp"` path row is the
/// whole mechanic for a server only this machine runs: the sweep reads the folder's `server.json`
/// every run and converges the scope's configs from it — no adopt, no record, no version.
#[test]
fn a_hand_written_local_row_converges_its_entries_on_update() {
    let rig = Rig::new("local-row");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let dir = rig.home.0.join("weather");
    write_server_folder(&dir, &good_server());
    rig.write_global(&format!(
        "[mcp]\nweather = {{ path = \"{}\", kind = \"mcp\" }}\n",
        dir.display()
    ));
    let ctx = rig.ctx_at(None);
    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();

    ops::manifest_update(
        &ctx,
        &connect(&lane, &publishes),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .expect("the sweep runs");
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://weather.acme.example/mcp"),
        "the row's own folder is the demand"
    );

    // The document is read EVERY run: an edit to the folder reaches the config on the next sweep,
    // with nothing to re-add and no version to publish.
    write_server_folder(
        &dir,
        &good_server().replace("weather.acme", "weather2.acme"),
    );
    ops::manifest_update(
        &ctx,
        &connect(&lane, &publishes),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .expect("the sweep runs again");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://weather2.acme.example/mcp"),
        "the edited document landed"
    );
}

/// A `~/`-spelled row resolves like every other local-path key, and `remove` of it takes its config
/// entries out IN THE SAME INVOCATION — they do not linger undisclosed for the next sweep — while
/// the folder, which is the person's own material, is never touched.
#[test]
fn a_tilde_spelled_local_row_takes_its_entries_out_with_the_row() {
    let rig = Rig::new("tilde");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let dir = rig.home.0.join("weather");
    write_server_folder(&dir, &good_server());
    let before = "[skills]\n# a comment that must survive\n\"topos.sh/eng/deploy\" = \"latest\"\n";
    rig.write_global(&format!(
        "{before}weather = {{ path = \"~/weather\", kind = \"mcp\" }}\n"
    ));
    let ctx = rig.ctx_at(None);
    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();

    ops::manifest_update(
        &ctx,
        &connect(&lane, &publishes),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .expect("the sweep runs");
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("weather.acme.example"),
        "the sweep placed the entry"
    );

    let outcome = ops::remove_global(
        &ctx,
        &connect(&lane, &publishes),
        &["~/weather".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("the re-spelled row still removes");
    };
    let note = removed.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("the server's entry was removed."),
        "the inline converge must reach a ~/ row's entries: {note}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .map(|t| !t.contains("weather.acme.example"))
            .unwrap_or(true),
        "the config entry left with the row"
    );
    assert_eq!(
        rig.global_text(),
        before,
        "the rest of the file is byte-identical"
    );
    // The folder is the person's own material — topos never created it, so it is never deleted.
    assert!(dir.join("server.json").exists());
}

// =================================================================================================
// The verbs a connected server cannot serve
// =================================================================================================

/// THE KIND-AWARE FILE VERBS. `diff`, `update --reset` and `update --keep-mine` all act on a
/// bundle's PLACED FILES, and a connected server has none — so all three refuse, in ONE voice,
/// naming what the verb acts on and a command that answers instead.
///
/// What each did before is the reason: `diff` answered an empty diff (which reads as "nothing has
/// changed" over a bundle it never looked at, hand-edited entry and all); `--reset --yes` reported
/// "local edits discarded" one command after its own preview said there were none, and discarded
/// nothing; `--keep-mine` answered in a skill's merge vocabulary.
#[test]
fn the_file_verbs_refuse_over_a_connected_server() {
    let rig = Rig::new("fileverbs");
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    record_connected(&ctx, "s_weather", "weather", &good_server());

    let says_it_all = |e: &ClientError, verb: &str| {
        let d = e.detail();
        assert_eq!(e.code(), "INVALID_ARGUMENT", "{d}");
        assert!(d.contains("'weather' is an MCP server bundle"), "{d}");
        assert!(d.contains(verb), "{d}");
        assert!(d.contains("places none"), "{d}");
        // Never a vague line: the refusal names a command that answers.
        assert!(d.contains("`topos list weather`"), "{d}");
    };

    let err = ops::diff(
        &ctx,
        "weather",
        None,
        ops::DiffBudget(None),
        &Default::default(),
        ops::StoreScope::Here,
    )
    .expect_err("diff refuses instead of answering an empty diff");
    says_it_all(&err, "`diff` compares");

    // Bare, and with EITHER targeted selector: the kind is asked ahead of the selection, whose
    // vocabulary is skills folders — `-a cursor` used to resolve the SKILLS dir and refuse about a
    // folder that was never the point.
    for sel in [
        ops::Selection::default(),
        ops::Selection::one(Some("cursor"), None),
        ops::Selection::one(None, Some("~/.cursor/mcp.json")),
    ] {
        for yes in [false, true] {
            let err = ops::reset(
                &ctx,
                &["weather".to_owned()],
                yes,
                ops::StoreScope::Machine,
                &sel,
            )
            .expect_err("reset refuses instead of reporting a discard it did not do");
            says_it_all(&err, "`--reset` discards your edits to");
        }
    }

    let err = pull_keep_mine(&ctx, "weather").expect_err("keep-mine refuses");
    says_it_all(&err, "`--keep-mine` ends a stopped merge over");
}

/// `update <name> --keep-mine` as the app dispatches it.
fn pull_keep_mine(ctx: &Ctx<'_>, name: &str) -> Result<ops::PullOutcome, ClientError> {
    ops::pull(
        ctx,
        ops::PullScope::One {
            name: name.to_owned(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
            store: ops::StoreScope::Machine,
        },
    )
}

/// THE KIND-AWARE VERSION VERBS. `log`, `revert` and `update <name>@<version>` all act on a
/// bundle's own version history, and a connected server has none HERE: what it holds is the one
/// catalog revision it was given, and every revision behind that one belongs to whoever publishes
/// the server. So all three refuse in the second shared voice rather than answering over a history
/// that is not this machine's to show, put back, or publish forward.
#[test]
fn the_version_verbs_refuse_over_a_connected_server() {
    let rig = Rig::new("versionverbs");
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sid = record_connected(&ctx, "s_weather", "weather", &good_server());
    // `revert` acts on a FOLLOWED bundle only, so the follow state has to claim this record before
    // the kind gate is the thing being tested.
    let follow = FollowedHere(sid.as_str().to_owned());
    let ctx = Ctx {
        follow: &follow,
        ..rig.ctx_at(Some(&rig.work.0))
    };

    let says_it_all = |e: &ClientError, verb: &str| {
        let d = e.detail();
        assert_eq!(e.code(), "INVALID_ARGUMENT", "{d}");
        assert!(d.contains("'weather' is an MCP server bundle"), "{d}");
        assert!(d.contains(verb), "{d}");
        assert!(d.contains("a server's versions are the catalog's"), "{d}");
        assert!(d.contains("`topos list weather`"), "{d}");
    };

    let no_session = |_: &Session| -> ops::SessionTransports {
        unreachable!("the kind is asked before any workspace read")
    };
    let err = ops::log(
        &ctx,
        &ops::LogConnectors {
            session: &no_session,
        },
        "weather",
        ops::RowPage::unlimited(),
    )
    .expect_err("log refuses");
    says_it_all(&err, "`log` lists the versions of");

    let no_contribute = |_: &str, _: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        unreachable!("the kind is asked before any write is built")
    };
    let err = ops::revert(
        &ctx,
        &ops::RevertConnectors {
            contribute: &no_contribute,
            session: &no_session,
        },
        "weather",
        &"a".repeat(64),
        true,
        None,
        ops::StoreScope::Here,
    )
    .expect_err("revert refuses");
    says_it_all(&err, "`revert` publishes an earlier version of");

    let err = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "weather".to_owned(),
            workspace: None,
            mode: ops::TargetMode::GoBack(crate::ops::VersionRef::Full([0u8; 32])),
            store: ops::StoreScope::Machine,
        },
    )
    .expect_err("a go-back refuses");
    says_it_all(&err, "`update <name>@<version>` puts an earlier version of");
}

/// A follow state claiming ONE record for a workspace — what makes the followed-only resolutions
/// (`revert`, `review`) see it at all.
struct FollowedHere(String);
impl crate::plane::FollowSource for FollowedHere {
    fn followed(&self) -> Vec<(String, crate::plane::FollowContext)> {
        vec![(
            self.0.clone(),
            crate::plane::FollowContext {
                workspace_id: WS.to_owned(),
                review_required: false,
                following: true,
            },
        )]
    }
}

/// The same verbs over an ORDINARY SKILL are untouched — the refusals are keyed on the kind, not
/// bolted onto the verbs.
#[test]
fn the_same_verbs_still_serve_a_skill() {
    let rig = Rig::new("verbs-skill");
    rig.write_global("schema = 1\n");
    let src = rig.work.0.join("notes");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# notes\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();
    ops::adopt_path(&ctx, &scope, &src, ops::KindDeclared::No).expect("the skill adopts");

    // A clean skill diffs (to nothing) rather than refusing.
    ops::diff(
        &ctx,
        "notes",
        None,
        ops::DiffBudget(None),
        &Default::default(),
        ops::StoreScope::Here,
    )
    .expect("a skill still diffs");
    // A reset reaches its own two-phase describe, not a kind refusal.
    let out = ops::reset(
        &ctx,
        &["notes".to_owned()],
        false,
        ops::StoreScope::Machine,
        &Default::default(),
    )
    .expect("a skill still resets");
    assert!(
        matches!(out, ops::ResetOutcome::Described { .. }),
        "{out:?}"
    );
    // …and its history is its own to read.
    let no_session = |_: &Session| -> ops::SessionTransports {
        unreachable!("a local log builds no session transports")
    };
    ops::log(
        &ctx,
        &ops::LogConnectors {
            session: &no_session,
        },
        "notes",
        ops::RowPage::unlimited(),
    )
    .expect("a skill still logs");
}

// =================================================================================================
// The kind word at the plain doors
// =================================================================================================

/// ITEM PAIR (saying no kind at all): a plain `topos add ./folder` — and publish's auto-add — on a
/// folder whose root holds `server.json` and no `SKILL.md` refuses instead of silently adopting a
/// SKILL that delivers raw JSON into skills dirs. The refusal states what the folder IS, then both
/// real answers: the line a machine-local server is, and the command that shares one.
#[test]
fn a_server_folder_at_a_skill_door_refuses_toward_the_kind_it_is() {
    let rig = Rig::new("miskind");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    let dir = rig.work.0.join("weather");
    write_server_folder(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The plain add door.
    let err = ops::add(&ctx, &dir).expect_err("refused");
    assert_eq!(err.code(), "KIND_REQUIRED");
    let detail = err.detail();
    assert!(
        detail.contains(
            "is an MCP server, not a skill: its root holds server.json and no \
                         SKILL.md"
        ),
        "{detail}"
    );
    assert!(detail.contains(r#"= { kind = "mcp" }"#), "{detail}");
    assert!(detail.contains("topos add --kind mcp"), "{detail}");
    assert!(detail.contains("topos add --kind skill"), "{detail}");
    assert!(
        !rig.layout().skills_dir().exists(),
        "nothing was adopted: {detail}"
    );

    // The publish door's auto-add pre-step refuses the same way, before anything lands.
    let err =
        ops::ensure_tracked(&ctx, None, dir.to_str().unwrap()).expect_err("publish refuses too");
    assert_eq!(err.code(), "KIND_REQUIRED");

    // ONE runnable answer rides the machine channel — adopting the folder as a skill. The other
    // two are a file edit and a command about a registry name, neither of which is an argv over
    // THIS folder, and an offered command that would not run is worse than none.
    let actions = crate::render::next_actions("add", &[], &err);
    let argvs: Vec<&[String]> = actions.iter().map(|a| a.argv.as_slice()).collect();
    assert_eq!(argvs.len(), 1, "{actions:?}");
    assert_eq!(
        argvs[0],
        [
            "topos".to_owned(),
            "add".to_owned(),
            "--kind".to_owned(),
            "skill".to_owned(),
            dir.display().to_string(),
            "--json".to_owned(),
        ]
    );
}

/// `--kind skill` on a `server.json`-rooted folder ADOPTS IT AS A SKILL. The guard exists to stop a
/// SILENT mis-kind, not to overrule a person who said which kind they meant: the explicit word
/// wins, the bytes land in skills folders, and the record's durable marker says `skill` — so no
/// later sweep re-kinds it.
#[test]
fn an_explicit_skill_word_adopts_a_server_folder_as_a_skill() {
    let rig = Rig::new("explicit-skill");
    rig.write_global("schema = 1\n");
    let dir = rig.work.0.join("weather");
    write_server_folder(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();

    // Silence still refuses — the guard's whole reason for standing.
    let err = ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::No)
        .expect_err("unflagged, the server folder refuses");
    assert_eq!(err.code(), "KIND_REQUIRED");

    // The person's own word wins.
    let data = ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::Yes)
        .expect("`--kind skill` adopts the folder as a skill");
    assert_eq!(data.name, "weather");
    assert!(
        data.mcp.is_none(),
        "a skill adopt carries no server block: {:?}",
        data.mcp
    );
    let sid = crate::id::SkillId::parse(data.skill_id.as_deref().expect("an adopted id")).unwrap();
    assert_eq!(
        crate::bundle_kind::kind_marker(&rig.fs, &rig.layout(), &sid).as_deref(),
        Some("skill")
    );
}

/// A FOLDER THAT IS BOTH. The server-folder guard armed only when there was NO `SKILL.md` — so a
/// folder holding both markers sailed past it, was adopted as a skill, and the server never
/// landed: half of what the person pointed at, with nothing said about the other half. Nothing in
/// the bytes says which kind it is, so the door refuses and names both answers rather than picking
/// one. An explicit `--kind` still wins, because the guard exists to stop a SILENT mis-kind.
#[test]
fn a_folder_holding_both_markers_refuses_and_names_both_answers() {
    let rig = Rig::new("both-markers");
    rig.write_global("schema = 1\n");
    let dir = rig.work.0.join("weather");
    write_server_folder(&dir, &good_server());
    std::fs::write(dir.join("SKILL.md"), b"# weather\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add(&ctx, &dir).expect_err("neither kind is guessable");
    assert_eq!(err.code(), "KIND_REQUIRED");
    let detail = err.detail();
    assert!(
        detail.contains("holds both a SKILL.md and a root server.json"),
        "{detail}"
    );
    assert!(detail.contains("topos add --kind skill"), "{detail}");
    assert!(detail.contains(r#"= { kind = "mcp" }"#), "{detail}");
    assert_eq!(rig.global_text(), "schema = 1\n", "nothing was recorded");

    // The declared word still wins — silence was the only thing being guarded.
    let scope = ops::add_scope(&ctx, true).unwrap();
    ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::Yes)
        .expect("an explicit kind adopts the folder");
}

/// A RECORD'S KIND IS FIXED WHEN IT IS WRITTEN. A folder that already holds a bundle this scope
/// records as a connected server cannot be re-linked as a skill: every later reader trusts the
/// durable marker, so a marker and a row that disagreed would put skill-dir placement on a record
/// whose delivery is config entries. The refusal names both kinds and the command that frees the
/// folder.
#[test]
fn a_server_record_cannot_be_re_linked_as_a_skill() {
    let rig = Rig::new("relink");
    rig.write_global("schema = 1\n");
    let dir = rig.work.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# weather\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // A server record whose map claims this folder — the state a re-link door meets.
    let sid = record_connected(&ctx, "s_weather", "weather", &good_server());
    claim_folder(&ctx, &sid, &dir);

    let scope = ops::add_scope(&ctx, true).unwrap();
    let err = ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::Yes)
        .expect_err("the record is a server's");
    let detail = err.detail();
    assert!(
        detail.contains("already tracked here as a mcp bundle"),
        "{detail}"
    );
    assert!(detail.contains("skill bundle"), "{detail}");
    assert!(detail.contains("topos remove weather"), "{detail}");
    assert_eq!(rig.global_text(), "schema = 1\n", "nothing was recorded");
    // And the marker still says what the record IS — never overwritten, never left to disagree.
    assert_eq!(
        crate::bundle_kind::kind_marker(&rig.fs, &rig.layout(), &sid).as_deref(),
        Some("mcp")
    );
}

/// Record `dir` as a placement of `sid` — the one thing that makes a folder resolve to a record.
fn claim_folder(ctx: &Ctx<'_>, sid: &crate::id::SkillId, dir: &Path) {
    let path = ctx.layout.published(sid).map;
    let mut map = crate::doc::read_map(ctx.fs, &path)
        .unwrap()
        .expect("the map");
    map.placements.push(dir.display().to_string());
    // Strictly 1:1 with the placements, as the document's own invariant demands.
    map.placement_state
        .push(topos_types::persisted::PlacementState {
            kind: topos_types::persisted::PlacementKind::Native,
            agent: None,
            materialized_sha: None,
            pre_existing_sha: None,
            swap_capability: topos_types::persisted::SwapCapability::Unsupported,
            adopted_source: true,
            claim: None,
        });
    crate::doc::write_map(ctx.fs, &path, &map).unwrap();
}

// =================================================================================================
// The TEARDOWN — the config entries only the sidecar's ledger can account for
// =================================================================================================

/// An MCP entry topos placed is LIVE WIRING: it points a running agent at a server. The ledger that
/// proves which entries are topos's lives in `~/.topos/`, so an uninstall that deleted the tree and
/// left the entries left them unaccountable forever — no later run could tell them from a hand
/// edit. The teardown retires them first, through the SAME owned-entry mechanics `remove` uses: a
/// clean entry goes, a hand-edited one stays byte-identical, and BOTH facts are named in the
/// preview before anything happens.
#[test]
fn the_teardown_retires_its_own_mcp_entries_and_leaves_a_hand_edited_one() {
    let rig = Rig::new("teardown-mcp");
    // Two USER-scope MCP surfaces, both detected off the fake home.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    // A line of the person's own, so the file is not WHOLLY topos-owned: the last-entry-deletion
    // rule then keeps it, and the assertion is about the ENTRY rather than the file.
    std::fs::write(rig.home.0.join(".codex/config.toml"), b"model = \"o3\"\n").unwrap();
    let dir = rig.home.0.join("weather");
    write_server_folder(&dir, &good_server());
    rig.write_global(&format!(
        "[mcp]\nweather = {{ path = \"{}\", kind = \"mcp\" }}\n",
        dir.display()
    ));
    let ctx = rig.ctx_at(None);
    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();
    ops::manifest_update(
        &ctx,
        &connect(&lane, &publishes),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .expect("the sweep places the entries");

    let cursor = rig.home.0.join(".cursor/mcp.json");
    let codex = rig.home.0.join(".codex/config.toml");
    for f in [&cursor, &codex] {
        assert!(
            std::fs::read_to_string(f)
                .unwrap()
                .contains("https://weather.acme.example/mcp"),
            "the sweep placed an entry in {}",
            f.display()
        );
    }
    // HAND-EDIT the cursor entry: its bytes no longer fingerprint to what topos wrote, so it is
    // the person's now — a teardown may not take it.
    let drifted = std::fs::read_to_string(&cursor)
        .unwrap()
        .replace("weather.acme.example", "weather.mine.example");
    std::fs::write(&cursor, &drifted).unwrap();

    // The PREVIEW names every file it will open, and says which entries it will leave.
    let described = ops::uninstall(&ctx, None, false).unwrap();
    let ops::UninstallOutcome::Described { describe, yes_argv } = described else {
        panic!("a bare uninstall describes")
    };
    assert_eq!(
        describe.mcp_files,
        vec![codex.to_string_lossy().into_owned()],
        "the clean surface is named for removal"
    );
    assert_eq!(
        describe.mcp_drifted,
        vec![cursor.to_string_lossy().into_owned()],
        "the hand-edited surface is named as one it will leave"
    );
    let preview = crate::render::uninstall_describe_tty(&describe, &yes_argv);
    assert!(
        preview.contains(&format!(
            "  · remove topos-placed MCP server entries from {}",
            codex.display()
        )),
        "{preview}"
    );
    assert!(
        preview.contains(&format!(
            "  · leave the hand-edited entries in {} in place",
            cursor.display()
        )),
        "{preview}"
    );
    // Nothing has changed yet.
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), drifted);

    let applied = ops::uninstall(&ctx, None, true).unwrap();
    let ops::UninstallOutcome::Applied { applied, messages } = applied else {
        panic!("--yes applies")
    };
    assert!(messages.is_empty(), "{messages:?}");
    assert!(!rig.layout().home().exists(), "the sidecar tree is gone");
    let codex_after = std::fs::read_to_string(&codex).unwrap();
    assert!(
        !codex_after.contains("weather.acme.example"),
        "the entry topos owned is gone from the clean surface: {codex_after}"
    );
    assert!(
        codex_after.contains("model = \"o3\""),
        "every byte that was not topos's is still there: {codex_after}"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        drifted,
        "the hand-edited entry is byte-identical — a teardown clobbers nothing"
    );
    assert_eq!(
        applied.mcp_files,
        vec![codex.to_string_lossy().into_owned()]
    );
    assert_eq!(
        applied.mcp_drifted,
        vec![cursor.to_string_lossy().into_owned()]
    );
}

// =================================================================================================
// The PUBLISH half — the kind on the WAL and on the wire, and no gate in front of it
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

/// Publish `name` through the session lane, capturing the wire bodies.
fn publish_through(
    ctx: &Ctx<'_>,
    lane: &ShareLane,
    publishes: &RecordingPublish,
    name: &str,
) -> Result<ops::PublishOutcome, ClientError> {
    ops::publish(
        ctx,
        Some(&connect(lane, publishes)),
        None,
        name,
        false,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
}

/// **A CONNECTED SERVER HAS NO FILES, SO THERE IS NOTHING TO PUBLISH — and the verb says that
/// rather than sending an empty candidate to be told so.** `publish` ships a bundle's placed
/// files; the durable kind marker in front of the verb already says this bundle places none, so
/// the refusal is the whole outcome and nothing reaches the wire. Reaching for a work tree first
/// would fail as "your own state is unreadable", which is a sentence about the wrong thing.
#[test]
fn a_connected_servers_publish_refuses_by_kind_and_sends_nothing() {
    let rig = Rig::new("pub-kind");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    let dir = rig.work.0.join("weather");
    write_server_folder(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();
    let mut data = ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&ctx, &mut data, &scope.target, &dir).unwrap();
    // The durable marker is the ONE thing publish classifies on, so this is what makes the bundle
    // a server as far as every later reader is concerned.
    mark_as_server(&ctx, data.skill_id.as_deref().unwrap());

    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();
    let err = publish_through(&ctx, &lane, &publishes, "weather").unwrap_err();
    let said = err.to_string();
    assert!(
        said.contains("is an MCP server bundle") && said.contains("`publish` ships"),
        "the refusal names what publish acts on and what this bundle is: {said}"
    );
    assert!(
        publishes.seen.lock().unwrap().is_empty(),
        "a refusal is the whole outcome — nothing was sent"
    );
    // …and the row is exactly where it was: a refused publish transfers no governance.
    let text = rig.global_text();
    assert!(
        text.contains(&dir.display().to_string()),
        "the path row stands: {text}"
    );
}

/// Re-mark a record as the connected-server kind. The marker is written once and never rewritten,
/// so the fixture replaces it — what is under test is what a kind does to the WIRE, not which door
/// wrote the marker.
fn mark_as_server(ctx: &Ctx<'_>, skill_id: &str) {
    let sid = crate::id::SkillId::parse(skill_id).unwrap();
    std::fs::remove_file(ctx.layout.published(&sid).kind).unwrap();
    crate::bundle_kind::write_kind_marker(ctx, &sid, crate::bundle_kind::BundleKind::Mcp);
}

/// An ordinary SKILL publish is untouched by any of this: no kind on the wire, no kind on the
/// receipt — an absent tag reads as `"skill"` by the request's own default.
#[test]
fn an_ordinary_skill_publish_carries_no_kind() {
    let rig = Rig::new("pub-skill");
    rig.seed_session();
    rig.write_global("schema = 1\n");
    let dir = rig.work.0.join("deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let mut data = ops::add(&ctx, &dir).unwrap();
    let scope = crate::ops::add_scope(&ctx, true).unwrap();
    crate::ops::note_added_path_in(&ctx, &mut data, &scope.target, &dir).unwrap();

    let lane = ShareLane::default();
    let publishes = RecordingPublish::default();
    let outcome = publish_through(&ctx, &lane, &publishes, "deploy").unwrap();
    let ops::PublishOutcome::Published(published) = outcome else {
        panic!("the publish LANDED");
    };
    assert_eq!(published.kind, None);
    assert_eq!(publishes.seen.lock().unwrap()[0].kind, None);
}
