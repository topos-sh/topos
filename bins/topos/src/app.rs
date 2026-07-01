//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use serde::Serialize;
use topos_harness::{ClaudeCode, ConfigStore, HarnessAdapter, OpenClaw};
use topos_types::HarnessId;

use crate::cli::{Cli, Command, RoleArg};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, RealFs};
use crate::ids::{Clock, RealClock, RealIds};
use crate::plane::{ContributeSource, EnrollSource, GovernanceSource, PlaneSource};
use crate::plane_http::{FileFollow, SkillCred, UreqEnroll, UreqPlane};
use crate::sidecar::{Layout, recover};
use crate::{enroll, identity, ops, render};

/// Run the CLI; returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    let command = cli.command;
    let cmd_name = command.name();

    let fs = RealFs;
    let ids = RealIds;
    let clock = RealClock;
    let layout = Layout::new(&resolve_home());
    // The harness adapter, selected through the one dispatch seam. v0 wires Claude Code only; the
    // adapter touches its config home only when adopting a recognized skill, arming currency, or on
    // uninstall.
    let harness = adapter_for(HarnessId::ClaudeCode, &fs);

    // Recovery runs at the start of every command (it also abandons an expired, never-redeemed
    // enrollment WAL against the real wall clock).
    let now_millis = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
    if let Err(e) = recover(&fs, &layout, now_millis) {
        return emit_err(json, cmd_name, &e);
    }

    // The plane + follow-state sources. When enrollment has been written (`instance.json` present AND
    // `follows.json` has at least one followed skill), wire the REAL `ureq` transport + the on-disk follow
    // state + the pinned plane key; otherwise keep the INERT pair (production stays a truthful no-op until
    // enrollment writes those docs — nothing writes them yet). The inert ZSTs and the loaded enrollment are
    // both stack locals so the `&dyn` trait objects below borrow correctly for the rest of `run`.
    let inert_plane = crate::plane::InertPlane;
    let inert_follow = crate::plane::InertFollow;
    let enrollment = match load_enrollment(&fs, &layout) {
        Ok(e) => e,
        Err(e) => return emit_err(json, cmd_name, &e),
    };
    let (plane, follow, plane_key): (
        &dyn crate::plane::PlaneSource,
        &dyn crate::plane::FollowSource,
        [u8; 32],
    ) = match &enrollment {
        Some(e) => (&e.plane, &e.follow, e.plane_key),
        None => (&inert_plane, &inert_follow, [0u8; 32]),
    };

    // `add` and `pull` author commits (adoption / a draft snapshot before a divergence), so both load
    // (and on first use, mint) the device identity — `uninstall` must never create or require it before
    // tearing the home down.
    let device_id = match &command {
        // `follow` also loads (and on first use mints) the device identity: the device-key signer it drives
        // requires `host.json` to exist, and `--approve` authors a draft snapshot through the pull engine.
        // `publish` authors the candidate commit and `revert` the forward-revert commit, so both load (and
        // on first use mint) the device id.
        Command::Add { .. }
        | Command::Pull { .. }
        | Command::Follow { .. }
        | Command::Publish { .. }
        | Command::Revert { .. } => match identity::load_or_create_device_id(&fs, &layout) {
            Ok(d) => d,
            Err(e) => return emit_err(json, cmd_name, &e),
        },
        _ => String::new(),
    };
    let ctx = Ctx {
        fs: &fs,
        ids: &ids,
        clock: &clock,
        device_id,
        layout,
        harness: harness.as_ref(),
        plane,
        plane_key,
        follow,
    };

    match command {
        Command::Add { path } => finish(json, cmd_name, ops::add(&ctx, &path), render::add_tty),
        Command::Follow {
            link,
            manual,
            resume,
            approve,
        } => {
            // The transports are built per-base-URL (known only after the op parses the link / reads the
            // WAL): a creds-free `ureq` enroll client + the read transport for the offer disclosure.
            let enroll_connect = |base_url: &str| -> Box<dyn EnrollSource> {
                Box::new(UreqEnroll::new(base_url.to_owned()))
            };
            let plane_connect =
                |base_url: &str, creds: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
                    Box::new(UreqPlane::new(base_url.to_owned(), creds))
                };
            let connectors = ops::FollowConnectors {
                enroll: &enroll_connect,
                plane: &plane_connect,
            };
            let opts = ops::FollowOpts {
                manual,
                resume,
                approve,
            };
            finish_follow(json, cmd_name, ops::follow(&ctx, &connectors, link, opts))
        }
        Command::Unfollow { skill } => finish(
            json,
            cmd_name,
            ops::unfollow(&ctx, &skill),
            render::unfollow_tty,
        ),
        Command::Invite {
            emails,
            role,
            skills,
        } => {
            // The owner's governance-write transport, built per the enrolled plane's base URL (read inside
            // the op from `instance.json`) — the same creds-free `ureq` client that speaks enrollment.
            let gov_connect = |base_url: &str| -> Box<dyn GovernanceSource> {
                Box::new(UreqEnroll::new(base_url.to_owned()))
            };
            finish(
                json,
                cmd_name,
                ops::invite(
                    &ctx,
                    &gov_connect,
                    emails,
                    role.map(RoleArg::to_workspace_role),
                    skills,
                ),
                render::invite_tty,
            )
        }
        Command::List { skill, footprint } => finish(
            json,
            cmd_name,
            ops::list(&ctx, skill.as_deref(), footprint),
            render::list_tty,
        ),
        Command::Diff { skill, r#ref } => finish(
            json,
            cmd_name,
            ops::diff(&ctx, &skill, r#ref.as_deref()),
            render::diff_tty,
        ),
        Command::Publish {
            skill,
            propose,
            approve,
        } => {
            // The device-signed contribute transport, built per the enrolled plane's base URL (read inside
            // the op from `instance.json`) — the same creds-free `ureq` client that speaks enrollment. The
            // governance connector is the same client (a genesis publish folds in an owner-signed invite).
            let contribute_connect = |base_url: &str| -> Box<dyn ContributeSource> {
                Box::new(UreqEnroll::new(base_url.to_owned()))
            };
            let gov_connect = |base_url: &str| -> Box<dyn GovernanceSource> {
                Box::new(UreqEnroll::new(base_url.to_owned()))
            };
            finish_publish(
                json,
                cmd_name,
                ops::publish(
                    &ctx,
                    &contribute_connect,
                    &gov_connect,
                    skill.as_deref(),
                    propose,
                    &approve,
                ),
            )
        }
        Command::Review {
            target,
            approve,
            reject,
        } => {
            // Exactly one of --approve / --reject (clap leaves both bool; the verdict is mutually exclusive).
            let resolved = match (approve, reject) {
                (true, false) => true,
                (false, true) => false,
                _ => {
                    return emit_err(
                        json,
                        cmd_name,
                        &ClientError::Corrupt(
                            "exactly one of --approve / --reject is required".into(),
                        ),
                    );
                }
            };
            let contribute_connect = |base_url: &str| -> Box<dyn ContributeSource> {
                Box::new(UreqEnroll::new(base_url.to_owned()))
            };
            finish(
                json,
                cmd_name,
                ops::review(&ctx, &contribute_connect, &target, resolved),
                render::review_tty,
            )
        }
        Command::Revert {
            skill,
            to,
            approve,
            confirm,
        } => {
            let contribute_connect = |base_url: &str| -> Box<dyn ContributeSource> {
                Box::new(UreqEnroll::new(base_url.to_owned()))
            };
            finish(
                json,
                cmd_name,
                ops::revert(
                    &ctx,
                    &contribute_connect,
                    skill.as_deref(),
                    &to,
                    &approve,
                    confirm,
                ),
                render::revert_tty,
            )
        }
        Command::Log { skill } => finish(json, cmd_name, ops::log(&ctx, &skill), render::log_tty),
        Command::Pull {
            skill,
            onto_current,
            quiet,
        } => {
            let result =
                build_pull_scope(skill, onto_current).and_then(|scope| ops::pull(&ctx, scope));
            if quiet {
                // Byte-silent stdout: the session-start hook injects stdout into the session context, so a
                // clean sweep emits nothing. An error surfaces on stderr with a non-zero exit — never on
                // stdout, even under `--json` (which `--quiet` overrides for the hook path).
                match result {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("{}", render::err_tty(&e));
                        ExitCode::FAILURE
                    }
                }
            } else {
                finish(json, cmd_name, result, render::pull_tty)
            }
        }
        Command::Uninstall { footprint } => {
            let binary = std::env::current_exe().ok();
            finish(
                json,
                cmd_name,
                ops::uninstall(&ctx, footprint, binary.as_deref()),
                render::uninstall_tty,
            )
        }
    }
}

fn finish<T: Serialize>(
    json: bool,
    command: &str,
    result: Result<T, ClientError>,
    tty: impl Fn(&T) -> String,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e),
    }
}

/// `follow`'s finisher — like [`finish`], but it carries the success-path `next_actions` (run
/// `follow --resume` while pending; `pull` once offers are disclosed) on the envelope.
fn finish_follow(
    json: bool,
    command: &str,
    result: Result<topos_types::results::FollowData, ClientError>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::follow_next_actions(&data);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::follow_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e),
    }
}

/// `publish`'s finisher — the verb yields either a direct publish ([`PublishData`]) or an opened proposal
/// ([`ProposeData`]); each renders through its own `--json` payload / TTY line. A typed failure
/// (APPROVAL_REQUIRED / CONFLICT / DENIED / …) flows through [`emit_err`], which attaches the right
/// `next_actions`.
fn finish_publish(
    json: bool,
    command: &str,
    result: Result<ops::PublishOutcome, ClientError>,
) -> ExitCode {
    match result {
        Ok(ops::PublishOutcome::Published(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::publish_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::PublishOutcome::Proposed(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::propose_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e),
    }
}

fn emit_err(json: bool, command: &str, err: &ClientError) -> ExitCode {
    if json {
        println!("{}", render::to_json(&render::err_envelope(command, err)));
    } else {
        eprintln!("{}", render::err_tty(err));
    }
    ExitCode::FAILURE
}

/// Parse the optional `pull` target into a [`ops::PullScope`]: absent = the sweep; `<name>` = accept a
/// pending update; `<name>@<hash>` = go back to that version's bytes; `--onto-current` = the escape.
///
/// A go-back suffix is recognized only when the part after the LAST `@` is a valid 64-char lowercase-hex
/// version id; otherwise the whole argument is the skill name. So a skill whose name contains `@` (e.g.
/// `team@cli`) is accepted as a name, and `team@cli@<hash>` still goes back correctly.
///
/// `--onto-current` (the escape) requires a `<skill>` target and is mutually exclusive with `@<hash>`.
fn build_pull_scope(
    skill: Option<String>,
    onto_current: bool,
) -> Result<ops::PullScope, ClientError> {
    let Some(arg) = skill else {
        if onto_current {
            return Err(ClientError::PlacementUnsupported {
                reason: "--onto-current requires a <skill> target".into(),
            });
        }
        return Ok(ops::PullScope::AllFollowed);
    };
    if let Some((name, suffix)) = arg.rsplit_once('@')
        && let Ok(hash) = ops::parse_hex32(suffix)
    {
        if onto_current {
            return Err(ClientError::PlacementUnsupported {
                reason: "--onto-current is not valid with @<hash>".into(),
            });
        }
        return Ok(ops::PullScope::One {
            name: name.to_owned(),
            mode: ops::TargetMode::GoBack(hash),
        });
    }
    Ok(ops::PullScope::One {
        name: arg,
        mode: if onto_current {
            ops::TargetMode::OntoCurrent
        } else {
            ops::TargetMode::AcceptPending
        },
    })
}

/// The real plane wiring, present only when enrollment has been written. Owns the transport + the on-disk
/// follow source so [`run`] can borrow them as `&dyn` trait objects for the lifetime of the command.
struct Enrollment {
    plane: UreqPlane,
    follow: FileFollow,
    plane_key: [u8; 32],
}

/// Load the enrollment docs read-only. Returns `Some` whenever `instance.json` is present — enrollment is
/// what writes it, so its presence IS the enrolled state; `follows.json` is optional (an empty membership
/// door, or every follow since flipped off by `unfollow`). The pinned plane key must stay loaded even with
/// zero active follows: the write verbs (publish/revert/review) verify the OK receipt's signed pointer
/// against it, and an enrolled author with nothing followed is a normal state. The bare `pull` stays an
/// honest no-op either way (the sweep skips a `following == false` entry, and renders "No followed
/// skills." over an empty set). A corrupt / newer-schema doc fails closed (propagated), never silently
/// degraded to inert.
fn load_enrollment(fs: &dyn FsOps, layout: &Layout) -> Result<Option<Enrollment>, ClientError> {
    let Some(instance) = enroll::read_instance(fs, layout)? else {
        return Ok(None);
    };
    let follows = enroll::read_follows(fs, layout)?.unwrap_or_else(|| enroll::Follows {
        schema_version: topos_types::SCHEMA_VERSION,
        follows: Vec::new(),
    });
    let plane_key = ops::parse_hex32(&instance.plane_key).map_err(|_| {
        ClientError::Corrupt("instance.json plane_key is not 32-byte lowercase hex".into())
    })?;
    let plane = UreqPlane::new(instance.base_url, enroll::skill_creds(&follows));
    let follow = FileFollow::new(enroll::follow_contexts(&follows));
    Ok(Some(Enrollment {
        plane,
        follow,
        plane_key,
    }))
}

/// Build the harness adapter for `id`, borrowing the shared config-store seam. Adding a harness is ONE
/// new match arm — no caller change. v0 only ever selects Claude Code (the CLI's one selection site
/// above passes `HarnessId::ClaudeCode`; it resolves its own config home, `$CLAUDE_CONFIG_DIR` else
/// `$HOME/.claude`). The OpenClaw arm serves the test rigs while its concrete config bytes stay
/// provisional behind the pilot readiness probe (the `openclaw` module doc); the Hermes arm lands with
/// its adapter.
fn adapter_for<'a>(id: HarnessId, fs: &'a dyn ConfigStore) -> Box<dyn HarnessAdapter + 'a> {
    match id {
        HarnessId::ClaudeCode => Box::new(ClaudeCode::new(ClaudeCode::resolve_home(), fs)),
        HarnessId::OpenClaw => Box::new(OpenClaw::new(OpenClaw::resolve_home(), fs)),
        HarnessId::Hermes => unreachable!("harness adapter not yet wired for {id:?}"),
    }
}

/// `$TOPOS_HOME`, else `$HOME/.topos` (`./.topos` as a last resort).
fn resolve_home() -> PathBuf {
    if let Some(home) = std::env::var_os("TOPOS_HOME") {
        return PathBuf::from(home);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".topos")
}
