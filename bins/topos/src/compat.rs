//! The ONE owner of this build's version facts: what version it is, how two versions compare, and
//! which servers it is willing to speak to.
//!
//! One binary talks to several servers, so compatibility is an INTERVAL with one boundary per
//! direction, and each side declares only what it can verifiably know: this build names the oldest
//! SERVER it speaks to ([`MIN_SERVER_VERSION`]), a server names the oldest CLI it speaks to (the
//! protocol card's `min_cli_version`, enforced with an HTTP 426 the transport maps to
//! [`crate::error::ClientError::UpdateRequired`]). Neither side negotiates and there is no second
//! dialect: pre-1.0 wire changes are clean breaks, so the boundary is simply the last release that
//! broke the wire.
//!
//! Both checks fail toward SILENCE. A version string this build cannot parse — a card from a
//! producer that predates the declaration, an odd build stamp — is never a refusal: a mechanism
//! meant to make the next break a sentence must not become a support incident of its own.

use topos_types::requests::WireProtocolCard;

use crate::error::ClientError;

/// This build's version, e.g. "0.1.0".
pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The oldest server this build can speak to. The floor sits at the last release that broke the
/// wire in a way a client CANNOT DEGRADE THROUGH — this one: a machine reports one state per
/// MCP-capable agent, and this build knows more agents than an older server's report door accepts,
/// so a machine that runs many of them has its WHOLE applied snapshot refused (the report is
/// atomic — there is no partial one), and the workspace's picture of that machine stays stale for
/// as long as it holds a server bundle. Refusing the old server here names the fix instead.
/// Removals a client rides out on its own do not move this floor; a break with no graceful
/// degradation moves it in the same change.
pub(crate) const MIN_SERVER_VERSION: &str = "0.1.37";

/// The oldest server that RECORDS MCP bundle kinds — the floor a `kind = "mcp"` publish checks
/// before its op WAL, because an older server ignores the additive `kind` field and silently
/// files the bundle as a SKILL while the client receipt claims otherwise. This constant rides the
/// RELEASE THAT INTRODUCES MCP KINDS: it must equal the workspace version that release ships as,
/// so the release's version-bump change moves it too (and never again after — later servers all
/// know the kind).
pub(crate) const MCP_MIN_SERVER_VERSION: &str = "0.1.23";

/// A minimal semver-core `>` : compare (major, minor, patch), ignoring any pre-release/build suffix. Tags
/// come from our own release pipeline (`vX.Y.Z`), so the core triple is sufficient; a malformed side is
/// treated as (0,0,0) so a valid newer version still wins. (Shared by the self-updater, the passive
/// version check, and the server floor — one newer-than decision, never two implementations to drift.)
pub(crate) fn version_gt(a: &str, b: &str) -> bool {
    parse_core(a) > parse_core(b)
}

pub(crate) fn parse_core(v: &str) -> (u64, u64, u64) {
    let core = v
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Whether a string READS as a version at all: a leading `v` and a `-pre`/`+build` suffix are
/// tolerated, but every dot-separated part of the core must be a number, and there must be at
/// least one. [`parse_core`] deliberately floors anything else at `(0, 0, 0)`, which is the right
/// answer for ORDERING two of our own tags and exactly the wrong one for a comparison that must
/// not fire on a string it does not understand.
fn is_semver_core(v: &str) -> bool {
    let core = v
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    !core.is_empty()
        && core
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The version RE-RENDERED from the three numbers this build parsed out of it — `None` when the
/// string is not a version. What a message shows a person is therefore always our own digits, never
/// a span of some server's bytes.
pub(crate) fn normalized_version(declared: &str) -> Option<String> {
    is_semver_core(declared).then(|| {
        let (major, minor, patch) = parse_core(declared);
        format!("{major}.{minor}.{patch}")
    })
}

/// Whether a server's declared version is provably below this build's floor. Anything that does
/// not parse as a version passes — a pre-mechanism producer, or one stamping something we do not
/// understand, is not evidence of an old server.
pub(crate) fn server_below_floor(server_version: &str) -> bool {
    normalized_version(server_version)
        .is_some_and(|v| parse_core(&v) < parse_core(MIN_SERVER_VERSION))
}

/// The client half of the floor, read off the constant protocol card BEFORE a login commits to a
/// server: a card declaring a version below [`MIN_SERVER_VERSION`] is refused with both remedies
/// named. A card that declares nothing passes — an older server's silence is not a claim, and the
/// server-side floor (the 426) still covers the other direction.
///
/// # Errors
/// [`ClientError::ServerTooOld`] when the card names a version below this build's floor.
pub(crate) fn ensure_server_supported(card: &WireProtocolCard) -> Result<(), ClientError> {
    // Below the floor ⇒ the version parsed ⇒ the normalization is `Some`; the two steps are the
    // decision and the words for it, never a second judgement.
    let refused = card
        .server_version
        .as_deref()
        .filter(|declared| server_below_floor(declared))
        .and_then(normalized_version);
    match refused {
        Some(server_version) => Err(ClientError::ServerTooOld { server_version }),
        None => Ok(()),
    }
}

/// The MCP half of the client-side floor, checked only when a publish would record a
/// `kind = "mcp"` bundle: a card provably below [`MCP_MIN_SERVER_VERSION`] refuses — the server
/// would silently record a SKILL. Everything else passes, the same fail-toward-silence rule as
/// [`ensure_server_supported`]: an unreadable card (`None`), one declaring nothing, and one
/// stamping a string this build cannot read are not evidence of an old server.
///
/// # Errors
/// [`ClientError::McpServerTooOld`] when the card names a version below the MCP floor.
pub(crate) fn ensure_server_records_mcp(
    card: Option<&WireProtocolCard>,
) -> Result<(), ClientError> {
    let refused = card
        .and_then(|c| c.server_version.as_deref())
        .filter(|declared| {
            normalized_version(declared)
                .is_some_and(|v| parse_core(&v) < parse_core(MCP_MIN_SERVER_VERSION))
        })
        .and_then(normalized_version);
    match refused {
        Some(server_version) => Err(ClientError::McpServerTooOld { server_version }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gt_compares_the_semver_core() {
        assert!(version_gt("0.2.0", "0.1.0"));
        assert!(!version_gt("0.1.0", "0.1.0"));
        assert!(version_gt("1.0.0", "0.9.9"));
        // A pre-release/build suffix is ignored — the core triple decides.
        assert_eq!(parse_core("0.2.0-rc1"), (0, 2, 0));
        assert_eq!(parse_core("0.2.0+build.7"), (0, 2, 0));
        // A malformed side parses to (0,0,0), so a valid newer version still wins.
        assert_eq!(parse_core(""), (0, 0, 0));
        assert!(version_gt("0.1.0", ""));
        assert!(!version_gt("", "0.1.0"));
    }

    #[test]
    fn the_floor_refuses_below_and_passes_at_or_above() {
        assert!(server_below_floor("0.1.14"));
        assert!(server_below_floor("0.0.9"));
        assert!(server_below_floor("v0.1.14"), "a leading v is tolerated");
        assert!(
            server_below_floor("0.1.35"),
            "the release before the floor's break refuses"
        );
        // At the floor and above, the two ends speak the same wire.
        assert!(!server_below_floor(MIN_SERVER_VERSION));
        assert!(!server_below_floor("1.0.0"));
    }

    #[test]
    fn an_unparseable_version_never_refuses() {
        // Fail toward silence: a producer stamping something this build cannot read is not
        // evidence of an old server, and a refusal here would be unanswerable.
        for odd in ["", "dev", "0.1.x", "main", "  ", "v", "nightly-2026-08-03"] {
            assert!(!server_below_floor(odd), "{odd:?} must pass");
            assert_eq!(normalized_version(odd), None, "{odd:?}");
        }
        // A version this build DOES read comes back as our own three numbers.
        assert_eq!(normalized_version("v0.1.15-rc2").as_deref(), Some("0.1.15"));
    }

    #[test]
    fn the_mcp_floor_refuses_below_and_fails_toward_silence() {
        let card = |server_version: Option<&str>| WireProtocolCard {
            schema_version: 1,
            card: "topos-protocol-card".to_owned(),
            api_base_url: "https://topos.example/api".to_owned(),
            server_version: server_version.map(str::to_owned),
        };
        // Provably below the MCP floor: refused with OUR rendering of the version.
        let err = ensure_server_records_mcp(Some(&card(Some("v0.1.9")))).unwrap_err();
        match err {
            ClientError::McpServerTooOld { server_version } => {
                assert_eq!(server_version, "0.1.9");
            }
            other => panic!("expected McpServerTooOld, got {other:?}"),
        }
        // At the floor and above: passes.
        assert!(ensure_server_records_mcp(Some(&card(Some(MCP_MIN_SERVER_VERSION)))).is_ok());
        assert!(ensure_server_records_mcp(Some(&card(Some("1.0.0")))).is_ok());
        // Silence is never a refusal: no card, no declaration, an unreadable stamp.
        assert!(ensure_server_records_mcp(None).is_ok());
        assert!(ensure_server_records_mcp(Some(&card(None))).is_ok());
        assert!(ensure_server_records_mcp(Some(&card(Some("dev")))).is_ok());
        // Day-one-quiet: this build's own server half is never refused (the release that carries
        // MCP kinds ships CLI and server at ONE workspace version).
        assert!(ensure_server_records_mcp(Some(&card(Some(CURRENT_VERSION)))).is_ok());
    }

    #[test]
    fn this_build_is_never_below_its_own_floor() {
        // The day-one-quiet property: a fleet running one release must see nothing at all. If a
        // floor is ever raised past the release that carries it, this fails before it ships.
        assert!(
            !server_below_floor(CURRENT_VERSION),
            "this build ({CURRENT_VERSION}) sits below its own floor ({MIN_SERVER_VERSION})"
        );
    }

    #[test]
    fn a_card_without_a_declaration_passes_and_an_old_one_refuses() {
        let card = |server_version: Option<&str>| WireProtocolCard {
            schema_version: 1,
            card: "topos-protocol-card".to_owned(),
            api_base_url: "https://topos.example/api".to_owned(),
            server_version: server_version.map(str::to_owned),
        };
        assert!(ensure_server_supported(&card(None)).is_ok());
        assert!(ensure_server_supported(&card(Some(CURRENT_VERSION))).is_ok());
        assert!(ensure_server_supported(&card(Some("what"))).is_ok());
        let err = ensure_server_supported(&card(Some("v0.1.9"))).unwrap_err();
        match err {
            // The refusal carries OUR rendering of the version, never the server's own bytes.
            ClientError::ServerTooOld { server_version } => assert_eq!(server_version, "0.1.9"),
            other => panic!("expected ServerTooOld, got {other:?}"),
        }
    }
}
