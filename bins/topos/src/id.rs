//! The validated-id boundary — parse-don't-validate for every id the client joins into a filesystem
//! path or splices into a URL path.
//!
//! A skill id keys `~/.topos/skills/<id>` (and the lock file, and the staging dir) **and** the harness
//! placement (`~/.claude/skills/<id>`), so under the TOFU model a plane-chosen string reaches a `join`.
//! [`SkillId`] mirrors the server's parse-don't-validate discipline (`plane-store`'s id newtypes): an id
//! is parsed ONCE at the boundary it enters the client — the redeem/bootstrap wire responses, the
//! persisted `follows.json` / enrollment-WAL loads, the local `skills/` directory names — and only the
//! already-parsed type reaches [`crate::sidecar::Layout`]'s path builders, so `"../../x"` can never
//! escape the sidecar or a harness skills dir.
//!
//! The charset is the **lowercase** path-safe set `[a-z0-9_-]` (stricter than the server's mixed-case
//! `SkillId`, which is a database key): on the client the id IS a directory component, and on a
//! case-insensitive filesystem (the macOS default) two case-only-distinct ids would fold to one physical
//! directory. It excludes `/`, `\`, `.` (so `.` / `..`), NUL, and every non-ASCII byte by construction.
//! Every real id fits: client-minted `topos_<hex>`, plane-minted `s_…`, and the plane's `w_…` workspace
//! ids (validated through the same rule before they are spliced into a request URL path).

use core::fmt;

use crate::error::ClientError;

/// The maximum id length, in bytes — far beyond any real `topos_<hex>` / `s_…` id; a bound at all keeps
/// a pathological wire value from becoming an unbounded path component (mirrors the server's cap).
const MAX_ID_LEN: usize = 128;

/// `[a-z0-9_-]` — the lowercase path-safe charset. Byte-wise over UTF-8: every allowed byte is ASCII, so
/// any non-ASCII byte fails and rejects all of Unicode without a separate check.
fn is_lower_path_safe(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
}

/// Whether `s` is a valid lowercase path-safe id (non-empty, bounded, charset-clean). The predicate
/// behind [`SkillId::parse`], shared by the workspace-id checks (a workspace id never becomes a client
/// path component, so it needs no newtype — but it IS spliced into URL paths, so it obeys the same rule).
pub(crate) fn is_valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID_LEN && s.bytes().all(is_lower_path_safe)
}

/// Validate a workspace id at a wire/persisted boundary. Same rule as [`SkillId::parse`]; the fixed
/// message never echoes the hostile bytes.
///
/// # Errors
/// [`ClientError::Corrupt`] (the malformed-document family — a plane/document that supplies such an id is
/// corrupt or forged, never a usable enrollment).
pub(crate) fn validate_workspace_id(s: &str) -> Result<(), ClientError> {
    if is_valid_id(s) {
        Ok(())
    } else {
        Err(ClientError::Corrupt(
            "workspace id is not a safe lowercase identifier".into(),
        ))
    }
}

/// A validated skill id — safe as a single path component (and as a URL path segment).
///
/// Constructed ONLY by [`SkillId::parse`], so holding one is proof the id passed the charset rule;
/// [`crate::sidecar::Layout`]'s path builders take `&SkillId`, never a raw `&str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SkillId(String);

impl SkillId {
    /// Parse a skill id, rejecting anything outside the lowercase path-safe charset (including empty,
    /// over-long, `.`/`..`, separators, uppercase, and non-ASCII). The fixed message never echoes the
    /// hostile bytes.
    ///
    /// # Errors
    /// [`ClientError::Corrupt`] — a document or wire response naming such an id is corrupt/forged.
    pub(crate) fn parse(s: &str) -> Result<Self, ClientError> {
        if is_valid_id(s) {
            Ok(Self(s.to_owned()))
        } else {
            Err(ClientError::Corrupt(
                "skill id is not a safe lowercase identifier".into(),
            ))
        }
    }

    /// The id as a string slice — safe to use as a single path component.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner `String` (for a result-payload field).
    pub(crate) fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_id_rejects_traversal_separators_case_and_empty() {
        // The exact hostile shapes a compromised plane could return: traversal, separators, case-fold
        // collisions, and the degenerate empties.
        for bad in [
            "../../x", "a/b", "A", "", ".", "..", "a\\b", "a.b", "a b", "a\0b", "wörk", "a\nb",
            "Topos_X",
        ] {
            assert!(SkillId::parse(bad).is_err(), "should reject {bad:?}");
            assert!(!is_valid_id(bad), "predicate should reject {bad:?}");
        }
    }

    #[test]
    fn skill_id_accepts_every_real_shape() {
        for ok in [
            "topos_0af3c9d2b1e845f7a6c0d9e8b7a61234",
            "s_deploy",
            "w_acme",
            "a_b-9",
            "identity",
        ] {
            assert!(SkillId::parse(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn skill_id_rejects_overlong_but_accepts_max() {
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert!(SkillId::parse(&long).is_err());
        let max = "a".repeat(MAX_ID_LEN);
        assert!(SkillId::parse(&max).is_ok());
    }

    #[test]
    fn validation_failure_is_the_corrupt_family_with_a_fixed_message() {
        // The wire code stays CORRUPT_STATE (no new code), and the message never echoes hostile bytes.
        let err = SkillId::parse("../../etc").unwrap_err();
        assert_eq!(err.code(), "CORRUPT_STATE");
        assert!(!err.to_string().contains("etc"), "never echo the input");
        let err = validate_workspace_id("../w").unwrap_err();
        assert_eq!(err.code(), "CORRUPT_STATE");
    }
}
