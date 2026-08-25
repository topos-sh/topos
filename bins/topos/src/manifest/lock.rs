//! `topos.lock` — the committed record of exactly which versions a project runs.
//!
//! `topos.toml` states intent (follow or pin); the lock records what "follow" currently
//! resolves to, one alphabetically-sorted block per entry, deterministic bytes, no timestamps —
//! so two branches updating different bundles merge cleanly, and a conflict regenerates with
//! `topos update`.
//!
//! ```toml
//! # Written by topos — the exact versions this project runs; commit it
//! schema = 1
//! workspace = "topos.sh/acme"
//!
//! [channels.backend]
//! members = ["deploy-checklist", "lint-rules"]
//!
//! [mcp.linear]
//! revision = "mcpr_8f4e21c9"
//!
//! [skills.code-review]
//! version = "3a1f…(64 hex)"
//!
//! [skills.deploy-checklist]
//! version = "ab12…"
//! via = "backend"
//!
//! [skills.find-skills]
//! source = "github:vercel-labs/skills"
//! commit = "9d0e8c1c2f4a5b6d7e8f90a1b2c3d4e5f6a7b8c9"
//! ```
//!
//! The lock is written by `topos install` (filling entries that are missing — npm-install
//! semantics: an existing entry is never bumped) and rewritten by `topos update`. The background
//! sweep only ever CONVERGES to it. Reading tolerates unknown sections exactly as the manifest
//! does (a newer topos may know more kinds); a newer `schema` refuses toward `topos self-update`.

use std::collections::BTreeMap;
use std::path::Path;

use toml_edit::{DocumentMut, Item, Value};

use crate::error::ClientError;
use crate::fs_seam::FsOps;

/// The lock format this build reads and writes.
const SCHEMA: i64 = 1;

/// The file's name, beside `topos.toml`.
pub(crate) const LOCK_FILE: &str = "topos.lock";

/// One skill entry: a workspace version OR a repo source+commit; `via` names the channel that
/// brings it when no explicit row does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LockSkill {
    /// The 64-hex version digest (workspace bundles).
    pub version: Option<String>,
    /// `github:<owner>/<repo>` (repo skills).
    pub source: Option<String>,
    /// The resolved commit (repo skills).
    pub commit: Option<String>,
    /// The channel that resolved this entry, when a channel (not an explicit row) brings it.
    pub via: Option<String>,
}

/// The parsed lock: entries by bare name, deterministic order by construction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LockDoc {
    /// The project's workspace address, mirrored from `topos.toml` at write time.
    pub workspace: Option<String>,
    pub skills: BTreeMap<String, LockSkill>,
    /// MCP servers: name → catalog revision id.
    pub mcp: BTreeMap<String, String>,
    /// Channels: name → the frozen member list (bare names; each member has its own entry).
    pub channels: BTreeMap<String, Vec<String>>,
    /// One line per unknown section skipped — surfaced, never fatal.
    pub warnings: Vec<String>,
}

/// A typed lock refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockError {
    pub message: String,
}

fn err(message: impl Into<String>) -> LockError {
    LockError {
        message: message.into(),
    }
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl LockDoc {
    /// Parse a lock text. Unknown sections skip with a warning; a newer schema refuses.
    ///
    /// # Errors
    /// Invalid TOML, a newer `schema`, or a malformed entry inside a known section.
    pub(crate) fn parse(text: &str) -> Result<Self, LockError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e| err(format!("topos.lock is not valid TOML: {e}")))?;
        let mut out = LockDoc::default();
        for (key, item) in doc.iter() {
            match key {
                "schema" => {
                    let v = item
                        .as_integer()
                        .ok_or_else(|| err("topos.lock: `schema` is a number"))?;
                    if v > SCHEMA {
                        return Err(err(format!(
                            "topos.lock is `schema = {v}` — written by a newer topos than this \
                             one reads; run `topos self-update`",
                        )));
                    }
                }
                "workspace" => {
                    out.workspace = item.as_str().map(str::to_owned);
                }
                "skills" => {
                    let t = section_table(item, "skills")?;
                    for (name, entry) in t.iter() {
                        let e = entry_table(entry, "skills", name)?;
                        let get = |k: &str| e.get(k).and_then(Item::as_str).map(str::to_owned);
                        let skill = LockSkill {
                            version: get("version"),
                            source: get("source"),
                            commit: get("commit"),
                            via: get("via"),
                        };
                        if skill.version.is_none() && skill.commit.is_none() {
                            return Err(err(format!(
                                "topos.lock: [skills.{name}] records no `version` and no \
                                 `commit` — delete the block and run `topos update {name}`",
                            )));
                        }
                        out.skills.insert(name.to_owned(), skill);
                    }
                }
                "mcp" => {
                    let t = section_table(item, "mcp")?;
                    for (name, entry) in t.iter() {
                        let e = entry_table(entry, "mcp", name)?;
                        let Some(rev) = e.get("revision").and_then(Item::as_str) else {
                            return Err(err(format!(
                                "topos.lock: [mcp.{name}] records no `revision` — delete the \
                                 block and run `topos update {name}`",
                            )));
                        };
                        out.mcp.insert(name.to_owned(), rev.to_owned());
                    }
                }
                "channels" => {
                    let t = section_table(item, "channels")?;
                    for (name, entry) in t.iter() {
                        let e = entry_table(entry, "channels", name)?;
                        let Some(arr) = e
                            .get("members")
                            .and_then(Item::as_value)
                            .and_then(Value::as_array)
                        else {
                            return Err(err(format!(
                                "topos.lock: [channels.{name}] records no `members` array — \
                                 delete the block and run `topos update {name}`",
                            )));
                        };
                        let mut members = Vec::new();
                        for m in arr.iter() {
                            members.push(
                                m.as_str()
                                    .ok_or_else(|| {
                                        err(format!(
                                            "topos.lock: [channels.{name}] members are strings"
                                        ))
                                    })?
                                    .to_owned(),
                            );
                        }
                        out.channels.insert(name.to_owned(), members);
                    }
                }
                other => out.warnings.push(format!(
                    "topos.lock also uses [{other}] — this topos does not read it; \
                     `topos self-update` gets it",
                )),
            }
        }
        Ok(out)
    }

    /// Serialize deterministically: header, schema, workspace, then `[channels.*]`, `[mcp.*]`,
    /// `[skills.*]` blocks, each section's names sorted (the maps are sorted by construction).
    ///
    /// The header names NO command. Three different runs write this file — `topos install` (which
    /// fills the entries a row lacks), `topos update` (which re-resolves them), and the silent
    /// sweep behind an agent's hook — and one serializer serves all three, so any verb spelled
    /// here is wrong for two of them. It said "Generated by `topos update`" on files `topos
    /// install` had written. What the reader needs from a header is what the file IS and what to
    /// do with it, and both survive without a verb.
    pub(crate) fn serialize(&self) -> String {
        let mut out = String::from(
            "# Written by topos — the exact versions this project runs; commit it\nschema = 1\n",
        );
        if let Some(ws) = &self.workspace {
            out.push_str(&format!("workspace = \"{ws}\"\n"));
        }
        for (name, members) in &self.channels {
            out.push_str(&format!("\n[channels.{}]\n", quoted(name)));
            let list = members
                .iter()
                .map(|m| format!("\"{m}\""))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("members = [{list}]\n"));
        }
        for (name, rev) in &self.mcp {
            out.push_str(&format!("\n[mcp.{}]\n", quoted(name)));
            out.push_str(&format!("revision = \"{rev}\"\n"));
        }
        for (name, skill) in &self.skills {
            out.push_str(&format!("\n[skills.{}]\n", quoted(name)));
            if let Some(v) = &skill.version {
                out.push_str(&format!("version = \"{v}\"\n"));
            }
            if let Some(s) = &skill.source {
                out.push_str(&format!("source = \"{s}\"\n"));
            }
            if let Some(c) = &skill.commit {
                out.push_str(&format!("commit = \"{c}\"\n"));
            }
            if let Some(via) = &skill.via {
                out.push_str(&format!("via = \"{via}\"\n"));
            }
        }
        out
    }
}

/// Move ONE workspace-bundle entry of the lock at `dir` to `version` — the way a commit moves
/// `HEAD`: a publish (or a revert) from inside this project makes the new version the one the
/// project runs, and the lock says so at once rather than after a separate `topos update`.
/// Touches nothing else in the file: the edit is made IN the document as it stands — that one
/// `version` string, its decor kept — never a re-serialization of the typed view, which would
/// drop the sections and fields a newer topos wrote and this one only tolerates (`parse` skips
/// them with a warning by design). `Ok(false)` when there is no lock, no entry for `name`, or
/// the entry records a repo commit rather than a workspace version — nothing to move.
///
/// # Errors
/// A lock that does not parse (typed, naming the file), or a read/write fault.
pub(crate) fn advance_entry(
    fs: &dyn FsOps,
    dir: &Path,
    name: &str,
    version: &str,
) -> Result<bool, ClientError> {
    let path = dir.join(LOCK_FILE);
    let Some(bytes) = fs.read_opt(&path)? else {
        return Ok(false);
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // The typed read is the shape gate (a newer schema refuses; a malformed entry is named).
    let typed = LockDoc::parse(&text)
        .map_err(|e| ClientError::InvalidArgument(format!("{}: {}", path.display(), e.message)))?;
    let Some(entry) = typed.skills.get(name) else {
        return Ok(false);
    };
    if entry.version.is_none() {
        return Ok(false);
    }
    if entry.version.as_deref() == Some(version) {
        return Ok(true);
    }
    let mut doc: DocumentMut = text.parse().map_err(|e| {
        ClientError::InvalidArgument(format!(
            "{}: topos.lock is not valid TOML: {e}",
            path.display()
        ))
    })?;
    let Some(slot) = doc
        .get_mut("skills")
        .and_then(|skills| skills.get_mut(name))
        .and_then(|entry| entry.get_mut("version"))
        .and_then(Item::as_value_mut)
    else {
        return Ok(false);
    };
    let decor = slot.decor().clone();
    let mut next = Value::from(version);
    *next.decor_mut() = decor;
    *slot = next;
    crate::atomic::atomic_write(fs, &path, doc.to_string().as_bytes())?;
    Ok(true)
}

/// Spell a block name: bare where TOML allows it, quoted otherwise.
fn quoted(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if bare {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

fn section_table<'a>(item: &'a Item, section: &str) -> Result<&'a toml_edit::Table, LockError> {
    item.as_table().ok_or_else(|| {
        err(format!(
            "topos.lock: `[{section}]` holds one block per entry"
        ))
    })
}

fn entry_table<'a>(
    item: &'a Item,
    section: &str,
    name: &str,
) -> Result<&'a toml_edit::Table, LockError> {
    item.as_table().ok_or_else(|| {
        err(format!(
            "topos.lock: `[{section}.{name}]` is a block of fields, one per line",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "0123456789abcdef".repeat(4)
    }

    /// A publish (or a revert) from inside a project moves the lock's entry the way a commit
    /// moves `HEAD`: that one line, nothing else — and only a workspace-version entry that
    /// exists.
    #[test]
    fn a_pointer_move_advances_exactly_one_entry() {
        let fs = crate::fs_seam::RealFs;
        let dir = std::env::temp_dir().join(format!(
            "topos-lock-advance-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // No lock at all: nothing to move.
        assert!(!advance_entry(&fs, &dir, "deploy-checklist", &"a".repeat(64)).unwrap());

        let doc = sample();
        std::fs::write(dir.join(LOCK_FILE), doc.serialize()).unwrap();
        let before = std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap();

        // An entry the lock does not carry, and a repo-commit entry: untouched, file byte-same.
        assert!(!advance_entry(&fs, &dir, "unknown", &"a".repeat(64)).unwrap());
        assert!(!advance_entry(&fs, &dir, "find-skills", &"a".repeat(64)).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap(),
            before
        );

        // The workspace-version entry moves; every other line stands.
        assert!(advance_entry(&fs, &dir, "deploy-checklist", &"a".repeat(64)).unwrap());
        let after = LockDoc::parse(&std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap()).unwrap();
        assert_eq!(
            after.skills["deploy-checklist"].version.as_deref(),
            Some("a".repeat(64).as_str())
        );
        let mut expected = doc.clone();
        expected.skills.get_mut("deploy-checklist").unwrap().version = Some("a".repeat(64));
        assert_eq!(after, expected);
        // Already there: reported as standing, nothing rewritten.
        let text = std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap();
        assert!(advance_entry(&fs, &dir, "deploy-checklist", &"a".repeat(64)).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap(), text);

        // A lock a NEWER topos wrote — a section this one only tolerates, a field it does not
        // know, its own spacing — round-trips byte for byte except the one value moved: the
        // edit is made in the document, never by re-serializing what this build understands.
        let newer = format!(
            "# Written by topos — the exact versions this project runs; commit it\nschema = \
             1\nworkspace = \"topos.sh/acme\"\n\n\n[memories.onboarding]\n\
             revision = \"mem_1\"\nkind = \"memory\"\n\n[skills.deploy-checklist]\nversion = \
             \"{}\"   # pinned by hand\nvia = \"backend\"\nchecksum = \"sha256:abc\"\n",
            "b".repeat(64)
        );
        std::fs::write(dir.join(LOCK_FILE), &newer).unwrap();
        assert!(advance_entry(&fs, &dir, "deploy-checklist", &"c".repeat(64)).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap(),
            newer.replace(&"b".repeat(64), &"c".repeat(64)),
            "only the version moved; the [memories] section and the unknown field survive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample() -> LockDoc {
        let mut d = LockDoc {
            workspace: Some("topos.sh/acme".into()),
            ..LockDoc::default()
        };
        d.skills.insert(
            "code-review".into(),
            LockSkill {
                version: Some(digest()),
                ..LockSkill::default()
            },
        );
        d.skills.insert(
            "deploy-checklist".into(),
            LockSkill {
                version: Some(digest()),
                via: Some("backend".into()),
                ..LockSkill::default()
            },
        );
        d.skills.insert(
            "find-skills".into(),
            LockSkill {
                source: Some("github:vercel-labs/skills".into()),
                commit: Some("9d0e8c1c2f4a5b6d".into()),
                ..LockSkill::default()
            },
        );
        d.mcp.insert("linear".into(), "mcpr_8f4e21c9".into());
        d.channels
            .insert("backend".into(), vec!["deploy-checklist".into()]);
        d
    }

    #[test]
    fn serialize_parse_round_trips_and_is_deterministic() {
        let doc = sample();
        let text = doc.serialize();
        let back = LockDoc::parse(&text).unwrap();
        assert_eq!(back.workspace, doc.workspace);
        assert_eq!(back.skills, doc.skills);
        assert_eq!(back.mcp, doc.mcp);
        assert_eq!(back.channels, doc.channels);
        assert_eq!(text, LockDoc::parse(&text).unwrap().serialize());
        // Sorted blocks: channels, then mcp, then skills; names alphabetical.
        let c = text.find("[channels.backend]").unwrap();
        let m = text.find("[mcp.linear]").unwrap();
        let s1 = text.find("[skills.code-review]").unwrap();
        let s2 = text.find("[skills.deploy-checklist]").unwrap();
        assert!(c < m && m < s1 && s1 < s2, "{text}");
        assert!(!text.contains("20"), "no timestamps: {text}");
    }

    #[test]
    fn unknown_sections_warn_and_a_newer_schema_refuses() {
        let doc = LockDoc::parse("schema = 1\n\n[memories.ctx]\nversion = \"abc\"\n").unwrap();
        assert!(doc.skills.is_empty());
        assert_eq!(doc.warnings.len(), 1);
        assert!(
            doc.warnings[0].contains("[memories]"),
            "{}",
            doc.warnings[0]
        );
        let e = LockDoc::parse("schema = 2\n").unwrap_err();
        assert!(e.message.contains("self-update"), "{e}");
    }

    #[test]
    fn malformed_entries_name_their_fix() {
        let e = LockDoc::parse("[skills.x]\nvia = \"backend\"\n").unwrap_err();
        assert!(e.message.contains("topos update x"), "{e}");
        let e = LockDoc::parse("[mcp.x]\nversion = \"abc\"\n").unwrap_err();
        assert!(e.message.contains("no `revision`"), "{e}");
        let e = LockDoc::parse("[channels.x]\nrevision = \"abc\"\n").unwrap_err();
        assert!(e.message.contains("no `members`"), "{e}");
        assert!(LockDoc::parse("not [ toml").is_err());
    }

    #[test]
    fn odd_names_quote_and_round_trip() {
        let mut d = LockDoc::default();
        d.mcp.insert("weird.name".into(), "mcpr_1".into());
        let text = d.serialize();
        assert!(text.contains("[mcp.\"weird.name\"]"), "{text}");
        assert_eq!(LockDoc::parse(&text).unwrap().mcp, d.mcp);
    }
}
