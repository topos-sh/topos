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
/// Admits only `[a-z0-9_-]` (the `w_…` id shape), which by construction excludes `/`, `\`, `.`, `..`,
/// NUL, and every other path metacharacter — so [`Self::as_str`] is always safe to join onto the confined
/// store root. The **lowercase** restriction is load-bearing: the per-workspace store + quarantine dirs are
/// `git_root/<id>`, so on a case-insensitive filesystem two case-only-distinct ids would fold to one
/// physical directory and a per-workspace GC unlink could destroy another tenant's bytes. Isolation itself
/// is the `workspace_id` database binding (every row and query carries it); the path-safety here only keeps
/// the physical store from escaping its root and one tenant's store from colliding with another's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(String);

/// A skill identifier (the `s_…` id shape) — a database key, never a path component on the plane.
///
/// Lowercase `[a-z0-9_-]` only, matching the CLIENT's rule: there the id IS a directory component
/// (`~/.topos/skills/<id>`, the harness placement), and on a case-insensitive filesystem two
/// case-only-distinct ids would fold to one physical directory. Every real id is lowercase
/// (client-minted `topos_<hex>`, plane-minted `s_…`), so one charset at both ends means an id the plane
/// accepts is an id every client can hold — no id can be mintable server-side yet unrepresentable
/// client-side.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillId(String);

/// A principal identifier — the rostered reader/uploader identity (device id, account, …).
///
/// Parsing **canonicalizes**: the accepted charset is mixed-case (humans type `Alice@Acme.com`),
/// but the stored form is the kernel's ASCII-lowercase fold ([`topos_core::sign::canonical_principal`]) —
/// so one mailbox is ONE identity at every roster gate, seat write, and idempotency hash, however it
/// was cased at the edge. Every durable principal column holds this canonical form: values fold at
/// their parse edge before storage (ephemeral flow rows copied between tables inherit it), and
/// migration `0010` pins the invariant with `lower()` CHECKs on the roster tables.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal(String);

/// An operation identifier — the client-minted id of one in-flight write (quarantine + promotion lease).
///
/// Admits only the lowercase path-safe charset (`[a-z0-9_-]`, which excludes `.`, `..`, and every
/// separator), because it is used as a **directory component** of the per-op quarantine objdir the janitor
/// `rm -rf`s — a `..` or `/` would let the destructive sweep escape its root, and (like [`WorkspaceId`]) a
/// case-only-distinct id would fold to one directory on a case-insensitive filesystem. A v4-UUID op id
/// (lowercase hex + `-`) fits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpId(String);

/// `[A-Za-z0-9_-]` — the mixed-case path-safe base charset. Only [`is_principal_char`] builds on it now
/// (principals legitimately ARRIVE mixed-case — humans type emails — and are folded to lowercase at
/// parse); the id newtypes all use the lowercase rule below.
fn is_path_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// `[a-z0-9_-]` — the LOWERCASE path-safe charset for ids that become **filesystem directory components**
/// (`WorkspaceId`, `OpId`). Forbidding uppercase makes the id→path mapping injective on a case-INSENSITIVE
/// filesystem (macOS/Windows default): without it, two database-distinct ids differing only by case would
/// fold to one physical directory, and a per-workspace GC unlink could then destroy another tenant's bytes.
/// Real ids (the `w_…` shape, lowercase-hex op ids) are already lowercase, so this rejects nothing real.
fn is_lower_path_safe(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
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
    /// Parse a workspace id, rejecting anything outside the lowercase path-safe charset.
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, or contains a disallowed character (including uppercase).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s, is_lower_path_safe)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice — safe to use as a single path component.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SkillId {
    /// Parse a skill id, rejecting anything outside the lowercase path-safe charset (including
    /// uppercase — the client's directory-component rule, held at both ends).
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, or contains a disallowed character.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s, is_lower_path_safe)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Principal {
    /// Parse a principal id, admitting the id-plus-email charset `[A-Za-z0-9_-.@+]` and
    /// **canonicalizing to the kernel's ASCII-lowercase fold** (`Alice@Acme.com` parses as
    /// `alice@acme.com`; an already-lowercase principal — every device-rooted `dev.dk_…` id — is a
    /// fixpoint). The charset is ASCII-only, so the fold is total.
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, or contains a disallowed character.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s, is_principal_char)?;
        Ok(Self(topos_core::sign::canonical_principal(s)))
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl OpId {
    /// Parse an operation id, rejecting anything outside the lowercase path-safe charset (so it is always
    /// safe as a single quarantine directory component, on a case-insensitive filesystem too).
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, or contains a disallowed character (including uppercase).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s, is_lower_path_safe)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice — safe to use as a single path component.
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
impl fmt::Display for OpId {
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
    fn workspace_id_rejects_path_traversal_separators_and_uppercase() {
        for bad in [
            // Uppercase is rejected: it would fold to a colliding directory on a case-insensitive
            // filesystem, where a per-workspace GC unlink could destroy another tenant's bytes.
            "", "..", ".", "a/b", "a\\b", "a.b", "a b", "a\0b", "wörk", "a\nb", "A_B-9", "Work",
            "wA",
        ] {
            assert!(
                WorkspaceId::parse(bad).is_err(),
                "should reject workspace id {bad:?}"
            );
        }
        for ok in ["w", "w_abc123", "ws-01", "a_b-9"] {
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
    fn skill_id_rejects_uppercase_and_accepts_every_real_shape() {
        // The lowercase rule, held at both ends: the client stores a skill id as a directory component,
        // so a mixed-case id the plane accepted would be unrepresentable (or case-fold-colliding) there.
        for bad in ["S_deploy", "sD", "TOPOS_ABC", "Topos_X", "s.d", "s/d", ""] {
            assert!(
                SkillId::parse(bad).is_err(),
                "should reject skill id {bad:?}"
            );
        }
        for ok in [
            "s_deploy",
            "topos_0af3c9d2b1e845f7a6c0d9e8b7a61234",
            "a_b-9",
        ] {
            assert!(SkillId::parse(ok).is_ok(), "should accept skill id {ok:?}");
        }
    }

    #[test]
    fn op_id_rejects_path_traversal_separators_and_uppercase() {
        // op_id is an rm -rf'd directory component, so it must reject `.`/`..`/separators (a `..` would let
        // the quarantine janitor escape its root) and uppercase (a case-fold collision on a
        // case-insensitive filesystem) exactly as the workspace id does.
        for bad in [
            "", "..", ".", "a/b", "a\\b", "a.b", "a b", "a\0b", "a\nb", "A_B-9", "Op",
        ] {
            assert!(OpId::parse(bad).is_err(), "should reject op id {bad:?}");
        }
        for ok in [
            "op",
            "op_1",
            "a1b2c3d4-e5f6-7890-ab12-cd34ef567890",
            "a_b-9",
        ] {
            assert!(OpId::parse(ok).is_ok(), "should accept op id {ok:?}");
        }
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

    #[test]
    fn principal_parse_folds_to_the_canonical_lowercase_form() {
        // One mailbox, one identity: mixed-case input is ACCEPTED and stored folded, so every SQL
        // bind and every == against a stored row compares canonical bytes. Already-canonical
        // strings (all-lowercase emails, `dev.dk_…` device-rooted ids) are fixpoints.
        assert_eq!(
            Principal::parse("Alice@Acme.COM").unwrap().as_str(),
            "alice@acme.com"
        );
        assert_eq!(
            Principal::parse("alice@acme.com").unwrap().as_str(),
            "alice@acme.com"
        );
        assert_eq!(
            Principal::parse("Dev+Second@X.io").unwrap().as_str(),
            "dev+second@x.io"
        );
        // Case-variant inputs parse EQUAL — the dedup/idempotency property every set-build relies on.
        assert_eq!(
            Principal::parse("Alice@Acme.COM").unwrap(),
            Principal::parse("alice@acme.com").unwrap()
        );
    }
}
