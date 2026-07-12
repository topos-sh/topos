//! `invite [EMAIL]... [--channel <N>]...` — seat emails as invited members of the workspace.
//!
//! An invitation is a ROSTER WRITE: `POST /v1/workspaces/{ws}/invitations` under the workspace Bearer
//! credential seats each email as an `invited` member (recording who invited whom) and optionally
//! pre-places each invitee into channels. There is no invite link and no role — every CLI invitee starts
//! as a member (roles are raised later, on the web); joining is `follow <address>` plus proof of the
//! invited email. Member-level unless the workspace's invite policy restricts inviting to owners.
//!
//! Requires prior enrollment: the plane (`base_url`) and the workspace (`workspace_id`) come from the
//! sidecar `follow` wrote; the acting device rides the transport's workspace **Bearer credential** (the
//! plane resolves the non-revoked registry row → principal → the invite-policy gate). Nothing is signed —
//! the trust level is git/GitHub-level. Emails are folded to the kernel's canonical (ASCII-lowercase)
//! principal form so the roster rows carry one identity per human.
//!
//! Bare `invite` (no emails) is the no-mutation read (address + policy) — a MARKED SEAM until the two-phase
//! describe leg lands.

use topos_types::requests::{InvitationData, InvitationRequest};
use topos_types::results::{InviteDescribeData, InviteReadData};

use super::follow::DirectoryConnect;
use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::GovernanceSource;

/// Builds the governance-write transport for a plane base URL — known only after reading `instance.json`,
/// so it can't be pre-built in the composition root (mirrors `follow`'s enroll connector). Production wires
/// `UreqDeviceClient`; the tests wire a fake (no HTTP).
pub(crate) type GovernanceConnect<'a> = dyn Fn(&str) -> Box<dyn GovernanceSource> + 'a;

/// The seams `invite` needs — the governance connector (the roster POST) and the directory connector
/// (the `/me` read the bare read + describe surface).
pub(crate) struct InviteConnectors<'a> {
    pub governance: &'a GovernanceConnect<'a>,
    pub directory: &'a DirectoryConnect<'a>,
}

/// The verb's outcome — a bare read (no emails), a describe (emails, no `--yes`), or an apply (`--yes`).
#[derive(Debug)]
pub(crate) enum InviteOutcome {
    /// `invite` with no emails — the no-mutation address/policy read.
    Read(InviteReadData),
    /// `invite <email>...` without `--yes` — who gets seated, the pre-placements, the mail-or-paste note.
    Described {
        describe: InviteDescribeData,
        yes_argv: Vec<String>,
    },
    /// `invite <email>... --yes` — the roster write landed.
    Applied(InvitationData),
}

/// Seat `emails` as invited members of the workspace (two-phase), or — with no emails — read the
/// workspace address + invite policy and change nothing.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled (no `instance.json`) or the workspace can't be inferred
/// (no `identity/user.json`); a transport failure otherwise (a policy-DENIED surfaces as
/// [`ClientError::Plane`] — "not authorized").
pub(crate) fn invite(
    ctx: &Ctx<'_>,
    connectors: &InviteConnectors<'_>,
    emails: Vec<String>,
    channels: Vec<String>,
    workspace: Option<&str>,
    yes: bool,
) -> Result<InviteOutcome, ClientError> {
    // Require enrollment: the pinned plane's base URL comes from what `follow` wrote.
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    // Pick the workspace (the invitation's scope) from the enrolled `user.json` memberships:
    // `--workspace <id>` when the install has joined several, else the sole one. `instance.json` carries
    // the plane but no workspace, so a present-instance-but-no-user state is a partial enrollment we guide
    // the user to complete rather than guess at.
    let user = enroll::read_user(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment(
            "could not determine your workspace; complete enrollment with `topos follow` first"
                .into(),
        )
    })?;
    let workspace_id = user
        .resolve_write_workspace(workspace)?
        .workspace_id
        .clone();

    // Bare `invite` (no emails) is the no-mutation read (the workspace address + invite policy): a single
    // `/me` read, nothing sent, nothing changed.
    if emails.is_empty() {
        let me = (connectors.directory)(&instance.base_url).me(&workspace_id)?;
        return Ok(InviteOutcome::Read(InviteReadData {
            address: me.address,
            invite_policy: me.invite_policy,
            changed: false,
        }));
    }

    // Fold the emails to the kernel's canonical (ASCII-lowercase) principal form ONCE, before they reach
    // the wire body / the describe — the plane folds at its parse boundary, so the roster rows carry one
    // identity per human regardless of how the address was typed.
    let emails: Vec<String> = emails
        .iter()
        .map(|e| topos_core::identity::canonical_principal(e))
        .collect();

    // The describe reads `/me` for the address + policy the two-phase surface discloses (nothing mutates).
    if !yes {
        let me = (connectors.directory)(&instance.base_url).me(&workspace_id)?;
        let mut yes_argv = vec!["topos".to_owned(), "invite".to_owned()];
        yes_argv.extend(emails.iter().cloned());
        for c in &channels {
            yes_argv.push("--channel".to_owned());
            yes_argv.push(c.clone());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(InviteOutcome::Described {
            describe: InviteDescribeData {
                address: me.address,
                invite_policy: me.invite_policy,
                seat: emails,
                channels,
            },
            yes_argv,
        });
    }

    // ---- APPLY (`--yes`) ----
    // POST the invitation under the workspace Bearer credential (the transport looks it up by
    // `workspace_id`; the plane resolves the credential's registry row → principal → the invite-policy
    // gate). The workspace id rides the URL path; the body carries only the emails + channel pre-placements.
    let body = InvitationRequest { emails, channels };
    let transport = (connectors.governance)(&instance.base_url);
    Ok(InviteOutcome::Applied(
        transport.invite(&workspace_id, body)?,
    ))
}
