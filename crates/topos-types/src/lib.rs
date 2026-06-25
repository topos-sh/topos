//! `topos-types` — the WIRE DTOs only.
//!
//! These are *deserialization shapes* for the boundary: the `--json` envelope, every per-verb
//! result shape, the frozen [`TerminalOutcome`] enum, the [`Receipt`] + [`WireError`], the
//! signed-`current` envelope, the [`ActionCode`] vocabulary, and the harness [`TriggerReport`].
//! **No logic.** The app libs parse these into `topos-core`'s validated domain newtypes at the
//! HTTP/CLI edge (parse-don't-validate), so `topos-core` never imports this crate.
//!
//! Naming: `version_id` = the commit SHA-256 (the user-facing
//! `<skill>@<version_id>`); `bundle_digest` = the byte-exact consent hash over the bundle's
//! files. The two are distinct values — never call both "digest."

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to a persisted/wire shape; every document carries it.
pub const SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------------------------
// The `--json` envelope (the single most important interface; the agent
// is the primary consumer). One JSON document on stdout, diagnostics on stderr only.
// ---------------------------------------------------------------------------------------------

/// The one common envelope every verb emits. Never prompts; prose (TTY) is rendered from the
/// SAME typed value where practical.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JsonEnvelope {
    pub schema_version: u32,
    /// The verb that produced this (`add`, `follow`, `pull`, `list`, `publish`, …).
    pub command: String,
    pub ok: bool,
    /// Command-specific payload (`{}` when empty).
    #[serde(default)]
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
// The frozen terminal-outcome set (all 11; APPROVAL_REQUIRED included).
// ---------------------------------------------------------------------------------------------

/// The closed set of terminal outcomes the agent branches on. SCREAMING_SNAKE on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TerminalOutcome {
    Ok,
    /// A direct `publish` under `review-required`: refused, uploads/opens **nothing**, carries the
    /// `publish --propose` next-action.
    ApprovalRequired,
    /// Returned only for a successfully-opened proposal (an explicit `--propose`).
    NeedsReview,
    Conflict,
    Diverged,
    Denied,
    Unavailable,
    AmbiguousName,
    KeyRepinRequired,
    RetryableFailure,
    PermanentFailure,
}

// ---------------------------------------------------------------------------------------------
// The next_actions action-code vocabulary — HYBRID: known variants + Unknown(String),
// so an old agent never *fails* on a perfectly executable future code (it has the argv).
// ---------------------------------------------------------------------------------------------

/// A machine-actionable next step. The `argv` is the ready-to-exec command; `code` lets an agent
/// branch on the known set and still pass through unknowns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NextAction {
    pub code: ActionCode,
    /// A complete argv array — execute as-is (no TTY parsing).
    pub argv: Vec<String>,
}

/// The action-code vocabulary. Known variants serialize to their SCREAMING_SNAKE string; an
/// unrecognized code round-trips through [`ActionCode::Unknown`]. Serialized as a plain string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionCode {
    ProposePublish,       // APPROVAL_REQUIRED
    RebaseAndRetry,       // CONFLICT
    ResolveDivergedDraft, // DIVERGED
    ApplyWaitingUpdate,   // `pull <skill>` — a previously observed-but-unapplied version
    DisambiguateName,     // AMBIGUOUS_NAME
    RepinPlaneKey,        // KEY_REPIN_REQUIRED
    RequestAccess,        // DENIED → invite/enroll
    Retry,                // RETRYABLE_FAILURE / UNAVAILABLE
    ContactAdmin,         // non-self-service denials
    /// A forward-compatible code this build doesn't know — execute the action's `argv` anyway.
    Unknown(String),
}

impl ActionCode {
    pub fn as_str(&self) -> &str {
        match self {
            ActionCode::ProposePublish => "PROPOSE_PUBLISH",
            ActionCode::RebaseAndRetry => "REBASE_AND_RETRY",
            ActionCode::ResolveDivergedDraft => "RESOLVE_DIVERGED_DRAFT",
            ActionCode::ApplyWaitingUpdate => "APPLY_WAITING_UPDATE",
            ActionCode::DisambiguateName => "DISAMBIGUATE_NAME",
            ActionCode::RepinPlaneKey => "REPIN_PLANE_KEY",
            ActionCode::RequestAccess => "REQUEST_ACCESS",
            ActionCode::Retry => "RETRY",
            ActionCode::ContactAdmin => "CONTACT_ADMIN",
            ActionCode::Unknown(s) => s,
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
            "REPIN_PLANE_KEY" => ActionCode::RepinPlaneKey,
            "REQUEST_ACCESS" => ActionCode::RequestAccess,
            "RETRY" => ActionCode::Retry,
            "CONTACT_ADMIN" => ActionCode::ContactAdmin,
            _ => ActionCode::Unknown(s),
        }
    }
}

impl From<ActionCode> for String {
    fn from(c: ActionCode) -> String {
        match c {
            ActionCode::Unknown(s) => s,
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

impl schemars::JsonSchema for ActionCode {
    fn schema_name() -> String {
        "ActionCode".to_owned()
    }
    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        // An open string set with known values — the agent matches the knowns, passes unknowns.
        String::json_schema(generator)
    }
}

// ---------------------------------------------------------------------------------------------
// Generation, Affected, Receipt, WireError (one canonical receipt + flat error).
// ---------------------------------------------------------------------------------------------

/// The internal anti-replay counter `(epoch, seq)`. NEVER rendered to a user; the
/// agent consumes `expected`/`current` on a CONFLICT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Generation {
    pub epoch: u64,
    pub seq: u64,
}

/// What an outcome refers to (every field optional — a policy flip has no skill/version).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Affected {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<String>,
}

/// The ONE canonical receipt across all 11 outcomes — the durable idempotency record (present even
/// on failures): one stable receipt per op, identical on retry.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Receipt {
    pub schema_version: u32,
    /// Client-minted UUIDv4, persisted before the first send.
    pub op_id: String,
    pub command: String,
    pub outcome: TerminalOutcome,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    /// The commit SHA-256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// The byte-exact consent hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<Generation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<Generation>,
    /// RFC 3339 timestamp (the plane stamps it; never an ambient clock in `topos-core`).
    pub created_at: String,
    /// The signing key id that covered this op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// Outcome-specific extra detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// The flat wire error (rich `thiserror` enums stay internal). Carries a
/// stable code + retryability + the safe next actions; never raw SQL/git strings.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WireError {
    /// A stable error code (distinct from the outcome; e.g. `STALE_BASE`, `OFF_ROSTER`).
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
// The signed `current` pointer envelope — public semantics {digest, generation},
// the signed preimage also binds workspace_id + skill_id (no cross-scope replay).
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PointerScope {
    pub workspace_id: String,
    pub skill_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CurrentRecord {
    /// The full commit SHA-256 (`version_id`).
    pub digest: String,
    pub generation: Generation,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Signature {
    /// Always `"Ed25519"` in v0.
    pub alg: String,
    pub key_id: String,
    /// base64url, raw 64-byte signature.
    pub value: String,
}

/// The signed `current` pointer — a versioned envelope; `ETag = "<epoch>.<seq>"`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SignedCurrentRecord {
    pub schema_version: u32,
    pub scope: PointerScope,
    pub record: CurrentRecord,
    pub signature: Signature,
}

// ---------------------------------------------------------------------------------------------
// Harness adapter report types — the frozen harness-INDEPENDENT unit. The trait
// itself lives in `topos-harness`; these wire/report DTOs live here.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum HarnessId {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "openclaw")]
    OpenClaw,
    #[serde(rename = "hermes")]
    Hermes,
}

/// What fires currency for a harness — drives HONEST receipt copy ("current by next session",
/// "next topos touch", …). `ExplicitPullOnly` is the honest-degrade floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CurrencyKind {
    SessionStart,
    FirstToposTouch,
    FirstTurn,
    ExplicitPullOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
        assert_eq!(unknown, ActionCode::Unknown("FUTURE_FLOW".to_owned()));
        assert_eq!(serde_json::to_string(&unknown).unwrap(), "\"FUTURE_FLOW\"");
    }

    #[test]
    fn terminal_outcome_is_screaming_snake() {
        assert_eq!(
            serde_json::to_string(&TerminalOutcome::ApprovalRequired).unwrap(),
            "\"APPROVAL_REQUIRED\""
        );
        assert_eq!(
            serde_json::to_string(&TerminalOutcome::KeyRepinRequired).unwrap(),
            "\"KEY_REPIN_REQUIRED\""
        );
    }

    #[test]
    fn envelope_serializes_minimally() {
        let env = JsonEnvelope {
            schema_version: SCHEMA_VERSION,
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
