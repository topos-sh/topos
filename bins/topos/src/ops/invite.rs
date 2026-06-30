//! `invite` — an OWNER mints an `/i/<token>` invite link by signing the governance Invite op + POSTing it.
//!
//! Requires prior enrollment: the pinned plane (`base_url`), the workspace (`workspace_id`), and the device
//! key all come from the sidecar `follow` wrote. The client SIGNS the governance frame the plane RE-DERIVES
//! + verifies, so a single disagreement on the bytes ⇒ every invite DENIED. The load-bearing agreements:
//!
//! - **the role byte** — `Owner=1, Reviewer=2, Member=3`, byte-for-byte the plane's `Role::signing_byte`,
//!   and an OMITTED `--role` defaults to **Member (=3)** (the plane's handler signs `role.unwrap_or(member)`);
//! - **`expires_at = 0`** — the plane's invite handler hardcodes `expires_at: None`, and the authority binds
//!   `expires_to_u64(None) == 0`, so the client must sign `0` (an invite never expires in v0);
//! - **emails + skill IDS as SETS** — the kernel `governance_op_preimage` sorts + dedups both in-frame, so
//!   the order is irrelevant; the skill **ids** (never names) are what the frame binds.

use topos_core::sign::{GovernanceOpFields, GovernanceOpKind};
use topos_types::requests::{InviteRequest, InviteSkill, WorkspaceRole};
use topos_types::results::InviteData;

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::GovernanceSource;

/// Builds the owner's governance-write transport for a plane base URL — known only after reading
/// `instance.json`, so it can't be pre-built in the composition root (mirrors `follow`'s enroll connector).
/// Production wires `UreqEnroll`; the tests wire a fake (no HTTP).
pub(crate) type GovernanceConnect<'a> = dyn Fn(&str) -> Box<dyn GovernanceSource> + 'a;

/// Map a [`WorkspaceRole`] (omitted ⇒ Member) to the governance signing byte — **the same mapping the plane's
/// `Role::signing_byte` uses: Owner=1, Reviewer=2, Member=3** — and the omitted-default Member the plane's
/// invite handler signs (`role.unwrap_or(member)`). A divergence here would make every invite DENIED.
fn role_signing_byte(role: Option<WorkspaceRole>) -> u8 {
    match role.unwrap_or(WorkspaceRole::Member) {
        WorkspaceRole::Owner => 1,
        WorkspaceRole::Reviewer => 2,
        WorkspaceRole::Member => 3,
    }
}

/// Mint an `/i/<token>` invite: sign the governance Invite op with this device's key, then POST it.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled (no `instance.json`) or the workspace can't be inferred (no
/// `identity/user.json`); a signing / transport failure otherwise (a role-DENIED surfaces as
/// [`ClientError::Plane`] — "not authorized").
pub(crate) fn invite(
    ctx: &Ctx<'_>,
    connect: &GovernanceConnect<'_>,
    emails: Vec<String>,
    role: Option<WorkspaceRole>,
    skills: Vec<String>,
) -> Result<InviteData, ClientError> {
    // Require enrollment: the pinned plane's base URL comes from what `follow` wrote.
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    // Infer the workspace (the governance frame's scope) from the enrolled `user.json`. `instance.json`
    // carries the plane but not the workspace id, so a present-instance-but-no-user state is a partial
    // enrollment we guide the user to complete rather than guess at.
    let workspace_id = enroll::read_user(ctx.fs, &ctx.layout)?
        .map(|u| u.workspace_id)
        .ok_or_else(|| {
            ClientError::Enrollment(
                "could not determine your workspace; complete enrollment with `topos follow` first"
                    .into(),
            )
        })?;

    // The device key (the client's only private-key edge) — its id is the one the plane re-derives + selects
    // to verify the governance signature against the registered (owner) public key.
    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;

    // The client-minted idempotency key: the raw 16 bytes are bound into the signed frame; the canonical
    // hyphenated UUID rides the wire (the plane's `parse_op_id` parses it back to the SAME 16 bytes, so a
    // lost-ack retry replays the deterministic link + receipt).
    let op_id_bytes = ctx.ids.new_op_id();
    let op_id = uuid::Uuid::from_bytes(op_id_bytes)
        .as_hyphenated()
        .to_string();

    // Build + sign the governance Invite frame the plane re-derives. emails + skill IDS are bound as SETS
    // (the kernel sorts + dedups in-frame), so we pass them in argv order; the skill **ids** (not names).
    let email_refs: Vec<&str> = emails.iter().map(String::as_str).collect();
    let skill_refs: Vec<&str> = skills.iter().map(String::as_str).collect();
    let fields = GovernanceOpFields {
        workspace_id: &workspace_id,
        op_id: op_id_bytes,
        device_key_id: signer.device_key_id(),
        op: GovernanceOpKind::Invite {
            role: role_signing_byte(role),
            expires_at: 0,
            emails: &email_refs,
            skills: &skill_refs,
        },
    };
    let signature = signer.sign_governance(&fields)?;
    // The borrows into `emails` / `skills` (via the *_refs above) end here — they are free to move below.
    drop(email_refs);
    drop(skill_refs);

    // POST the signed op. The role rides the body as the SAME `WorkspaceRole` whose byte we signed (omitted
    // ⇒ the plane defaults it to member, matching our default byte). The link never carries a role — it is a
    // property of the seeded roster row. `name: None` — only the skill id is bound into the signing frame.
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
    transport.create_invite(body, signature)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_core::sign::{GovernanceOpFields, GovernanceOpKind, verify_governance_op};
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

    // ---- the fake governance transport: captures the request + signature, returns a canned outcome ----

    /// One captured POST: the wire body + the 64-byte governance signature that rode the header.
    type CapturedInvite = (InviteRequest, [u8; 64]);
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
        fn create_invite(
            &self,
            body: InviteRequest,
            governance_sig: [u8; 64],
        ) -> Result<InviteData, ClientError> {
            *self.captured.borrow_mut() = Some((body, governance_sig));
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
                    plane_key: "a".repeat(64),
                    plane_key_id: "pk_acme".to_owned(),
                    deployment_mode: DeploymentMode::Cloud,
                    enrollment_method: "device_code".to_owned(),
                    workspace_display_name: Some("Acme".to_owned()),
                    verified_domain: None,
                    verified_domain_status: VerifiedDomainStatus::Unverified,
                },
            )
            .unwrap();
            enroll::write_user(
                &self.fs,
                &self.layout(),
                &enroll::UserDoc {
                    schema_version: 1,
                    workspace_id: WS.to_owned(),
                    deployment_mode: DeploymentMode::Cloud,
                    email: None,
                    roles: Vec::new(),
                    invite_rooted: true,
                    enrolled_at: 1,
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
                plane_key: [0u8; 32],
                follow,
            }
        }
        /// The device public key the op's signer would mint (load-or-generate is idempotent — same key).
        fn signer_pubkey(&self) -> [u8; 32] {
            DeviceSigner::load_or_generate(&self.fs, &self.layout())
                .unwrap()
                .public_key()
        }
    }

    /// Run the `invite` op over the fake transport, returning the op result + the captured (body, sig).
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
        );
        let cap = captured.borrow().clone();
        (out, cap)
    }

    /// The cross-component proof: the captured signature VERIFIES over the SAME `GovernanceOpFields` the
    /// plane rebuilds — its `op_id` parsed back from the canonical wire string, the chosen role byte, the
    /// `expires_at = 0`, and the email + skill-id sets. (Run for an explicit role AND the omitted default.)
    fn assert_frame_agreement(rig: &Rig, role: Option<WorkspaceRole>, expected_byte: u8) {
        rig.seed_enrolled();
        let (out, cap) = run_invite(
            rig,
            Resp::Ok(InviteData {
                invite_link: format!("{BASE_URL}/i/tok_abc"),
                roster_added: vec!["alice@acme.com".to_owned()],
                skills: vec!["s_deploy".to_owned()],
            }),
            // DELIBERATELY unsorted + duplicated — the kernel canonicalizes both sets in-frame.
            &["bob@acme.com", "alice@acme.com", "bob@acme.com"],
            role,
            &["s_review", "s_deploy"],
        );
        out.expect("the op POSTs the signed invite");
        let (body, sig) = cap.expect("the op reached the transport");

        // The wire `op_id` must be the canonical hyphenated form (the plane's `parse_op_id` requires it),
        // and it parses back to the 16 bytes the frame binds.
        let uuid = uuid::Uuid::parse_str(&body.op_id).expect("op_id is a UUID");
        assert_eq!(
            uuid.as_hyphenated().to_string(),
            body.op_id,
            "op_id must be the canonical hyphenated UUID the plane re-parses"
        );
        let op_id_bytes = uuid.into_bytes();

        // Rebuild the frame from the WIRE body exactly as the plane's `govern_preamble` does (skill IDS).
        let email_refs: Vec<&str> = body.emails.iter().map(String::as_str).collect();
        let skill_refs: Vec<&str> = body.skills.iter().map(|s| s.skill_id.as_str()).collect();
        let fields = GovernanceOpFields {
            workspace_id: &body.workspace_id,
            op_id: op_id_bytes,
            device_key_id: &body.device_key_id,
            op: GovernanceOpKind::Invite {
                role: expected_byte,
                expires_at: 0,
                emails: &email_refs,
                skills: &skill_refs,
            },
        };
        assert!(
            verify_governance_op(&fields, &sig, &rig.signer_pubkey()),
            "the client signature must verify over the frame the plane rebuilds (role byte {expected_byte})"
        );
        // The scope + device id on the wire match the enrolled state + the signing key.
        assert_eq!(body.workspace_id, WS);
        assert_eq!(body.device_key_id, rig_device_key_id(rig));
    }

    fn rig_device_key_id(rig: &Rig) -> String {
        DeviceSigner::load_or_generate(&rig.fs, &rig.layout())
            .unwrap()
            .device_key_id()
            .to_owned()
    }

    // ---- tests ----

    #[test]
    fn signs_a_frame_the_plane_verifies_for_an_explicit_role() {
        let rig = Rig::new("explicit-role");
        // Reviewer = byte 2 (the plane's Role::signing_byte).
        assert_frame_agreement(&rig, Some(WorkspaceRole::Reviewer), 2);
    }

    #[test]
    fn an_omitted_role_signs_the_member_byte() {
        let rig = Rig::new("default-role");
        // Omitted --role ⇒ the plane defaults to member (byte 3); the client must sign the same byte.
        assert_frame_agreement(&rig, None, 3);
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
        // The op still SIGNED + POSTed (the deny is the plane's authority verdict, surfaced as a typed error).
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
    fn role_byte_mapping_matches_the_plane() {
        // The exact mapping the plane's Role::signing_byte uses (Owner=1, Reviewer=2, Member=3), plus the
        // omitted-default (Member=3).
        assert_eq!(super::role_signing_byte(Some(WorkspaceRole::Owner)), 1);
        assert_eq!(super::role_signing_byte(Some(WorkspaceRole::Reviewer)), 2);
        assert_eq!(super::role_signing_byte(Some(WorkspaceRole::Member)), 3);
        assert_eq!(super::role_signing_byte(None), 3);
    }
}
