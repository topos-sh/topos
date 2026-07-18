//! `topos self-update` — the native self-updater for the CLI binary itself. Resolve the target release,
//! download the asset for THIS build's target triple, verify its minisign signature when this build
//! carries a release public key (mandatory + fail-closed then), verify its sha256 against the release
//! SHA256SUMS (never skippable), and atomically replace the running binary. A maintenance command: no
//! skills, no plane, no account. (Skills are updated by `topos update`; this updates the `topos` program.)

use std::io::Read;
use std::path::Path;

use serde::Serialize;

use topos_core::digest::{sha256, to_hex};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::release::ReleaseSource;

/// The compiled target triple (from build.rs) — the asset this binary knows how to replace itself with.
const TARGET_TRIPLE: &str = env!("TOPOS_TARGET");
/// This build's version, e.g. "0.1.0".
pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The default release download base (GitHub). Overridable via TOPOS_INSTALL_BASE_URL (mirrors/air-gap).
pub(crate) const DEFAULT_BASE_URL: &str = "https://github.com/topos-sh/topos/releases";

/// The COMPILED-IN minisign release public key (the base64 line of `minisign.pub`).
///
/// `None` — today's pre-key-ceremony state — keeps self-update checksum-only, with an honest
/// "unsigned build" note on every install. `Some(key)` makes signature verification MANDATORY and
/// fail-closed: the asset's `.minisig` is fetched and verified over the downloaded bytes BEFORE the
/// checksum gate (and long before the binary is touched); a missing or invalid signature is a typed
/// `INTEGRITY_ERROR` refusal with no unsigned fallback. The key ceremony
/// (`scripts/mint-release-key.sh`) prints the exact `Some("…")` line to paste here — this constant
/// and `scripts/install.sh`'s `MINISIGN_PUBKEY` are the only two places it flips.
pub(crate) const RELEASE_PUBKEY: Option<&str> =
    Some("RWRsqhNImLJGum9BdXy1X/p7Dhr+xc0JQTyNPxaGW5emP/K/+828Euav");

#[derive(Debug, Clone)]
pub(crate) struct SelfUpdateOpts {
    pub check: bool,
    /// An explicit tag, verbatim-ish (normalized to a leading 'v').
    pub version: Option<String>,
    /// TOPOS_INSTALL_BASE_URL override; None => DEFAULT_BASE_URL.
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SelfUpdateAction {
    Checked,
    Upgraded,
    AlreadyCurrent,
}

/// Ad-hoc outcome — this maintenance command has no frozen wire schema (the envelope stays valid with a
/// free-form `data`). Serialized into the `--json` envelope's `data`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SelfUpdateOutcome {
    pub action: SelfUpdateAction,
    pub current_version: String,
    /// The resolved target (latest, or the pinned tag), sans 'v'.
    pub latest_version: Option<String>,
    pub update_available: bool,
    /// Whether the downloaded asset's minisign signature was verified against this build's
    /// compiled-in release public key. Always `false` on a build with no key, and on the
    /// `--check` / already-current outcomes (nothing was downloaded to verify).
    pub signed: bool,
    /// The target triple this binary was built for.
    pub target: String,
    /// e.g. a non-HTTPS base URL warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// e.g. the unsigned-build note (this build carries no release public key — checksum only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Update the running `topos` binary to the target release, verifying against this build's
/// compiled-in [`RELEASE_PUBKEY`]. The production entry — tests drive [`self_update_with_key`] to
/// exercise both key postures.
///
/// # Errors
/// See [`self_update_with_key`].
pub(crate) fn self_update(
    ctx: &Ctx<'_>,
    releases: &dyn ReleaseSource,
    current_exe: &Path,
    opts: SelfUpdateOpts,
) -> Result<SelfUpdateOutcome, ClientError> {
    self_update_with_key(ctx, releases, current_exe, opts, RELEASE_PUBKEY)
}

/// Update the running `topos` binary to the target release. `current_exe` is injected (the binary to
/// replace) so a test replaces a fake target, never the test runner; `pubkey` is injected so tests
/// exercise both the unsigned (`None`) and the mandatory-signature (`Some`) postures.
///
/// # Errors
/// [`ClientError::Plane`] on a release-check / download transport fault;
/// [`ClientError::SignatureInvalid`] when this build carries a release public key and the asset's
/// `.minisig` is missing or does not verify (mandatory, fail-closed — checked BEFORE the checksum);
/// [`ClientError::ChecksumMismatch`] when the download does not match the release SHA256SUMS (never
/// skippable); [`ClientError::WireInvalid`] on an unreadable tarball / a SHA256SUMS missing the asset;
/// an [`FsOps`](crate::fs_seam::FsOps) failure (e.g. a not-writable install dir) on the atomic replace.
fn self_update_with_key(
    ctx: &Ctx<'_>,
    releases: &dyn ReleaseSource,
    current_exe: &Path,
    opts: SelfUpdateOpts,
    pubkey: Option<&str>,
) -> Result<SelfUpdateOutcome, ClientError> {
    let base_url = opts
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_owned();
    let warning = (!base_url.starts_with("https://")).then(|| {
        format!(
            "downloading over a non-HTTPS base URL ({base_url}) — checksum still enforced, but only \
             do this against a local mirror you control"
        )
    });

    // 1. Resolve the target tag + version. A custom download base (a mirror / air-gap) has no GitHub
    //    "latest release" API to resolve against, so it MUST be paired with an explicit `--version`.
    let (tag, explicit) = match &opts.version {
        Some(v) => (normalize_tag(v), true),
        None if opts.base_url.is_some() => {
            return Err(ClientError::InvalidArgument(
                "a custom TOPOS_INSTALL_BASE_URL cannot auto-resolve the latest release — pass \
                 `--version <tag>` to name the release to install."
                    .into(),
            ));
        }
        None => (releases.latest_tag()?, false),
    };
    let latest_version = tag.trim_start_matches('v').to_owned();
    let update_available = version_gt(&latest_version, CURRENT_VERSION);

    // 2. check mode — report and stop.
    if opts.check {
        return Ok(SelfUpdateOutcome {
            action: SelfUpdateAction::Checked,
            current_version: CURRENT_VERSION.to_owned(),
            latest_version: Some(latest_version),
            update_available,
            signed: false,
            target: TARGET_TRIPLE.to_owned(),
            warning,
            note: None,
        });
    }

    // 3. no-op unless newer (or an explicit different tag was requested — allows a pinned downgrade).
    let should_install = if explicit {
        latest_version != CURRENT_VERSION
    } else {
        update_available
    };
    if !should_install {
        return Ok(SelfUpdateOutcome {
            action: SelfUpdateAction::AlreadyCurrent,
            current_version: CURRENT_VERSION.to_owned(),
            latest_version: Some(latest_version),
            update_available: false,
            signed: false,
            target: TARGET_TRIPLE.to_owned(),
            warning,
            note: None,
        });
    }

    // 4. download the asset + SHA256SUMS.
    let asset = format!("topos-{TARGET_TRIPLE}.tar.gz");
    let asset_url = format!("{base_url}/download/{tag}/{asset}");
    let sums_url = format!("{base_url}/download/{tag}/SHA256SUMS");
    let tarball = releases.download(&asset_url)?;
    let sums = releases.download(&sums_url)?;

    // 5. MANDATORY minisign verification when this build carries a release public key: fetch the
    //    asset's `.minisig` and verify it over the downloaded bytes BEFORE the checksum gate (and
    //    long before the binary is touched). Fail-closed — with a key compiled in there is no
    //    unsigned fallback; a missing or invalid signature refuses typed. Without a key (the
    //    pre-key-ceremony state) the checksum-only behavior is preserved, disclosed by a note.
    let signed = match pubkey {
        Some(key) => {
            let sig_url = format!("{asset_url}.minisig");
            let sig_bytes =
                releases
                    .download(&sig_url)
                    .map_err(|e| ClientError::SignatureInvalid {
                        asset: asset.clone(),
                        reason: format!("is required by this build but could not be fetched ({e})"),
                    })?;
            let sig_text =
                std::str::from_utf8(&sig_bytes).map_err(|_| ClientError::SignatureInvalid {
                    asset: asset.clone(),
                    reason: "file is not valid UTF-8".into(),
                })?;
            verify_release_signature(key, sig_text, &tarball, &tag, &asset)?;
            true
        }
        None => false,
    };
    let note = (!signed).then(|| {
        "this build carries no release-signing public key — the download was verified by checksum \
         only"
            .to_owned()
    });

    // 6. verify sha256 (never skippable) — exact filename match in SHA256SUMS.
    let sums_text = std::str::from_utf8(&sums)
        .map_err(|_| ClientError::WireInvalid("SHA256SUMS is not valid UTF-8".into()))?;
    let expected = expected_sum(sums_text, &asset)
        .ok_or_else(|| ClientError::WireInvalid(format!("SHA256SUMS does not list {asset}")))?;
    let actual = to_hex(&sha256(&tarball));
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(ClientError::ChecksumMismatch {
            asset,
            expected,
            actual,
        });
    }

    // 7. extract the `topos` binary from the tarball (in memory — never unpack attacker paths to disk).
    let bin_bytes = extract_topos(&tarball)?;

    // 8. atomically replace the running binary. Stage a sibling temp so the rename is same-filesystem.
    let dir = current_exe.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".topos-upgrade.{}.tmp", std::process::id()));
    if let Err(e) = crate::atomic::atomic_write_executable(ctx.fs, current_exe, &tmp, &bin_bytes) {
        // The install dir has no recovery sweep (unlike ~/.topos/), so never leave the staged temp behind.
        let _ = ctx.fs.remove_file(&tmp);
        return Err(map_replace_error(e, current_exe));
    }

    Ok(SelfUpdateOutcome {
        action: SelfUpdateAction::Upgraded,
        current_version: CURRENT_VERSION.to_owned(),
        latest_version: Some(latest_version),
        update_available: true,
        signed,
        target: TARGET_TRIPLE.to_owned(),
        warning,
        note,
    })
}

/// Verify `tarball` against the minisign signature document `sig_text` using the compiled-in release
/// public key. Strict, twice over:
/// - only the modern PRE-HASHED minisign algorithm is accepted (`allow_legacy = false` — the
///   release pipeline's signing command produces exactly that);
/// - the SIGNED trusted comment must name the exact `tag` + `asset` this update resolved (the
///   pipeline signs `topos-sh/topos <tag> <asset>`, and minisign's global signature covers the
///   comment). Without the binding, a valid signature over an OLD release's bytes could be
///   re-served under a newer tag — a substitution the checksum cannot catch either, because the
///   attacker who moves the asset also moves its SHA256SUMS.
///
/// Every failure maps to the one typed [`ClientError::SignatureInvalid`] refusal.
fn verify_release_signature(
    pubkey_b64: &str,
    sig_text: &str,
    tarball: &[u8],
    tag: &str,
    asset: &str,
) -> Result<(), ClientError> {
    let refuse = |reason: String| ClientError::SignatureInvalid {
        asset: asset.to_owned(),
        reason,
    };
    let key = minisign_verify::PublicKey::from_base64(pubkey_b64).map_err(|e| {
        refuse(format!(
            "cannot be checked — this build's compiled-in public key is malformed ({e})"
        ))
    })?;
    let sig = minisign_verify::Signature::decode(sig_text)
        .map_err(|e| refuse(format!("is malformed ({e})")))?;
    key.verify(tarball, &sig, false)
        .map_err(|e| refuse(format!("does not verify ({e})")))?;
    // The comment is authenticated by the global signature `verify` just checked, so token-matching
    // it here is sound. Exact whitespace tokens — never a substring (a tag must not match inside a
    // longer tag or a filename).
    let comment = sig.trusted_comment();
    let names = |token: &str| comment.split_whitespace().any(|t| t == token);
    if !(names(tag) && names(asset)) {
        return Err(refuse(format!(
            "is not bound to this release — the signed trusted comment ({comment:?}) does not \
             name {tag} and {asset}"
        )));
    }
    Ok(())
}

/// Prepend a leading 'v' if the tag looks like a bare `X.Y.Z`.
fn normalize_tag(v: &str) -> String {
    if v.starts_with('v') {
        v.to_owned()
    } else {
        format!("v{v}")
    }
}

/// Parse the `<sha256>  <name>` SHA256SUMS line whose filename EXACTLY equals `asset` (last whitespace
/// field, a leading '*' binary-mode marker stripped). Returns the hex sum. Mirrors the installer's
/// exact-match-anchored-at-end rule — never a substring match.
fn expected_sum(sums: &str, asset: &str) -> Option<String> {
    for line in sums.lines() {
        // A blank or single-field line is skipped, never a search-ending abort (a stray line must not
        // hide a later, valid entry).
        let mut fields = line.split_whitespace();
        let Some(sum) = fields.next() else { continue };
        let Some(name) = fields.last() else { continue };
        let name = name.strip_prefix('*').unwrap_or(name);
        if name == asset {
            return Some(sum.to_owned());
        }
    }
    None
}

/// A minimal semver-core `>` : compare (major, minor, patch), ignoring any pre-release/build suffix. Tags
/// come from our own release pipeline (`vX.Y.Z`), so the core triple is sufficient; a malformed side is
/// treated as (0,0,0) so a valid newer version still wins. (Shared with the passive version check —
/// one newer-than decision, never two implementations to drift.)
pub(super) fn version_gt(a: &str, b: &str) -> bool {
    parse_core(a) > parse_core(b)
}

fn parse_core(v: &str) -> (u64, u64, u64) {
    let core = v
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// A generous ceiling on the extracted binary, so a crafted asset can't expand to exhaust memory. This is
/// only reachable PAST the checksum gate (an attacker controlling both the tarball AND its SHA256SUMS —
/// the default HTTPS+GitHub path is TLS-authenticated); it is defense-in-depth for a self-updater.
const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;

/// Read the `topos` regular-file entry out of a gzip'd tar, into memory. The release tarball holds `topos`
/// (0755) and `LICENSE` at the TOP LEVEL (deterministic packaging), so the match is an exact top-level
/// path — never a nested or basename match.
fn extract_topos(targz: &[u8]) -> Result<Vec<u8>, ClientError> {
    let gz = flate2::read::GzDecoder::new(targz);
    let mut ar = tar::Archive::new(gz);
    let entries = ar
        .entries()
        .map_err(|e| ClientError::WireInvalid(format!("release tarball unreadable: {e}")))?;
    for entry in entries {
        let e = entry.map_err(|err| {
            ClientError::WireInvalid(format!("release tarball entry unreadable: {err}"))
        })?;
        let is_topos = e.header().entry_type().is_file()
            && e.path().map(|p| p == Path::new("topos")).unwrap_or(false);
        if is_topos {
            // Reject an implausibly large declared size before allocating, and cap the read itself so a
            // lying header can't stream past the ceiling either.
            let declared = e.header().size().unwrap_or(u64::MAX);
            if declared > MAX_BINARY_BYTES {
                return Err(ClientError::WireInvalid(format!(
                    "release binary is implausibly large ({declared} bytes) — refusing to extract"
                )));
            }
            let mut buf = Vec::new();
            e.take(MAX_BINARY_BYTES)
                .read_to_end(&mut buf)
                .map_err(|err| {
                    ClientError::WireInvalid(format!("reading topos from tarball: {err}"))
                })?;
            return Ok(buf);
        }
    }
    Err(ClientError::WireInvalid(
        "release tarball does not contain a top-level `topos` binary".into(),
    ))
}

/// Map an atomic-replace failure into a typed error. A read-only / package-managed install location gets
/// actionable reinstall guidance (it will not heal on a retry); any other filesystem failure keeps its
/// plain identity so its transient-vs-permanent classification is preserved.
fn map_replace_error(e: std::io::Error, current_exe: &Path) -> ClientError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
            ClientError::UpgradeUnwritable(format!(
                "cannot replace {} — the install location is not writable. topos looks \
                 package-managed or read-only here; reinstall the latest release with the topos \
                 installer, or via your package manager.",
                current_exe.display()
            ))
        }
        _ => ClientError::from(e),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_core::digest::{sha256, to_hex};
    use topos_harness::ClaudeCode;

    use super::{
        CURRENT_VERSION, RELEASE_PUBKEY, SelfUpdateAction, SelfUpdateOpts, TARGET_TRIPLE,
        expected_sum, extract_topos, map_replace_error, normalize_tag, parse_core, self_update,
        self_update_with_key, version_gt,
    };
    use crate::ctx::Ctx;
    use crate::error::ClientError;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::release::ReleaseSource;
    use crate::sidecar::Layout;

    /// A throwaway directory under the OS temp dir (no `tempfile` dep in this crate).
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-upg-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a real `.tar.gz` over an in-memory Vec — `(name, bytes, mode)` entries.
    fn build_targz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for (name, bytes, mode) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            header.set_mtime(0);
            tar.append_data(&mut header, name, *bytes).unwrap();
        }
        let gz = tar.into_inner().unwrap();
        gz.finish().unwrap()
    }

    /// A fake release source: a canned latest tag + a `url -> bytes` map for downloads.
    struct FakeReleases {
        latest: String,
        blobs: std::collections::HashMap<String, Vec<u8>>,
    }

    impl ReleaseSource for FakeReleases {
        fn latest_tag(&self) -> Result<String, ClientError> {
            Ok(self.latest.clone())
        }
        fn download(&self, url: &str) -> Result<Vec<u8>, ClientError> {
            self.blobs
                .get(url)
                .cloned()
                .ok_or_else(|| ClientError::Plane(format!("download {url}: 404 (fake)")))
        }
    }

    /// The default (GitHub) asset + SHA256SUMS urls for `tag`, matching the op's URL builder.
    fn urls(tag: &str) -> (String, String, String) {
        let base = "https://github.com/topos-sh/topos/releases";
        let asset = format!("topos-{TARGET_TRIPLE}.tar.gz");
        (
            asset.clone(),
            format!("{base}/download/{tag}/{asset}"),
            format!("{base}/download/{tag}/SHA256SUMS"),
        )
    }

    /// A resolver-only [`Ctx`] over a real fs + inert seams — `upgrade` touches only `ctx.fs`.
    fn with_ctx<R>(f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("adapter"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&scratch("home")),
            harness: &harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        f(&ctx)
    }

    #[test]
    fn normalize_tag_forces_a_leading_v() {
        assert_eq!(normalize_tag("v0.2.0"), "v0.2.0");
        assert_eq!(normalize_tag("0.2.0"), "v0.2.0");
    }

    #[test]
    fn version_gt_compares_the_semver_core() {
        assert!(version_gt("0.2.0", "0.1.0"));
        assert!(!version_gt("0.1.0", "0.1.0"));
        assert!(version_gt("1.0.0", "0.9.9"));
        // A pre-release/build suffix is ignored — the core triple decides.
        assert_eq!(parse_core("0.2.0-rc1"), (0, 2, 0));
        assert_eq!(parse_core("0.2.0+build.7"), (0, 2, 0));
        // A malformed side parses to (0,0,0), so a valid newer version still wins.
        assert_eq!(parse_core(""), (0, 0, 0));
        assert!(version_gt("0.1.0", ""));
        assert!(!version_gt("", "0.1.0"));
    }

    #[test]
    fn expected_sum_matches_the_exact_asset_line() {
        let asset = "topos-x86_64-unknown-linux-gnu.tar.gz";
        let sums = format!(
            // A leading BLANK line must not abort the scan (it once did). A DIFFERENT asset whose name is
            // a substring of ours must never match; a leading '*' binary-mode marker is stripped; the real
            // line uses the coreutils two-space format.
            "\n\
             1111111111111111111111111111111111111111111111111111111111111111  topos-x86_64.tar.gz\n\
             2222222222222222222222222222222222222222222222222222222222222222 *{asset}\n"
        );
        assert_eq!(
            expected_sum(&sums, asset).as_deref(),
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        );
        // Absent → None.
        assert!(expected_sum(&sums, "topos-aarch64-apple-darwin.tar.gz").is_none());
        // The substring asset resolves to its OWN line, never ours.
        assert_eq!(
            expected_sum(&sums, "topos-x86_64.tar.gz").as_deref(),
            Some("1111111111111111111111111111111111111111111111111111111111111111")
        );
    }

    #[test]
    fn extract_topos_reads_the_binary_and_rejects_a_tarball_without_it() {
        let targz = build_targz(&[
            ("LICENSE", b"Apache-2.0\n", 0o644),
            ("topos", b"#!/bin/sh\nnew binary\n", 0o755),
        ]);
        assert_eq!(extract_topos(&targz).unwrap(), b"#!/bin/sh\nnew binary\n");
        // A tarball with no `topos` regular file is a corrupt/forged asset.
        let no_topos = build_targz(&[("LICENSE", b"Apache-2.0\n", 0o644)]);
        let err = extract_topos(&no_topos).unwrap_err();
        assert!(matches!(err, ClientError::WireInvalid(_)), "got {err:?}");
        // A NESTED `subdir/topos` is not the top-level binary — the match is an exact top-level path.
        let nested = build_targz(&[
            ("subdir/topos", b"not the binary", 0o755),
            ("LICENSE", b"Apache-2.0\n", 0o644),
        ]);
        let err = extract_topos(&nested).unwrap_err();
        assert!(matches!(err, ClientError::WireInvalid(_)), "got {err:?}");
    }

    #[test]
    fn upgrade_replaces_the_binary_when_the_checksum_matches() {
        let tag = "v9.9.9";
        let (asset, asset_url, sums_url) = urls(tag);
        let new_bin = b"#!/bin/sh\nthe upgraded binary\n";
        let targz = build_targz(&[
            ("LICENSE", b"Apache-2.0\n", 0o644),
            ("topos", new_bin, 0o755),
        ]);
        let sums = format!("{}  {asset}\n", to_hex(&sha256(&targz)));
        let releases = FakeReleases {
            latest: tag.to_owned(),
            blobs: [(asset_url, targz), (sums_url, sums.into_bytes())]
                .into_iter()
                .collect(),
        };

        let exe_dir = scratch("exe-ok");
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();

        let out = with_ctx(|ctx| {
            self_update_with_key(
                ctx,
                &releases,
                &current_exe,
                SelfUpdateOpts {
                    check: false,
                    version: Some(tag.to_owned()),
                    base_url: None,
                },
                None,
            )
        })
        .expect("a matching checksum installs");

        assert!(matches!(out.action, SelfUpdateAction::Upgraded));
        assert_eq!(out.latest_version.as_deref(), Some("9.9.9"));
        // The running binary now holds the extracted `topos` bytes, byte-exact.
        assert_eq!(std::fs::read(&current_exe).unwrap(), new_bin);
        // Driven KEYLESS (the pre-ceremony shape), the install is checksum-only — disclosed
        // honestly: not signed, with the unsigned-build note. The production entry compiles in
        // `RELEASE_PUBKEY` (mandatory signatures — the signature tests cover that path).
        assert!(!out.signed);
        assert!(
            out.note
                .as_deref()
                .is_some_and(|n| n.contains("no release-signing public key")),
            "{:?}",
            out.note
        );
    }

    #[test]
    fn upgrade_refuses_a_checksum_mismatch_and_leaves_the_binary_untouched() {
        let tag = "v9.9.9";
        let (asset, asset_url, sums_url) = urls(tag);
        let targz = build_targz(&[("topos", b"tampered binary", 0o755)]);
        // A WRONG sum (all zeros) for the asset → the verify must refuse before any fs write.
        let sums = format!("{}  {asset}\n", "0".repeat(64));
        let releases = FakeReleases {
            latest: tag.to_owned(),
            blobs: [(asset_url, targz), (sums_url, sums.into_bytes())]
                .into_iter()
                .collect(),
        };

        let exe_dir = scratch("exe-bad");
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();

        let err = with_ctx(|ctx| {
            self_update_with_key(
                ctx,
                &releases,
                &current_exe,
                SelfUpdateOpts {
                    check: false,
                    version: Some(tag.to_owned()),
                    base_url: None,
                },
                None,
            )
        })
        .unwrap_err();

        assert!(
            matches!(err, ClientError::ChecksumMismatch { .. }),
            "got {err:?}"
        );
        assert_eq!(err.code(), "INTEGRITY_ERROR");
        // The binary is byte-intact — no torn/partial write, and no download was trusted.
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn upgrade_check_mode_reports_without_writing() {
        let tag = "v9.9.9";
        // Check mode resolves the latest tag but downloads nothing and writes nothing.
        let releases = FakeReleases {
            latest: tag.to_owned(),
            blobs: std::collections::HashMap::new(),
        };
        let exe_dir = scratch("exe-check");
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();

        let out = with_ctx(|ctx| {
            self_update(
                ctx,
                &releases,
                &current_exe,
                SelfUpdateOpts {
                    check: true,
                    version: None,
                    base_url: None,
                },
            )
        })
        .expect("check mode reports");

        assert!(matches!(out.action, SelfUpdateAction::Checked));
        assert!(out.update_available, "v9.9.9 is newer than this build");
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn upgrade_is_a_no_op_when_the_pinned_tag_equals_the_current_version() {
        // An explicit `--version` naming THIS build's version installs nothing.
        let pinned = format!("v{CURRENT_VERSION}");
        let releases = FakeReleases {
            latest: pinned.clone(),
            blobs: std::collections::HashMap::new(),
        };
        let exe_dir = scratch("exe-current");
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();

        let out = with_ctx(|ctx| {
            self_update(
                ctx,
                &releases,
                &current_exe,
                SelfUpdateOpts {
                    check: false,
                    version: Some(pinned),
                    base_url: None,
                },
            )
        })
        .expect("already current is a clean success");

        assert!(matches!(out.action, SelfUpdateAction::AlreadyCurrent));
        assert!(!out.update_available);
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn upgrade_warns_on_a_non_https_base_url() {
        let tag = "v9.9.9";
        let base = "http://127.0.0.1:8080/mirror";
        let asset = format!("topos-{TARGET_TRIPLE}.tar.gz");
        let asset_url = format!("{base}/download/{tag}/{asset}");
        let sums_url = format!("{base}/download/{tag}/SHA256SUMS");
        let new_bin = b"mirror binary";
        let targz = build_targz(&[("topos", new_bin, 0o755)]);
        let sums = format!("{}  {asset}\n", to_hex(&sha256(&targz)));
        let releases = FakeReleases {
            latest: tag.to_owned(),
            blobs: [(asset_url, targz), (sums_url, sums.into_bytes())]
                .into_iter()
                .collect(),
        };
        let exe_dir = scratch("exe-mirror");
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();

        let out = with_ctx(|ctx| {
            self_update_with_key(
                ctx,
                &releases,
                &current_exe,
                SelfUpdateOpts {
                    check: false,
                    version: Some(tag.to_owned()),
                    // A non-HTTPS mirror the operator controls — the checksum is still enforced.
                    base_url: Some(base.to_owned()),
                },
                None,
            )
        })
        .expect("a non-HTTPS mirror still installs with the checksum enforced");

        assert!(matches!(out.action, SelfUpdateAction::Upgraded));
        assert!(
            out.warning
                .as_deref()
                .is_some_and(|w| w.contains("non-HTTPS")),
            "{:?}",
            out.warning
        );
        assert_eq!(std::fs::read(&current_exe).unwrap(), new_bin);
    }

    /// A guard that the injected `current_exe` never escapes into the real binary path.
    #[test]
    fn extract_topos_ignores_a_directory_named_topos() {
        // Only a REGULAR file named `topos` is the binary — a dir entry named `topos` is skipped.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut dir_header = tar::Header::new_ustar();
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_size(0);
        dir_header.set_mode(0o755);
        dir_header.set_mtime(0);
        tar.append_data(&mut dir_header, "topos/", &b""[..])
            .unwrap();
        let gz = tar.into_inner().unwrap();
        let targz = gz.finish().unwrap();
        let err = extract_topos(&targz).unwrap_err();
        assert!(matches!(err, ClientError::WireInvalid(_)), "got {err:?}");
    }

    #[test]
    fn mirror_mode_requires_an_explicit_version() {
        // A custom base URL has no "latest release" API — a bare upgrade against it is a usage error,
        // caught BEFORE any network call (the fake would panic if `latest_tag`/`download` were reached).
        struct Unreachable;
        impl ReleaseSource for Unreachable {
            fn latest_tag(&self) -> Result<String, ClientError> {
                panic!("latest_tag must not be called in mirror mode without --version")
            }
            fn download(&self, _url: &str) -> Result<Vec<u8>, ClientError> {
                panic!("download must not be called")
            }
        }
        let exe_dir = scratch("exe-mirror-nover");
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();

        let err = with_ctx(|ctx| {
            self_update(
                ctx,
                &Unreachable,
                &current_exe,
                SelfUpdateOpts {
                    check: false,
                    version: None,
                    base_url: Some("https://mirror.example/releases".to_owned()),
                },
            )
        })
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    // ---- the mandatory-signature posture (a compiled-in release public key) --------------------
    //
    // Signing capability is a DEV-dependency only (the `minisign` crate) — the shipped binary
    // carries verify-only code (`minisign-verify`, zero deps). Each test mints a throwaway keypair.

    /// A throwaway minisign keypair: (the base64 public key — what the compiled-in constant holds,
    /// the keypair for signing).
    fn test_keypair() -> (String, minisign::KeyPair) {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("keygen");
        (kp.pk.to_base64(), kp)
    }

    /// Sign `bytes` the way the release pipeline does: pre-hashed (the modern minisign default),
    /// with the pipeline's trusted-comment format binding the signature to one release. Returns the
    /// full `.minisig` document text.
    fn sign_bytes(kp: &minisign::KeyPair, bytes: &[u8], trusted_comment: &str) -> String {
        minisign::sign(
            Some(&kp.pk),
            &kp.sk,
            std::io::Cursor::new(bytes),
            Some(trusted_comment),
            Some("topos test"),
        )
        .expect("sign")
        .into_string()
    }

    /// The release pipeline's trusted comment for `tag` + this build's asset — what CI's
    /// `minisign -S -t` writes and what the verifier binds against.
    fn release_comment(tag: &str) -> String {
        format!("topos-sh/topos {tag} topos-{TARGET_TRIPLE}.tar.gz")
    }

    /// A signed-release fixture: the tarball + correct SHA256SUMS + a `.minisig` (as `sig` says) at
    /// the default GitHub URLs for `tag`, plus a scratch `current_exe` holding the OLD binary.
    fn signed_release(
        tag: &str,
        sig: Option<String>,
        scratch_tag: &str,
    ) -> (FakeReleases, PathBuf) {
        let (asset, asset_url, sums_url) = urls(tag);
        let targz = build_targz(&[
            ("LICENSE", b"Apache-2.0\n", 0o644),
            ("topos", b"#!/bin/sh\nthe signed binary\n", 0o755),
        ]);
        let sums = format!("{}  {asset}\n", to_hex(&sha256(&targz)));
        let mut blobs: std::collections::HashMap<String, Vec<u8>> =
            [(asset_url.clone(), targz), (sums_url, sums.into_bytes())]
                .into_iter()
                .collect();
        if let Some(sig) = sig {
            blobs.insert(format!("{asset_url}.minisig"), sig.into_bytes());
        }
        let releases = FakeReleases {
            latest: tag.to_owned(),
            blobs,
        };
        let exe_dir = scratch(scratch_tag);
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();
        (releases, current_exe)
    }

    fn run_with_key(
        releases: &FakeReleases,
        current_exe: &Path,
        tag: &str,
        pubkey: &str,
    ) -> Result<super::SelfUpdateOutcome, ClientError> {
        with_ctx(|ctx| {
            self_update_with_key(
                ctx,
                releases,
                current_exe,
                SelfUpdateOpts {
                    check: false,
                    version: Some(tag.to_owned()),
                    base_url: None,
                },
                Some(pubkey),
            )
        })
    }

    #[test]
    fn signed_build_happy_path_verifies_and_installs() {
        let tag = "v9.9.9";
        let (pubkey, kp) = test_keypair();
        // Sign the EXACT tarball the fixture serves (rebuild the same bytes deterministically).
        let targz = build_targz(&[
            ("LICENSE", b"Apache-2.0\n", 0o644),
            ("topos", b"#!/bin/sh\nthe signed binary\n", 0o755),
        ]);
        let (releases, current_exe) = signed_release(
            tag,
            Some(sign_bytes(&kp, &targz, &release_comment(tag))),
            "sig-ok",
        );

        let out = run_with_key(&releases, &current_exe, tag, &pubkey)
            .expect("a valid signature + checksum installs");
        assert!(matches!(out.action, SelfUpdateAction::Upgraded));
        assert!(out.signed, "the outcome discloses the verified signature");
        assert!(
            out.note.is_none(),
            "no unsigned-build note on a signed install"
        );
        assert_eq!(
            std::fs::read(&current_exe).unwrap(),
            b"#!/bin/sh\nthe signed binary\n"
        );
    }

    #[test]
    fn signed_build_refuses_a_missing_minisig() {
        // With a key compiled in, signature verification is mandatory: no `.minisig` on the release
        // is a typed INTEGRITY refusal, not a silent checksum-only downgrade.
        let tag = "v9.9.9";
        let (pubkey, _kp) = test_keypair();
        let (releases, current_exe) = signed_release(tag, None, "sig-missing");

        let err = run_with_key(&releases, &current_exe, tag, &pubkey).unwrap_err();
        assert!(
            matches!(err, ClientError::SignatureInvalid { .. }),
            "got {err:?}"
        );
        assert_eq!(err.code(), "INTEGRITY_ERROR");
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn signed_build_refuses_a_corrupted_signature() {
        let tag = "v9.9.9";
        let (pubkey, kp) = test_keypair();
        let targz = build_targz(&[
            ("LICENSE", b"Apache-2.0\n", 0o644),
            ("topos", b"#!/bin/sh\nthe signed binary\n", 0o755),
        ]);
        // Corrupt the base64 signature line (line 2 of the .minisig document) — one flipped char.
        let good = sign_bytes(&kp, &targz, &release_comment(tag));
        let corrupted: String = good
            .lines()
            .enumerate()
            .map(|(i, line)| {
                if i == 1 {
                    let mut chars: Vec<char> = line.chars().collect();
                    let mid = chars.len() / 2;
                    chars[mid] = if chars[mid] == 'A' { 'B' } else { 'A' };
                    chars.into_iter().collect::<String>() + "\n"
                } else {
                    line.to_owned() + "\n"
                }
            })
            .collect();
        assert_ne!(good, corrupted, "the corruption must change the document");
        let (releases, current_exe) = signed_release(tag, Some(corrupted), "sig-corrupt");

        let err = run_with_key(&releases, &current_exe, tag, &pubkey).unwrap_err();
        assert!(
            matches!(err, ClientError::SignatureInvalid { .. }),
            "got {err:?}"
        );
        assert_eq!(err.code(), "INTEGRITY_ERROR");
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn signed_build_refuses_a_signature_over_different_bytes() {
        // A VALID signature by the right key — but over other bytes. The classic swap: serve asset B
        // with asset A's signature. Must refuse before the binary is touched.
        let tag = "v9.9.9";
        let (pubkey, kp) = test_keypair();
        let wrong_sig = sign_bytes(&kp, b"entirely different bytes", &release_comment(tag));
        let (releases, current_exe) = signed_release(tag, Some(wrong_sig), "sig-wrong-bytes");

        let err = run_with_key(&releases, &current_exe, tag, &pubkey).unwrap_err();
        assert!(
            matches!(err, ClientError::SignatureInvalid { .. }),
            "got {err:?}"
        );
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn signature_is_checked_before_the_checksum() {
        // Both the signature AND the checksum are wrong: the refusal must be the SIGNATURE one —
        // proving the ordering (sig gate first, checksum second, binary never touched).
        let tag = "v9.9.9";
        let (pubkey, kp) = test_keypair();
        let (asset, asset_url, sums_url) = urls(tag);
        let targz = build_targz(&[("topos", b"tampered binary", 0o755)]);
        let wrong_sums = format!("{}  {asset}\n", "0".repeat(64));
        let wrong_sig = sign_bytes(&kp, b"not the tarball", &release_comment(tag));
        let releases = FakeReleases {
            latest: tag.to_owned(),
            blobs: [
                (asset_url.clone(), targz),
                (sums_url, wrong_sums.into_bytes()),
                (format!("{asset_url}.minisig"), wrong_sig.into_bytes()),
            ]
            .into_iter()
            .collect(),
        };
        let exe_dir = scratch("sig-order");
        let current_exe = exe_dir.join("topos");
        std::fs::write(&current_exe, b"the OLD binary").unwrap();

        let err = run_with_key(&releases, &current_exe, tag, &pubkey).unwrap_err();
        assert!(
            matches!(err, ClientError::SignatureInvalid { .. }),
            "the signature gate must fire first, got {err:?}"
        );
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn a_wrong_keys_signature_is_refused() {
        // A valid signature over the right bytes — by a DIFFERENT key. The compiled-in key decides.
        let tag = "v9.9.9";
        let (pubkey, _kp) = test_keypair();
        let (_other_pub, other_kp) = test_keypair();
        let targz = build_targz(&[
            ("LICENSE", b"Apache-2.0\n", 0o644),
            ("topos", b"#!/bin/sh\nthe signed binary\n", 0o755),
        ]);
        let (releases, current_exe) = signed_release(
            tag,
            Some(sign_bytes(&other_kp, &targz, &release_comment(tag))),
            "sig-wrong-key",
        );

        let err = run_with_key(&releases, &current_exe, tag, &pubkey).unwrap_err();
        assert!(
            matches!(err, ClientError::SignatureInvalid { .. }),
            "got {err:?}"
        );
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn a_valid_signature_for_a_different_release_is_refused() {
        // The substitution attack the trusted-comment binding closes: a VALID signature by the
        // RIGHT key over the EXACT bytes served — but minted for an OLD release (its signed
        // trusted comment names the old tag). Re-served under a newer tag, the crypto verifies and
        // the checksum matches (the attacker moved SHA256SUMS too) — the comment binding must
        // still refuse it.
        let tag = "v9.9.9";
        let (pubkey, kp) = test_keypair();
        let targz = build_targz(&[
            ("LICENSE", b"Apache-2.0\n", 0o644),
            ("topos", b"#!/bin/sh\nthe signed binary\n", 0o755),
        ]);
        let old_release_sig = sign_bytes(&kp, &targz, &release_comment("v1.0.0"));
        let (releases, current_exe) = signed_release(tag, Some(old_release_sig), "sig-substitute");

        let err = run_with_key(&releases, &current_exe, tag, &pubkey).unwrap_err();
        assert!(
            matches!(err, ClientError::SignatureInvalid { .. }),
            "got {err:?}"
        );
        assert_eq!(err.code(), "INTEGRITY_ERROR");
        assert!(
            format!("{err}").contains("not bound to this release"),
            "{err}"
        );
        assert_eq!(std::fs::read(&current_exe).unwrap(), b"the OLD binary");
    }

    #[test]
    fn release_pubkey_when_present_is_a_valid_minisign_key() {
        // The key-ceremony guard: a pasted Some("…") constant must be a decodable minisign public
        // key. Today's None state passes vacuously; after the ceremony this catches a bad paste in
        // `cargo test` before any release ships it.
        if let Some(key) = RELEASE_PUBKEY {
            minisign_verify::PublicKey::from_base64(key)
                .expect("RELEASE_PUBKEY must be the base64 line of a minisign public key");
        }
    }

    #[test]
    fn map_replace_error_gives_guidance_only_for_unwritable() {
        use std::io::{Error, ErrorKind};
        let p = std::path::Path::new("/opt/topos/bin/topos");
        // A read-only / package-managed location → actionable guidance (permanent, no retry loop).
        for kind in [ErrorKind::PermissionDenied, ErrorKind::ReadOnlyFilesystem] {
            let e = map_replace_error(Error::from(kind), p);
            assert!(matches!(e, ClientError::UpgradeUnwritable(_)), "{kind:?}");
            assert_eq!(e.code(), "IO_ERROR");
            assert!(format!("{e}").contains("/opt/topos/bin/topos"));
        }
        // Any other filesystem failure keeps its plain identity — no false "reinstall" guidance.
        let other = map_replace_error(Error::from(ErrorKind::NotFound), p);
        assert!(
            matches!(other, ClientError::IoKind { .. } | ClientError::Io(_)),
            "got {other:?}"
        );
    }
}
