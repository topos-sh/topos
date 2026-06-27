//! The validated identifier newtypes — the parse-don't-validate boundary.
//!
//! Every authority operation takes these *already-parsed* types, never raw strings, so a malformed
//! id can never reach an SQL bind or a filesystem path. [`WorkspaceId`] is the strictest: it is used
//! as a directory component for the per-workspace git store, so it admits only a path-safe charset
//! (no separators, no `..`, no dots, no control bytes) — isolation is the database binding, but the
//! store path must still never escape its confined root.

use core::fmt;

/// The maximum length of a textual id, in bytes. Far beyond any real `w_…`/`s_…`/principal id; a
/// bound at all keeps a pathological input from becoming an unbounded key or path component.
const MAX_ID_LEN: usize = 128;

/// A rejected identifier — produced only at the parse boundary, never inside authority logic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdError {
    /// The id was empty.
    #[error("identifier is empty")]
    Empty,
    /// The id exceeded the maximum length.
    #[error("identifier exceeds the maximum length")]
    TooLong,
    /// The id contained a byte outside the type's permitted set.
    #[error("identifier contains a disallowed character")]
    DisallowedChar,
}

/// A workspace identifier — the hard tenant scope and a per-workspace git-store **path component**.
///
/// Admits only `[A-Za-z0-9_-]` (the `w_…` id shape), which by construction excludes `/`, `\`, `.`,
/// `..`, NUL, and every other path metacharacter — so [`Self::as_str`] is always safe to join onto
/// the confined store root. Isolation itself is the `workspace_id` database binding (every row and
/// query carries it); the path-safety here only keeps the physical store from escaping its root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(String);

/// A skill identifier (the `s_…` id shape) — a database key, never a path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillId(String);

/// A principal identifier — the rostered reader/uploader identity (device id, account, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal(String);

/// `[A-Za-z0-9_-]` — the path-safe id charset (workspace + skill ids).
fn is_path_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// `is_path_safe` plus the characters real principal ids carry (email-like + device ids): `.@+`.
fn is_principal_char(b: u8) -> bool {
    is_path_safe(b) || matches!(b, b'.' | b'@' | b'+')
}

fn validate(s: &str, allowed: fn(u8) -> bool) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    // Byte-wise over UTF-8: every allowed byte is ASCII, so any multi-byte (non-ASCII) byte has the
    // high bit set and fails `allowed`, rejecting all non-ASCII without a separate check.
    if s.bytes().all(allowed) {
        Ok(())
    } else {
        Err(IdError::DisallowedChar)
    }
}

impl WorkspaceId {
    /// Parse a workspace id, rejecting anything outside the path-safe charset.
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, or contains a disallowed character.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s, is_path_safe)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice — safe to use as a single path component.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SkillId {
    /// Parse a skill id, rejecting anything outside the path-safe charset.
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, or contains a disallowed character.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s, is_path_safe)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Principal {
    /// Parse a principal id, admitting the id-plus-email charset `[A-Za-z0-9_-.@+]`.
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, or contains a disallowed character.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s, is_principal_char)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for SkillId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A commit id (= `version_id`) — the kernel `commit_id`, a byte-exact sha256. Stored as a 32-byte
/// BLOB; rendered to hex only at a display edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommitId(pub [u8; 32]);

/// An object id — the `blob_id = sha256(raw bytes)` of one stored file. Stored as a 32-byte BLOB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub [u8; 32]);

impl CommitId {
    /// The raw 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl ObjectId {
    /// The raw 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_rejects_path_traversal_and_separators() {
        for bad in [
            "", "..", ".", "a/b", "a\\b", "a.b", "a b", "a\0b", "wörk", "a\nb",
        ] {
            assert!(
                WorkspaceId::parse(bad).is_err(),
                "should reject workspace id {bad:?}"
            );
        }
        for ok in ["w", "w_abc123", "ws-01", "A_B-9"] {
            assert!(
                WorkspaceId::parse(ok).is_ok(),
                "should accept workspace id {ok:?}"
            );
        }
    }

    #[test]
    fn workspace_id_rejects_overlong() {
        let long = "w".repeat(MAX_ID_LEN + 1);
        assert_eq!(WorkspaceId::parse(&long), Err(IdError::TooLong));
        let max = "w".repeat(MAX_ID_LEN);
        assert!(WorkspaceId::parse(&max).is_ok());
    }

    #[test]
    fn principal_admits_email_shape_but_skill_does_not() {
        assert!(Principal::parse("dev@example.com").is_ok());
        assert!(Principal::parse("device+1_abc").is_ok());
        // The path-safe charset (skill/workspace) rejects the email metacharacters.
        assert!(SkillId::parse("dev@example.com").is_err());
        assert!(WorkspaceId::parse("a.b").is_err());
    }

    #[test]
    fn principal_still_rejects_separators_and_control() {
        // A principal is a database key (never a path component), so dots are fine (email shape);
        // only separators, control bytes, whitespace, and the empty string are rejected.
        for bad in ["", "a/b", "a\\b", "a\0b", "a b", "a\tb"] {
            assert!(
                Principal::parse(bad).is_err(),
                "should reject principal {bad:?}"
            );
        }
        for ok in ["a.b", "..", "dev@x.io"] {
            assert!(
                Principal::parse(ok).is_ok(),
                "should accept principal {ok:?}"
            );
        }
    }
}
