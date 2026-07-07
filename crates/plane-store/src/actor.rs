//! The write-actor vocabulary — WHICH LANE a pointer-move/reject request arrived on, and how the
//! receipts layer records it.
//!
//! One module so the three types stay together: [`WriteActor`] is what the orchestration hands the
//! transaction (the device lane's key + signature, or the session lane's verified principal + its
//! domain-tagged request identity), and [`WriteActor::receipt_actor`] is the ONE projection into the
//! receipts layer ([`ReceiptActor`]) — every terminal writer derives its `(actor, method,
//! request_sha256)` triple here, so the lane vocabulary cannot drift per writer. The kernel's frozen
//! device-op frame is untouched: none of these types crosses the wire or enters a signing preimage.

use crate::id::Principal;

/// The lane a contribute write arrived on. The transaction bodies branch on this ONLY at their
/// authorization step; every other step is actor-blind.
#[derive(Debug, Clone)]
pub(crate) enum WriteActor<'a> {
    /// The device-signed lane (the CLI): byte-identical to the pre-lane behavior.
    Device {
        device_key_id: &'a str,
        signature: &'a [u8; 64],
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
    /// The ONE projection into the receipts layer.
    pub(crate) fn receipt_actor(&self) -> ReceiptActor<'_> {
        match self {
            WriteActor::Device { device_key_id, .. } => ReceiptActor {
                actor: device_key_id,
                method: ReceiptMethod::DeviceSigned,
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
    /// A device-signed op; the receipt's `actor` is the signing device key id.
    DeviceSigned,
    /// A web-session op; the receipt's `actor` is the acting principal's verified email.
    WebSession,
}

impl ReceiptMethod {
    /// The stored string form (matches the `op_receipts.method` CHECK constraint).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ReceiptMethod::DeviceSigned => "device_signed",
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
