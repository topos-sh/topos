//! `invite` — an OWNER mints an `/i/<token>` invite link by POSTing the governance Invite op.
//!
//! Requires prior enrollment: the plane (`base_url`), the workspace (`workspace_id`), and the device key
//! all come from the sidecar `follow` wrote. The request names the acting `device_key_id`; the plane
//! resolves the non-revoked registry row for it → its principal → the role matrix (a non-owner is DENIED).
//! Nothing is signed — the trust level is git/GitHub-level. The role rides the wire body; emails are folded
//! to the kernel's canonical (ASCII-lowercase) principal form so the roster rows carry one identity per
//! human; the skill **ids** (never names) are what the invite pre-offers.

use topos_types::requests::{InviteRequest, InviteSkill, WorkspaceRole};
use topos_types::results::InviteData;

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::GovernanceSource;

/// Builds the owner's governance-write transport for a plane base URL — known only after reading
/// `instance.json`, so it can't be pre-built in the composition root (mirrors `follow`'s enroll connector).
/// Production wires `UreqDeviceClient`; the tests wire a fake (no HTTP).
pub(crate) type GovernanceConnect<'a> = dyn Fn(&str) -> Box<dyn GovernanceSource> + 'a;

/// Validate every `--skills` token at the argv boundary with the client id rules (`crate::id` — the same
/// lowercase path-safe charset every other id boundary enforces, and the rule the plane's own parse
/// applies). Failing here names the bad token BEFORE anything is signed or sent; a token carrying
/// non-printable or non-ASCII bytes is described, never echoed.
fn validate_skill_tokens(skills: &[String]) -> Result<(), ClientError> {
    for token in skills {
        if crate::id::is_valid_id(token) {
            continue;
        }
        let shown = if token.is_empty() {
            "an empty token".to_owned()
        } else if token.chars().all(|c| c.is_ascii_graphic()) {
            format!("`{token}`")
        } else {
            "a token containing non-printable or non-ASCII characters".to_owned()
        };
        return Err(ClientError::InvalidArgument(format!(
            "--skills takes skill ids (lowercase, [a-z0-9_-], at most 128 bytes); {shown} is not one"
        )));
    }
    Ok(())
}

/// Mint an `/i/<token>` invite: sign the governance Invite op with this device's key, then POST it.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a malformed `--skills` token (refused at the argv boundary);
/// [`ClientError::Enrollment`] if not enrolled (no `instance.json`) or the workspace can't be inferred (no
/// `identity/user.json`); a signing / transport failure otherwise (a role-DENIED surfaces as
/// [`ClientError::Plane`] — "not authorized").
pub(crate) fn invite(
    ctx: &Ctx<'_>,
    connect: &GovernanceConnect<'_>,
    emails: Vec<String>,
    role: Option<WorkspaceRole>,
    skills: Vec<String>,
    workspace: Option<&str>,
) -> Result<InviteData, ClientError> {
    // The argv boundary: refuse a malformed skill id before contacting anything.
    validate_skill_tokens(&skills)?;
    // Fold the emails to the kernel's canonical (ASCII-lowercase) principal form ONCE, before they reach
    // the wire body — the plane folds at its parse boundary, so the roster rows carry one identity per human
    // regardless of how the address was typed.
    let emails: Vec<String> = emails
        .iter()
        .map(|e| topos_core::identity::canonical_principal(e))
        .collect();
    // Require enrollment: the pinned plane's base URL comes from what `follow` wrote.
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    // Pick the workspace (the governance frame's scope) from the enrolled `user.json` memberships:
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

    // The device key — its id is the one the plane resolves to the acting registry row (its principal + role
    // authorize the invite).
    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;

    // The client-minted idempotency key: the canonical hyphenated UUID rides the wire (the plane's
    // `parse_op_id` parses it back to the SAME 16 bytes, so a lost-ack retry replays the deterministic
    // link + receipt).
    let op_id = uuid::Uuid::from_bytes(ctx.ids.new_op_id())
        .as_hyphenated()
        .to_string();

    // POST the op (naming the acting `device_key_id`). The role rides the body as a `WorkspaceRole` (omitted
    // ⇒ the plane defaults it to member). The link never carries a role — it is a property of the seeded
    // roster row. `name: None` — only the skill id pre-offers.
    let body = InviteRequest {
        workspace_id,
        op_id,
        device_key_id: signer.device_key_id().to_owned(),
        emails,
        role,
        skills: skills
            .into_iter()
            .map(|skill_id| InviteSkill {
                skill_id,
                name: None,
            })
            .collect(),
    };
    let transport = connect(&instance.base_url);
    transport.create_invite(body)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
    use topos_types::bootstrap::{DeploymentMode, VerifiedDomainStatus};
    use topos_types::requests::{InviteRequest, WorkspaceRole};
    use topos_types::results::InviteData;
    use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

    use super::{GovernanceConnect, invite};
    use crate::ctx::Ctx;
    use crate::device_signer::DeviceSigner;
    use crate::enroll;
    use crate::error::ClientError;
    use crate::fs_seam::RealFs;
    use crate::ids::test_sources::{FixedClock, SeqIds};
    use crate::plane::{FollowSource, GovernanceSource, InertFollow, InertPlane, PlaneSource};
    use crate::sidecar::Layout;

    const WS: &str = "w_acme";
    const BASE_URL: &str = "https://acme.topos.test";

    // ---- scratch ----

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-inv-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---- a null harness (invite never touches placement/currency) ----

    struct NullHarness;
    impl HarnessAdapter for NullHarness {
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
            _d: Option<&DiscoveredPlacement>,
        ) -> PlacementTarget {
            PlacementTarget {
                dir: PathBuf::from("/nonexistent").join(skill_id),
            }
        }
        fn currency_kind(&self) -> CurrencyKind {
            CurrencyKind::ExplicitPullOnly
        }
        fn install_currency_trigger(&self) -> TriggerReport {
            no_trigger()
        }
        fn remove_currency_trigger(&self) -> TriggerReport {
            no_trigger()
        }
        fn uninstall_footprint(&self) -> Vec<PathBuf> {
            Vec::new()
        }
    }
    fn no_trigger() -> TriggerReport {
        TriggerReport {
            harness: HarnessId::ClaudeCode,
            currency_kind: CurrencyKind::ExplicitPullOnly,
            touched_path: None,
            marker_id: "test".into(),
            state: TriggerState::Inactive,
        }
    }

    // ---- the fake governance transport: captures the request, returns a canned outcome ----

    /// One captured POST: the wire body (the request names the acting `device_key_id`; nothing is signed).
    type CapturedInvite = InviteRequest;
    type Captured = Rc<RefCell<Option<CapturedInvite>>>;
    /// A `run_invite` outcome: the op result + whatever reached the transport (`None` if it never did).
    type InviteRun = (Result<InviteData, ClientError>, Option<CapturedInvite>);

    #[derive(Clone)]
    enum Resp {
        Ok(InviteData),
        /// An already-mapped DENIED outcome (the real envelope mapping is unit-tested in `plane_http`).
        Denied(String),
    }

    #[derive(Clone)]
    struct FakeGov {
        captured: Captured,
        resp: Resp,
    }
    impl GovernanceSource for FakeGov {
        fn create_invite(&self, body: InviteRequest) -> Result<InviteData, ClientError> {
            *self.captured.borrow_mut() = Some(body);
            match &self.resp {
                Resp::Ok(data) => Ok(data.clone()),
                Resp::Denied(code) => Err(ClientError::Plane(format!("invite refused ({code})"))),
            }
        }
    }

    // ---- rig ----

    struct Rig {
        home: Scratch,
        fs: RealFs,
        ids: SeqIds,
        clock: FixedClock,
        harness: NullHarness,
    }
    impl Rig {
        fn new(tag: &str) -> Self {
            Self {
                home: Scratch::new(tag),
                fs: RealFs,
                ids: SeqIds::new("s"),
                clock: FixedClock(1),
                harness: NullHarness,
            }
        }
        fn layout(&self) -> Layout {
            Layout::new(&self.home.0)
        }
        /// Seed the enrolled state the op requires: `instance.json` (the plane) + `user.json` (the workspace).
        fn seed_enrolled(&self) {
            enroll::write_instance(
                &self.fs,
                &self.layout(),
                &enroll::Instance {
                    schema_version: 1,
                    base_url: BASE_URL.to_owned(),
                    deployment_mode: DeploymentMode::Cloud,
                    enrollment_method: "device_code".to_owned(),
                },
            )
            .unwrap();
            enroll::write_user(
                &self.fs,
                &self.layout(),
                &enroll::UserDoc {
                    schema_version: 1,
                    email: None,
                    principal: None,
                    workspaces: vec![enroll::Membership {
                        workspace_id: WS.to_owned(),
                        display_name: Some("Acme".to_owned()),
                        roles: Vec::new(),
                        verified_domain: None,
                        verified_domain_status: VerifiedDomainStatus::Unverified,
                        invite_rooted: true,
                        enrolled_at: 1,
                    }],
                },
            )
            .unwrap();
        }
        fn ctx<'a>(&'a self, plane: &'a dyn PlaneSource, follow: &'a dyn FollowSource) -> Ctx<'a> {
            Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: String::new(),
                layout: self.layout(),
                harness: &self.harness,
                plane,
                follow,
            }
        }
    }

    /// Run the `invite` op over the fake transport, returning the op result + the captured wire body.
    fn run_invite(
        rig: &Rig,
        resp: Resp,
        emails: &[&str],
        role: Option<WorkspaceRole>,
        skills: &[&str],
    ) -> InviteRun {
        let captured: Captured = Rc::new(RefCell::new(None));
        let fake = FakeGov {
            captured: captured.clone(),
            resp,
        };
        let connect: Box<GovernanceConnect> =
            Box::new(move |_b: &str| -> Box<dyn GovernanceSource> { Box::new(fake.clone()) });
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = rig.ctx(&inert_p, &inert_f);
        let out = invite(
            &ctx,
            &*connect,
            emails.iter().map(|s| (*s).to_owned()).collect(),
            role,
            skills.iter().map(|s| (*s).to_owned()).collect(),
            None,
        );
        let cap = captured.borrow().clone();
        (out, cap)
    }

    fn rig_device_key_id(rig: &Rig) -> String {
        DeviceSigner::load_or_generate(&rig.fs, &rig.layout())
            .unwrap()
            .device_key_id()
            .to_owned()
    }

    // ---- tests ----

    #[test]
    fn posts_the_expected_wire_body_naming_the_acting_device_key() {
        // The op POSTs a body naming the enrolled workspace, the acting `device_key_id` (the plane resolves
        // its registry row → principal → role matrix), the chosen role, the canonical hyphenated op_id, and
        // the skill ids. Nothing is signed.
        let rig = Rig::new("wire-body");
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(InviteData {
                invite_link: format!("{BASE_URL}/i/tok_abc"),
                roster_added: vec!["alice@acme.com".to_owned()],
                skills: vec!["s_deploy".to_owned()],
            }),
            &["alice@acme.com"],
            Some(WorkspaceRole::Reviewer),
            &["s_review", "s_deploy"],
        );
        out.expect("the op POSTs the invite");
        let body = cap.expect("the op reached the transport");
        // The op_id is the canonical hyphenated form (the plane's `parse_op_id` requires it).
        let uuid = uuid::Uuid::parse_str(&body.op_id).expect("op_id is a UUID");
        assert_eq!(uuid.as_hyphenated().to_string(), body.op_id);
        // Scope + acting device + role ride the body.
        assert_eq!(body.workspace_id, WS);
        assert_eq!(body.device_key_id, rig_device_key_id(&rig));
        assert_eq!(body.role, Some(WorkspaceRole::Reviewer));
        let skill_ids: Vec<&str> = body.skills.iter().map(|s| s.skill_id.as_str()).collect();
        assert_eq!(skill_ids, vec!["s_review", "s_deploy"]);
    }

    #[test]
    fn ok_envelope_maps_to_invite_data() {
        let rig = Rig::new("ok");
        rig.seed_enrolled();
        let (out, _cap) = run_invite(
            &rig,
            Resp::Ok(InviteData {
                invite_link: format!("{BASE_URL}/i/tok_xyz"),
                roster_added: vec!["alice@acme.com".to_owned()],
                skills: vec!["s_deploy".to_owned()],
            }),
            &["alice@acme.com"],
            Some(WorkspaceRole::Member),
            &["s_deploy"],
        );
        let data = out.expect("ok");
        assert_eq!(data.invite_link, format!("{BASE_URL}/i/tok_xyz"));
        assert_eq!(data.roster_added, vec!["alice@acme.com".to_owned()]);
        assert_eq!(data.skills, vec!["s_deploy".to_owned()]);
    }

    #[test]
    fn a_denied_outcome_is_a_typed_error() {
        let rig = Rig::new("denied");
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            &rig,
            Resp::Denied("NOT_AUTHORIZED".to_owned()),
            &["alice@acme.com"],
            Some(WorkspaceRole::Owner),
            &[],
        );
        // The op still POSTed (the deny is the plane's authority verdict, surfaced as a typed error).
        assert!(cap.is_some(), "the op reached the transport");
        let err = out.unwrap_err();
        match err {
            ClientError::Plane(m) => assert!(m.contains("NOT_AUTHORIZED"), "got {m}"),
            other => panic!("expected a typed Plane error, got {other:?}"),
        }
    }

    #[test]
    fn invite_without_enrollment_is_run_follow_first() {
        // No instance.json seeded — the op refuses before signing or contacting any transport.
        let rig = Rig::new("not-enrolled");
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(InviteData {
                invite_link: String::new(),
                roster_added: Vec::new(),
                skills: Vec::new(),
            }),
            &["alice@acme.com"],
            None,
            &[],
        );
        assert!(
            cap.is_none(),
            "a not-enrolled invite never reaches the transport"
        );
        match out.unwrap_err() {
            ClientError::Enrollment(m) => assert!(m.contains("follow"), "got {m}"),
            other => panic!("expected an Enrollment error, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_skills_token_is_refused_at_the_argv_boundary() {
        let rig = Rig::new("bad-skill-token");
        rig.seed_enrolled();
        // A mixed-case token: the client refuses with the id rule BEFORE signing or POSTing (the plane's
        // own parse enforces the same lowercase charset).
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(InviteData {
                invite_link: String::new(),
                roster_added: Vec::new(),
                skills: Vec::new(),
            }),
            &["alice@acme.com"],
            None,
            &["S_Deploy"],
        );
        assert!(cap.is_none(), "a refused token never reaches the transport");
        match out.unwrap_err() {
            ClientError::InvalidArgument(m) => {
                assert!(m.contains("S_Deploy"), "an ASCII token is named: {m}");
                assert!(m.contains("--skills"), "the flag is named: {m}");
            }
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }

        // A non-ASCII token is DESCRIBED, never echoed.
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(InviteData {
                invite_link: String::new(),
                roster_added: Vec::new(),
                skills: Vec::new(),
            }),
            &["alice@acme.com"],
            None,
            &["s_dëploy"],
        );
        assert!(cap.is_none());
        match out.unwrap_err() {
            ClientError::InvalidArgument(m) => {
                assert!(!m.contains("dëploy"), "never echo non-ASCII bytes: {m}");
                assert!(m.contains("non-ASCII"), "the shape is described: {m}");
            }
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }
    }

    #[test]
    fn mixed_case_argv_emails_fold_into_the_wire_body() {
        // The op folds argv emails to the kernel canonical (ASCII-lowercase) form ONCE, before the wire
        // body — so the roster rows carry one identity per human regardless of how the address was typed.
        // (The plane re-folds at its parse boundary, so the client fold is a courtesy, not a trust step.)
        let rig = Rig::new("fold-mixed-case");
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(InviteData {
                invite_link: format!("{BASE_URL}/i/tok_fold"),
                roster_added: vec!["alice@acme.com".to_owned()],
                skills: vec!["s_deploy".to_owned()],
            }),
            &["Alice@Acme.COM", "bob@x.io"],
            None,
            &["s_deploy"],
        );
        out.expect("the op POSTs the invite");
        let body = cap.expect("the op reached the transport");
        // The wire body carries the FOLDED forms, argv order preserved (no client-side dedup — the plane's
        // set-build dedups server-side).
        assert_eq!(
            body.emails,
            vec!["alice@acme.com".to_owned(), "bob@x.io".to_owned()],
            "the wire body's emails are the folded forms, order preserved"
        );
    }

    #[test]
    fn case_variant_argv_emails_both_fold_to_one_canonical_form() {
        // Two case-variants of one address fold to the SAME canonical form; the wire body carries both
        // folded entries verbatim (the plane's parse+set-build dedups them server-side).
        let rig = Rig::new("fold-dup-variants");
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(InviteData {
                invite_link: format!("{BASE_URL}/i/tok_dup"),
                roster_added: vec!["alice@x.io".to_owned()],
                skills: vec!["s_deploy".to_owned()],
            }),
            &["Alice@X.io", "alice@x.io"],
            None,
            &["s_deploy"],
        );
        out.expect("the op POSTs the invite");
        let body = cap.expect("the op reached the transport");
        assert_eq!(
            body.emails,
            vec!["alice@x.io".to_owned(), "alice@x.io".to_owned()],
            "both case-variants fold to the SAME canonical form on the wire"
        );
    }
}
