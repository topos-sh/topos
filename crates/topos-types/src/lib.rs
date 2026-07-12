//! `topos-types` — the boundary DTOs (wire + persisted).
//!
//! These are *deserialization shapes* for the boundary: the `--json` envelope, every per-verb
//! result shape ([`results`]), the frozen [`TerminalOutcome`] enum, the [`Receipt`] + [`WireError`],
//! the unsigned [`WireCurrentRecord`] pointer body, the [`ActionCode`] vocabulary, the harness
//! [`TriggerReport`], and the on-disk client documents ([`persisted`]). **No logic.** The app libs parse these into
//! `topos-core`'s validated domain newtypes at the HTTP/CLI edge (parse-don't-validate), so
//! `topos-core` never imports this crate.
//!
//! Naming: `version_id` = the commit SHA-256 (the user-facing
//! `<skill>@<version_id>`); `bundle_digest` = the byte-exact consent hash over the bundle's
//! files. The two are distinct values — never call both "digest."

use serde::{Deserialize, Serialize};

/// Per-verb `--json` `data` payloads (what the envelope's `data` deserializes into per command).
pub mod results;

/// On-disk persisted client documents under `~/.topos/` (sync / lock / map / op records).
pub mod persisted;

/// Wire request/response DTOs for the plane's HTTP write + version-metadata routes (the OpenAPI bodies).
pub mod requests;

/// The unauthenticated invite-bootstrap payload (`GET /i/{token}`) — read BEFORE enrollment (TOFU).
pub mod bootstrap;

#[doc(inline)]
pub use bootstrap::{
    BootstrapData, BootstrapInvite, BootstrapPlane, BootstrapSkill, BootstrapWorkspace,
    ConsentMode, DeploymentMode, VerifiedDomainStatus,
};

/// Bumped on any breaking change to a WIRE shape (the `--json` envelope, the `current`
/// pointer, the HTTP request/response bodies); every wire document carries it.
pub const WIRE_SCHEMA_VERSION: u32 = 1;

/// Bumped on any breaking change to an on-disk persisted client document ([`persisted`]).
pub const PERSISTED_SCHEMA_VERSION: u32 = 1;

/// Bumped on any breaking change to the [`Receipt`] shape (the durable idempotency record evolves
/// with the op vocabulary, not with the envelope or the sidecar docs).
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

/// The `data` payload defaults to an empty object (`{}`), not `null`, when absent — matching the
/// emitted envelope (a pure-signal command still carries `"data": {}`).
fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

// ---------------------------------------------------------------------------------------------
// The `--json` envelope (the single most important interface; the agent
// is the primary consumer). One JSON document on stdout, diagnostics on stderr only.
// ---------------------------------------------------------------------------------------------

/// The one common envelope every verb emits. Never prompts; prose (TTY) is rendered from the
/// SAME typed value where practical.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct JsonEnvelope {
    /// Always `1` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The verb that produced this (`add`, `follow`, `pull`, `list`, `publish`, …).
    pub command: String,
    pub ok: bool,
    /// Command-specific payload (`{}` when empty).
    #[serde(default = "empty_object")]
    pub data: serde_json::Value,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Stable, machine-actionable next steps — each carries a complete argv.
    #[serde(default)]
    pub next_actions: Vec<NextAction>,
    /// The durable idempotency record — present on **every** terminal op, including failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
    /// The actionable failure detail (when `ok == false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

// ---------------------------------------------------------------------------------------------
// The frozen terminal-outcome set (all 10; APPROVAL_REQUIRED included).
// ---------------------------------------------------------------------------------------------

/// The closed set of terminal outcomes the agent branches on. SCREAMING_SNAKE on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TerminalOutcome {
    Ok,
    /// A successfully-opened proposal — an explicit `--propose`, or a direct publish/revert the
    /// per-bundle protection DOWNGRADED to one (the receipt's `details.downgraded` says which).
    NeedsReview,
    Conflict,
    Diverged,
    Denied,
    Unavailable,
    AmbiguousName,
    RetryableFailure,
    PermanentFailure,
}

// ---------------------------------------------------------------------------------------------
// The next_actions action-code vocabulary — HYBRID: known variants + a sealed Unknown,
// so an old agent never *fails* on a perfectly executable future code (it has the argv).
// ---------------------------------------------------------------------------------------------

/// A machine-actionable next step. The `argv` is the ready-to-exec command; `code` lets an agent
/// branch on the known set and still pass through unknowns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct NextAction {
    pub code: ActionCode,
    /// A complete argv array — execute as-is (no TTY parsing).
    pub argv: Vec<String>,
}

/// The closed initial action-code vocabulary (each maps to its producing outcome). Advertised in
/// the [`ActionCode`] schema's `examples` so a cross-language consumer learns the set without
/// reading Rust; additive-only — new codes append here.
pub const KNOWN_ACTION_CODES: [&str; 8] = [
    "PROPOSE_PUBLISH",
    "REBASE_AND_RETRY",
    "RESOLVE_DIVERGED_DRAFT",
    "APPLY_WAITING_UPDATE",
    "DISAMBIGUATE_NAME",
    "REQUEST_ACCESS",
    "RETRY",
    "CONTACT_ADMIN",
];

/// The action-code vocabulary. Known variants serialize to their SCREAMING_SNAKE string; an
/// unrecognized code round-trips through [`ActionCode::Unknown`]. Serialized as a plain string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionCode {
    ProposePublish,       // APPROVAL_REQUIRED
    RebaseAndRetry,       // CONFLICT
    ResolveDivergedDraft, // DIVERGED
    ApplyWaitingUpdate,   // `pull <skill>` — a previously observed-but-unapplied version
    DisambiguateName,     // AMBIGUOUS_NAME
    RequestAccess,        // DENIED → invite/enroll
    Retry,                // RETRYABLE_FAILURE / UNAVAILABLE
    ContactAdmin,         // non-self-service denials
    /// A forward-compatible code this build doesn't know — execute the action's `argv` anyway.
    /// Only constructible via the normalizing [`From<String>`], which maps a known string to its
    /// variant first, so an `Unknown` can never alias a known code (the inner value is private).
    Unknown(UnknownActionCode),
}

/// An action code outside this build's known set — its inner string is private so it can only be
/// produced by [`ActionCode::from`], which canonicalizes known codes first. This prevents an
/// `Unknown("PROPOSE_PUBLISH")` that would serialize as a known code yet compare unequal to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownActionCode(String);

impl UnknownActionCode {
    /// The raw, unrecognized code string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ActionCode {
    pub fn as_str(&self) -> &str {
        match self {
            ActionCode::ProposePublish => "PROPOSE_PUBLISH",
            ActionCode::RebaseAndRetry => "REBASE_AND_RETRY",
            ActionCode::ResolveDivergedDraft => "RESOLVE_DIVERGED_DRAFT",
            ActionCode::ApplyWaitingUpdate => "APPLY_WAITING_UPDATE",
            ActionCode::DisambiguateName => "DISAMBIGUATE_NAME",
            ActionCode::RequestAccess => "REQUEST_ACCESS",
            ActionCode::Retry => "RETRY",
            ActionCode::ContactAdmin => "CONTACT_ADMIN",
            ActionCode::Unknown(s) => s.as_str(),
        }
    }
}

impl From<String> for ActionCode {
    fn from(s: String) -> Self {
        match s.as_str() {
            "PROPOSE_PUBLISH" => ActionCode::ProposePublish,
            "REBASE_AND_RETRY" => ActionCode::RebaseAndRetry,
            "RESOLVE_DIVERGED_DRAFT" => ActionCode::ResolveDivergedDraft,
            "APPLY_WAITING_UPDATE" => ActionCode::ApplyWaitingUpdate,
            "DISAMBIGUATE_NAME" => ActionCode::DisambiguateName,
            "REQUEST_ACCESS" => ActionCode::RequestAccess,
            "RETRY" => ActionCode::Retry,
            "CONTACT_ADMIN" => ActionCode::ContactAdmin,
            _ => ActionCode::Unknown(UnknownActionCode(s)),
        }
    }
}

impl From<ActionCode> for String {
    fn from(c: ActionCode) -> String {
        match c {
            ActionCode::Unknown(s) => s.0,
            other => other.as_str().to_owned(),
        }
    }
}

impl Serialize for ActionCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ActionCode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(ActionCode::from(String::deserialize(d)?))
    }
}

#[cfg(feature = "contract-derives")]
impl schemars::JsonSchema for ActionCode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ActionCode".into()
    }
    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // An OPEN string set: the known vocabulary is advertised (so a cross-language consumer can
        // branch without reading Rust), but any string validates — an old build must pass an unknown
        // future code through, not reject it. Contrast the CLOSED `TerminalOutcome`.
        schemars::json_schema!({
            "type": "string",
            "title": "ActionCode",
            "description": "A machine-actionable next-action code. The values in `examples` are the \
                known set; an unrecognized future code is still valid and MUST be executed via the \
                action's `argv` (never rejected).",
            "examples": KNOWN_ACTION_CODES,
        })
    }
}

// utoipa's `ToSchema` (split into `PartialSchema` + `ToSchema` in utoipa 5) is hand-written here to
// MIRROR the OPEN-string schemars schema above: `ActionCode` is an open string — any value validates,
// the known vocabulary rides in `examples`, and an unrecognized future code passes through (executed via
// the action's `argv`, never rejected). The derive can't express "open enum", so both halves are manual.
#[cfg(feature = "contract-derives")]
impl utoipa::PartialSchema for ActionCode {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .title(Some("ActionCode"))
            .description(Some(
                "A machine-actionable next-action code. The values in `examples` are the known set; an \
                 unrecognized future code is still valid and MUST be executed via the action's `argv` \
                 (never rejected).",
            ))
            .examples(KNOWN_ACTION_CODES)
            .into()
    }
}

#[cfg(feature = "contract-derives")]
impl utoipa::ToSchema for ActionCode {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ActionCode")
    }
}

// ---------------------------------------------------------------------------------------------
// Generation, Affected, Receipt, WireError (one canonical receipt + flat error).
// ---------------------------------------------------------------------------------------------

/// The internal anti-replay counter `(epoch, seq)`. NEVER rendered to a user; the
/// agent consumes `expected`/`current` on a CONFLICT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct Generation {
    pub epoch: u64,
    pub seq: u64,
}

/// What an outcome refers to (every field optional — a policy flip has no skill/version).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct Affected {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// A `version_id` (commit SHA-256, lowercase hex) when the outcome refers to a version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<String>,
}

/// The ONE canonical receipt across all outcomes — the durable idempotency record (present even
/// on failures): one stable receipt per op, identical on retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct Receipt {
    /// Always `1` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// Client-minted UUIDv4, persisted before the first send.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    pub command: String,
    pub outcome: TerminalOutcome,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    /// The commit SHA-256 (lowercase hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: Option<String>,
    /// The byte-exact consent hash (lowercase hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<Generation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<Generation>,
    /// RFC 3339 timestamp (the plane stamps it; never an ambient clock in `topos-core`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "date-time")))]
    pub created_at: String,
    /// Outcome-specific extra detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// The flat wire error (rich `thiserror` enums stay internal). Carries a
/// stable code + retryability + the safe next actions; never raw SQL/git strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireError {
    /// A stable, machine-branchable error code, distinct from (and finer than) `outcome`. This is an
    /// **open, additive** string vocabulary — it is intentionally NOT a closed enum: new codes are
    /// added over time, and a consumer must treat an unrecognized `code` as the `outcome` it carries.
    pub code: String,
    pub outcome: TerminalOutcome,
    pub retryable: bool,
    #[serde(default)]
    pub affected: Affected,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<Generation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<Generation>,
    /// Structured context (no TTY prose to parse).
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub next_actions: Vec<NextAction>,
}

// ---------------------------------------------------------------------------------------------
// The `current` pointer wire body — public semantics {version_id, generation}; the scope binds
// workspace_id + skill_id. UNSIGNED: authority is the database row behind the pointer, integrity is
// the content-addressed version id re-verified byte-for-byte on apply.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PointerScope {
    pub workspace_id: String,
    pub skill_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct CurrentRecord {
    /// The full commit SHA-256 (lowercase hex) — the `version_id`. NOT the `bundle_digest`: the
    /// pointer names the version, and the commit transitively pins the bytes.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    pub generation: Generation,
}

/// The `current` pointer's wire body — the UNSIGNED document the plane serves at the pointer read,
/// embeds in OK receipts, and stores. It is unsigned: authority is the database row behind the pointer,
/// and integrity is the content-addressed `version_id` re-verified byte-for-byte on apply. A versioned
/// envelope; `ETag = "<epoch>.<seq>"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireCurrentRecord {
    /// Always `1` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    pub scope: PointerScope,
    pub record: CurrentRecord,
}

// ---------------------------------------------------------------------------------------------
// Harness adapter report types — the frozen harness-INDEPENDENT unit. The trait
// itself lives in `topos-harness`; these wire/report DTOs live here.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub enum HarnessId {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "openclaw")]
    OpenClaw,
    #[serde(rename = "hermes")]
    Hermes,
}

impl HarnessId {
    /// The stable registry slug for this harness — matches `topos-harness`'s baked registry (which mirrors
    /// the `vercel-labs/skills` ecosystem slugs), so an adapter-backed skill's `harness_slug` agrees with a
    /// discovered one. Note Hermes's ecosystem slug is `hermes-agent` (the [`HarnessId`] serde rename stays
    /// the shorter `hermes` for wire back-compat — the two are deliberately distinct).
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            HarnessId::ClaudeCode => "claude-code",
            HarnessId::OpenClaw => "openclaw",
            HarnessId::Hermes => "hermes-agent",
        }
    }
}

/// What fires currency for a harness — drives HONEST receipt copy ("current by next session",
/// "next topos touch", …). `ExplicitPullOnly` is the honest-degrade floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CurrencyKind {
    SessionStart,
    FirstToposTouch,
    FirstTurn,
    ExplicitPullOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TriggerState {
    Active,
    Inactive,
    Degraded,
    /// An equivalent command exists without our marker — adopt-or-leave, never blind-append.
    AlreadyPresentUnmanaged,
}

/// The result of installing a currency trigger — what was touched + the marker, so re-install is
/// idempotent (the marker is a managed sentinel block in the config edit, NEVER in skill bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct TriggerReport {
    pub harness: HarnessId,
    pub currency_kind: CurrencyKind,
    /// The config file edited (never a skill dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touched_path: Option<String>,
    /// `topos` + harness id + schema version + command identity.
    pub marker_id: String,
    pub state: TriggerState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_code_round_trips_known_and_unknown() {
        let known = ActionCode::ProposePublish;
        let j = serde_json::to_string(&known).unwrap();
        assert_eq!(j, "\"PROPOSE_PUBLISH\"");
        assert_eq!(serde_json::from_str::<ActionCode>(&j).unwrap(), known);

        // A forward-compatible code this build doesn't know is preserved, not rejected.
        let unknown: ActionCode = serde_json::from_str("\"FUTURE_FLOW\"").unwrap();
        assert!(matches!(&unknown, ActionCode::Unknown(u) if u.as_str() == "FUTURE_FLOW"));
        assert_eq!(serde_json::to_string(&unknown).unwrap(), "\"FUTURE_FLOW\"");
    }

    #[test]
    fn action_code_unknown_cannot_alias_a_known_code() {
        // Deserializing a known string ALWAYS canonicalizes to its variant — never an `Unknown`
        // that would compare unequal yet serialize identically. The private inner field makes the
        // aliasing `Unknown` unconstructible from outside the crate.
        assert_eq!(
            serde_json::from_str::<ActionCode>("\"RETRY\"").unwrap(),
            ActionCode::Retry
        );
        assert_eq!(
            ActionCode::from("REQUEST_ACCESS".to_owned()),
            ActionCode::RequestAccess
        );
        // Every advertised known code parses to a known (non-`Unknown`) variant.
        for code in KNOWN_ACTION_CODES {
            assert!(!matches!(
                ActionCode::from(code.to_owned()),
                ActionCode::Unknown(_)
            ));
        }
    }

    #[test]
    fn terminal_outcome_is_screaming_snake() {
        assert_eq!(
            serde_json::to_string(&TerminalOutcome::AmbiguousName).unwrap(),
            "\"AMBIGUOUS_NAME\""
        );
    }

    #[test]
    fn envelope_serializes_minimally() {
        let env = JsonEnvelope {
            schema_version: WIRE_SCHEMA_VERSION,
            command: "pull".to_owned(),
            ok: true,
            data: serde_json::json!({}),
            warnings: vec![],
            next_actions: vec![],
            receipt: None,
            error: None,
        };
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["command"], "pull");
        assert_eq!(v["ok"], true);
        // optional receipt/error are omitted when None
        assert!(v.get("error").is_none());
    }
}
