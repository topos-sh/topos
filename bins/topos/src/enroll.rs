//! The LOGIN WAL — the one surviving identity document of the enrollment era: a live
//! device-authorization flow awaiting the human's browser approval (`topos login` writes it,
//! resumes it, and deletes it once the granted poll persists the session). Everything else this
//! module once held — the pinned instance, the device credential, the membership roster, the
//! subscription file — is RETIRED: sessions (`identity/sessions.json`) are the identity, and
//! demand lives in manifests. The recovery sweep still deletes those legacy files on sight.

use serde::{Deserialize, Serialize};

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// Which verb owns a pending flow. The session login is the ONE live variant; the retired
/// spellings still parse (a leftover WAL from an older binary is swept, not a crash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum EnrollIntentDoc {
    /// A `topos login <workspace-address>` SESSION login (the grant mints ONE workspace-scoped
    /// session into `identity/sessions.json`).
    Session,
    /// RETIRED (parse-tolerated): the device-era `follow` enrollment.
    #[serde(other)]
    Retired,
}

/// The enrollment WAL document — ONE live device-authorization flow, awaiting the human's approval.
/// The whole document is a `0600` secret (the device code is promoted to the credential on approval).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingEnrollment {
    pub schema_version: u32,
    /// The API base the flow runs against (the card's declared base, re-root-gated).
    pub base_url: String,
    /// The ADDRESS host the human typed (`topos.sh`, `topos.example.com[:port]`) — the manifest
    /// grammar's host half, recorded so the minted session carries it. ADDITIVE with a serde
    /// default (a pre-field WAL reads as empty; the session persist falls back to the base URL's
    /// host).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// The requested workspace ADDRESS slug (the `device/authorize` body's `workspace`). Whether it
    /// exists is never disclosed pre-approval; the granted poll carries the authoritative workspace.
    pub workspace_name: String,
    /// Which verb owns the resume (and, for a follow, the intent to continue into).
    pub intent: EnrollIntentDoc,
    /// **SECRET** — the device code the client polls with. Redacted in `Debug`.
    pub device_code: String,
    /// The short user code (the cross-check shown on the approval page).
    pub user_code: String,
    /// The SERVER-built approval URL with the code embedded — re-emitted verbatim while pending.
    #[serde(alias = "verification_uri_complete")]
    pub verification_uri: String,
    /// The minimum poll interval, in seconds.
    pub interval_secs: u64,
    /// The flow expiry as epoch-millis — the recovery sweep abandons a WAL past this.
    pub expires_at_millis: i64,
}

// Redact the WAL's secret (the device code — the credential-to-be) so the whole document, held
// transiently in memory, can never leak it through a Debug dump / panic / log.
impl std::fmt::Debug for PendingEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingEnrollment")
            .field("schema_version", &self.schema_version)
            .field("base_url", &self.base_url)
            .field("host", &self.host)
            .field("workspace_name", &self.workspace_name)
            .field("intent", &self.intent)
            .field("device_code", &"<redacted>")
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
/// AND on a persisted workspace name outside the address grammar (the WAL is a durable copy of wire
/// data; the name rides request BODIES only — never a path join — but a hand-edited traversal shape
/// still fails the load closed, the same boundary discipline as every other persisted identifier).
/// An EMPTY name is the legitimate ORIGIN enrollment (the workspace the origin itself addresses —
/// single-tenant installs); the granted poll carries the authoritative workspace back.
pub(crate) fn read_wal(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<PendingEnrollment>, ClientError> {
    let wal: Option<PendingEnrollment> = doc::read_doc_private(fs, &layout.enrollment_path())?;
    if let Some(w) = &wal
        && !w.workspace_name.is_empty()
        && !crate::resolve::is_workspace_name(&w.workspace_name)
    {
        return Err(ClientError::Corrupt(
            "the enrollment WAL's workspace name is not a valid address name".into(),
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
