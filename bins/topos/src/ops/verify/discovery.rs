//! **The sign-in walk** — which of the two sign-in worlds a refusing server lives in.
//!
//! A `401` says a server wants a sign-in. It does not say whether anything on this machine can
//! complete one, and two very different worlds hide behind the same status:
//!
//! - its authorization server **registers clients on demand** — an agent app that has never seen
//!   this server registers itself and signs in on first use, with nothing arranged beforehand;
//! - it accepts only **clients or tokens registered in advance** — a personal access token, or an
//!   OAuth app an administrator created. No agent completes that by itself, and a person who is
//!   told "your agent app signs in on first use" will wait for something that never happens.
//!
//! The server publishes which one it is, and this walk reads it: the protected-resource metadata
//! (RFC 9728) names the authorization server, and that server's own metadata (RFC 8414, or the
//! OpenID Connect discovery document) either carries a `registration_endpoint` or does not.
//!
//! ## The discipline
//!
//! **Bounded twice — in requests AND in time.** At most [`MAX_LOOKUPS`] extra GETs, each under a
//! [`LOOKUP_TIMEOUT_SECS`]-second ceiling, and the whole walk under [`WALK_BUDGET`] measured from
//! its first step. The dial's own agent is deliberately NOT reused: its deadlines are sized for a
//! protocol conversation a person is waiting on the answer to, and six lookups inheriting them
//! turned a stalled metadata host into minutes of silence. This is an aside on the way to a verdict
//! that is already decided — it gets an aside's patience, and runs out of it quietly.
//!
//! **Never a downgrade.** A candidate is fetched only over `https`, or on the EXACT origin the dial
//! already used. A server pointing its `resource_metadata` at a plaintext address elsewhere is
//! pointing somewhere this walk will not go.
//!
//! **Fail-open, one direction only.** Every fetch here is best-effort: a document that 404s, times
//! out, is not JSON, or is not SHAPED like the metadata it stands in for ends the walk with NO
//! answer, and the verdict prints exactly what it has always printed. The pessimistic sentence is
//! said only where the documents said it — a chain this machine could not read is never evidence
//! against a server, and neither is a document it could not recognize.
//!
//! Nothing here supplies a credential, and nothing here signs anything in: the walk reads public
//! metadata, which is what makes it safe to do on the way to a verdict.

use std::time::{Duration, Instant};

use serde_json::Value;
use topos_types::results::SignInPath;
use ureq::http::Uri;

use super::wire::MAX_BODY_BYTES;

/// The most extra GETs one walk may cost: the two protected-resource spellings plus the four
/// authorization-server ones. A budget rather than a per-stage cap, so a server that points its own
/// `resource_metadata` at one address still gets every metadata location tried.
const MAX_LOOKUPS: usize = 6;

/// The ceiling on ONE lookup — DNS, connect, TLS, headers and body together. A metadata document is
/// a small static file; a host that cannot produce one in this long is a host this walk stops
/// waiting for.
const LOOKUP_TIMEOUT_SECS: u64 = 5;

/// The ceiling on the WHOLE walk, checked before every GET. Six lookups can each be slow without
/// any one of them timing out, and the sum is what a person actually waits — so the sum is what is
/// bounded. An expired budget is exactly like an unreadable document: no answer, today's line.
const WALK_BUDGET: Duration = Duration::from_secs(12);

/// The `WWW-Authenticate` parameter a challenge points at its own metadata with (RFC 9728). Auth
/// parameter names are case-insensitive on the wire, so the search is too.
const RESOURCE_METADATA: &str = "resource_metadata";

/// The three well-known paths, in the two families that answer this question.
const PROTECTED_RESOURCE: &str = "/.well-known/oauth-protected-resource";
const AUTHORIZATION_SERVER: &str = "/.well-known/oauth-authorization-server";
const OPENID_CONFIGURATION: &str = "/.well-known/openid-configuration";

/// **Walk the discovery chain and answer who can sign in**, or answer nothing.
///
/// `dialed` is the address the verification just got refused at; `challenges` are the
/// `WWW-Authenticate` values that came back with the refusal, in wire order.
pub(crate) fn sign_in_path(dialed: &str, challenges: &[String]) -> Option<SignInPath> {
    let mut walk = Walk::starting_now(dialed.parse().ok()?);

    // 1. The protected resource, which names its authorization server. The FIRST location that
    //    answers with such a document is the one believed — a resource that answers without naming
    //    an authorization server has not finished the chain, and the next spelling may.
    let issuer = resource_metadata_urls(&walk.dialed, challenges)
        .into_iter()
        .find_map(|url| {
            walk.fetch_json(&url)
                .and_then(|doc| authorization_server(&doc))
        })?;

    // 2. That server's own metadata. `registration_endpoint` is the whole question: it is the door
    //    an agent app registers itself through, and a server without one has no door. A candidate
    //    that answers something OTHER than that metadata is passed over, not believed: several of
    //    these paths sit under hosts that answer every unknown path with a catch-all document, and
    //    "no registration endpoint" read off one of those is a definite verdict from noise.
    authorization_metadata_urls(&issuer)
        .into_iter()
        .find_map(|url| walk.fetch_json(&url).filter(is_authorization_metadata))
        .map(|doc| {
            if registers_clients(&doc) {
                SignInPath::SelfService
            } else {
                SignInPath::Manual
            }
        })
}

/// **One walk's whole allowance** — its own client, its own deadline, its own request budget. Held
/// together because they are one rule: this is a best-effort aside, and it stops when any of the
/// three runs out.
struct Walk {
    agent: ureq::Agent,
    dialed: Uri,
    /// When the walk stops asking, whatever it has learned by then.
    deadline: Instant,
    /// How many GETs are left.
    budget: usize,
}

impl Walk {
    fn starting_now(dialed: Uri) -> Self {
        Self {
            // The address arm's own agent shape (no redirects followed, a status is never an
            // error), on this walk's much shorter clock.
            agent: super::remote::agent_with(
                LOOKUP_TIMEOUT_SECS,
                LOOKUP_TIMEOUT_SECS,
                LOOKUP_TIMEOUT_SECS,
            ),
            dialed,
            deadline: Instant::now() + WALK_BUDGET,
            budget: MAX_LOOKUPS,
        }
    }

    /// **One metadata GET**, bounded by the deadline, the budget, the scheme rule, and the same body
    /// cap the arm reads a protocol reply under. Anything other than a `200` carrying a JSON OBJECT
    /// is not a document, and says so by being absent — including a redirect, which this verb does
    /// not follow anywhere.
    fn fetch_json(&mut self, url: &str) -> Option<Value> {
        // The clock is read BEFORE anything is dialed, so an expired walk costs nothing at all.
        if self.budget == 0 || Instant::now() >= self.deadline || !walkable(url, &self.dialed) {
            return None;
        }
        self.budget -= 1;
        // No bundle header rides here: the bundle's headers are what its MCP endpoint needs, and a
        // metadata document is public by construction. Nothing is sent that the server did not
        // publish this URL for.
        let response = self
            .agent
            .get(url)
            .header("accept", "application/json")
            .call()
            .ok()?;
        if response.status().as_u16() != 200 {
            return None;
        }
        let bytes = response
            .into_body()
            .into_with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_vec()
            .ok()?;
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
    }
}

/// **Where the protected-resource metadata is**, in the order RFC 9728 gives.
///
/// The challenge's own `resource_metadata` wins outright where the server sent one: it is the
/// server saying where its document lives, and a pointer that does not answer is a broken chain,
/// not an invitation to guess. Otherwise the two conventional spellings — the resource's path
/// INSERTED after the well-known prefix, then the bare prefix at the origin, which is where a
/// server whose resource is the origin itself publishes.
fn resource_metadata_urls(dialed: &Uri, challenges: &[String]) -> Vec<String> {
    let named: Vec<String> = challenges
        .iter()
        .filter_map(|challenge| resource_metadata_param(challenge))
        // A parameter with an empty value points nowhere, and must not stand in the way of the
        // conventions the way a real pointer does.
        .filter(|url| !url.trim().is_empty())
        .collect();
    if !named.is_empty() {
        return named;
    }
    let Some(origin) = origin(dialed) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    push_new(
        &mut out,
        format!("{origin}{PROTECTED_RESOURCE}{}", path(dialed)),
    );
    push_new(&mut out, format!("{origin}{PROTECTED_RESOURCE}"));
    out
}

/// **Where an authorization server's metadata is**, in the order the specifications and the field
/// disagree about — which is exactly why all four are tried.
///
/// RFC 8414 INSERTS the issuer's path after the well-known prefix; a great many deployments publish
/// at the appended spelling instead, and OpenID Connect discovery has the same two shapes again. An
/// issuer with no path of its own collapses each pair into one URL, and the duplicates are dropped
/// rather than dialed twice.
fn authorization_metadata_urls(issuer: &str) -> Vec<String> {
    let Ok(uri) = issuer.parse::<Uri>() else {
        return Vec::new();
    };
    let Some(origin) = origin(&uri) else {
        return Vec::new();
    };
    let path = path(&uri);
    let mut out = Vec::new();
    for well_known in [AUTHORIZATION_SERVER, OPENID_CONFIGURATION] {
        push_new(&mut out, format!("{origin}{well_known}{path}"));
        push_new(&mut out, format!("{origin}{path}{well_known}"));
    }
    out
}

/// The first authorization server a protected-resource document names.
fn authorization_server(doc: &Value) -> Option<String> {
    doc.get("authorization_servers")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|issuer| !issuer.is_empty())
        .map(ToOwned::to_owned)
}

/// Whether an authorization server registers clients on demand.
///
/// One field decides it, and one field deliberately does not: a document may also advertise that it
/// accepts a URL as a client id (the client-id-metadata-document flag), and that flag is IGNORED
/// here. It describes a way of identifying a client that already exists somewhere; it is not a door
/// this machine's agent app can walk through today, and calling it self-service would put the
/// reassuring sentence in front of a person who then cannot sign in.
fn registers_clients(doc: &Value) -> bool {
    doc.get("registration_endpoint")
        .and_then(Value::as_str)
        .is_some_and(|endpoint| !endpoint.trim().is_empty())
}

/// **Whether a document is authorization-server metadata at all**, rather than whatever else a host
/// serves at an unknown path.
///
/// The test is the minimum RFC 8414 shape — an `issuer` and a `token_endpoint`, both non-empty
/// strings. It is deliberately the floor and not the whole schema: the walk's job is to tell a
/// metadata document from a catch-all error page or an index, and a real server that omits an
/// optional field must not be pushed into "unreadable" for it. What this closes is the one way the
/// walk could be DEFINITE about noise — an arbitrary JSON object has no `registration_endpoint`
/// either, and reading that as "registers nobody" is a verdict from a page that answered a question
/// nobody asked.
fn is_authorization_metadata(doc: &Value) -> bool {
    ["issuer", "token_endpoint"].iter().all(|field| {
        doc.get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

/// Whether a candidate may be fetched at all: `https`, or the very origin the dial already used.
/// The bundle gate admits only `https` addresses, so in the field this is https-and-nothing-else;
/// the second arm is what keeps the walk from being a different code path when the address it
/// followed was reached over plaintext already.
fn walkable(url: &str, dialed: &Uri) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    match uri.scheme_str() {
        Some("https") => true,
        _ => origin(&uri).is_some_and(|candidate| origin(dialed) == Some(candidate)),
    }
}

/// The scheme and authority of a URL, as one string — `None` when either is missing, which is a URL
/// nothing here will fetch.
fn origin(uri: &Uri) -> Option<String> {
    Some(format!("{}://{}", uri.scheme_str()?, uri.authority()?))
}

/// The path a well-known URL is built around: the URL's own, with any trailing slash dropped so the
/// two spellings of a root path are ONE spelling.
fn path(uri: &Uri) -> &str {
    uri.path().trim_end_matches('/')
}

/// Append `url` unless the list already holds it — the budget is small, and a duplicate spends it
/// on an answer already given.
fn push_new(out: &mut Vec<String>, url: String) {
    if !out.contains(&url) {
        out.push(url);
    }
}

/// **The `resource_metadata` a challenge names**, or `None`.
///
/// The name is matched only where it starts a parameter — a challenge carrying
/// `x_resource_metadata=…` names a different thing — and the value is read in either legal form: a
/// quoted string, or a bare token up to the next comma or space. A URL carries neither a quote nor
/// a space of its own, so the first closing quote ends the quoted form.
fn resource_metadata_param(challenge: &str) -> Option<String> {
    // ASCII-lowercasing preserves every byte's width, so an index found in the lowered copy is the
    // same index in the original.
    let lowered = challenge.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lowered[from..].find(RESOURCE_METADATA) {
        let at = from + found;
        from = at + RESOURCE_METADATA.len();
        let starts_a_parameter = at == 0 || !is_name_byte(lowered.as_bytes()[at - 1]);
        let Some(value) = challenge[from..].trim_start().strip_prefix('=') else {
            continue;
        };
        if starts_a_parameter {
            return Some(read_value(value.trim_start()));
        }
    }
    None
}

/// Whether a byte can be part of an auth-parameter name (so a name ending right before ours proves
/// the match is the tail of a longer name, not a parameter of its own).
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// A parameter value, quoted or bare.
fn read_value(rest: &str) -> String {
    match rest.strip_prefix('"') {
        Some(quoted) => quoted
            .split_once('"')
            .map_or_else(|| quoted.to_owned(), |(value, _)| value.to_owned()),
        None => rest
            .split([',', ' ', '\t'])
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use serde_json::json;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    // ---- where the documents are looked for ---------------------------------------------------

    #[test]
    fn the_challenges_own_pointer_wins_over_the_conventional_spellings() {
        let challenge = r#"Bearer error="invalid_token", resource_metadata="https://acme.test/.well-known/oauth-protected-resource/mcp""#;
        assert_eq!(
            resource_metadata_urls(&uri("https://acme.test/mcp"), &[challenge.to_owned()]),
            ["https://acme.test/.well-known/oauth-protected-resource/mcp"]
        );
        // A challenge that points at nothing leaves the conventions in charge: the path-INSERTED
        // spelling first, then the origin's own.
        assert_eq!(
            resource_metadata_urls(&uri("https://acme.test/mcp"), &["Bearer".to_owned()]),
            [
                "https://acme.test/.well-known/oauth-protected-resource/mcp",
                "https://acme.test/.well-known/oauth-protected-resource",
            ]
        );
        // A resource at the root has ONE spelling, not two of the same.
        assert_eq!(
            resource_metadata_urls(&uri("https://acme.test/"), &[]),
            ["https://acme.test/.well-known/oauth-protected-resource"]
        );
    }

    #[test]
    fn the_authorization_server_is_looked_for_at_four_locations_in_one_order() {
        assert_eq!(
            authorization_metadata_urls("https://auth.acme.test/tenant-7"),
            [
                // RFC 8414 inserts the issuer's path — the spelling a probe that only appends
                // never reaches.
                "https://auth.acme.test/.well-known/oauth-authorization-server/tenant-7",
                "https://auth.acme.test/tenant-7/.well-known/oauth-authorization-server",
                "https://auth.acme.test/.well-known/openid-configuration/tenant-7",
                "https://auth.acme.test/tenant-7/.well-known/openid-configuration",
            ]
        );
        // A path-less issuer collapses each pair, and the budget is not spent twice on one URL.
        assert_eq!(
            authorization_metadata_urls("https://auth.acme.test"),
            [
                "https://auth.acme.test/.well-known/oauth-authorization-server",
                "https://auth.acme.test/.well-known/openid-configuration",
            ]
        );
        assert!(authorization_metadata_urls("not a url").is_empty());
    }

    // ---- what the documents say ---------------------------------------------------------------

    #[test]
    fn the_first_named_authorization_server_is_the_one_followed() {
        let doc = json!({"resource":"https://acme.test/mcp",
            "authorization_servers":["  ","https://auth.acme.test","https://other.test"]});
        assert_eq!(
            authorization_server(&doc).as_deref(),
            Some("https://auth.acme.test")
        );
        // A document that names none has not finished the chain.
        assert_eq!(authorization_server(&json!({"resource":"x"})), None);
        assert_eq!(
            authorization_server(&json!({"authorization_servers":[]})),
            None
        );
    }

    /// **A document has to LOOK like the metadata it stands in for before it is read as one.** A
    /// host answering every unknown path with a catch-all JSON body would otherwise hand the walk a
    /// definite "registers nobody" — a pessimistic verdict from a page that answered nothing.
    #[test]
    fn only_a_document_shaped_like_authorization_metadata_is_read_as_one() {
        assert!(is_authorization_metadata(&json!({
            "issuer": "https://auth.acme.test",
            "token_endpoint": "https://auth.acme.test/token"
        })));
        // The catch-all shapes a walk actually meets.
        assert!(!is_authorization_metadata(
            &json!({"error": "not_found", "status": 404})
        ));
        assert!(!is_authorization_metadata(&json!({})));
        // Half a document is not a document: each required field is required.
        assert!(!is_authorization_metadata(
            &json!({"issuer": "https://auth.acme.test"})
        ));
        assert!(!is_authorization_metadata(
            &json!({"token_endpoint": "https://auth.acme.test/token"})
        ));
        // Present but empty, and present but not a string.
        assert!(!is_authorization_metadata(
            &json!({"issuer": "  ", "token_endpoint": "https://auth.acme.test/token"})
        ));
        assert!(!is_authorization_metadata(
            &json!({"issuer": "https://auth.acme.test", "token_endpoint": ["x"]})
        ));
    }

    #[test]
    fn only_a_registration_endpoint_makes_a_server_self_service() {
        assert!(registers_clients(
            &json!({"registration_endpoint":"https://auth.acme.test/register"})
        ));
        assert!(!registers_clients(&json!({"registration_endpoint":"  "})));
        assert!(!registers_clients(
            &json!({"issuer":"https://auth.acme.test"})
        ));
        // The client-id-metadata-document flag is not a door: it does not rescue a server that
        // registers nobody.
        assert!(!registers_clients(&json!({
            "issuer":"https://auth.acme.test",
            "client_id_metadata_document_supported": true
        })));
    }

    // ---- the challenge parse ------------------------------------------------------------------

    #[test]
    fn the_resource_metadata_parameter_is_read_in_both_legal_forms() {
        assert_eq!(
            resource_metadata_param(r#"Bearer resource_metadata="https://a.test/rm""#).as_deref(),
            Some("https://a.test/rm")
        );
        // Bare token, and a parameter after it.
        assert_eq!(
            resource_metadata_param("Bearer resource_metadata=https://a.test/rm, realm=x")
                .as_deref(),
            Some("https://a.test/rm")
        );
        // The name is case-insensitive, and whitespace around the `=` is legal.
        assert_eq!(
            resource_metadata_param(r#"Bearer Resource_Metadata = "https://a.test/rm""#).as_deref(),
            Some("https://a.test/rm")
        );
        // A LONGER name that merely ends in ours is a different parameter.
        assert_eq!(
            resource_metadata_param(r#"Bearer x-resource_metadata="https://evil.test/rm""#),
            None
        );
        assert_eq!(resource_metadata_param("Bearer realm=\"acme\""), None);
        // A mention with no value at all is not a parameter either.
        assert_eq!(resource_metadata_param("Bearer resource_metadata"), None);
    }

    // ---- the two ceilings ---------------------------------------------------------------------

    /// **The deadline is read before anything is dialed.** A walk that has run out of time costs
    /// the machine nothing more — not a connection, not a name lookup — which is what keeps six
    /// lookups an aside on the way to a verdict rather than six protocol-sized waits stacked end
    /// to end. The budget is untouched too: an expired walk did not spend a lookup, it declined
    /// to make one.
    #[test]
    fn an_expired_walk_dials_nothing_and_spends_nothing() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        std::thread::spawn(move || {
            // Every connection is counted and then dropped: what is under test is whether the
            // socket was reached at all, never what came back from it.
            for stream in listener.incoming() {
                if stream.is_err() {
                    return;
                }
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        let dialed = uri(&format!("http://127.0.0.1:{port}/mcp"));
        let url = format!("http://127.0.0.1:{port}{PROTECTED_RESOURCE}");

        // A walk with time left reaches the socket (and learns nothing, which is fine).
        let mut live = Walk::starting_now(dialed.clone());
        assert_eq!(live.fetch_json(&url), None);
        assert_eq!(live.budget, MAX_LOOKUPS - 1, "a real lookup was spent");
        // `accept` is asynchronous to the peer's `connect`, so the honest question is asked for a
        // moment rather than once.
        let mut reached = false;
        for _ in 0..100 {
            if accepted.load(Ordering::Relaxed) == 1 {
                reached = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reached, "the live walk dials");

        // A walk whose time is up does not.
        let mut expired = Walk::starting_now(dialed);
        expired.deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(expired.fetch_json(&url), None);
        assert_eq!(
            accepted.load(Ordering::Relaxed),
            1,
            "nothing new was dialed"
        );
        assert_eq!(expired.budget, MAX_LOOKUPS, "and no lookup was spent");

        // The request budget is the other ceiling, and reads the same way.
        let mut spent = Walk::starting_now(uri("https://acme.test/mcp"));
        spent.budget = 0;
        assert_eq!(spent.fetch_json("https://acme.test/.well-known/x"), None);
    }

    /// The walk runs on its OWN clock, not the dial's: a lookup gets seconds, and the whole walk
    /// gets less time than a single one of the address arm's requests may take.
    #[test]
    fn the_walks_ceilings_are_an_asides_and_not_a_conversations() {
        assert!(Duration::from_secs(LOOKUP_TIMEOUT_SECS) < WALK_BUDGET);
        assert!(
            WALK_BUDGET < Duration::from_secs(LOOKUP_TIMEOUT_SECS) * MAX_LOOKUPS as u32,
            "the aggregate deadline is what bounds six slow-but-not-timing-out lookups"
        );
        let walk = Walk::starting_now(uri("https://acme.test/mcp"));
        assert!(walk.deadline > Instant::now());
        assert!(walk.deadline <= Instant::now() + WALK_BUDGET);
    }

    // ---- the scheme rule ----------------------------------------------------------------------

    #[test]
    fn the_walk_follows_https_or_the_origin_it_already_dialed_and_nothing_else() {
        let dialed = uri("https://acme.test/mcp");
        assert!(walkable("https://auth.acme.test/.well-known/x", &dialed));
        // A server pointing its metadata at plaintext elsewhere points somewhere this walk
        // will not go.
        assert!(!walkable("http://auth.acme.test/.well-known/x", &dialed));
        assert!(!walkable("ftp://acme.test/x", &dialed));
        assert!(!walkable("/.well-known/x", &dialed));

        // A dial that was already plaintext may read that same origin's documents — nothing is
        // exposed that the dial itself did not expose.
        let plain = uri("http://127.0.0.1:8080/mcp");
        assert!(walkable("http://127.0.0.1:8080/.well-known/x", &plain));
        assert!(!walkable("http://127.0.0.1:9090/.well-known/x", &plain));
    }
}
