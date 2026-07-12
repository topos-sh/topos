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

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::ops::not_yet;
use crate::plane::GovernanceSource;

/// Builds the governance-write transport for a plane base URL — known only after reading `instance.json`,
/// so it can't be pre-built in the composition root (mirrors `follow`'s enroll connector). Production wires
/// `UreqDeviceClient`; the tests wire a fake (no HTTP).
pub(crate) type GovernanceConnect<'a> = dyn Fn(&str) -> Box<dyn GovernanceSource> + 'a;

/// Seat `emails` as invited members of the workspace, optionally pre-placing them into `channels`.
///
/// # Errors
/// [`ClientError::InvalidArgument`] (via [`not_yet`]) for a bare `invite` with no emails (the read-only
/// describe is a marked seam); [`ClientError::Enrollment`] if not enrolled (no `instance.json`) or the
/// workspace can't be inferred (no `identity/user.json`); a transport failure otherwise (a policy-DENIED
/// surfaces as [`ClientError::Plane`] — "not authorized").
pub(crate) fn invite(
    ctx: &Ctx<'_>,
    connect: &GovernanceConnect<'_>,
    emails: Vec<String>,
    channels: Vec<String>,
    workspace: Option<&str>,
) -> Result<InvitationData, ClientError> {
    // Bare `invite` (no emails) is the no-mutation read (the workspace address + invite policy). SEAM.
    if emails.is_empty() {
        return Err(not_yet("invite (the address/policy read)"));
    }
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

    // POST the invitation under the workspace Bearer credential (the transport looks it up by
    // `workspace_id`; the plane resolves the credential's registry row → principal → the invite-policy
    // gate). The workspace id rides the URL path; the body carries only the emails + channel pre-placements.
    let body = InvitationRequest { emails, channels };
    let transport = connect(&instance.base_url);
    transport.invite(&workspace_id, body)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
    use topos_types::bootstrap::{DeploymentMode, VerifiedDomainStatus};
    use topos_types::requests::{InvitationData, InvitationRequest};
    use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

    use super::{GovernanceConnect, invite};
    use crate::ctx::Ctx;
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

    /// One captured POST: the target workspace id + the wire body (the acting device rides the transport's
    /// Bearer credential — never a body field; nothing is signed).
    type Captured = Rc<RefCell<Option<(String, InvitationRequest)>>>;
    /// A `run_invite` outcome: the op result + whatever reached the transport (`None` if it never did).
    type InviteRun = (
        Result<InvitationData, ClientError>,
        Option<(String, InvitationRequest)>,
    );

    #[derive(Clone)]
    enum Resp {
        Ok(InvitationData),
        /// An already-mapped DENIED outcome (the real envelope mapping is unit-tested in `plane_http`).
        Denied(String),
    }

    #[derive(Clone)]
    struct FakeGov {
        captured: Captured,
        resp: Resp,
    }
    impl GovernanceSource for FakeGov {
        fn invite(
            &self,
            workspace_id: &str,
            body: InvitationRequest,
        ) -> Result<InvitationData, ClientError> {
            *self.captured.borrow_mut() = Some((workspace_id.to_owned(), body));
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

    fn ok_data() -> InvitationData {
        InvitationData {
            address: format!("{BASE_URL}/acme"),
            invited: vec!["alice@acme.com".to_owned()],
            mailed: false,
        }
    }

    /// Run the `invite` op over the fake transport, returning the op result + the captured (ws, wire body).
    fn run_invite(rig: &Rig, resp: Resp, emails: &[&str], channels: &[&str]) -> InviteRun {
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
            channels.iter().map(|s| (*s).to_owned()).collect(),
            None,
        );
        let cap = captured.borrow().clone();
        (out, cap)
    }

    // ---- tests ----

    #[test]
    fn posts_the_expected_wire_body_under_the_workspace_credential() {
        // The op POSTs a body naming the folded emails + channel pre-placements, to the enrolled workspace
        // id (the URL path segment). The acting device rides the transport's Bearer credential (the plane
        // resolves its registry row → principal → the invite-policy gate), never a body field.
        let rig = Rig::new("wire-body");
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(ok_data()),
            &["alice@acme.com"],
            &["design", "eng"],
        );
        out.expect("the op POSTs the invitation");
        let (ws, body) = cap.expect("the op reached the transport");
        assert_eq!(ws, WS);
        assert_eq!(body.emails, vec!["alice@acme.com".to_owned()]);
        assert_eq!(body.channels, vec!["design".to_owned(), "eng".to_owned()]);
    }

    #[test]
    fn ok_envelope_maps_to_invitation_data() {
        let rig = Rig::new("ok");
        rig.seed_enrolled();
        let (out, _cap) = run_invite(&rig, Resp::Ok(ok_data()), &["alice@acme.com"], &[]);
        let data = out.expect("ok");
        assert_eq!(data.address, format!("{BASE_URL}/acme"));
        assert_eq!(data.invited, vec!["alice@acme.com".to_owned()]);
        assert!(!data.mailed);
    }

    #[test]
    fn a_denied_outcome_is_a_typed_error() {
        let rig = Rig::new("denied");
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            &rig,
            Resp::Denied("NOT_AUTHORIZED".to_owned()),
            &["alice@acme.com"],
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
    fn a_bare_invite_with_no_emails_is_a_marked_seam() {
        // The no-mutation read (address + policy) is a later leg — bare `invite` refuses typed, never POSTs.
        let rig = Rig::new("bare");
        rig.seed_enrolled();
        let (out, cap) = run_invite(&rig, Resp::Ok(ok_data()), &[], &[]);
        assert!(cap.is_none(), "a bare invite never reaches the transport");
        assert_eq!(out.unwrap_err().code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn invite_without_enrollment_is_run_follow_first() {
        // No instance.json seeded — the op refuses before contacting any transport.
        let rig = Rig::new("not-enrolled");
        let (out, cap) = run_invite(&rig, Resp::Ok(ok_data()), &["alice@acme.com"], &[]);
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
    fn mixed_case_argv_emails_fold_into_the_wire_body() {
        // The op folds argv emails to the kernel canonical (ASCII-lowercase) form ONCE, before the wire
        // body — so the roster rows carry one identity per human regardless of how the address was typed.
        let rig = Rig::new("fold-mixed-case");
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            &rig,
            Resp::Ok(ok_data()),
            &["Alice@Acme.COM", "bob@x.io"],
            &[],
        );
        out.expect("the op POSTs the invitation");
        let (_ws, body) = cap.expect("the op reached the transport");
        assert_eq!(
            body.emails,
            vec!["alice@acme.com".to_owned(), "bob@x.io".to_owned()],
            "the wire body's emails are the folded forms, order preserved"
        );
    }
}
