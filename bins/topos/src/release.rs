//! The injectable release-source seams for `topos self-update` and the passive version check — the
//! only places the binary reaches the release upstream. Kept behind traits (mirroring the plane
//! transport seams) so the whole self-update flow — latest-tag resolution, asset + `SHA256SUMS` +
//! `.minisig` download, signature + checksum verify, atomic replace — and the version-check nag are
//! unit-tested with fakes and no HTTP. The real `ureq` implementations
//! ([`crate::plane_http::UreqReleases`], [`crate::plane_http::UreqVersionProbe`]) live beside the
//! other network transports.

use crate::error::ClientError;

/// The upstream release source — GitHub in production, a fake in tests. Kept behind a trait so the whole
/// upgrade flow is unit-tested with no HTTP.
pub(crate) trait ReleaseSource {
    /// Resolve the latest published release tag (e.g. "v0.2.0") from the upstream.
    fn latest_tag(&self) -> Result<String, ClientError>;
    /// Download raw bytes from an absolute URL (a release tarball, its SHA256SUMS, or a `.minisig`).
    fn download(&self, url: &str) -> Result<Vec<u8>, ClientError>;
}

/// The PASSIVE version-check probe — the one network touch of the after-command nag. A separate,
/// deliberately narrower seam than [`ReleaseSource`]: the probe follows NO redirects (the
/// `releases/latest` 302's `Location` header IS the answer — no API, no auth, no JSON), enforces a
/// hard short timeout, and never errors (silence on every failure is the contract).
pub(crate) trait ReleaseProbe {
    /// One redirect-disabled GET of the public `releases/latest` URL, answering the redirect's
    /// `Location` header verbatim — or `None` on ANY failure (timeout, non-redirect, no header).
    fn latest_release_location(&self) -> Option<String>;
}
