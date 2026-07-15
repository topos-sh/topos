//! The validated identifier newtypes — the parse-don't-validate boundary.
//!
//! Every authority operation takes these *already-parsed* types, never raw strings, so a malformed
//! id can never reach an SQL bind or a filesystem path. The vault treats every id as an OPAQUE
//! string minted by its one caller (the app): validation is **shape only** — charset and length,
//! matching the schema's CHECK constraints — never meaning. [`WorkspaceId`] and [`OpId`] double as
//! filesystem **directory components** (the per-workspace git store, the per-op quarantine), so
//! their shape rule additionally forbids anything that could escape or collide with a reserved
//! sibling (a leading dot).

use core::fmt;

/// The maximum length of a textual id, in bytes — the schema CHECKs pin the same bound. A bound at
/// all keeps a pathological input from becoming an unbounded key or path component.
const MAX_ID_LEN: usize = 128;

/// The maximum length of an attribution display string, in characters — the schema CHECKs on
/// `author_display` / `moved_by_display` pin the same bound.
const MAX_ATTRIBUTION_CHARS: usize = 200;

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
    /// The id began with a dot — reserved (and a path hazard for the ids that become directories).
    #[error("identifier starts with a reserved character")]
    LeadingDot,
}

/// A workspace identifier — the hard tenant scope and a per-workspace git-store **path component**.
///
/// Shape: `[A-Za-z0-9._-]`, 1–128 bytes, no leading dot — the schema CHECK's charset with the two
/// path hazards carved out (`.`/`..` and the reserved `.quarantine` sibling both need a leading
/// dot; `/`, `\`, NUL and every other separator are outside the charset), so [`Self::as_str`] is
/// always safe to join onto the confined store root. The id is app-minted and opaque; isolation is
/// the `workspace_id` database binding (every row and query carries it) — the path rule only keeps
/// the physical store inside its root. Residual: on a case-INSENSITIVE filesystem two ids differing
/// only by case would share a physical store directory; app-minted random ids never collide this
/// way in practice, and the database rows stay distinct regardless.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(String);

/// A bundle identifier — custody's key: a database key, never a path component on the plane. The
/// app maps names and kinds onto this id; the vault never learns what kind of bundle it holds.
/// Same shape rule as [`WorkspaceId`] (one rule for every opaque id).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BundleId(String);

/// An operation identifier — the vault-minted id of one in-flight write (quarantine + promotion
/// lease + the `upload` audit row). Minted fresh per ingest (never client-supplied), and used as a
/// **directory component** of the per-op quarantine objdir the janitor `rm -rf`s — hence the same
/// no-leading-dot path-safe shape as [`WorkspaceId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpId(String);

/// `[A-Za-z0-9._-]` — the opaque-id charset, matching the schema CHECKs. ASCII-only: any non-ASCII
/// byte has the high bit set and fails this test, so no separate check is needed.
fn is_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

fn validate(s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    if !s.bytes().all(is_id_char) {
        return Err(IdError::DisallowedChar);
    }
    // A leading dot is reserved: it excludes `.`/`..` (path traversal) and keeps the
    // `git_root/.quarantine/` sibling collision-free against any workspace id.
    if s.starts_with('.') {
        return Err(IdError::LeadingDot);
    }
    Ok(())
}

impl WorkspaceId {
    /// Parse a workspace id, rejecting anything outside the opaque-id shape.
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, carries a disallowed character, or starts with a dot.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice — safe to use as a single path component.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl BundleId {
    /// Parse a bundle id, rejecting anything outside the opaque-id shape.
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, carries a disallowed character, or starts with a dot.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl OpId {
    /// Parse an operation id (vault-minted; re-parsed when read back from a stored row before any
    /// destructive path is built from it).
    ///
    /// # Errors
    /// [`IdError`] if the id is empty, too long, carries a disallowed character, or starts with a dot.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// The id as a string slice — safe to use as a single path component.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validate an attribution display string (the app's pass-through `author_display` /
/// `moved_by_display`): non-empty, at most 200 characters (the schema CHECK's bound), and free of
/// control characters. The CONTENT is never interpreted — the vault stores it verbatim.
///
/// # Errors
/// [`IdError::Empty`] / [`IdError::TooLong`] / [`IdError::DisallowedChar`] on a shape violation.
pub fn validate_attribution(s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.chars().count() > MAX_ATTRIBUTION_CHARS {
        return Err(IdError::TooLong);
    }
    if s.chars().any(char::is_control) {
        return Err(IdError::DisallowedChar);
    }
    Ok(())
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for BundleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A commit id (= `version_id`) — the kernel `commit_id`, a byte-exact sha256. Stored as lowercase
/// hex in the app-facing tables; rendered/parsed at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommitId(pub [u8; 32]);

/// An object id — the `blob_id = sha256(raw bytes)` of one stored file. Stored as a 32-byte BYTEA
/// in the custody-internal tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub [u8; 32]);

impl CommitId {
    /// The raw 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The 64-char lowercase-hex spelling (the app-facing column value + the wire form).
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex32(&self.0)
    }

    /// Parse EXACTLY 64 lowercase-hex characters. `None` on any other shape — callers map that to
    /// the uniform not-found (a non-canonical spelling is simply not a known id).
    #[must_use]
    pub fn parse_hex(s: &str) -> Option<Self> {
        parse_hex32(s).map(Self)
    }
}

impl ObjectId {
    /// The raw 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The 64-char lowercase-hex spelling.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex32(&self.0)
    }

    /// Parse EXACTLY 64 lowercase-hex characters (`None` otherwise, mapped to the uniform not-found).
    #[must_use]
    pub fn parse_hex(s: &str) -> Option<Self> {
        parse_hex32(s).map(Self)
    }
}

/// Lowercase-hex encode 32 bytes (the one spelling every app-facing id column carries).
pub(crate) fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use core::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Parse EXACTLY 64 lowercase-hex characters into 32 bytes. `None` on any other length or a
/// non-lowercase-hex byte.
pub(crate) fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_path_traversal_and_reserved_shapes() {
        for bad in [
            "", ".", "..", ".hidden", "a/b", "a\\b", "a b", "a\0b", "wörk",
        ] {
            assert!(WorkspaceId::parse(bad).is_err(), "should reject {bad:?}");
            assert!(BundleId::parse(bad).is_err(), "should reject {bad:?}");
            assert!(OpId::parse(bad).is_err(), "should reject {bad:?}");
        }
        // The schema charset is accepted verbatim, mixed case and inner dots included (opaque,
        // app-minted ids — shape, never meaning).
        for ok in ["w", "w_abc123", "ws-01", "a.b", "A_B-9", "Xy7"] {
            assert!(WorkspaceId::parse(ok).is_ok(), "should accept {ok:?}");
            assert!(BundleId::parse(ok).is_ok(), "should accept {ok:?}");
            assert!(OpId::parse(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn ids_reject_overlong() {
        let long = "w".repeat(MAX_ID_LEN + 1);
        assert_eq!(WorkspaceId::parse(&long), Err(IdError::TooLong));
        let max = "w".repeat(MAX_ID_LEN);
        assert!(WorkspaceId::parse(&max).is_ok());
    }

    #[test]
    fn attribution_is_shape_checked_never_interpreted() {
        assert!(validate_attribution("Alice Chen (alice)").is_ok());
        assert!(validate_attribution("日本語の名前").is_ok()); // any printable UTF-8
        assert!(validate_attribution("").is_err());
        assert!(validate_attribution("a\nb").is_err());
        assert!(validate_attribution(&"x".repeat(201)).is_err());
        assert!(validate_attribution(&"x".repeat(200)).is_ok());
    }

    #[test]
    fn hex_round_trip_and_rejects() {
        let id = CommitId([0xab; 32]);
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(CommitId::parse_hex(&hex), Some(id));
        assert_eq!(CommitId::parse_hex(&hex.to_uppercase()), None);
        assert_eq!(CommitId::parse_hex("ab"), None);
        assert_eq!(CommitId::parse_hex(&"g".repeat(64)), None);
    }
}
