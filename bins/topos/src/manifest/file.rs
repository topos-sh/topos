//! `topos.toml` — the manifest FILE: read, edit, write. Format-preserving (toml_edit — the
//! cargo-proven editor), so a hand-written manifest's comments and layout survive every
//! `topos add`/`remove`.
//!
//! The shape (boring, npm-adjacent — keys are CANONICAL references, values are version specs):
//!
//! ```toml
//! exclude = ["topos.sh/acme/noisy-skill"]   # top-level, above the tables
//!
//! [skills]
//! "topos.sh/acme/code-review" = "*"                       # track current
//! "topos.sh/acme/deploy" = "9f2c…"                        # pinned to a version digest
//! "github.com/vercel-labs/skills/find-skills" = "3aa01e2" # external: pinned to a commit
//! "./tools/my-skill" = "*"                                # a local folder (this repo)
//!
//! [channels]
//! "topos.sh/acme/channels/backend" = "*"
//!
//! [placement]                       # optional, explicit — one location every agent reads
//! "topos.sh/acme/code-review" = ".claude/skills"
//! [placement.kind]
//! skill = ".agents/skills"
//! ```

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::ClientError;
use crate::fs_seam::FsOps;

/// The manifest's one filename, at a project root (or any nested folder) and in `~/.topos/`.
pub(crate) const MANIFEST_FILE: &str = "topos.toml";

/// One include line: a canonical reference + its version spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestEntry {
    /// The canonical reference (the TOML key).
    pub reference: String,
    /// `None` = `"*"` (track current); `Some(pin)` = a version digest / commit.
    pub pin: Option<String>,
}

/// A parsed manifest: the include lines, the excludes, and the placement overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub skills: Vec<ManifestEntry>,
    pub channels: Vec<ManifestEntry>,
    pub exclude: Vec<String>,
    /// Per-reference placement pins: canonical ref → a project-relative directory.
    pub placement: Vec<(String, String)>,
    /// Per-kind placement pins: kind name → a project-relative directory.
    pub placement_kind: Vec<(String, String)>,
}

impl Manifest {
    /// Whether nothing at all is declared (an empty file / a fresh `init`).
    pub(crate) fn is_empty(&self) -> bool {
        self.skills.is_empty()
            && self.channels.is_empty()
            && self.exclude.is_empty()
            && self.placement.is_empty()
            && self.placement_kind.is_empty()
    }
}

fn corrupt(path: &Path, what: impl std::fmt::Display) -> ClientError {
    ClientError::Corrupt(format!("{}: {what}", path.display()))
}

/// Read + parse a manifest file. `Ok(None)` when the file does not exist.
pub(crate) fn read_manifest(
    fs: &dyn FsOps,
    path: &Path,
) -> Result<Option<Manifest>, ClientError> {
    let Some(bytes) = fs.read_opt(path)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes).map_err(|_| corrupt(path, "not UTF-8"))?;
    let doc: DocumentMut = text.parse().map_err(|e| corrupt(path, e))?;
    Ok(Some(manifest_from(&doc, path)?))
}

fn entries_of(doc: &DocumentMut, table: &str, path: &Path) -> Result<Vec<ManifestEntry>, ClientError> {
    let Some(item) = doc.get(table) else {
        return Ok(Vec::new());
    };
    let t = item
        .as_table()
        .ok_or_else(|| corrupt(path, format!("[{table}] is not a table")))?;
    let mut out = Vec::new();
    for (key, value) in t.iter() {
        let spec = value
            .as_str()
            .ok_or_else(|| corrupt(path, format!("[{table}] \"{key}\" is not a string")))?;
        out.push(ManifestEntry {
            reference: key.to_string(),
            pin: if spec == "*" { None } else { Some(spec.to_string()) },
        });
    }
    Ok(out)
}

fn manifest_from(doc: &DocumentMut, path: &Path) -> Result<Manifest, ClientError> {
    let mut m = Manifest {
        skills: entries_of(doc, "skills", path)?,
        channels: entries_of(doc, "channels", path)?,
        ..Manifest::default()
    };
    if let Some(item) = doc.get("exclude") {
        let arr = item
            .as_array()
            .ok_or_else(|| corrupt(path, "`exclude` is not an array"))?;
        for v in arr {
            let s = v
                .as_str()
                .ok_or_else(|| corrupt(path, "`exclude` entries must be strings"))?;
            m.exclude.push(s.to_string());
        }
    }
    if let Some(item) = doc.get("placement") {
        let t = item
            .as_table()
            .ok_or_else(|| corrupt(path, "[placement] is not a table"))?;
        for (key, value) in t.iter() {
            if key == "kind" {
                let kt = value
                    .as_table()
                    .ok_or_else(|| corrupt(path, "[placement.kind] is not a table"))?;
                for (kind, dir) in kt.iter() {
                    let d = dir
                        .as_str()
                        .ok_or_else(|| corrupt(path, "[placement.kind] values must be strings"))?;
                    m.placement_kind.push((kind.to_string(), d.to_string()));
                }
            } else {
                let d = value
                    .as_str()
                    .ok_or_else(|| corrupt(path, "[placement] values must be strings"))?;
                m.placement.push((key.to_string(), d.to_string()));
            }
        }
    }
    Ok(m)
}

/// The edits `add`/`remove` apply — surgical, format-preserving.
pub(crate) struct ManifestEditor {
    doc: DocumentMut,
}

impl ManifestEditor {
    /// Open an existing manifest for editing, or start a fresh one.
    pub(crate) fn open(fs: &dyn FsOps, path: &Path) -> Result<Self, ClientError> {
        let doc = match fs.read_opt(path)? {
            Some(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| corrupt(path, "not UTF-8"))?;
                text.parse().map_err(|e| corrupt(path, e))?
            }
            None => DocumentMut::new(),
        };
        Ok(Self { doc })
    }

    /// A fresh `topos init` document — a short header comment, no entries.
    pub(crate) fn init_template() -> String {
        "# topos.toml — the skills this project uses. Managed by `topos add` / `topos remove`;\n\
         # hand-edits are fine. `topos update` reconciles agents in this folder against it.\n"
            .to_string()
    }

    fn table_mut(&mut self, name: &str) -> &mut Table {
        if self.doc.get(name).is_none() {
            let mut t = Table::new();
            t.set_implicit(false);
            self.doc.insert(name, Item::Table(t));
        }
        self.doc[name].as_table_mut().expect("just ensured")
    }

    /// Upsert an include line (`[skills]` or `[channels]`) — the value is the pin, `"*"` for
    /// track-current. Returns whether the entry existed before.
    pub(crate) fn set_entry(&mut self, table: &str, reference: &str, pin: Option<&str>) -> bool {
        let t = self.table_mut(table);
        let existed = t.contains_key(reference);
        t.insert(reference, toml_edit::value(pin.unwrap_or("*")));
        existed
    }

    /// Remove an include line; true when a line was actually removed.
    pub(crate) fn remove_entry(&mut self, table: &str, reference: &str) -> bool {
        match self.doc.get_mut(table).and_then(|i| i.as_table_mut()) {
            Some(t) => {
                let removed = t.remove(reference).is_some();
                if t.is_empty() {
                    self.doc.remove(table);
                }
                removed
            }
            None => false,
        }
    }

    /// Add an exclude line (idempotent).
    pub(crate) fn add_exclude(&mut self, reference: &str) -> bool {
        let arr = match self.doc.get_mut("exclude").and_then(|i| i.as_array_mut()) {
            Some(a) => a,
            None => {
                self.doc.insert(
                    "exclude",
                    Item::Value(Value::Array(toml_edit::Array::new())),
                );
                self.doc["exclude"].as_array_mut().expect("just inserted")
            }
        };
        if arr.iter().any(|v| v.as_str() == Some(reference)) {
            return false;
        }
        arr.push(reference);
        // One entry per line reads better once the list grows past one.
        for item in arr.iter_mut() {
            item.decor_mut().set_prefix("\n  ");
        }
        arr.set_trailing("\n");
        arr.set_trailing_comma(true);
        true
    }

    /// Remove an exclude line; true when one was removed.
    pub(crate) fn remove_exclude(&mut self, reference: &str) -> bool {
        let Some(arr) = self.doc.get_mut("exclude").and_then(|i| i.as_array_mut()) else {
            return false;
        };
        let before = arr.len();
        arr.retain(|v: &Value| v.as_str() != Some(reference));
        let removed = arr.len() != before;
        if arr.is_empty() {
            self.doc.remove("exclude");
        }
        removed
    }

    /// The serialized document (what `write` persists).
    pub(crate) fn rendered(&self) -> String {
        self.doc.to_string()
    }

    /// Persist atomically through the one crash-safe write.
    pub(crate) fn write(&self, fs: &dyn FsOps, path: &Path) -> Result<(), ClientError> {
        crate::atomic::atomic_write(fs, path, self.rendered().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-mani-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_round_trips_the_documented_shape() {
        let dir = scratch("read");
        let path = dir.join(MANIFEST_FILE);
        std::fs::write(
            &path,
            r#"exclude = ["topos.sh/acme/noisy-skill"]

[skills]
"topos.sh/acme/code-review" = "*"
"github.com/vercel-labs/skills/find-skills" = "3aa01e2"
"./tools/my-skill" = "*"

[channels]
"topos.sh/acme/channels/backend" = "*"

[placement]
"topos.sh/acme/code-review" = ".claude/skills"
[placement.kind]
skill = ".agents/skills"
"#,
        )
        .unwrap();
        let m = read_manifest(&RealFs, &path).unwrap().unwrap();
        assert_eq!(m.skills.len(), 3);
        assert_eq!(m.skills[0].reference, "topos.sh/acme/code-review");
        assert_eq!(m.skills[0].pin, None);
        assert_eq!(m.skills[1].pin.as_deref(), Some("3aa01e2"));
        assert_eq!(m.channels[0].reference, "topos.sh/acme/channels/backend");
        assert_eq!(m.exclude, vec!["topos.sh/acme/noisy-skill".to_string()]);
        assert_eq!(
            m.placement,
            vec![("topos.sh/acme/code-review".into(), ".claude/skills".into())]
        );
        assert_eq!(m.placement_kind, vec![("skill".into(), ".agents/skills".into())]);
    }

    #[test]
    fn missing_file_reads_none_and_garbage_refuses() {
        let dir = scratch("miss");
        assert!(read_manifest(&RealFs, &dir.join(MANIFEST_FILE)).unwrap().is_none());
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[skills\n").unwrap();
        assert!(read_manifest(&RealFs, &bad).is_err());
    }

    #[test]
    fn edits_preserve_comments_and_layout() {
        let dir = scratch("edit");
        let path = dir.join(MANIFEST_FILE);
        std::fs::write(
            &path,
            "# Our team's skills. Ask #platform before removing anything.\n\
             [skills]\n\
             # the review workflow every PR runs\n\
             \"topos.sh/acme/code-review\" = \"*\"\n",
        )
        .unwrap();
        let fs = RealFs;
        let mut ed = ManifestEditor::open(&fs, &path).unwrap();
        assert!(!ed.set_entry("skills", "topos.sh/acme/deploy", None));
        ed.write(&fs, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Our team's skills"), "{text}");
        assert!(text.contains("# the review workflow"), "{text}");
        assert!(text.contains("\"topos.sh/acme/deploy\" = \"*\""), "{text}");

        // Upsert flips a pin in place; remove deletes the line and, when the table empties, the header.
        let mut ed = ManifestEditor::open(&fs, &path).unwrap();
        assert!(ed.set_entry("skills", "topos.sh/acme/deploy", Some("9f2c11")));
        assert!(ed.remove_entry("skills", "topos.sh/acme/code-review"));
        ed.write(&fs, &path).unwrap();
        let m = read_manifest(&fs, &path).unwrap().unwrap();
        assert_eq!(m.skills.len(), 1);
        assert_eq!(m.skills[0].pin.as_deref(), Some("9f2c11"));
    }

    #[test]
    fn exclude_lines_add_and_remove_idempotently() {
        let dir = scratch("excl");
        let path = dir.join(MANIFEST_FILE);
        let fs = RealFs;
        let mut ed = ManifestEditor::open(&fs, &path).unwrap();
        assert!(ed.add_exclude("topos.sh/acme/noisy"));
        assert!(!ed.add_exclude("topos.sh/acme/noisy"));
        assert!(ed.add_exclude("topos.sh/acme/louder"));
        ed.write(&fs, &path).unwrap();
        let m = read_manifest(&fs, &path).unwrap().unwrap();
        assert_eq!(m.exclude.len(), 2);
        let mut ed = ManifestEditor::open(&fs, &path).unwrap();
        assert!(ed.remove_exclude("topos.sh/acme/noisy"));
        assert!(!ed.remove_exclude("topos.sh/acme/noisy"));
        ed.write(&fs, &path).unwrap();
        let m = read_manifest(&fs, &path).unwrap().unwrap();
        assert_eq!(m.exclude, vec!["topos.sh/acme/louder".to_string()]);
    }

    #[test]
    fn a_fresh_init_template_parses_empty() {
        let dir = scratch("init");
        let path = dir.join(MANIFEST_FILE);
        std::fs::write(&path, ManifestEditor::init_template()).unwrap();
        let m = read_manifest(&RealFs, &path).unwrap().unwrap();
        assert!(m.is_empty());
    }
}
