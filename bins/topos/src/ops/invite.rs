//! `invite [EMAIL]... [--skill <N> | --channel <N>]` — invite emails into the workspace.
//!
//! An invitation is an INVITATION-ROW write: `POST /v1/workspaces/{ws}/invitations` under the
//! workspace Bearer credential seats each email as a pending 7-day claim, and the SERVER mails each
//! address its single-use invite link (the token never appears in this exchange — the mailbox is
//! its one channel; the receipt carries the workspace address only). At most ONE optional
//! first-destination hint — `--skill <name>` or `--channel <name>` — rides the invitation: the
//! accept subscribes it (the seat first, then the follow/membership, one transaction server-side),
//! and the invitee's post-enrollment describe targets it. No role field — every CLI invitee starts
//! as a member (roles are raised later, on the web). Owner-only: only a workspace owner may send
//! (and revoke) invitations.
//!
//! Requires prior enrollment: the plane (`base_url`) and the workspace (`workspace_id`) come from the
//! sidecar `follow` wrote; the acting device rides the transport's ONE **Bearer credential** (the
//! server resolves credential → device → user → the owner gate). Nothing is signed —
//! the trust level is git/GitHub-level. Emails are folded to the canonical (ASCII-lowercase)
//! form so the roster rows carry one identity per human.
//!
//! Bare `invite` (no emails) is the no-mutation read (the workspace address) — a MARKED SEAM until
//! the two-phase describe leg lands.

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
    /// `invite` with no emails — the no-mutation address read.
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
/// workspace address and change nothing.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled (no `instance.json`) or the workspace can't be inferred
/// (no `identity/user.json`); a transport failure otherwise (a policy-DENIED surfaces as
/// [`ClientError::Plane`] — "not authorized").
pub(crate) fn invite(
    ctx: &Ctx<'_>,
    connectors: &InviteConnectors<'_>,
    emails: Vec<String>,
    skill: Option<String>,
    channel: Option<String>,
    workspace: Option<&str>,
    yes: bool,
) -> Result<InviteOutcome, ClientError> {
    if skill.is_some() && channel.is_some() {
        return Err(ClientError::InvalidArgument(
            "an invitation carries at most one first destination — `--skill` OR `--channel`".into(),
        ));
    }
    // Require enrollment: the pinned plane's base URL comes from what `follow` wrote.
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or(ClientError::NotEnrolled)?;
    // Pick the workspace (the invitation's scope) from the enrolled `user.json` memberships:
    // `--workspace` (name or id) when the install has joined several, else the sole one. `instance.json` carries
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

    // Bare `invite` (no emails) is the no-mutation read (the workspace address): a single `/me`
    // read, nothing sent, nothing changed.
    if emails.is_empty() {
        let me = (connectors.directory)(&instance.base_url).me(&workspace_id)?;
        return Ok(InviteOutcome::Read(InviteReadData {
            address: me.address,
            changed: false,
        }));
    }

    // Fold the emails to the canonical (ASCII-lowercase) form ONCE, before they reach the wire body /
    // the describe — the server folds at its parse boundary, so the roster rows carry one identity per
    // human regardless of how the address was typed.
    let emails: Vec<String> = emails
        .iter()
        .map(|e| enroll::canonical_principal(e))
        .collect();

    // The describe reads `/me` for the address the two-phase surface discloses (nothing mutates).
    if !yes {
        let me = (connectors.directory)(&instance.base_url).me(&workspace_id)?;
        let mut yes_argv = vec!["topos".to_owned(), "invite".to_owned()];
        yes_argv.extend(emails.iter().cloned());
        if let Some(s) = &skill {
            yes_argv.push("--skill".to_owned());
            yes_argv.push(s.clone());
        }
        if let Some(c) = &channel {
            yes_argv.push("--channel".to_owned());
            yes_argv.push(c.clone());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(InviteOutcome::Described {
            describe: InviteDescribeData {
                address: me.address,
                seat: emails,
                skill,
                channel,
            },
            yes_argv,
        });
    }

    // ---- APPLY (`--yes`) ----
    // POST the invitation under the workspace Bearer credential (the transport looks it up by
    // `workspace_id`; the plane resolves the credential's registry row → principal → the
    // owner gate). The workspace id rides the URL path; the body carries the emails + the
    // optional hint.
    let body = InvitationRequest {
        emails,
        skill,
        channel,
    };
    let transport = (connectors.governance)(&instance.base_url);
    Ok(InviteOutcome::Applied(
        transport.invite(&workspace_id, body)?,
    ))
}
