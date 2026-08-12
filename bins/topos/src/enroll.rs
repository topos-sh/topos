//! The LOGIN WAL — the one surviving identity document of the enrollment era: a live
//! device-authorization flow awaiting the human's browser approval (`topos login` writes it,
//! resumes it, and deletes it once the granted poll persists the session). Everything else this
//! module once held — the pinned instance, the device credential, the membership roster, the
//! subscription file — is RETIRED: sessions (`identity/sessions.json`) are the identity, and
//! demand lives in manifests. Only the SECRET of that set is still swept — the recovery pass
//! shreds a stray `identity/credentials.json` (and the `device.key` beside it) on sight, because
//! an unread credential is still a credential; the plain documents are simply inert.

use serde::{Deserialize, Serialize};

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// Which verb owns a pending flow. The session login is the only one there is; a document naming
/// anything else fails the parse, which is the right answer for a transient file — the flow it
/// describes cannot be resumed by this binary anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum EnrollIntentDoc {
    /// A `topos login [<address>]` SESSION login (the grant mints ONE workspace-scoped session
    /// into `identity/sessions.json`).
    Session,
}

/// The enrollment WAL document — ONE live device-authorization flow, awaiting the human's approval.
/// The whole document is a `0600` secret (the device code is promoted to the credential on approval).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingEnrollment {
    pub schema_version: u32,
    /// The API base the flow runs against (the card's declared base, re-root-gated).
    pub base_url: String,
    /// The ADDRESS host the human typed (`topos.sh`, `topos.example.com[:port]`) — the manifest
    /// grammar's host half, recorded so the minted session carries it. Empty when the flow names
    /// only an origin; the session persist then falls back to the base URL's host.
    pub host: String,
    /// The workspace ADDRESS slug a `login <workspace>` shortcut PRESELECTED for the browser
    /// chooser (empty = none: the human picks or creates the workspace at the approval). Whether
    /// it exists is never disclosed pre-approval; the granted poll carries the authoritative
    /// workspace. The bare login is the headline usage, so the empty case stays out of the bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preselect: String,
    /// Which verb owns the resume.
    pub intent: EnrollIntentDoc,
    /// **SECRET** — the device code the client polls with. Redacted in `Debug`.
    pub device_code: String,
    /// The short user code (the cross-check shown on the approval page).
    pub user_code: String,
    /// The SERVER-built approval URL with the code embedded — re-emitted verbatim while pending.
    pub verification_uri: String,
    /// The minimum poll interval, in seconds.
    pub interval_secs: u64,
    /// The flow expiry as epoch-millis — the recovery sweep abandons a WAL past this.
    pub expires_at_millis: i64,
    /// This flow was started LOOPBACK-bound: a browser on this machine approves it and the
    /// redirect wakes our listener. A wake-up only — the poll is the completion mechanism either
    /// way; the flag exists so a resume knows whether the approval page pre-arms itself (and the
    /// terminal therefore never printed the short code).
    pub loopback: bool,
}

// Redact the WAL's secret (the device code — the credential-to-be) so the whole document, held
// transiently in memory, can never leak it through a Debug dump / panic / log.
impl std::fmt::Debug for PendingEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingEnrollment")
            .field("schema_version", &self.schema_version)
            .field("base_url", &self.base_url)
            .field("host", &self.host)
            .field("preselect", &self.preselect)
            .field("intent", &self.intent)
            .field("device_code", &"<redacted>")
            .field("loopback", &self.loopback)
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("interval_secs", &self.interval_secs)
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

/// Write the enrollment WAL `0600` (a secret). The identity dir must exist.
pub(crate) fn write_wal(
    fs: &dyn FsOps,
    layout: &Layout,
    wal: &PendingEnrollment,
) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.identity_dir())?;
    doc::write_doc_private(fs, &layout.enrollment_path(), wal)
}

/// Read the enrollment WAL (a `0600` secret), or `None` if absent. Fail-closed on a permissive secret
/// AND on a persisted preselection outside the address grammar (the WAL is a durable copy of wire
/// data; the name rides request BODIES only — never a path join — but a hand-edited traversal shape
/// still fails the load closed, the same boundary discipline as every other persisted identifier).
/// An EMPTY preselection is the ordinary bare `topos login` (the human chooses the workspace in the
/// browser); the granted poll carries the authoritative workspace back either way.
pub(crate) fn read_wal(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<PendingEnrollment>, ClientError> {
    let wal: Option<PendingEnrollment> = doc::read_doc_private(fs, &layout.enrollment_path())?;
    if let Some(w) = &wal
        && !w.preselect.is_empty()
        && !crate::resolve::is_workspace_name(&w.preselect)
    {
        return Err(ClientError::Corrupt(
            "the enrollment WAL's preselected workspace is not a valid address name".into(),
        ));
    }
    Ok(wal)
}

/// Delete the enrollment WAL (once the grant persisted, or on a swept abandon). NotFound-tolerant.
pub(crate) fn delete_wal(fs: &dyn FsOps, layout: &Layout) -> Result<(), ClientError> {
    fs.remove_file(&layout.enrollment_path())?;
    Ok(())
}

/// The recovery sweep for the enrollment WAL: remove a WAL whose flow has expired
/// (`now_millis > expires_at_millis`) — a clean abandon (the server's flow row expired with it). An
/// unexpired WAL is preserved (a resume can still poll it; a granted flow re-answers the same grant).
/// Best-effort: an unreadable/corrupt WAL is left in place for the owning op to diagnose, never
/// hard-failing recovery.
///
/// The read → decide → delete runs UNDER the `"identity"` lock (the same lock every identity write
/// holds), and the expiry is decided from the read taken under that lock — never from an earlier
/// observation.
pub(crate) fn sweep_expired_wal(
    fs: &dyn FsOps,
    layout: &Layout,
    now_millis: i64,
) -> Result<(), ClientError> {
    // A cheap unlocked probe first: no WAL at all (the overwhelmingly common case — the sweep runs at the
    // start of EVERY command) takes no lock and touches nothing.
    if !fs.exists(&layout.enrollment_path()) {
        return Ok(());
    }
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    // The authoritative read, under the lock, immediately before any delete decision.
    let wal = match read_wal(fs, layout) {
        Ok(Some(wal)) => wal,
        // Absent → nothing to sweep. Unreadable/permissive/corrupt → leave it for the op to surface.
        Ok(None) | Err(_) => return Ok(()),
    };
    if now_millis > wal.expires_at_millis {
        delete_wal(fs, layout)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use topos_types::PERSISTED_SCHEMA_VERSION;

    use super::*;
    use crate::fs_seam::RealFs;
    use crate::sidecar::Layout;

    /// The WAL round-trips whole, and its SECRET never surfaces in a Debug dump — the document is
    /// held in memory across the whole pending flow, so a panic or a log line must not carry the
    /// device code out with it.
    #[test]
    fn the_wal_round_trips_and_never_debugs_its_secret() {
        let fs = RealFs;
        let dir = std::env::temp_dir().join(format!("topos-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        let written = PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            base_url: "https://topos.sh/api".to_owned(),
            host: "topos.sh".to_owned(),
            preselect: "acme".to_owned(),
            intent: EnrollIntentDoc::Session,
            device_code: "dc_secret".to_owned(),
            user_code: "AB12-CD34".to_owned(),
            verification_uri: "https://topos.sh/verify".to_owned(),
            interval_secs: 5,
            expires_at_millis: 9_000,
            loopback: true,
        };
        write_wal(&fs, &layout, &written).unwrap();

        let wal = read_wal(&fs, &layout).unwrap().unwrap();
        assert_eq!(wal, written);
        assert!(!format!("{wal:?}").contains("dc_secret"), "{wal:?}");
    }
}
