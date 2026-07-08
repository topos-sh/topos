//! The injectable release-source seam for `topos upgrade` — the one place the updater reaches the
//! upstream. Kept behind a trait (mirroring the plane transport seams) so the whole self-update flow —
//! latest-tag resolution, asset + `SHA256SUMS` download, checksum verify, atomic replace — is unit-tested
//! with a fake and no HTTP. The real `ureq` implementation ([`crate::plane_http::UreqReleases`]) lives
//! beside the other network transports.

use crate::error::ClientError;

/// The upstream release source — GitHub in production, a fake in tests. Kept behind a trait so the whole
/// upgrade flow is unit-tested with no HTTP.
pub(crate) trait ReleaseSource {
    /// Resolve the latest published release tag (e.g. "v0.2.0") from the upstream.
    fn latest_tag(&self) -> Result<String, ClientError>;
    /// Download raw bytes from an absolute URL (a release tarball or its SHA256SUMS).
    fn download(&self, url: &str) -> Result<Vec<u8>, ClientError>;
}
