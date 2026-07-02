//! `dist` — offline release packaging: the deterministic CLI tarball + the checksum manifest.
//!
//! Strictly filesystem-only: it never builds and never touches the network — CI builds the
//! release binary first, then calls `dist package` to wrap it.
//!
//! `cargo xtask dist package --target <triple> [--out <dir>]`
//!     Wrap the prebuilt `target/<triple>/release/topos` + the repo `LICENSE` into
//!     `<out>/topos-<triple>.tar.gz`. The filename is deliberately VERSIONLESS — it is the
//!     stable asset name a `releases/latest/download/…` URL resolves — and the tarball is
//!     byte-deterministic (sorted entries; zeroed mtime/uid/gid/uname/gname; a gzip stream
//!     with mtime 0 and one fixed compression level), so packaging the same binary twice
//!     yields the identical archive. Prints the coreutils-format `<sha256>  <name>` line.
//!
//! `cargo xtask dist sums --dir <dir> [--check]`
//!     Write `<dir>/SHA256SUMS` over every regular file in `<dir>` (except the manifest
//!     itself), sorted by filename, coreutils format — so `shasum -a 256 -c SHA256SUMS`
//!     verifies a download. With `--check`, re-hash and fail on any mismatch, missing file,
//!     or file present but unlisted (the same convention as `gen-schema --check`).

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("package") => package(&args[1..]),
        Some("sums") => sums(&args[1..]),
        _ => bail!(
            "usage: cargo xtask dist <package --target <triple> [--out <dir>] | sums --dir <dir> [--check]>"
        ),
    }
}

/// The value of `--<name> <value>` in `args`, if the flag is present.
fn flag_value(args: &[String], name: &str) -> Result<Option<String>> {
    let flag = format!("--{name}");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if *a == flag {
            return match it.next() {
                Some(v) => Ok(Some(v.clone())),
                None => bail!("`{flag}` needs a value"),
            };
        }
    }
    Ok(None)
}

/// Resolve a possibly-relative path against the workspace root, so the subcommand is
/// independent of the current working directory (like every other xtask subcommand).
fn from_root(p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::workspace_root().join(path)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `dist package` — wrap a PREBUILT release binary into the deterministic release tarball.
fn package(args: &[String]) -> Result<()> {
    let Some(target) = flag_value(args, "target")? else {
        bail!("`dist package` needs `--target <triple>`");
    };
    let out_dir = from_root(&flag_value(args, "out")?.unwrap_or_else(|| "dist".to_owned()));
    let root = crate::workspace_root();

    let binary = root.join(format!("target/{target}/release/topos"));
    if !binary.is_file() {
        bail!(
            "no prebuilt binary at {} — `dist package` never builds; produce it first with\n  \
             cargo build --release --locked -p topos --bin topos --target {target}",
            binary.display()
        );
    }
    let bin_bytes = fs::read(&binary).with_context(|| format!("reading {}", binary.display()))?;
    let license_path = root.join("LICENSE");
    let license_bytes =
        fs::read(&license_path).with_context(|| format!("reading {}", license_path.display()))?;

    // Entry order is part of the determinism contract: sorted by archive path.
    let mut entries: Vec<(&str, &[u8], u32)> = vec![
        ("LICENSE", license_bytes.as_slice(), 0o644),
        ("topos", bin_bytes.as_slice(), 0o755),
    ];
    entries.sort_by_key(|&(name, _, _)| name);

    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let name = format!("topos-{target}.tar.gz");
    let path = out_dir.join(&name);

    // gzip with mtime pinned to 0 and ONE fixed compression level, so the compressed stream
    // (not just the tar payload) is byte-reproducible.
    let file = fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(file, flate2::Compression::new(6));
    let mut tar = tar::Builder::new(gz);
    for (entry_name, bytes, mode) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        // Zero every per-build source of variation: timestamp, builder uid/gid, builder names.
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_username("").context("clearing the tar uname")?;
        header.set_groupname("").context("clearing the tar gname")?;
        // `append_data` writes the path + checksum into the header for us.
        tar.append_data(&mut header, entry_name, bytes)
            .with_context(|| format!("appending {entry_name} to {name}"))?;
    }
    let gz = tar.into_inner().context("finishing the tar stream")?;
    gz.finish().context("finishing the gzip stream")?;

    let written = fs::read(&path).with_context(|| format!("re-reading {}", path.display()))?;
    println!("{}  {name}", sha256_hex(&written));
    Ok(())
}

/// `dist sums` — write (or `--check`) the coreutils-format `SHA256SUMS` manifest for a dir.
fn sums(args: &[String]) -> Result<()> {
    let Some(dir_arg) = flag_value(args, "dir")? else {
        bail!("`dist sums` needs `--dir <dir>`");
    };
    let check = args.iter().any(|a| a == "--check");
    let dir = from_root(&dir_arg);

    // Every regular file directly in <dir>, except the manifest itself — sorted by filename so
    // the manifest is deterministic. Anything else (a symlink, a subdirectory, a device) is a
    // hard error, not a skip: a silently-uncovered entry would ship outside checksum coverage.
    let mut files: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "SHA256SUMS" {
            continue;
        }
        // `file_type()` does not follow symlinks, so a symlink-to-file reports as a symlink here.
        if !entry.file_type()?.is_file() {
            bail!(
                "{name}: not a regular file — only regular files can ship as release assets \
                 (a symlink or directory would fall outside SHA256SUMS coverage)"
            );
        }
        files.push(name);
    }
    files.sort();

    let manifest = dir.join("SHA256SUMS");
    if check {
        let text = fs::read_to_string(&manifest).with_context(|| {
            format!(
                "reading {} — run `cargo xtask dist sums --dir {dir_arg}` first",
                manifest.display()
            )
        })?;
        let mut problems = Vec::new();
        let mut listed = BTreeSet::new();
        for (i, line) in text.lines().enumerate() {
            let Some((expected, name)) = line.split_once("  ") else {
                problems.push(format!("line {}: not `<sha256>  <name>`", i + 1));
                continue;
            };
            listed.insert(name.to_owned());
            match fs::read(dir.join(name)) {
                Ok(bytes) => {
                    let actual = sha256_hex(&bytes);
                    if actual != expected {
                        problems.push(format!(
                            "{name}: digest mismatch (listed {expected}, actual {actual})"
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    problems.push(format!("{name}: listed but missing"));
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", dir.join(name).display()));
                }
            }
        }
        // A file that exists but isn't listed is a failure too — otherwise a stray (or swapped-in)
        // asset would ship unverified.
        for name in &files {
            if !listed.contains(name) {
                problems.push(format!("{name}: present but unlisted"));
            }
        }
        if problems.is_empty() {
            println!("SHA256SUMS verified ({} files)", files.len());
            Ok(())
        } else {
            bail!("SHA256SUMS check failed: {}", problems.join("; "));
        }
    } else {
        let mut out = String::new();
        for name in &files {
            let path = dir.join(name);
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            out.push_str(&sha256_hex(&bytes));
            out.push_str("  ");
            out.push_str(name);
            out.push('\n');
        }
        fs::write(&manifest, out).with_context(|| format!("writing {}", manifest.display()))?;
        println!("wrote {} ({} files)", manifest.display(), files.len());
        Ok(())
    }
}
