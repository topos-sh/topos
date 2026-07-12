//! The write-actor vocabulary — WHICH LANE a pointer-move/reject request arrived on, and how the
//! receipts layer records it.
//!
//! One module so the three types stay together: [`WriteActor`] is what the orchestration hands the
//! transaction — the device lane's presented workspace credential (a bearer SECRET, carried only as
//! its sha256 past the public boundary) beside its pool-pre-resolved `device_key_id`, or the session
//! lane's verified principal + its domain-tagged request identity — and [`WriteActor::receipt_actor`]
//! is the ONE projection into the receipts layer ([`ReceiptActor`]): every terminal writer derives
//! its `(actor, method, request_sha256)` triple here, so the lane vocabulary cannot drift per writer.
//! A crate-root shared leaf: custody consumes it, the directory's session legs construct it, and
//! neither imports the other to do so.

use crate::id::Principal;

/// The ONE uniform acting-gate denial for the session lane: a non-member, a merely-invited seat, an
/// absent workspace, and a self-host caller past the posture belt all read the same (the static reason
/// is for the composing wrapper's classification, never an oracle — and it is never persisted). Lane
/// vocabulary, so it lives with the actor types both sides of the seam share.
pub const SESSION_REVIEW_ACTING_DENIED: &str =
    "session review ops require a confirmed workspace member";

/// The machine-branchable code on the DURABLE role denial (a confirmed plain member).
pub const REVIEWER_ROLE_REQUIRED_CODE: &str = "REVIEWER_ROLE_REQUIRED";

/// The role denial's message — a plane→web byte contract (the cloud pins it verbatim).
pub(crate) const REVIEWER_ROLE_REQUIRED_MSG: &str =
    "approving or rejecting needs an owner or reviewer seat";

/// The lane a contribute write arrived on. The transaction bodies branch on this ONLY at their
/// authorization step; every other step is actor-blind.
#[derive(Debug, Clone)]
pub(crate) enum WriteActor<'a> {
    /// The device lane (the CLI): the presented workspace credential's sha256 (the stored form — the
    /// plaintext never crosses the orchestration boundary) plus the POOL-pre-resolved
    /// `device_key_id` the pre-transaction receipt machinery keys on. The transaction authenticates
    /// by LOOKUP — it re-resolves `credential_sha256` against the live registry row INSIDE the
    /// write transaction and requires the row to name exactly this `device_key_id` (they can only
    /// diverge if the credential rotated mid-flight, which fails closed as a pre-auth denial) —
    /// never by a possession proof.
    Device {
        credential_sha256: [u8; 32],
        device_key_id: &'a str,
    },
    /// The web-session lane (hosted compositions only; self-host is denied upstream). `acting` is
    /// the composing caller's session-verified, canonical principal; `request_sha256` is the
    /// domain-tagged full-request identity (reason included on a reject) the replay probe compares.
    Session {
        acting: &'a Principal,
        request_sha256: [u8; 32],
    },
}

impl WriteActor<'_> {
    /// The ONE projection into the receipts layer. The device lane's actor is its (pre-resolved,
    /// in-transaction re-verified) `device_key_id` — the device's stable name, never the presented
    /// credential (a credential rotates on re-enrollment; a lost-ack retry across that rotation must
    /// still name the same slot).
    pub(crate) fn receipt_actor(&self) -> ReceiptActor<'_> {
        match self {
            WriteActor::Device { device_key_id, .. } => ReceiptActor {
                actor: device_key_id,
                method: ReceiptMethod::Device,
                request_sha256: None,
            },
            WriteActor::Session {
                acting,
                request_sha256,
            } => ReceiptActor {
                actor: acting.as_str(),
                method: ReceiptMethod::WebSession,
                request_sha256: Some(*request_sha256),
            },
        }
    }
}

/// The stored `op_receipts.method` discriminant — which leg wrote the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptMethod {
    /// A device-credential op; the receipt's `actor` is the acting device key id.
    Device,
    /// A web-session op; the receipt's `actor` is the acting principal's verified email.
    WebSession,
}

impl ReceiptMethod {
    /// The stored string form (matches the `op_receipts.method` CHECK constraint).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ReceiptMethod::Device => "device",
            ReceiptMethod::WebSession => "web_session",
        }
    }
}

/// A receipt slot's owning identity, as the receipts layer binds and probes it. Built ONLY via
/// [`WriteActor::receipt_actor`].
#[derive(Debug, Clone)]
pub(crate) struct ReceiptActor<'a> {
    /// The device key id (device lane) or the acting principal's email (session lane).
    pub(crate) actor: &'a str,
    /// Which leg is writing.
    pub(crate) method: ReceiptMethod,
    /// The session lane's full-request identity; `None` on the device lane (stored NULL).
    pub(crate) request_sha256: Option<[u8; 32]>,
}
