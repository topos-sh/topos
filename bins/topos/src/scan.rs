//! The I/O bundle scanner — a distinct artifact from the kernel's pure path predicate. It walks a real
//! skill directory and applies the **filesystem-level** reject rules to on-disk reality (symlink /
//! device / fifo / socket / any non-regular file / non-UTF-8 name) that the `no_std` kernel can never
//! see, then feeds `(path, mode, sha256)` to the kernel digest — which re-applies the byte-pure path
//! rejects (absolute / `..` / NUL / control) and the NFC/case-fold **collision** rejects.

use std::path::Path;

use topos_core::digest::{self, FileMode, ManifestEntry};

use crate::error::ClientError;

/// One scanned file: its bundle-relative forward-slash path, mode, and raw bytes.
#[derive(Debug, Clone)]
pub(crate) struct ScannedFile {
    pub path: String,
    pub mode: FileMode,
    pub bytes: Vec<u8>,
}

/// A scanned bundle: the files (sorted by raw path bytes), the kernel `bundle_digest`, and the optional
/// name parsed from `SKILL.md` frontmatter.
#[derive(Debug, Clone)]
pub(crate) struct ScannedBundle {
    pub files: Vec<ScannedFile>,
    pub bundle_digest: [u8; 32],
    pub name_hint: Option<String>,
}

/// Scan a real skill directory.
///
/// # Errors
/// [`ClientError::Scan`] on a filesystem-level reject (symlink/device/non-regular/non-UTF-8) or a kernel
/// path/collision reject; [`ClientError::EmptyBundle`] if nothing adoptable remains; [`ClientError::Io`]
/// on a read failure.
pub(crate) fn scan(root: &Path) -> Result<ScannedBundle, ClientError> {
    let mut files = Vec::new();
    walk(root, "", &mut files)?;
    if files.is_empty() {
        return Err(ClientError::EmptyBundle);
    }
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    // The kernel re-runs check_path + the collision rules and computes the consent digest.
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let bundle_digest = digest::bundle_digest(&entries)?;

    let name_hint = files
        .iter()
        .find(|f| f.path == "SKILL.md")
        .and_then(|f| frontmatter_name(&f.bytes));

    Ok(ScannedBundle {
        files,
        bundle_digest,
        name_hint,
    })
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<ScannedFile>) -> Result<(), ClientError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| ClientError::Io(format!("read_dir {}: {e}", dir.display())))?
        .collect::<Result<_, _>>()
        .map_err(|e| ClientError::Io(format!("{e}")))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ClientError::Scan(format!("non-UTF-8 name under {}", dir.display())))?;

        // Never follow a symlink — inspect the link itself.
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| ClientError::Io(format!("stat {}: {e}", path.display())))?;
        let ft = meta.file_type();

        if ft.is_symlink() {
            return Err(ClientError::Scan(format!("symlink: {name}")));
        }
        if ft.is_dir() {
            // Drop the VCS dir; recurse everything else.
            if name == ".git" {
                continue;
            }
            let child_prefix = join(prefix, &name);
            walk(&path, &child_prefix, out)?;
            continue;
        }
        if !ft.is_file() {
            // device / fifo / socket / anything else non-regular.
            return Err(ClientError::Scan(format!("not a regular file: {name}")));
        }
        // A regular file. Drop the macOS dropping; keep the rest byte-exact.
        if name == ".DS_Store" {
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| ClientError::Io(format!("read {}: {e}", path.display())))?;
        let mode = file_mode(&meta);
        out.push(ScannedFile {
            path: join(prefix, &name),
            mode,
            bytes,
        });
    }
    Ok(())
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

fn file_mode(meta: &std::fs::Metadata) -> FileMode {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        FileMode::Executable
    } else {
        FileMode::Regular
    }
}

/// Extract a `name:` value from leading YAML frontmatter (`---` … `---`). A minimal line scan — no YAML
/// dependency; an unquoted or single/double-quoted scalar is accepted. Returns `None` if absent.
fn frontmatter_name(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            let v = rest.trim().trim_matches(['"', '\'']).trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}
