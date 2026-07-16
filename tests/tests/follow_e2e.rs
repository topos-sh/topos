//! E2E — the real `topos follow <address>` against the real composed stack.
//!
//! Proves the whole ADDRESS enrollment loop end to end on the unified-identity model: the constant
//! protocol card is fetched over the real socket and asserted on BOTH faces (JSON carries
//! `api_base_url`; markdown is the constant agent hand-off, no path echo), `follow` starts the
//! gh-style device flow (call 1 pends with the user code + the `0600` WAL), a SIGNED-IN person
//! approves at the real `/verify` ceremony (session cookie + step-up password — the browser half,
//! driven over HTTP), and the re-invoked `follow` polls granted → persists the ONE bearer credential
//! → continues into the two-phase DESCRIBE and the `--yes` apply that lands `everyone`'s genesis
//! byte-exact (incl. the executable bit).
//!
//! The denied arm is the approver's: the person clicks Deny at `/verify`, and the device's next poll
//! is the ONE typed denial (`DENIED` + the ask-an-owner `REQUEST_ACCESS` guidance) with zero
//! enrollment state left behind.

mod common;

use common::{OWNER_EMAIL, SKILL, Stack, WS_NAME, expected, genesis_files};
use topos::test_support::FollowHarness;
use topos_types::requests::WireProtocolCard;

/// A member the suite seats besides the owner.
const MEMBER_EMAIL: &str = "alice@acme.test";

/// Stand the stack up, claim the owner, and publish the genesis (an owner-enrolled authoring CLI —
/// the same device flow every rig walks). A new bundle's genesis lands in the structural `everyone`,
/// so a fresh member is entitled to it the instant they join.
fn stack_with_genesis(tag: &str) -> (Stack, common::Session) {
    let stack = common::start_stack(tag);
    let owner = stack.claim_owner(OWNER_EMAIL);

    let author = FollowHarness::new(&format!("{tag}-author"));
    stack.enroll_begin_and_approve(&author, &owner);
    let applied = author.resume_apply().expect("the author's resume applies");
    assert!(applied.enrolled_now, "the author's device enrolled");
    assert!(
        applied.installed.is_empty(),
        "an empty workspace delivers nothing yet: {:?}",
        applied.installed
    );

    author.adopt(SKILL, &genesis_files());
    let digest = author.draft_digest(SKILL);
    let receipt = author
        .publish_message("", &format!("{SKILL}@{digest}"), "genesis: deploy runbook")
        .expect("the genesis publish lands");
    match receipt {
        topos::test_support::PublishResult::Published(d) => {
            assert_eq!(d.skill_id, SKILL);
            assert_eq!(d.current_generation, 1, "genesis creates the pointer at 1");
        }
        other => panic!("the genesis publish landed directly, got {other:?}"),
    }
    (stack, owner)
}

// ── the keystone: a real address follow enrolls, describes, and lands `everyone`'s set ─────────────

#[test]
fn e2e_real_follow_enrolls_describes_and_lands_the_first_skill() {
    let (stack, _owner) = stack_with_genesis("follow");
    let address = stack.address();

    // The constant protocol card, over the REAL socket, BOTH faces:
    //  - JSON carries the API base the client re-roots onto (this origin's own /api mount);
    //  - the markdown face is the constant agent hand-off (no path echo — no existence oracle).
    let card = fetch_card_json(&address);
    assert_eq!(card.schema_version, 1);
    assert_eq!(card.card, "topos-protocol-card");
    assert_eq!(
        card.api_base_url, stack.api_base,
        "the JSON card discloses the API base (the machine bootstrap)"
    );
    let markdown = http_get(&address, "*/*");
    assert!(
        markdown.contains("text/plain"),
        "the markdown face: {markdown}"
    );
    assert!(markdown.contains("A Topos resource address"));
    assert!(markdown.contains("releases/latest/download/install.sh"));

    // The card is BYTE-IDENTICAL to a NON-browser fetch on every path — the origin root, a skill
    // face, and the retired `/workspaces/...` grammar all answer the same constant card (no face is
    // an existence oracle, and a stale bookmark of the old grammar discloses nothing).
    let at_root = http_body(&stack.origin, "application/json");
    let at_address = http_body(&address, "application/json");
    let at_skill = http_body(&format!("{}/skills/x", stack.origin), "application/json");
    let at_workspaces = http_body(
        &format!("{}/workspaces/x", stack.origin),
        "application/json",
    );
    let at_miss = http_body(
        &format!("{}/no-such-thing/at-all", stack.origin),
        "application/json",
    );
    assert_eq!(at_root, at_address, "root face == address face");
    assert_eq!(at_address, at_skill, "address face == skill-path face");
    assert_eq!(
        at_address, at_workspaces,
        "address face == retired-grammar face"
    );
    assert_eq!(at_address, at_miss, "address face == unmatched-path face");

    // A BROWSER-shaped GET of the retired `/workspaces/...` grammar is the house 404 (there is no
    // such page anymore — the signed-in surface is origin-rooted in single-tenant mode).
    let browser_workspaces =
        common::Session::new(&stack.origin).get(&format!("/workspaces/{}", stack.workspace_id));
    assert_eq!(
        browser_workspaces.status, 404,
        "the retired `/workspaces/...` page is a browser 404"
    );

    // The member: an account (the open-registration arrangement) + a seat, then the device flow.
    let member = stack.add_member(MEMBER_EMAIL, "member");
    let client = FollowHarness::new("follow-member");

    // Call 1 — `topos follow <address>`: card → re-root → device-authorize → the pending WAL.
    let pending = client.follow(&address).expect("follow call 1");
    assert!(!pending.enrolled, "call 1 only begins enrollment");
    let handle = pending.pending.expect("the pending verification handle");
    assert!(
        handle.verification_uri_complete.starts_with(&stack.origin),
        "the approval URL rides this origin: {}",
        handle.verification_uri_complete
    );
    assert!(
        client.wal_exists(),
        "the pending WAL is written (0600 resume journal)"
    );

    // The human half: the signed-in member approves at /verify (step-up gated).
    stack.approve_device(&member, &handle.user_code);

    // Call 2 — re-invoke `topos follow`: poll granted → persist the ONE credential → the DESCRIBE.
    let describe = client.resume_describe().expect("the resume describes");
    assert!(
        client.instance_written(),
        "instance.json committed at promote"
    );
    assert!(
        !client.wal_exists(),
        "the WAL is consumed once promotion completes"
    );
    assert_eq!(
        client.credentials_mode(),
        Some(0o600),
        "the ONE bearer credential is a 0600 secret"
    );
    assert!(describe.enrolled_now, "THIS invocation enrolled the device");
    assert_eq!(describe.workspace_id, stack.workspace_id);
    assert_eq!(describe.workspace_name, WS_NAME);
    assert_eq!(
        describe.role, "member",
        "the seat's role rides the describe"
    );
    assert_eq!(
        describe.installs.len(),
        1,
        "one install: {:?}",
        describe.installs
    );
    let install = &describe.installs[0];
    assert_eq!(install.name, SKILL);
    assert!(
        install.bundle_digest.is_some(),
        "the consent digest is disclosed"
    );
    assert_eq!(install.via_channels, vec!["everyone".to_owned()]);
    assert!(!install.via_direct, "it arrives via the everyone channel");
    assert!(
        !describe.all_devices_note.is_empty(),
        "the person-scoped disclosure"
    );
    assert!(
        !describe.reporting_note.is_empty(),
        "the fleet-reporting disclosure"
    );

    // Call 3 — `topos follow <address> --yes`: the reconcile lands `everyone`'s set this invocation.
    let applied = client.follow_apply(&address).expect("the --yes apply");
    assert!(!applied.enrolled_now, "already enrolled by call 2");
    assert_eq!(
        applied.installed.len(),
        1,
        "the genesis landed: {:?}",
        applied.installed
    );
    assert_eq!(applied.installed[0].name, SKILL);
    assert!(
        applied.warnings.is_empty(),
        "a clean apply: {:?}",
        applied.warnings
    );

    // The placement holds the EXACT genesis bytes — path/mode/content, incl. the exec bit.
    let placed = &applied.installed[0].skill_id;
    let got = client.placement_files(placed);
    assert_eq!(
        got,
        expected(&genesis_files()),
        "the genesis is placed byte-exact"
    );
    let run_sh = got
        .iter()
        .find(|(p, _, _)| p == "run.sh")
        .expect("run.sh present");
    assert_eq!(run_sh.1 & 0o111, 0o111, "run.sh keeps its executable bit");
    let skill_md = got
        .iter()
        .find(|(p, _, _)| p == "SKILL.md")
        .expect("SKILL.md present");
    assert_eq!(skill_md.1 & 0o111, 0, "SKILL.md is not executable");

    // The row witness: the approval minted ONE device owned by the member.
    let devices = stack.count(&format!(
        "SELECT count(*) FROM web.device d JOIN web.\"user\" u ON u.id = d.user_id \
         WHERE u.email = '{MEMBER_EMAIL}' AND d.revoked_at IS NULL"
    ));
    assert_eq!(devices, 1, "one live device for the member");
}

// ── follow by BARE ORIGIN (no workspace slug): the single-tenant address form ───────────────────────

/// `topos follow <bare-origin>` — a server address with NO workspace slug ("the workspace this
/// origin itself addresses"). The device-authorize goes out with an EMPTY workspace; the granted
/// poll carries the AUTHORITATIVE workspace back, and everything persisted/described/applied uses
/// THAT — never the empty request string. Then the reconcile lands the genesis byte-exact, proving
/// the bare-origin form enrolls and delivers exactly like the slug form.
#[test]
fn e2e_follow_by_bare_origin_enrolls_describes_and_delivers() {
    let (stack, _owner) = stack_with_genesis("bareorigin");
    // The BARE origin — no `/acme` slug. In single-tenant mode the origin IS the one workspace.
    let origin = stack.origin.clone();

    let member = stack.add_member(MEMBER_EMAIL, "member");
    let client = FollowHarness::new("bareorigin-member");

    // Call 1 — `topos follow <bare-origin>`: card at the bare origin → re-root → device-authorize
    // with an EMPTY workspace → the pending WAL.
    let pending = client.follow(&origin).expect("follow call 1 (bare origin)");
    assert!(!pending.enrolled, "call 1 only begins enrollment");
    let handle = pending.pending.expect("the pending verification handle");
    assert!(
        handle.verification_uri_complete.starts_with(&stack.origin),
        "the approval URL rides this origin: {}",
        handle.verification_uri_complete
    );
    assert!(client.wal_exists(), "the pending WAL is written");

    // The signed-in member approves (a plain accept — no step-up).
    stack.approve_device(&member, &handle.user_code);

    // Call 2 — re-invoke: poll granted → persist → DESCRIBE. The AUTHORITATIVE workspace (never the
    // empty request string) is what got persisted and described.
    let describe = client.resume_describe().expect("the resume describes");
    assert!(describe.enrolled_now, "THIS invocation enrolled the device");
    assert_eq!(
        describe.workspace_id, stack.workspace_id,
        "the granted authoritative workspace id is described, never the empty request"
    );
    assert_eq!(
        describe.workspace_name, WS_NAME,
        "the granted authoritative slug is described, never an empty string"
    );
    assert_eq!(
        describe.installs.len(),
        1,
        "one install delivered by everyone: {:?}",
        describe.installs
    );
    assert_eq!(describe.installs[0].name, SKILL);

    // The persisted membership carries the AUTHORITATIVE workspace id (offline witness — never the
    // empty request string that started the flow).
    assert_eq!(
        client.user_workspace().as_deref(),
        Some(stack.workspace_id.as_str()),
        "the persisted membership is the granted workspace, not the empty request"
    );

    // Call 3 — `topos follow <bare-origin> --yes`: the bare origin resolves to the already-enrolled
    // workspace (NO second enrollment) and the reconcile lands `everyone`'s genesis this invocation.
    let applied = client
        .follow_apply(&origin)
        .expect("the --yes apply (bare origin)");
    assert!(!applied.enrolled_now, "already enrolled by call 2");
    assert_eq!(
        applied.installed.len(),
        1,
        "the genesis landed: {:?}",
        applied.installed
    );
    assert_eq!(applied.installed[0].name, SKILL);
    assert!(
        applied.warnings.is_empty(),
        "a clean apply: {:?}",
        applied.warnings
    );

    // Delivery works — the placement holds the EXACT genesis bytes (incl. the exec bit).
    let placed = &applied.installed[0].skill_id;
    assert_eq!(
        client.placement_files(placed),
        expected(&genesis_files()),
        "the genesis is placed byte-exact"
    );

    // Exactly one live device for the member, minted by the one approval.
    let devices = stack.count(&format!(
        "SELECT count(*) FROM web.device d JOIN web.\"user\" u ON u.id = d.user_id \
         WHERE u.email = '{MEMBER_EMAIL}' AND d.revoked_at IS NULL"
    ));
    assert_eq!(devices, 1, "one live device for the member");
}

// ── the denied arm: the approver clicks Deny, the device's next poll is the ONE typed denial ───────

#[test]
fn e2e_a_denied_approval_is_the_uniform_ask_an_owner_refusal() {
    let (stack, owner) = stack_with_genesis("denied");
    let client = FollowHarness::new("denied");

    let pending = client.follow(&stack.address()).expect("follow call 1");
    let user_code = pending.pending.expect("pending handle").user_code;

    // The person at /verify denies — destroys the pending request, mints nothing (no step-up).
    stack.deny_device(&owner, &user_code);

    // The resume polls the terminal denial: the ask-an-owner guidance, and NO enrollment state.
    let denial = client.resume_expect_denied();
    assert_eq!(denial.code, "DENIED");
    assert_eq!(
        denial.next_action_codes,
        vec!["REQUEST_ACCESS".to_owned()],
        "the denied poll carries the REQUEST_ACCESS next-action"
    );
    assert!(
        !client.enrolled(),
        "no enrollment state lands for a denied flow"
    );
    assert!(!client.instance_written(), "nothing was promoted");

    // The row witness: no device row was minted, and the denied ceremony row LINGERS (terminal
    // answers are delivered idempotently — a re-poll must re-answer denied — so the row is reaped
    // by the expiry sweep, never deleted on read).
    assert_eq!(
        stack.count("SELECT count(*) FROM web.device"),
        1,
        "only the owner's authoring device exists"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_auth_session \
             WHERE user_code = '{user_code}' AND status = 'denied'"
        )),
        1,
        "the denied ceremony row lingers for the idempotent re-poll (swept later by TTL)"
    );
}

// ── an unknown workspace name at the device-auth start is the uniform miss ─────────────────────────

#[test]
fn e2e_a_wrong_address_name_is_the_uniform_404_at_the_flow_start() {
    let (stack, _owner) = stack_with_genesis("wrongname");
    let start = stack.device_post_json(
        None,
        "/v1/device/authorize",
        &serde_json::json!({ "requested_name": "topos CLI (e2e)", "workspace": "not-this-team" }),
    );
    assert_eq!(
        start.status, 404,
        "a name that is not this install's workspace: {}",
        start.body
    );
    let wrong_path = stack.device_post_json(None, "/v1/no/such/route", &serde_json::json!({}));
    assert_eq!(
        start.body, wrong_path.body,
        "the wrong-name miss is byte-identical to a wrong-path miss (no existence oracle)"
    );
}

// ── raw HTTP helpers (the e2e crate carries no client library) ──────────────────────────────────────

/// A minimal HTTP/1.1 GET over a plain `TcpStream` with an explicit `Accept`, returning the raw
/// response (status line + headers + body).
fn http_get(url: &str, accept: &str) -> String {
    use std::io::{Read as _, Write as _};

    let rest = url.strip_prefix("http://").expect("a loopback http url");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = std::net::TcpStream::connect(host).expect("connect the host");
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nAccept: {accept}\r\nConnection: close\r\n\r\n"
    )
    .expect("send the request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");
    response
}

/// GET `url` and return the DECODED body alone (chunked framing stripped) — for byte-identity
/// asserts across paths.
fn http_body(url: &str, accept: &str) -> String {
    let raw = http_get(url, accept);
    let (headers, body) = raw
        .split_once("\r\n\r\n")
        .expect("a response body follows the headers");
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_owned()
    }
}

/// GET `url` asking for JSON and parse the [`WireProtocolCard`] body (the card's machine face).
fn fetch_card_json(url: &str) -> WireProtocolCard {
    let decoded = http_body(url, "application/json");
    serde_json::from_str(decoded.trim())
        .unwrap_or_else(|e| panic!("a WireProtocolCard: {e}\n{decoded}"))
}

/// Strip HTTP chunked-transfer framing: each chunk is `<hex-size>\r\n<size bytes>\r\n`, terminated
/// by a zero-size chunk. Tolerant enough for the small card responses this suite reads.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&tail[..size.min(tail.len())]);
        rest = tail.get(size + 2..).unwrap_or("");
    }
    out
}
