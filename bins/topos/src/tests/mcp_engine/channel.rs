//! A CHANNEL narrows its two kinds SEPARATELY. A channel is the one row whose members can be of
//! both kinds, so one array could never speak for them: `dest` names placement FOLDERS for its
//! skill members, `mcp_dest` names CONFIG FILES for its mcp members, and neither says anything
//! about the other's kind.

use std::path::PathBuf;

use topos_types::requests::{
    WireChannelEntry, WireChannelSkill, WireMcpIndexEntry, WireSkillIndexEntry,
};

use crate::config_custody::ScopeEntries;
use crate::ctx::Ctx;
use crate::ops;
use crate::sessions::Session;

use super::rig::*;

/// A `tools` channel carrying exactly these members — of BOTH kinds, because a channel is the one
/// row whose members can be either, and the catalog answers for them in two lists.
fn channel_of(skills: Vec<WireSkillIndexEntry>, servers: Vec<WireMcpIndexEntry>) -> FakeDirectory {
    let members = skills
        .iter()
        .map(|e| (e.skill_id.clone(), e.name.clone()))
        .chain(servers.iter().map(|e| (e.skill_id.clone(), e.name.clone())))
        .map(|(skill_id, name)| WireChannelSkill { skill_id, name })
        .collect();
    FakeDirectory {
        channels: vec![WireChannelEntry {
            name: "tools".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: members,
        }],
        skills,
        revisions: servers.clone(),
        servers,
    }
}

/// A checkout whose committed manifest holds `body`, PICKING the four project-capable agents so
/// every project MCP surface engages hermetically (they are checkout-relative; a picked agent
/// needs no detect dir).
fn project_with(rig: &Rig, tag: &str, body: &str) -> Scratch {
    let proj = Scratch::new(tag);
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), body).unwrap();
    rig.project_pick(&proj.0, &["claude-code", "cursor", "codex", "opencode"]);
    proj
}

/// The skill + server pair every channel test below expands — ids deliberately unequal to their
/// display names, so nothing can pass while confusing one for the other.
fn skill_and_server() -> (Version, ServedServer) {
    (
        mk_version(&[("SKILL.md", b"# deploy\n")]),
        served_at("https://mcp.example/a"),
    )
}

/// **D6 — `dest` places the skills, and a channel with no `mcp_dest` narrows nothing.** The
/// channel's `dest` freezes where its SKILL members land; its MCP member is untouched by that
/// array and reaches every MCP-capable agent, which is what "no narrowing" has always meant. A
/// healthy delivery says nothing about either.
#[test]
fn a_channel_dest_places_its_skills_while_its_servers_reach_every_agent() {
    let rig = Rig::new("ch-dest");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    // PROJECT scope: every MCP surface is checkout-relative there, so "every agent" is hermetic.
    let proj = project_with(
        &rig,
        "ch-dest-co",
        &format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[channels]\ntools = {{ dest = [\"prompts/skills\"] }}\n"
        ),
    );
    let (sk, sv) = skill_and_server();
    let plane = FakePlane::new().with_version("s_dep", &sk);
    let dir = channel_of(
        vec![skill_catalog_entry("s_dep", "deploy", &sk)],
        vec![mcp_catalog_entry("s_a", "alpha", &sv)],
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);

    // The SKILL member is frozen to the folder the channel names, and to nowhere else.
    assert!(
        proj.0.join("prompts/skills/deploy/SKILL.md").exists(),
        "{:?} / {:?}",
        out.data.skills,
        out.warnings
    );
    assert!(
        !proj.0.join(".claude/skills/deploy").exists(),
        "the default project dirs get nothing — the channel froze its skills' destination"
    );
    // The MCP member reached every project surface: the channel's `dest` said nothing about it.
    for rel in [
        ".codex/config.toml",
        ".cursor/mcp.json",
        ".opencode/opencode.json",
    ] {
        let text =
            std::fs::read_to_string(proj.0.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(text.contains("topos-eng-alpha"), "{rel}: {text}");
    }
    // Claude Code's is the checkout's own slot in its machine file, and nothing in the checkout.
    let claude = claude_project_servers(&rig.home.0, &proj.0);
    assert!(claude.contains("topos-eng-alpha"), "{claude}");
    assert!(
        !proj.0.join(".mcp.json").exists(),
        "nothing in the repo root"
    );
    assert!(
        !proj.0.join("prompts/skills/alpha").exists(),
        "a server is never placed as a folder"
    );
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .chain(crate::message::legacy_lines(&out.advisories).into_iter())
            .any(|w| w.contains("MCP_DEST")),
        "a healthy channel is silent: {:?} / {:?}",
        out.warnings,
        out.advisories
    );
    assert!(out.failed_bundles.is_empty(), "{:?}", out.failed_bundles);
}

/// **F5 — a channel that narrows a server to NOTHING is a failure, said out loud.** The old code
/// asked for this narrowing with warnings switched off, so one typo delivered the server nowhere
/// while the receipt read healthy. It is the ordinary one-bundle-one-bucket outcome now: nothing
/// is placed, the bundle is counted failed under its IDENTITY, its success row stands down, and
/// the line names the channel it came through plus both ways out. The channel's SKILL members are
/// untouched — the arrays are separate, and so are their fates.
#[test]
fn a_channels_typoed_mcp_dest_fails_the_server_and_names_the_channel() {
    let rig = Rig::new("ch-zero");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let (sk, sv) = skill_and_server();
    let plane = FakePlane::new().with_version("s_dep", &sk);
    let dir = channel_of(
        vec![skill_catalog_entry("s_dep", "deploy", &sk)],
        vec![mcp_catalog_entry("s_a", "alpha", &sv)],
    );
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/tools\" = \
         {{ mcp_dest = [\"~/.cursor/mcp.jsonn\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // Fail-closed: not one config file was written, detected agents and all.
    assert!(!rig.home.0.join(".cursor/mcp.json").exists());
    assert!(!rig.home.0.join(".openclaw/openclaw.json").exists());

    // The line, verbatim — one code, the channel as provenance INSIDE it.
    let loud: Vec<String> = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .filter(|w| w.contains("MCP_DEST_NO_AGENT"))
        .collect();
    assert_eq!(loud.len(), 1, "{:?}", out.warnings);
    const CHANNEL_LINE: &str = "\"alpha\" reaches no agent: the mcp_dest on the \"tools\" line in \
                                your topos.toml names no MCP config file. Add one to that \
                                mcp_dest, or drop mcp_dest so the bundle reaches every MCP-capable \
                                agent.";
    assert_eq!(loud[0], format!("MCP_DEST_NO_AGENT {CHANNEL_LINE}"));
    // …and the TTY prints the TEXT — the same sentence, with no code in front of it.
    let typed: Vec<&topos_types::Message> = out
        .warnings
        .iter()
        .filter(|w| w.code.as_deref() == Some("MCP_DEST_NO_AGENT"))
        .collect();
    assert_eq!(typed.len(), 1, "{:?}", out.warnings);
    assert_eq!(typed[0].text, CHANNEL_LINE);
    assert!(
        !crate::message::legacy_lines(&out.advisories)
            .into_iter()
            .any(|w| w.contains("MCP_DEST")),
        "a server reaching nothing is never filed as an advisory: {:?}",
        out.advisories
    );

    // ONE BUNDLE, ONE BUCKET: the tally counts the SERVER under its identity (the run exits
    // non-zero on exactly this), and the row that would have read `installed` stands down.
    assert_eq!(
        out.failed_bundles,
        [("person".to_owned(), "s_a".to_owned())]
            .into_iter()
            .collect(),
        "the skill member is no part of this: {:?}",
        out.failed_bundles
    );
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "alpha"),
        "the server's success row stood down: {:?}",
        out.data.skills
    );
    // The SKILL member delivered, and keeps its own row.
    assert!(
        out.data.skills.iter().any(|s| s.skill == "deploy"),
        "{:?}",
        out.data.skills
    );
    // The fix is a file edit, so no argv-shaped action is offered — the sentence IS the remedy.
    assert!(out.fault_actions.is_empty(), "{:?}", out.fault_actions);

    // …AND THE DEEP DIVE SAYS IT TOO. `topos list alpha` is where a person goes after reading
    // that warning, and it answered as if nothing were wrong: the channel's `mcp_dest` was never
    // resolved for its members, so the row that reaches nobody read like a healthy install.
    let list = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("alpha".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let detail = list.data.detail.clone().unwrap();
    assert!(
        detail
            .mcp_unreachable
            .as_deref()
            .is_some_and(|w| w.contains("~/.cursor/mcp.jsonn")),
        "the member carries the channel's own zero-reach: {detail:?}"
    );
    let text = crate::render::list_tty(&list);
    assert!(
        text.contains("an MCP server bundle that reaches no agent"),
        "{text}"
    );
    // The channel's SKILL member says nothing of the sort — `mcp_dest` is not about it.
    let skill_dive = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("deploy".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    assert_eq!(
        skill_dive.data.detail.clone().unwrap().mcp_unreachable,
        None
    );
}

/// **A channel's `mcp_dest` that maps SOME of its entries still delivers, and stays quiet.** A
/// bundle row's unmapped entry earns an advisory because that row is one machine's own choice; a
/// channel is curated for a whole team, so an entry aimed at an agent this particular machine
/// never installed is ordinary — not news.
#[test]
fn a_channels_partly_mapping_mcp_dest_delivers_narrowed_with_no_advisory() {
    let rig = Rig::new("ch-partial");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let (_, sv) = skill_and_server();
    let plane = FakePlane::new();
    let dir = channel_of(Vec::new(), vec![mcp_catalog_entry("s_a", "alpha", &sv)]);
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/tools\" = \
         {{ mcp_dest = [\"~/.cursor/mcp.json\", \"~/.typo/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert!(
        std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json"))
            .unwrap()
            .contains("topos-eng-alpha"),
        "the entries that DID map still deliver"
    );
    assert!(
        !rig.home.0.join(".openclaw/openclaw.json").exists(),
        "the narrowing held: openclaw was not named"
    );
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .chain(crate::message::legacy_lines(&out.advisories).into_iter())
            .any(|w| w.contains("MCP_DEST")),
        "no advisory is owed on a channel: {:?} / {:?}",
        out.warnings,
        out.advisories
    );
    assert!(out.failed_bundles.is_empty(), "{:?}", out.failed_bundles);
}

/// **A channel's `dest` is a FOLDER even when it is spelled like a config file — and its servers
/// never read it.** This is the whole point of the split: before it, fixing a server by adding a
/// config path to `dest` moved the channel's skills into a directory of that name. The skill
/// members still take it as a folder (that is what `dest` means to them); the server ignores it
/// and reaches every MCP-capable agent, none of which is the file `dest` happens to name.
#[test]
fn a_config_file_path_in_a_channels_dest_is_a_folder_to_skills_and_nothing_to_servers() {
    let rig = Rig::new("ch-crossed");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    // `.openclaw/openclaw.json` is a real MCP config spelling — and openclaw has no PROJECT
    // surface, so nothing in this checkout could ever read it as one.
    let proj = project_with(
        &rig,
        "ch-crossed-co",
        &format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[channels]\ntools = \
             {{ dest = [\".openclaw/openclaw.json\"] }}\n"
        ),
    );
    let (sk, sv) = skill_and_server();
    let plane = FakePlane::new().with_version("s_dep", &sk);
    let dir = channel_of(
        vec![skill_catalog_entry("s_dep", "deploy", &sk)],
        vec![mcp_catalog_entry("s_a", "alpha", &sv)],
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);

    // To the SKILL member it is a directory, and its copy is inside it.
    let as_folder = proj.0.join(".openclaw/openclaw.json");
    assert!(
        as_folder.join("deploy/SKILL.md").exists(),
        "{:?} / {:?}",
        out.data.skills,
        out.warnings
    );
    assert!(as_folder.is_dir(), "never a config file topos wrote");
    // To the SERVER it is nothing at all: it reached every project surface, unnarrowed.
    for rel in [
        ".codex/config.toml",
        ".cursor/mcp.json",
        ".opencode/opencode.json",
    ] {
        let text =
            std::fs::read_to_string(proj.0.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(text.contains("topos-eng-alpha"), "{rel}: {text}");
    }
    // Claude Code's is the checkout's own slot in its machine file, and nothing in the checkout.
    let claude = claude_project_servers(&rig.home.0, &proj.0);
    assert!(claude.contains("topos-eng-alpha"), "{claude}");
    assert!(
        !proj.0.join(".mcp.json").exists(),
        "nothing in the repo root"
    );
    assert!(out.failed_bundles.is_empty(), "{:?}", out.failed_bundles);
}

/// **One line per (scope, bundle identity, channel).** The dedupe is keyed on the IDENTITY for the
/// same reason the failed tally is: two servers narrowed to nothing by one channel are two facts
/// about two bundles, and a key that collapsed them would leave a server reaching nobody with
/// nothing said about it. Both are counted, both are named, neither is said twice.
#[test]
fn each_server_a_channel_strands_is_named_once_and_counted_once() {
    let rig = Rig::new("ch-dedupe");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let va = served_at("https://mcp.example/a");
    let vb = served_at("https://mcp.example/b");
    let plane = FakePlane::new();
    let dir = channel_of(
        Vec::new(),
        vec![
            mcp_catalog_entry("s_a", "alpha", &va),
            mcp_catalog_entry("s_b", "beta", &vb),
        ],
    );
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/tools\" = \
         {{ mcp_dest = [\"~/.typo/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    let loud: Vec<String> = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .filter(|w| w.contains("MCP_DEST_NO_AGENT"))
        .collect();
    assert_eq!(loud.len(), 2, "{:?}", out.warnings);
    for name in ["alpha", "beta"] {
        assert_eq!(
            loud.iter()
                .filter(|w| {
                    w.contains(&format!("\"{name}\" reaches no agent"))
                        && w.contains("the mcp_dest on the \"tools\" line")
                })
                .count(),
            1,
            "{name}: {loud:?}"
        );
    }
    assert_eq!(
        out.failed_bundles,
        [
            ("person".to_owned(), "s_a".to_owned()),
            ("person".to_owned(), "s_b".to_owned())
        ]
        .into_iter()
        .collect()
    );
    assert!(
        out.data.skills.is_empty(),
        "both success rows stood down: {:?}",
        out.data.skills
    );
}

/// **A MIXED CHANNEL SPLITS PER KIND.** Removing one member replaces the curated line with its
/// survivors' own rows, and each survivor takes the destinations written in ITS vocabulary: the
/// skill members the line's `dest` (placement folders), the mcp members its `mcp_dest` (config
/// files) as the row `dest` a member row spells them in. One shared value handed the server the
/// channel's skill folders and the very next reconcile refused it — a removal that cost an
/// untouched bundle every agent.
#[test]
fn a_mixed_channel_split_gives_each_survivor_its_own_kinds_destinations() {
    use crate::manifest::document::{EntryValue, ManifestScope, parse_manifest};
    let rig = Rig::new("ch-split");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let (sk, sv) = skill_and_server();
    let plane = FakePlane::new().with_version("s_dep", &sk);
    let dir = channel_of(
        vec![skill_catalog_entry("s_dep", "deploy", &sk)],
        vec![mcp_catalog_entry("s_a", "alpha", &sv)],
    );
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/tools\" = \
         {{ dest = [\"~/.claude/skills\"], mcp_dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // One healthy delivery first — it is what records each member's kind.
    let first = sweep(&ctx, &plane, &dir);
    assert!(first.failed_bundles.is_empty(), "{:?}", first.warnings);

    // The describe says which array went to which kind, in its own clause.
    let described = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    match described {
        ops::RemoveOutcome::Described { data, .. } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(
                note.contains(
                    "its mcp_dest settings carry onto each new mcp row as that row's dest"
                ),
                "{note}"
            );
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)), "{out:?}");

    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = parse_manifest(&text, ManifestScope::Global).unwrap();
    let survivor = doc
        .rows
        .iter()
        .find(|r| r.reference == format!("{HOST}/{WS_NAME}/alpha"))
        .unwrap_or_else(|| panic!("the server survives with its own row: {text}"));
    match &survivor.value {
        EntryValue::Fields(f) => assert_eq!(
            f.dest.as_deref(),
            Some(&["~/.cursor/mcp.json".to_owned()][..]),
            "the CHANNEL's mcp_dest rides the mcp survivor's row: {text}"
        ),
        other => panic!("the mcp survivor carries its config file: {other:?}: {text}"),
    }
    // …and the row reconciles clean: reach resolves, nothing fails, the entry is still there.
    let after = sweep(&ctx, &plane, &dir);
    assert!(
        after.failed_bundles.is_empty(),
        "{:?} / {:?}",
        after.failed_bundles,
        after.warnings
    );
    let cursor = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap();
    assert!(cursor.contains("topos-eng-alpha"), "{cursor}");
}

/// `mcp_dest` is the CHANNEL's word alone — a bundle row's own kind already says which thing its
/// `dest` means, so the field refuses there exactly as any other unknown key on that shape does:
/// by naming what the shape takes.
#[test]
fn mcp_dest_on_a_bundle_row_refuses_like_any_other_illegal_field() {
    use crate::manifest::document::{ManifestScope, parse_manifest};
    let refusal = |text: &str| {
        parse_manifest(text, ManifestScope::Global)
            .expect_err("mcp_dest does not fit a bundle row")
            .message
    };
    let workspace = refusal(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ mcp_dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    assert_eq!(
        workspace,
        "`mcp_dest` does not fit a workspace bundle — \
         `acme.test/eng/alpha` takes `version`, `dest`, `name`"
    );
    let local = refusal(
        "[mcp]\nx = { path = \"~/servers/x\", kind = \"mcp\", mcp_dest = [\"~/.cursor/mcp.json\"] }\n",
    );
    assert_eq!(
        local,
        "`mcp_dest` does not fit a local folder — `x` takes `dest`, `name`, `kind`"
    );
    // …and it IS legal on a channel, beside `dest`.
    parse_manifest(
        &format!(
            "[channels]\n\"{HOST}/{WS_NAME}/tools\" = \
             {{ dest = [\"~/.claude/skills\"], mcp_dest = [\"~/.cursor/mcp.json\"] }}\n"
        ),
        ManifestScope::Global,
    )
    .unwrap();
}

/// A workspace SKILL row whose `dest` names a skills FOLDER — exactly what
/// `add -g @ws/skill -a codex` writes (`dest = ["~/.codex/skills"]`) — is not an MCP demand:
/// no MCP_DEST_UNKNOWN warning, no failure count, and the untouched update reads exactly clean.
#[test]
fn a_skill_rows_folder_dest_never_warns_mcp_dest_unknown() {
    let rig = Rig::new("skilldest");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[("SKILL.md", b"# deploy\n")]);
    let plane = FakePlane::new().with_version("s_dep", &v);
    let dir = FakeDirectory::of_skills(vec![skill_catalog_entry("s_dep", "deploy", &v)]);
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let first = sweep(&ctx, &plane, &dir);
    assert!(
        !crate::message::legacy_lines(&first.warnings)
            .into_iter()
            .chain(crate::message::legacy_lines(&first.advisories))
            .any(|w| w.contains("MCP_DEST_UNKNOWN")),
        "a skill row's folder dest is not an MCP config file: {:?} / {:?}",
        first.warnings,
        first.advisories
    );
    assert!(
        rig.home.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "delivery itself lands"
    );
    // The second sweep is untouched — and wholly clean: one skill, up to date, nothing failed.
    let clean = sweep(&ctx, &plane, &dir);
    assert!(
        !crate::message::legacy_lines(&clean.warnings)
            .into_iter()
            .chain(crate::message::legacy_lines(&clean.advisories))
            .any(|w| w.contains("MCP_DEST_UNKNOWN")),
        "{:?} / {:?}",
        clean.warnings,
        clean.advisories
    );
    assert_eq!(
        crate::render::pull_tty(
            &clean.data,
            &clean.decisions,
            &clean.warnings,
            &clean.advisories,
            &clean.disclosures,
            0,
            clean.unplaced_bundles.len(),
        ),
        "checked machine-wide\nChecked 1 bundle: all up to date."
    );
}

/// F3: a local `kind = "mcp"` row's `server.json` is re-read from disk EVERY sweep — a later edit
/// leaving a header this build refuses to render (secret, variable-substituted, value-less) must
/// be refused at the converge boundary: nothing new placed, the standing entries left as-is
/// (removal only on demand-drop, never on a now-unreadable source).
#[test]
fn a_tampered_local_row_is_held_with_the_typed_refusal_and_prior_entries_stay() {
    let rig = Rig::new("tamper");
    seed_harness_dirs(&rig.home.0);
    let dir = rig.home.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        "{\"name\":\"io.test/w\",\"description\":\"W.\",\"version\":\"1.0.0\",\
         \"remotes\":[{\"type\":\"streamable-http\",\"url\":\"https://w.example/mcp\",\
         \"headers\":[{\"name\":\"X-R\",\"value\":\"eu\"}]}]}",
    )
    .unwrap();
    rig.write_global(&format!(
        "[mcp]\nweather = {{ path = \"{}\", kind = \"mcp\", dest = [\"~/.cursor/mcp.json\"] }}\n",
        dir.display()
    ));
    let plane = FakePlane::new();
    let fdir = FakeDirectory::default();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &fdir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    assert!(placed.contains("https://w.example/mcp"), "{placed}");

    // Each tamper: the sweep warns with the refusal and the sentence naming what it could not
    // read, the config file stays byte-identical (the header never reaches it), and the custody
    // keeps the entry.
    let header = |body: &str| {
        format!(
            "{{\"name\":\"io.test/w\",\"description\":\"W.\",\"version\":\"1.0.0\",\
             \"remotes\":[{{\"type\":\"streamable-http\",\"url\":\"https://w.example/mcp\",\
             \"headers\":[{{\"name\":\"X-R\",{body}}}]}}]}}"
        )
    };
    for (tampered, said) in [
        (
            header("\"isSecret\":true"),
            "its X-R header is marked secret",
        ),
        (
            header("\"value\":\"eu\",\"variables\":{\"region\":{\"default\":\"eu\"}}"),
            "its X-R header carries variable substitutions",
        ),
        (
            header("\"isRequired\":true"),
            "its X-R header carries no literal value",
        ),
    ] {
        std::fs::write(dir.join("server.json"), &tampered).unwrap();
        let out = sweep(&ctx, &plane, &fdir);
        assert!(
            crate::message::legacy_lines(&out.warnings)
                .into_iter()
                .any(|w| w.starts_with("MCP_UNPLACEABLE") && w.contains(said)),
            "{said}: {:?}",
            out.warnings
        );
        let now = std::fs::read_to_string(&cursor).unwrap();
        assert_eq!(now, placed, "{said}: the placed config never moves");
        assert!(
            ScopeEntries::load(&rig.fs, &rig.layout())
                .unwrap()
                .has_entries_for(&crate::config_custody::local_identity(
                    &dir.display().to_string()
                )),
            "{said}: the standing entry is held, not dropped"
        );
        // AND THE SUMMARY AGREES WITH THE STATUS. The refused bundle is a bundle that could not
        // be carried forward, so it is counted as one: the refusal's line alone left the run
        // exiting non-zero while the receipt said "1 already up to date".
        assert_eq!(
            out.failed_bundles.len(),
            1,
            "{said}: the refused bundle is counted: {:?}",
            out.failed_bundles
        );
        let tty = crate::render::pull_tty(
            &out.data,
            &out.decisions,
            &out.warnings,
            &out.advisories,
            &out.disclosures,
            out.failed_bundles.len(),
            out.unplaced_bundles.len(),
        );
        assert!(
            !tty.contains("already up to date"),
            "{said}: a run that placed nothing never claims it was already current: {tty}"
        );
    }
}

/// INVISIBLE AND UNFIXABLE. An MCP bundle whose manifest row is hand-deleted while its config
/// entry carries a hand edit leaves a record nothing demands whose ENTRY is still live in the
/// agent's config — and neither surface said so: `list` builds its inventory from manifest rows,
/// and the sweep's orphan resolution deliberately passes over a record whose entries still stand
/// (they are placed, not abandoned). `update` read "all up to date" while the server sat in
/// Cursor's config. One line in `list` now names it and the command that ends it.
#[test]
fn an_orphaned_record_whose_entries_still_stand_gets_one_line_in_list() {
    let rig = Rig::new("orphan-visible");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let plane = FakePlane::new();
    let fdir = take_server(&rig, "s_wx", "wx");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // The ordinary door: the row delivers, which writes the RECORD and places the entry.
    sweep(&ctx, &plane, &fdir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists(), "the entry landed");

    // The row goes; the ENTRY stays. A hand-edited entry is never destroyed, so its custody row
    // survives the removal — and a record that still holds entries is exactly what the sweep's
    // orphan pass declines to retire.
    let placed = std::fs::read_to_string(&cursor).unwrap();
    std::fs::write(&cursor, placed.replace("wx.example", "edited.example")).unwrap();
    rig.write_global("schema = 1\n");
    let _ = sweep(&ctx, &plane, &fdir);
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("edited.example"),
        "the hand-edited entry survives — that is what makes the record stick"
    );

    let listing = |ctx: &Ctx<'_>| {
        ops::list_with(
            ctx,
            &ops::ListRequest {
                view: ops::ScopeView::Machine,
                ..Default::default()
            },
            None,
            None,
            ops::RowPage::unlimited(),
        )
        .unwrap()
    };

    // THE LINE. One, naming the bundle, where it still stands, and the command that ends it.
    let listed = listing(&ctx);
    let orphans = &listed.data.scopes[0].orphans;
    assert_eq!(orphans.len(), 1, "{orphans:?}");
    assert_eq!(orphans[0].name, "wx");
    assert!(
        orphans[0].standing.iter().any(|p| p.contains("mcp.json")),
        "it names WHERE it still is: {orphans:?}"
    );
    let tty = crate::render::list_tty(&listed);
    assert!(
        tty.contains(
            "wx  no longer in this file — still in ~/.cursor/mcp.json (`topos remove wx` ends it)"
        ),
        "{tty}"
    );

    // AND THE WAY OUT WORKS — the whole point of naming a command. The record goes, and the line
    // goes with it: it reports a state, never a permanent mark.
    let named = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(fdir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::remove(
        &ctx,
        &ops::RemoveConnectors { session: &named },
        &["wx".to_owned()],
        &[],
        None,
        true,
        false,
    )
    .expect("the offered command is runnable");
    assert!(
        listing(&ctx).data.scopes[0].orphans.is_empty(),
        "{:?}",
        listing(&ctx).data.scopes[0].orphans
    );
    // AND IT COSTS NO BYTES OF THEIRS. The entry standing in Cursor's config is one the person
    // edited by hand, so it is theirs now: the record is the whole of what this command may end.
    // A line that offered `remove` and took a hand-written entry with it would be the listing
    // talking somebody into losing their own work.
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("edited.example"),
        "the hand-edited entry survives the command the listing offered"
    );
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("--yes applies");
    };
    let tty = crate::render::remove_applied_tty(&data);
    assert!(tty.contains("wx"), "the receipt names what it ended: {tty}");
}

/// A workspace catalog sharing ONE connected server, plus the machine-wide row that takes it —
/// what mints the RECORD every orphan question below is about (a server only this machine runs is
/// a line in a file, and leaves no record behind to abandon).
fn take_server(rig: &Rig, id: &str, name: &str) -> FakeDirectory {
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/{name}\" = \"latest\"\n"
    ));
    FakeDirectory::of_servers(vec![mcp_catalog_entry(
        id,
        name,
        &served_at(&format!("https://{name}.example/mcp")),
    )])
}

/// A PAGE IS NOT A DEMAND. The orphan pass asks "does anything still demand this record?" and it
/// was asking the row vector the listing had already narrowed and CUT — so `--limit`/`--offset`,
/// which exist only to shorten output, turned every row that fell off the page into a record
/// reported as abandoned, under a line offering `topos remove <name>`. Following that on a
/// perfectly healthy bundle would have retired its config entries. The question is about the
/// scope's whole resolution now, and paging cannot reach it.
#[test]
fn paging_the_rows_never_invents_an_orphan() {
    let rig = Rig::new("orphan-paged");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let plane = FakePlane::new();
    let fdir = FakeDirectory::of_servers(vec![
        mcp_catalog_entry("s_aaa", "aaa", &served_at("https://aaa.example/mcp")),
        mcp_catalog_entry("s_zzz", "zzz", &served_at("https://zzz.example/mcp")),
    ]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/aaa\" = \"latest\"\n\"{HOST}/{WS_NAME}/zzz\" = \"latest\"\n"
    ));
    sweep(&ctx, &plane, &fdir);

    let listing = |page: ops::RowPage| {
        ops::list_with(
            &ctx,
            &ops::ListRequest {
                view: ops::ScopeView::Machine,
                ..Default::default()
            },
            None,
            None,
            page,
        )
        .unwrap()
    };
    // Both rows demanded, both records live: nothing is abandoned at any page size.
    let full = listing(ops::RowPage::unlimited());
    assert_eq!(full.data.scopes[0].rows.len(), 2, "{:?}", full.data.scopes);
    assert!(
        full.data.scopes[0].orphans.is_empty(),
        "{:?}",
        full.data.scopes[0].orphans
    );
    for page in [
        ops::RowPage {
            offset: 0,
            limit: Some(1),
        },
        ops::RowPage {
            offset: 1,
            limit: Some(1),
        },
    ] {
        let listed = listing(page);
        assert_eq!(listed.data.scopes[0].rows.len(), 1, "the page cut one row");
        assert!(
            listed.data.scopes[0].orphans.is_empty(),
            "the row that fell off the page is still demanded, not abandoned: {:?}",
            listed.data.scopes[0].orphans
        );
    }
}

/// A NAME IS NOT A RECORD. Two non-retired records in one scope can carry one display name — a
/// server the workspace has since withdrawn beside the skill that took its name is the everyday
/// case — and the orphan pass suppressed by name, so a healthy `wx` silenced an ABANDONED `wx`
/// whose config entry was sitting live in Cursor with nothing naming it. Suppression keys on the
/// record the row actually resolved.
#[test]
fn a_same_named_healthy_record_never_silences_an_abandoned_one() {
    let rig = Rig::new("orphan-twin");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let server_dir = take_server(&rig, "s_wx_mcp", "wx");
    sweep(&ctx, &FakePlane::new(), &server_dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    std::fs::write(&cursor, placed.replace("wx.example", "edited.example")).unwrap();

    // The server leaves the catalog and its row goes with it — its record is now abandoned with a
    // live, hand-edited entry. A SKILL of the same name is demanded in its place; its row names
    // its OWN record (the delivery carries the id), which is the whole point: one name, two
    // records.
    let v = mk_version(&[("SKILL.md", b"# wx\n")]);
    let plane = FakePlane::new().with_version("s_wx", &v);
    plane.serves(vec![delivered_skill("s_wx", "wx", &v)]);
    let fdir = FakeDirectory::of_skills(vec![skill_catalog_entry("s_wx", "wx", &v)]);
    rig.write_global(&format!("[skills]\n\"{HOST}/{WS_NAME}/wx\" = \"latest\"\n"));
    let _ = sweep(&ctx, &plane, &fdir);

    let listed = ops::list_with(
        &ctx,
        &ops::ListRequest {
            view: ops::ScopeView::Machine,
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let scope = &listed.data.scopes[0];
    assert!(
        scope.rows.iter().any(|r| r.skill == "wx"),
        "the healthy twin is demanded: {:?}",
        scope.rows
    );
    let orphans = &scope.orphans;
    assert_eq!(
        orphans.len(),
        1,
        "the abandoned twin earns its own line: {orphans:?}"
    );
    assert_eq!(orphans[0].name, "wx");
    assert!(
        orphans[0].standing.iter().any(|p| p.contains("mcp.json")),
        "it names the live entry: {orphans:?}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("edited.example"),
        "the hand-edited entry is untouched by a read"
    );
}

/// THE OFFERED COMMAND HAS TO RUN — and hit the record the line was about. A project store's
/// orphan printed `topos remove <name>` like any other, but the classic arm resolved names in the
/// HOME store alone: from inside the checkout the command answered "no such skill", and on a
/// machine that happened to hold a same-named record it described THAT one instead — a listing
/// line about one bundle offering a delete of another. `remove` resolves where you stand now, and
/// runs every per-record read and write against the store that answered.
#[test]
fn a_project_orphans_offered_command_reaches_the_project_record() {
    let rig = Rig::new("orphan-proj");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let proj = project_with(&rig, "orphan-proj-checkout", "schema = 1\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let plane = FakePlane::new();
    let fdir = take_server(&rig, "s_wx", "wx");

    // A MACHINE record of the same name stands beside it — the twin the home-only resolver used
    // to answer with. Its row is dropped so no demand-guard stands between the test and the
    // resolution being proven; the RECORD is what must come through untouched.
    sweep_machine(&ctx, &plane, &fdir);
    let machine_records: Vec<PathBuf> = std::fs::read_dir(rig.layout().skills_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(machine_records.len(), 1, "{machine_records:?}");
    rig.write_global("schema = 1\n");

    // The project takes the same server: the checkout's own store, its own row, its own config
    // entry.
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nwx = \"latest\"\n"),
    )
    .unwrap();
    sweep(&ctx, &plane, &fdir);
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the project delivery minted the checkout's store");
    let cursor = proj.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    std::fs::write(&cursor, placed.replace("wx.example", "edited.example")).unwrap();

    // The project row goes; the hand-edited entry stays, so the record sticks — an orphan.
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "schema = 1\n").unwrap();
    let _ = sweep(&ctx, &plane, &FakeDirectory::default());

    let listing = || {
        ops::list_with(
            &ctx,
            &ops::ListRequest {
                view: ops::ScopeView::Here,
                ..Default::default()
            },
            None,
            None,
            ops::RowPage::unlimited(),
        )
        .unwrap()
    };
    let listed = listing();
    let project_scope = listed
        .data
        .scopes
        .iter()
        .find(|s| s.scope == "project")
        .expect("a project scope");
    assert_eq!(
        project_scope.orphans.len(),
        1,
        "{:?}",
        project_scope.orphans
    );
    assert_eq!(project_scope.orphans[0].name, "wx");
    let tty = crate::render::list_tty(&listed);
    assert!(tty.contains("`topos remove wx` ends it"), "{tty}");

    // RUN EXACTLY WHAT IT OFFERED, standing where the listing was read.
    let named = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(fdir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    ops::remove(
        &ctx,
        &ops::RemoveConnectors { session: &named },
        &["wx".to_owned()],
        &[],
        None,
        true,
        false,
    )
    .expect("the offered command is runnable from inside the checkout");

    // The PROJECT record is what ended.
    let after = listing();
    let project_scope = after
        .data
        .scopes
        .iter()
        .find(|s| s.scope == "project")
        .expect("a project scope");
    assert!(
        project_scope.orphans.is_empty(),
        "{:?}",
        project_scope.orphans
    );
    assert!(
        std::fs::read_dir(playout.skills_dir())
            .map(|d| d.count())
            .unwrap_or(0)
            == 0,
        "the project store's record is gone"
    );
    // And the MACHINE twin — the record the home-only resolver would have hit — still stands.
    assert!(
        machine_records.iter().all(|p| p.is_dir()),
        "the machine record was not the one deleted: {machine_records:?}"
    );
}

/// A repo row tagged `kind = "mcp"` never becomes a demand: the grammar refuses it when the file
/// LOADS, exactly like any other field the shape does not take — so the update refuses with the
/// teaching instead of syncing a row whose bytes no config converge could ever place.
#[test]
fn a_github_sourced_mcp_row_refuses_when_the_manifest_loads() {
    let rig = Rig::new("ghmcp");
    rig.seed_session();
    let plane = FakePlane::new();
    let dir = FakeDirectory::default();
    rig.write_global("[skills]\ntool = { source = \"github:o/r\", kind = \"mcp\" }\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("`github.com/o/r/tool` cannot deliver an MCP server")
            && msg.contains("publish the bundle to a workspace"),
        "{msg}"
    );
}

/// F6: the SAME folder named by a row in BOTH scopes must keep each scope's config key stable.
/// The reconcile resolves a local row's custody identity against THE SCOPE'S OWN store — never
/// the other scope's — because the key was minted under that identity, and a cross-scope answer
/// would retire that key and re-mint `-2` (the one path an entry name could move, orphaning any
/// OAuth token filed under it).
#[test]
fn a_folder_named_in_both_scopes_keeps_each_scopes_config_key_stable() {
    let rig = Rig::new("dual");
    seed_harness_dirs(&rig.home.0);
    let proj = project_with(&rig, "dual-proj", "schema = 1\n");
    let dir = proj.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        "{\"name\":\"io.test/w\",\"description\":\"W.\",\"version\":\"1.0.0\",\
         \"remotes\":[{\"type\":\"streamable-http\",\"url\":\"https://w.example/mcp\"}]}",
    )
    .unwrap();
    // The same folder is a line in the PROJECT's file and in the machine's — two scopes, two
    // custody documents, two keys that must not reach across at each other.
    let row = format!(
        "[mcp]\nweather = {{ path = \"{}\", kind = \"mcp\" }}\n",
        dir.display()
    );
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), &row).unwrap();
    rig.write_global(&row);
    let ctx = rig.ctx_at(Some(&proj.0));
    let plane = FakePlane::new();
    let fdir = FakeDirectory::default();
    sweep_machine(&ctx, &plane, &fdir);
    sweep(&ctx, &plane, &fdir);
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the project row minted the checkout's store");
    let before = ScopeEntries::load(&rig.fs, &playout).unwrap();
    assert_eq!(before.doc.keys.len(), 1, "{before:?}");
    let (proj_bundle, proj_key) = before.doc.keys.iter().next().unwrap();
    let (proj_bundle, proj_key) = (proj_bundle.clone(), proj_key.clone());
    let cursor = proj.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    assert!(placed.contains(&proj_key), "{placed}");

    // The next sweep must resolve the SAME identity the key was minted under: no retire, no
    // `-2` re-mint, the config entry's name never moves.
    sweep(&ctx, &plane, &fdir);
    let after = ScopeEntries::load(&rig.fs, &playout).unwrap();
    assert_eq!(
        after.doc.keys.get(&proj_bundle),
        Some(&proj_key),
        "the project scope's key must survive the sweep: {after:?}"
    );
    assert!(
        after.doc.retired.is_empty(),
        "nothing was retired: {after:?}"
    );
    let now = std::fs::read_to_string(&cursor).unwrap();
    assert!(now.contains(&proj_key), "{now}");
    assert!(
        !now.contains(&format!("{proj_key}-2")),
        "the entry name never moves: {now}"
    );
}

#[test]
fn remove_of_an_mcp_row_converges_inline_and_the_receipt_names_the_removals() {
    let rig = Rig::new("rm");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &s)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("topos-eng-alpha")
    );

    // `remove -g` drops the row AND converges the scope inline — the receipt names the per-agent
    // removals; nothing waits for the next sweep.
    let outcome = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/alpha")],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("applies under --yes");
    };
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("the server's entry was removed.") && note.contains("cursor"),
        "{note}"
    );
    assert!(
        !cursor.exists()
            || !std::fs::read_to_string(&cursor)
                .unwrap()
                .contains("topos-eng-alpha"),
        "the entry is gone now, not at the next sweep"
    );
    let custody = ScopeEntries::load(&rig.fs, &rig.layout()).unwrap();
    assert!(!custody.has_entries_for("s_a"));
    assert_eq!(custody.key_of("s_a"), None, "the key is given up");
    assert!(
        custody.doc.retired.values().any(|b| b == "s_a"),
        "and the name it minted stays reserved until a mint for the same server asks: {custody:?}"
    );
}
