//! Workspace-standup wrappers — the leak-free [`PlaneState`] surface for the PRIVILEGED genesis ops.
//!
//! Deliberately LIB-ONLY (there is no OSS HTTP route for any of these): the bin's `mint-claim`
//! subcommand and a downstream composition's authenticated admin routes call them. Like
//! [`restore_cmd`](crate::restore_cmd), every signature carries only plain/owned types — ids are `&str`,
//! outcomes are owned enums/structs, faults are stringified — so a composing plane never names a
//! `plane_store` type. Each wrapper parses the plane's deployment mode STRICTLY (a mode string the
//! constructor could only warn-fallback refuses here, fail closed) and threads it into the authority op:
//! the mode a genesis writes is always the plane's OWN, never a request's.

use plane_store::{
    ApproveStandupOutcome, AuthorityError, CreateWorkspaceOutcome, DeploymentMode,
    MintClaimOutcome, WorkspaceId,
};

use crate::state::PlaneState;
use crate::wire;

/// The outcome of [`PlaneState::create_workspace`] — a created (or idempotently replayed) workspace, or a
/// typed denial. Plain owned fields only.
#[derive(Debug, Clone)]
pub enum CreateWorkspaceSummary {
    /// A fresh workspace was created; `invite_link` is the owner's paste-to-agent `/i/` link.
    Created {
        /// The server-minted workspace id.
        workspace_id: String,
        /// The stored display name.
        display_name: String,
        /// The owner's self-invite link (`<base_url>/i/<token>`, deterministic per request).
        invite_link: String,
    },
    /// The SAME request already created a workspace — the identical result, replayed.
    Replayed {
        /// The workspace the original request created.
        workspace_id: String,
        /// The stored display name.
        display_name: String,
        /// The identical self-invite link, re-derived.
        invite_link: String,
    },
    /// The request was denied (the per-owner cap, a reused request id, a bad email).
    Denied {
        /// The static, typed reason.
        reason: String,
    },
}

/// The outcome of [`PlaneState::approve_standup`]. `NotFound` is the UNIFORM miss (an unknown/expired/raced
/// code, a different email's re-approve, an enroll-intent session) — the caller must render it
/// indistinguishably.
#[derive(Debug, Clone)]
pub enum ApproveStandupSummary {
    /// The session was approved: the workspace exists and the agent's next poll is granted.
    Approved {
        /// The fresh workspace's id.
        workspace_id: String,
        /// The stored display name.
        display_name: String,
    },
    /// The SAME email already approved this session (an idempotent re-click).
    AlreadyApproved {
        /// The workspace the earlier approval created.
        workspace_id: String,
    },
    /// The approval was denied (the per-owner workspace cap).
    Denied {
        /// The static, typed reason.
        reason: String,
    },
    /// The uniform miss.
    NotFound,
}

/// The outcome of [`PlaneState::approve_session`] — the member/owner web-approve leg over an ENROLL
/// session, with the first-writer-wins semantics surfaced: a re-approve by the SAME email is an idempotent
/// `Confirmed`; a different email — or any dead/unknown session — is the uniform `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproveSessionSummary {
    /// The session's identity is confirmed (the device's next poll yields a grant).
    Confirmed,
    /// The uniform miss (unknown/expired code, a standup session, or a different email's session).
    NotFound,
}

impl PlaneState {
    /// The plane's deployment mode, parsed STRICTLY at construction — the genesis wrappers refuse to run
    /// off the constructor's warn-fallback (an operator typo must not decide what mode a workspace is born
    /// with).
    fn strict_mode(&self) -> anyhow::Result<DeploymentMode> {
        self.enroll().strict_deployment_mode.ok_or_else(|| {
            anyhow::anyhow!(
                "the plane mode is not a recognized value; set TOPOS_PLANE_MODE to 'cloud' or 'self_host' \
                 before running workspace-standup operations"
            )
        })
    }

    /// Mint a one-time admin-claim link for a workspace that does not exist yet, returning the full
    /// `<base_url>/i/<token>` link — ONCE. The token is a bearer OWNER capability: the caller prints or
    /// delivers it and must never log it (this wrapper and the authority never do). On a cloud-mode plane
    /// `owner_email` is required. `ttl_secs` bounds the FIRST redeem (a consumed claim's same-device replay
    /// still answers after expiry — lost-200 recovery).
    ///
    /// # Errors
    /// A typed refusal (existing workspace, missing/invalid owner email, unparseable plane mode) or a
    /// stringified authority fault.
    pub async fn mint_admin_claim(
        &self,
        workspace_id: &str,
        display_name: Option<&str>,
        owner_email: Option<&str>,
        ttl_secs: u64,
    ) -> anyhow::Result<String> {
        let mode = self.strict_mode()?;
        let ws = WorkspaceId::parse(workspace_id)
            .map_err(|error| anyhow::anyhow!("invalid workspace id `{workspace_id}`: {error}"))?;
        let ttl_ms = i64::try_from(ttl_secs.saturating_mul(1000))
            .map_err(|_| anyhow::anyhow!("ttl too large"))?;
        let (created_at, now) = wire::now_utc();
        let outcome = self
            .authority()
            .mint_admin_claim(
                &ws,
                display_name,
                owner_email,
                mode,
                ttl_ms,
                now,
                &created_at,
            )
            .await
            .map_err(|error| anyhow::anyhow!("minting the claim: {error}"))?;
        match outcome {
            MintClaimOutcome::Minted(minted) => {
                Ok(format!("{}/i/{}", self.enroll().base_url, minted.token))
            }
            MintClaimOutcome::Denied(reason) => Err(anyhow::anyhow!("{reason}")),
        }
    }

    /// Create a workspace for an ALREADY-VERIFIED owner email (door 2 — the composing web surface proves
    /// the email; this wrapper never does). Idempotent per `request_id` (same request + same owner replays
    /// the same workspace and link). `display_name = None` takes the server default.
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault; a protocol-level
    /// refusal is the typed [`CreateWorkspaceSummary::Denied`], not an error.
    pub async fn create_workspace(
        &self,
        request_id: &str,
        display_name: Option<&str>,
        owner_email: &str,
    ) -> anyhow::Result<CreateWorkspaceSummary> {
        let mode = self.strict_mode()?;
        let (created_at, _now) = wire::now_utc();
        let outcome = self
            .authority()
            .create_workspace(request_id, display_name, owner_email, mode, &created_at)
            .await
            .map_err(|error| anyhow::anyhow!("creating the workspace: {error}"))?;
        let link = |token: &str| format!("{}/i/{token}", self.enroll().base_url);
        Ok(match outcome {
            CreateWorkspaceOutcome::Created(c) => CreateWorkspaceSummary::Created {
                workspace_id: c.workspace_id.as_str().to_owned(),
                display_name: c.display_name,
                invite_link: link(&c.invite_token),
            },
            CreateWorkspaceOutcome::Replayed(c) => CreateWorkspaceSummary::Replayed {
                workspace_id: c.workspace_id.as_str().to_owned(),
                display_name: c.display_name,
                invite_link: link(&c.invite_token),
            },
            CreateWorkspaceOutcome::Denied(reason) => CreateWorkspaceSummary::Denied {
                reason: reason.to_owned(),
            },
        })
    }

    /// Approve a STANDUP session with an ALREADY-VERIFIED email (door 1's human leg). The session CAS is
    /// the idempotency; every indistinguishable miss is [`ApproveStandupSummary::NotFound`].
    ///
    /// # Errors
    /// An unparseable plane mode (typed, fail closed) or a stringified authority fault.
    pub async fn approve_standup(
        &self,
        user_code: &str,
        verified_email: &str,
        display_name: Option<&str>,
    ) -> anyhow::Result<ApproveStandupSummary> {
        let mode = self.strict_mode()?;
        let (created_at, now) = wire::now_utc();
        let outcome = self
            .authority()
            .approve_standup(
                user_code,
                verified_email,
                display_name,
                mode,
                now,
                &created_at,
            )
            .await;
        Ok(match outcome {
            Ok(ApproveStandupOutcome::Approved {
                workspace_id,
                display_name,
            }) => ApproveStandupSummary::Approved {
                workspace_id: workspace_id.as_str().to_owned(),
                display_name,
            },
            Ok(ApproveStandupOutcome::AlreadyApproved { workspace_id }) => {
                ApproveStandupSummary::AlreadyApproved {
                    workspace_id: workspace_id.as_str().to_owned(),
                }
            }
            Ok(ApproveStandupOutcome::Denied(reason)) => ApproveStandupSummary::Denied {
                reason: reason.to_owned(),
            },
            Err(AuthorityError::NotFound) => ApproveStandupSummary::NotFound,
            Err(error) => return Err(anyhow::anyhow!("approving the standup session: {error}")),
        })
    }

    /// Approve an ENROLL session with an ALREADY-VERIFIED email — the member/owner web-approve leg (a
    /// leak-free wrapper over the external-identity confirm, with the first-writer-wins semantics
    /// surfaced: same-email re-approve ⇒ idempotent `Confirmed`; different email / dead session ⇒ the
    /// uniform `NotFound`).
    ///
    /// # Errors
    /// A stringified authority fault (never a protocol miss — that is the typed `NotFound`).
    pub async fn approve_session(
        &self,
        user_code: &str,
        verified_email: &str,
    ) -> anyhow::Result<ApproveSessionSummary> {
        let now = wire::now_utc().1;
        match self
            .authority()
            .confirm_external_identity(user_code, verified_email, now)
            .await
        {
            Ok(_confirmed) => Ok(ApproveSessionSummary::Confirmed),
            Err(AuthorityError::NotFound) => Ok(ApproveSessionSummary::NotFound),
            Err(error) => Err(anyhow::anyhow!("approving the session: {error}")),
        }
    }
}
