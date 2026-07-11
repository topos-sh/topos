//! The unauthenticated invite-bootstrap payload — what `GET /i/{token}` returns BEFORE enrollment.
//!
//! A device reads this the instant it opens an `/i/<token>` link, *before* it has any credential: it
//! carries the workspace identity, the offered skills, the enrollment posture, and the plane's API base
//! URL to dial. It carries **no trust root to pin** — a `current` pointer is unsigned, its authority the
//! database row and its integrity the content-addressed version id. It carries **no bytes and no role**
//! (the role lives server-side on the pre-seeded member rows), and `first_receive_auto_land` is
//! **always false** — a received skill is offered, never silently landed.
//!
//! Field names are snake_case as written. These are **deserialization shapes** only (no logic); the route
//! maps the authority's `InviteBootstrap` domain struct into this at the edge.

use serde::{Deserialize, Serialize};

/// The payload an `/i/<token>` invite link resolves to — read once, before enrollment, to learn the
/// workspace + the plane API base to dial. No bytes, no role.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct BootstrapData {
    /// Always `1` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The invite's non-secret descriptor (consent posture, expiry).
    pub invite: BootstrapInvite,
    /// The plane the workspace lives on — its API base and enrollment method.
    pub plane: BootstrapPlane,
    /// The workspace the invite is for.
    pub workspace: BootstrapWorkspace,
    /// The skills the invite pre-offers (a name may be absent → the client shows the id). Empty for a
    /// membership-only door.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offered_skills: Vec<BootstrapSkill>,
}

/// The invite's non-secret descriptor — the consent posture a redeemer agrees to, never a role.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct BootstrapInvite {
    /// A stable, non-secret reference for this invite (the link token the client used to reach the
    /// payload). Empty for an admin-claim bootstrap — a claim token is a live one-time bearer capability
    /// the server never echoes into a body; the claim client uses the token it parsed from the link.
    pub token_id: String,
    /// The invite's expiry as an RFC-3339 string, if it expires (`None` = never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The consent posture every first-receive follows.
    pub consent: ConsentMode,
    /// Whether a first-received skill auto-lands. **ALWAYS false** — a received skill is offered, never
    /// silently applied (a direct human yes is required).
    pub first_receive_auto_land: bool,
}

/// The consent posture of an invite — a CLOSED set. v0 is a single mode: a received skill is disclosed and
/// awaits a direct human yes on first receive (never auto-landed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub enum ConsentMode {
    /// First receive re-discloses the bytes and awaits a direct human yes (TOFU) — never auto-landed.
    #[serde(rename = "direct_human_first_receive")]
    DirectHumanFirstReceive,
}

/// The plane a workspace lives on — its public base URL, deployment posture, and offered enrollment method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct BootstrapPlane {
    /// The plane's public API base URL — the root the client dials for enrollment and sync after this
    /// bootstrap read. Always the plane itself, never a web front: a share link may ride another host,
    /// but the client re-roots onto this value the moment the bootstrap is fetched.
    pub base_url: String,
    /// This plane's deployment posture.
    pub deployment_mode: DeploymentMode,
    /// The enrollment method advertised to a bootstrapping device (e.g. `"device_code"` / `"passcode"`).
    pub enrollment_method: String,
}

/// A plane's deployment posture — a CLOSED set. `cloud` requires a confirmed identity step at enrollment;
/// `self_host` grants membership straight from a valid grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    /// The hosted plane: enrollment requires a confirmed, already-rostered identity.
    Cloud,
    /// A self-hosted plane: enrollment grants membership from a valid grant (no human identity step).
    SelfHost,
}

/// The workspace an invite is for — its id, display name, and domain-verification state. No role (the role
/// lives server-side on the pre-seeded member rows).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct BootstrapWorkspace {
    /// The workspace id.
    pub workspace_id: String,
    /// The workspace display name (shown on the verification page + in the agent's disclosure).
    pub display_name: String,
    /// The org-domain claim, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_domain: Option<String>,
    /// The domain-verification state.
    pub verified_domain_status: VerifiedDomainStatus,
}

/// A workspace's domain-verification state — snake_case on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum VerifiedDomainStatus {
    /// No domain claim, or unverified.
    Unverified,
    /// A claim is pending verification.
    Pending,
    /// The domain is verified.
    Verified,
}

/// One skill an invite pre-offers — its id, plus an optional display name (names ride the invite; a missing
/// name means the client shows the id).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct BootstrapSkill {
    /// The offered skill id.
    pub skill_id: String,
    /// The skill's display name, if the invite carried one (else the client shows the id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_mode_serializes_to_the_pinned_literal() {
        assert_eq!(
            serde_json::to_string(&ConsentMode::DirectHumanFirstReceive).unwrap(),
            "\"direct_human_first_receive\""
        );
        assert_eq!(
            serde_json::from_str::<ConsentMode>("\"direct_human_first_receive\"").unwrap(),
            ConsentMode::DirectHumanFirstReceive
        );
    }

    #[test]
    fn deployment_mode_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&DeploymentMode::SelfHost).unwrap(),
            "\"self_host\""
        );
        assert_eq!(
            serde_json::to_string(&DeploymentMode::Cloud).unwrap(),
            "\"cloud\""
        );
    }

    #[test]
    fn verified_domain_status_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&VerifiedDomainStatus::Unverified).unwrap(),
            "\"unverified\""
        );
        assert_eq!(
            serde_json::from_str::<VerifiedDomainStatus>("\"verified\"").unwrap(),
            VerifiedDomainStatus::Verified
        );
    }

    #[test]
    fn bootstrap_round_trips_snake_case_with_no_role_and_auto_land_false() {
        let data = BootstrapData {
            schema_version: 1,
            invite: BootstrapInvite {
                token_id: "tok-abc".to_owned(),
                expires_at: None,
                consent: ConsentMode::DirectHumanFirstReceive,
                first_receive_auto_land: false,
            },
            plane: BootstrapPlane {
                base_url: "https://plane.test".to_owned(),
                deployment_mode: DeploymentMode::Cloud,
                enrollment_method: "passcode".to_owned(),
            },
            workspace: BootstrapWorkspace {
                workspace_id: "w_acme".to_owned(),
                display_name: "Acme".to_owned(),
                verified_domain: None,
                verified_domain_status: VerifiedDomainStatus::Unverified,
            },
            offered_skills: vec![BootstrapSkill {
                skill_id: "s_deploy".to_owned(),
                name: Some("Deploy".to_owned()),
            }],
        };
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["workspace"]["workspace_id"], "w_acme");
        assert_eq!(v["plane"]["deployment_mode"], "cloud");
        // No trust root to pin — the plane block carries only base_url + posture + enrollment method.
        assert!(v["plane"].get("enrollment_method").is_some());
        assert_eq!(v["invite"]["consent"], "direct_human_first_receive");
        assert_eq!(v["invite"]["first_receive_auto_land"], false);
        // No role anywhere in the bootstrap — the role lives server-side.
        assert!(v.get("role").is_none());
        assert!(v["invite"].get("role").is_none());
        assert!(v["workspace"].get("role").is_none());
        let back: BootstrapData = serde_json::from_value(v).unwrap();
        assert_eq!(back.offered_skills[0].skill_id, "s_deploy");
    }
}
