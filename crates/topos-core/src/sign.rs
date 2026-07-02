//! The Ed25519 signing PREIMAGES + verify — the one shared implementation.
//!
//! `topos-core` builds the exact preimage bytes for each identified/signed object and verifies
//! Ed25519 signatures over them. The concrete `sign` (which holds a secret key + needs an RNG-free
//! deterministic signing key) lives in the caller — the plane's in-process signer and the client
//! device signer — over the **same** `ed25519-dalek` crate. Because signer and verifier share these
//! preimage builders, the two halves of every signature agree on the bytes by construction (the
//! classic two-halves-of-a-signature footgun is closed by one shared encoder).
//!
//! Five frozen, domain-separated encodings:
//!
//! - the **commit** identity (a content hash, not a signature): `commit_id = sha256(frame)` — the
//!   user-facing `version_id`. A length-prefixed binary frame. ([`commit_id`])
//! - the **device-op** signature (publish / revert / review): a length-prefixed binary frame the
//!   device signs; verified by both the plane and other clients. ([`verify_device_op`])
//! - the signed **current pointer** (what a follower re-verifies on *every* pull — the trust root):
//!   RFC 8785 (JCS) canonical JSON, binding `alg` to foreclose algorithm-confusion. ([`verify_pointer`])
//! - the **device-enrollment possession proof** (verify-only): a length-prefixed binary frame an
//!   enrolling device signs to prove it controls the very key it registers. ([`verify_enroll`])
//! - the **governance-op** signature (verify-only): a length-prefixed binary frame an owner's
//!   registered device signs for an invite / roster mutation / device revoke. ([`verify_governance_op`])
//!
//! Domain separation: the four binary frames carry distinct ASCII context tags (no one frame's
//! signature can verify as another — a publish never as a revert, an enrollment never as a governance
//! op, neither as a commit); the pointer is a JSON object — a different leading byte (`{`) entirely —
//! and binds its algorithm. No signature under one preimage can verify under another.
//!
//! The **cross-component identity derivations these frames bind** also live here, once: the
//! pubkey-derived [`device_key_id`], the governance [`GovernanceRole`] signing byte, and the invite
//! no-expiry sentinel [`INVITE_NO_EXPIRY`]. Each is a signature-preimage input the client computes at
//! sign time and the plane independently re-derives at verify time — a second implementation of any of
//! them could silently fork the two halves, so neither half may re-implement one.
//!
//! ## Why a hand-specified binary frame, not a serialization crate
//!
//! A signing preimage is a cryptographic commitment: its bytes must be reproducible *forever* and
//! across independent implementations. General serialization formats (`bincode`, `borsh`, `postcard`)
//! are the wrong tool — their byte layout is a property of the library version, not a stability
//! contract, so an upgrade can silently change what verifies. The established practice (TLS
//! transcripts, SSH wire format, Noise) is an explicit, length-prefixed, domain-separated frame. The
//! libraries we *do* lean on are the primitives: `ed25519-dalek` (verify) and `sha2`. The pointer's
//! JCS bytes are cross-checked against the `json-canon` crate in tests (it is not `no_std`, so it
//! cannot be a runtime dependency of this kernel — it serves as the correctness oracle instead).

use crate::digest::{sha256, to_hex};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use ed25519_dalek::{Signature, VerifyingKey};

/// The ASCII context tag for the commit-id frame (15 chars + NUL = 16 bytes).
const COMMIT_TAG: &[u8] = b"TOPOS_COMMIT_V1\0";
/// The ASCII context tag for the device-op signature frame (22 chars + NUL = 23 bytes).
const DEVICE_OP_TAG: &[u8] = b"TOPOS_DEVICE_OP_SIG_V1\0";
/// The ASCII context tag for the device-enrollment possession-proof frame (22 chars + NUL = 23 bytes).
const DEVICE_ENROLL_TAG: &[u8] = b"TOPOS_DEVICE_ENROLL_V1\0";
/// The ASCII context tag for the governance-op signature frame (26 chars + NUL = 27 bytes).
const GOVERNANCE_OP_TAG: &[u8] = b"TOPOS_GOVERNANCE_OP_SIG_V1\0";

/// Why a preimage could not be built. Every case is unreachable for well-formed inputs (a commit has
/// ≤ 2 parents; ids/messages are far under 4 GiB; generations are small counters) — they exist so the
/// builders stay **total and panic-free** rather than silently emitting bytes a verifier won't match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreimageError {
    /// A commit may have at most two parents (0 = genesis, 1 = normal, 2 = author-merge).
    TooManyParents,
    /// A length-prefixed string field exceeded `u32::MAX` bytes and cannot be framed.
    FieldTooLong,
    /// A pointer `epoch`/`seq` exceeded the JCS / I-JSON safe-integer bound (2^53 − 1). Above it a
    /// plain-decimal encoding is not guaranteed to match a conforming (ECMAScript-number) verifier, so
    /// the trust-root preimage is refused rather than emitted ambiguously. (Unreachable for the
    /// monotonic counters `epoch`/`seq` actually are — they live far below this bound.)
    GenerationOutOfRange,
}

// ---------------------------------------------------------------------------------------------
// The shared Ed25519 verify primitive — the single verification path for both signature frames.
// ---------------------------------------------------------------------------------------------

/// Verify an Ed25519 signature over `message` with a raw 32-byte public key.
///
/// Uses `verify_strict` (rejects the small-order / non-canonical edge cases that make plain `verify`
/// signature-malleable). Returns `false` — never panics — on a malformed key, a bad signature, or
/// any verification failure, so a caller can treat the boolean as a hard integrity gate.
#[must_use]
pub fn verify_ed25519(message: &[u8], signature: &[u8; 64], public_key: &[u8; 32]) -> bool {
    let Ok(verifying_key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let signature = Signature::from_bytes(signature);
    verifying_key.verify_strict(message, &signature).is_ok()
}

// ---------------------------------------------------------------------------------------------
// Cross-component identity: the device key id every signed frame binds.
// ---------------------------------------------------------------------------------------------

/// The `dk_`-prefixed hex length of a [`device_key_id`] (the first 32 hex chars of the sha256).
const DEVICE_KEY_ID_HEX_LEN: usize = 32;

/// The device key id derived from a raw Ed25519 device public key: `dk_` + the first
/// 32 hex chars of `sha256(public_key)`.
///
/// A **cross-component identity**: the client binds this id into every frame it signs (device-op,
/// enroll, governance) and the plane RE-DERIVES it server-side from the registered public key — a
/// client-asserted id is never trusted — so it is a signature-preimage input, written once here and
/// called by both halves (a divergent derivation would make every signature fail). Stable across
/// restarts (derived from the persisted key, never random) and public (it does not reveal the key).
#[must_use]
pub fn device_key_id(public_key: &[u8; 32]) -> String {
    let hex = to_hex(&sha256(public_key));
    let mut id = String::with_capacity(3 + DEVICE_KEY_ID_HEX_LEN);
    id.push_str("dk_");
    id.push_str(&hex[..DEVICE_KEY_ID_HEX_LEN]);
    id
}

// ---------------------------------------------------------------------------------------------
// Commit — the content hash that yields `commit_id` (= `version_id`). Not a signature.
// ---------------------------------------------------------------------------------------------

/// The content a commit commits to (git's model, reused): ordered parents + the bundle digest as the
/// tree + the author device-id + the message. `parents[0]` is the trunk parent (the first-parent rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit<'a> {
    /// 0 (genesis), 1 (normal), or 2 (author-merge) parent commit ids; `parents[0]` is the trunk parent.
    pub parents: &'a [[u8; 32]],
    /// The bundle digest (the byte-exact consent hash) — git's "tree".
    pub tree: [u8; 32],
    /// The author device-id.
    pub author: &'a str,
    /// The commit message (title + body, already composed into one string).
    pub message: &'a str,
}

/// Build the canonical commit frame (the bytes hashed to form `commit_id`).
///
/// Layout: `TOPOS_COMMIT_V1\0` ‖ `u8`(parent count) ‖ each parent (32 B) ‖ `tree` (32 B) ‖
/// `u32be`(author len) ‖ author ‖ `u32be`(message len) ‖ message. Every multi-byte integer is
/// big-endian; every string length prefix is `u32be` (one width rule across the binary frames).
///
/// # Errors
/// [`PreimageError::TooManyParents`] if more than two parents are supplied;
/// [`PreimageError::FieldTooLong`] if a string field exceeds `u32::MAX` bytes.
pub fn commit_preimage(commit: &Commit) -> Result<Vec<u8>, PreimageError> {
    // Checked, like the length prefixes (no silent `as u8` truncation if the cap is ever raised).
    let parent_count = u8::try_from(commit.parents.len())
        .ok()
        .filter(|&n| n <= 2)
        .ok_or(PreimageError::TooManyParents)?;
    let mut out = Vec::new();
    out.extend_from_slice(COMMIT_TAG);
    out.push(parent_count);
    for parent in commit.parents {
        out.extend_from_slice(parent);
    }
    out.extend_from_slice(&commit.tree);
    put_lp_str(&mut out, commit.author)?;
    put_lp_str(&mut out, commit.message)?;
    Ok(out)
}

/// The commit id (= `version_id`): `sha256` over the canonical commit frame.
///
/// # Errors
/// As [`commit_preimage`].
pub fn commit_id(commit: &Commit) -> Result<[u8; 32], PreimageError> {
    Ok(sha256(&commit_preimage(commit)?))
}

// ---------------------------------------------------------------------------------------------
// Device-op signature — the device signs publish / revert / review over a binary frame.
// ---------------------------------------------------------------------------------------------

/// The closed set of device-signed operations. Modeled as **one** enum so an invalid
/// `(op_type, op_subtype)` pair is unrepresentable in the kernel (parse-don't-validate). The frame
/// emits `op_type` then `op_subtype` as two `u8`s via [`DeviceOp::op_type`] / [`DeviceOp::op_subtype`].
///
/// Binding the subtype is a real integrity property: a signed `ReviewApprove` must never be replayable
/// as a `ReviewReject`, nor a `PublishDirect` as a `PublishPropose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceOp {
    /// `publish` that moves `current` directly.
    PublishDirect,
    /// `publish --propose` that opens a proposal.
    PublishPropose,
    /// `revert --to <good>` (the single revert form).
    Revert,
    /// `review --approve` of a proposal.
    ReviewApprove,
    /// `review --reject` of a proposal.
    ReviewReject,
}

impl DeviceOp {
    /// The coarse verb byte: `publish` = 1, `revert` = 2, `review` = 3.
    #[must_use]
    pub fn op_type(self) -> u8 {
        match self {
            DeviceOp::PublishDirect | DeviceOp::PublishPropose => 1,
            DeviceOp::Revert => 2,
            DeviceOp::ReviewApprove | DeviceOp::ReviewReject => 3,
        }
    }

    /// The subtype byte, numbered **within** its `op_type`: publish `{direct=1, propose=2}`,
    /// revert `{1}`, review `{approve=1, reject=2}`.
    #[must_use]
    pub fn op_subtype(self) -> u8 {
        match self {
            DeviceOp::PublishDirect | DeviceOp::Revert | DeviceOp::ReviewApprove => 1,
            DeviceOp::PublishPropose | DeviceOp::ReviewReject => 2,
        }
    }
}

/// The fields a device signs for a publish / revert / review. The signed value is **identical** to
/// the compare-and-set / authorization / replay identity the plane's promotion transaction checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceOpFields<'a> {
    /// Scopes the signature to one workspace (no cross-workspace replay).
    pub workspace_id: &'a str,
    /// Scopes the signature to one skill.
    pub skill_id: &'a str,
    /// The operation (its `op_type` + `op_subtype` are bound into the frame).
    pub op: DeviceOp,
    /// The client-minted op id (a UUIDv4's raw 16 bytes), the idempotency key.
    pub op_id: [u8; 16],
    /// The id of the device signing key (the verifier selects the public key by this).
    pub device_key_id: &'a str,
    /// The `(epoch, seq)` the compare-and-set targets — `epoch`.
    pub expected_epoch: u64,
    /// The `(epoch, seq)` the compare-and-set targets — `seq`.
    pub expected_seq: u64,
    /// The candidate commit id (`version_id`) this op publishes / reverts / reviews.
    pub commit_id: [u8; 32],
    /// The byte-exact consent hash of that commit's bundle.
    pub bundle_digest: [u8; 32],
}

/// Build the canonical device-op signing frame.
///
/// Layout: `TOPOS_DEVICE_OP_SIG_V1\0` ‖ `u32be`(ws len) ‖ workspace_id ‖ `u32be`(skill len) ‖ skill_id
/// ‖ `u8` op_type ‖ `u8` op_subtype ‖ op_id (16 B) ‖ `u32be`(key len) ‖ device_key_id ‖
/// `u64be` expected_epoch ‖ `u64be` expected_seq ‖ commit_id (32 B) ‖ bundle_digest (32 B).
///
/// # Errors
/// [`PreimageError::FieldTooLong`] if a string field exceeds `u32::MAX` bytes.
pub fn device_op_preimage(fields: &DeviceOpFields) -> Result<Vec<u8>, PreimageError> {
    let mut out = Vec::new();
    out.extend_from_slice(DEVICE_OP_TAG);
    put_lp_str(&mut out, fields.workspace_id)?;
    put_lp_str(&mut out, fields.skill_id)?;
    out.push(fields.op.op_type());
    out.push(fields.op.op_subtype());
    out.extend_from_slice(&fields.op_id);
    put_lp_str(&mut out, fields.device_key_id)?;
    out.extend_from_slice(&fields.expected_epoch.to_be_bytes());
    out.extend_from_slice(&fields.expected_seq.to_be_bytes());
    out.extend_from_slice(&fields.commit_id);
    out.extend_from_slice(&fields.bundle_digest);
    Ok(out)
}

/// Verify a device-op signature with the device's raw 32-byte public key.
///
/// Returns `false` — never panics — on any malformed input or verification failure (including a field
/// too long to frame, which therefore could never have been signed).
#[must_use]
pub fn verify_device_op(
    fields: &DeviceOpFields,
    signature: &[u8; 64],
    device_public_key: &[u8; 32],
) -> bool {
    match device_op_preimage(fields) {
        Ok(message) => verify_ed25519(&message, signature, device_public_key),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Device-enrollment possession proof — the enrolling client signs this binary frame to prove it
// controls the very key it registers. The frame binds the registered public key itself, so the
// signature is a proof-of-possession: it can only verify under that one key, never another.
// ---------------------------------------------------------------------------------------------

/// The fields an enrolling device signs to prove possession of the key it registers. The signed value
/// binds the consumed single-use grant, the device-auth session, the key id + the **raw key being
/// registered**, and the offered skill set — so the proof is non-transferable to a different key and
/// scoped to one workspace / grant / session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnrollFields<'a> {
    /// Scopes the enrollment to one workspace.
    pub workspace_id: &'a str,
    /// `sha256` of the opaque single-use enrollment grant being consumed (binds this one grant).
    pub grant_hash: [u8; 32],
    /// The non-secret device-auth session handle (the RFC 8628 device-authorization id).
    pub device_auth_id: &'a str,
    /// The id the registered device key will be known by.
    pub device_key_id: &'a str,
    /// The raw Ed25519 key being registered — **also** the key this signature verifies under, which is
    /// what makes the frame a proof-of-possession (a signature under any other key cannot match it).
    pub device_public_key: [u8; 32],
    /// The skills the device offers to follow, bound as a **set** (order + duplicates canonicalized away).
    pub offered_skill_ids: &'a [&'a str],
}

/// Build the canonical device-enrollment signing frame.
///
/// Layout: `TOPOS_DEVICE_ENROLL_V1\0` ‖ `u32be`(ws len) ‖ workspace_id ‖ grant_hash (32 B) ‖
/// `u32be`(auth len) ‖ device_auth_id ‖ `u32be`(key-id len) ‖ device_key_id ‖ device_public_key (32 B)
/// ‖ `u32be`(skill count) ‖ each (`u32be`(skill len) ‖ skill_id). The `offered_skill_ids` are bound as
/// a **set** — the kernel sorts them byte-lexicographically and dedups them — so a client and the plane
/// can never disagree on order or duplicates; the count is the deduped count. An empty set is valid
/// (count 0). Every multi-byte integer is big-endian; every string length prefix is `u32be`.
///
/// # Errors
/// [`PreimageError::FieldTooLong`] if a string field — or the deduped skill count — exceeds `u32::MAX`.
pub fn enroll_preimage(fields: &EnrollFields) -> Result<Vec<u8>, PreimageError> {
    let mut out = Vec::new();
    out.extend_from_slice(DEVICE_ENROLL_TAG);
    put_lp_str(&mut out, fields.workspace_id)?;
    out.extend_from_slice(&fields.grant_hash);
    put_lp_str(&mut out, fields.device_auth_id)?;
    put_lp_str(&mut out, fields.device_key_id)?;
    out.extend_from_slice(&fields.device_public_key);
    put_lp_str_set(&mut out, fields.offered_skill_ids)?;
    Ok(out)
}

/// Verify a device-enrollment possession proof.
///
/// The verify key is the caller's authoritative key (the one actually being registered), passed
/// separately from `fields.device_public_key`: if that **field** is tampered to some other value the
/// frame bytes change and verification fails — a key-substitution is caught. Returns `false` — never
/// panics — on any malformed input or verification failure. Mirrors [`verify_device_op`].
#[must_use]
pub fn verify_enroll(
    fields: &EnrollFields,
    signature: &[u8; 64],
    device_public_key: &[u8; 32],
) -> bool {
    match enroll_preimage(fields) {
        Ok(message) => verify_ed25519(&message, signature, device_public_key),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Governance-op signature — an owner's registered device key signs an invite / roster mutation /
// device revoke. The frame binds `op_type` plus the op's full parameter set, so an invite signature
// can never replay as a revoke, and the plane can use `sha256(preimage)` as a request-replay identity.
// ---------------------------------------------------------------------------------------------

/// The `expires_at` value the Invite frame binds for "never expires" — v0 invites carry no expiry, so
/// BOTH halves (the client's invite signer and the plane's invite handler + token derivation) bind this
/// one sentinel. A disagreement here is a signature-preimage fork: every invite would verify DENIED.
pub const INVITE_NO_EXPIRY: u64 = 0;

/// The workspace governance role whose byte the invite / roster-set frames bind — `Owner` = 1,
/// `Reviewer` = 2, `Member` = 3, via [`GovernanceRole::signing_byte`]. ONE mapping: the client signer
/// and the plane's in-transaction verify both map their own role types onto this enum, so a
/// signature-preimage input can never fork between the halves. An **omitted** role means `Member` on
/// both halves too — that shared convention is this enum's `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GovernanceRole {
    /// Full governance authority (invite, roster, revoke). Byte 1.
    Owner,
    /// Review-gate authority (no governance authority in v0). Byte 2.
    Reviewer,
    /// An ordinary member (no governance authority) — the omitted-role default. Byte 3.
    #[default]
    Member,
}

impl GovernanceRole {
    /// The `u8` bound into the invite / roster-set signing frame (owner = 1, reviewer = 2, member = 3).
    #[must_use]
    pub fn signing_byte(self) -> u8 {
        match self {
            GovernanceRole::Owner => 1,
            GovernanceRole::Reviewer => 2,
            GovernanceRole::Member => 3,
        }
    }
}

/// The closed set of governance operations an owner signs. Each variant carries its full parameter
/// set, which the frame binds — so a signature for one operation can never be replayed as another (the
/// `op_type` byte + the per-variant tail are both part of the signed bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernanceOpKind<'a> {
    /// Invite principals at `role`, expiring at `expires_at`, optionally pre-offering `skills`.
    /// `emails` and `skills` are each bound as a **set** (sorted byte-lexicographically + deduped).
    Invite {
        /// The role the invitees are granted.
        role: u8,
        /// The invite's expiry deadline (an opaque integer bound into the frame).
        expires_at: u64,
        /// The invited email addresses, bound as a set.
        emails: &'a [&'a str],
        /// The skills pre-offered to the invitees, bound as a set.
        skills: &'a [&'a str],
    },
    /// Set `target`'s role (add or change a roster entry).
    RosterSet {
        /// The role to set.
        role: u8,
        /// The principal whose role is set.
        target: &'a str,
    },
    /// Remove `target` from the roster.
    RosterRemove {
        /// The principal removed.
        target: &'a str,
    },
    /// Revoke a registered device key.
    DeviceRevoke {
        /// The id of the device key being revoked.
        target_device_key_id: &'a str,
    },
}

impl GovernanceOpKind<'_> {
    /// The operation-type byte bound into the frame: `Invite` = 1, `RosterSet` = 2, `RosterRemove` = 3,
    /// `DeviceRevoke` = 4. Binding it is a real integrity property — a signed invite must never be
    /// replayable as a revoke (mirrors the device-op subtype binding).
    #[must_use]
    pub fn op_type(&self) -> u8 {
        match self {
            GovernanceOpKind::Invite { .. } => 1,
            GovernanceOpKind::RosterSet { .. } => 2,
            GovernanceOpKind::RosterRemove { .. } => 3,
            GovernanceOpKind::DeviceRevoke { .. } => 4,
        }
    }
}

/// The fields an owner's device signs for a governance operation. The signed value is the full
/// canonical parameter set, so the plane can use `sha256(preimage)` as a request-replay identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernanceOpFields<'a> {
    /// Scopes the operation to one workspace (no cross-workspace replay).
    pub workspace_id: &'a str,
    /// The client-minted op id (a UUIDv4's raw 16 bytes), the idempotency key.
    pub op_id: [u8; 16],
    /// The id of the **signing** owner's device key (the verifier selects the public key by this).
    pub device_key_id: &'a str,
    /// The operation (its `op_type` + its parameter tail are bound into the frame).
    pub op: GovernanceOpKind<'a>,
}

/// Build the canonical governance-op signing frame.
///
/// Layout: `TOPOS_GOVERNANCE_OP_SIG_V1\0` ‖ `u32be`(ws len) ‖ workspace_id ‖ `u8` op_type ‖ op_id
/// (16 B) ‖ `u32be`(key len) ‖ device_key_id ‖ the op-specific tail:
/// - `Invite`: `u8` role ‖ `u64be` expires_at ‖ `u32be`(email count) ‖ each email ‖ `u32be`(skill
///   count) ‖ each skill — emails and skills are each a sorted + deduped **set**;
/// - `RosterSet`: `u8` role ‖ `u32be`(target len) ‖ target;
/// - `RosterRemove`: `u32be`(target len) ‖ target;
/// - `DeviceRevoke`: `u32be`(key len) ‖ target_device_key_id.
///
/// Every multi-byte integer is big-endian; every string length prefix is `u32be`.
///
/// # Errors
/// [`PreimageError::FieldTooLong`] if a string field — or a deduped set count — exceeds `u32::MAX`.
pub fn governance_op_preimage(fields: &GovernanceOpFields) -> Result<Vec<u8>, PreimageError> {
    let mut out = Vec::new();
    out.extend_from_slice(GOVERNANCE_OP_TAG);
    put_lp_str(&mut out, fields.workspace_id)?;
    out.push(fields.op.op_type());
    out.extend_from_slice(&fields.op_id);
    put_lp_str(&mut out, fields.device_key_id)?;
    match &fields.op {
        GovernanceOpKind::Invite {
            role,
            expires_at,
            emails,
            skills,
        } => {
            out.push(*role);
            out.extend_from_slice(&expires_at.to_be_bytes());
            put_lp_str_set(&mut out, emails)?;
            put_lp_str_set(&mut out, skills)?;
        }
        GovernanceOpKind::RosterSet { role, target } => {
            out.push(*role);
            put_lp_str(&mut out, target)?;
        }
        GovernanceOpKind::RosterRemove { target } => {
            put_lp_str(&mut out, target)?;
        }
        GovernanceOpKind::DeviceRevoke {
            target_device_key_id,
        } => {
            put_lp_str(&mut out, target_device_key_id)?;
        }
    }
    Ok(out)
}

/// Verify a governance-op signature with the signing owner's registered raw 32-byte public key.
///
/// Returns `false` — never panics — on any malformed input or verification failure. Mirrors
/// [`verify_device_op`].
#[must_use]
pub fn verify_governance_op(
    fields: &GovernanceOpFields,
    signature: &[u8; 64],
    signer_public_key: &[u8; 32],
) -> bool {
    match governance_op_preimage(fields) {
        Ok(message) => verify_ed25519(&message, signature, signer_public_key),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Signed current pointer — RFC 8785 (JCS). The trust root every follower re-verifies on each pull.
// ---------------------------------------------------------------------------------------------

/// The fields the plane signs into a `current` pointer. The preimage also binds `alg` (the signature
/// algorithm) and the `workspace_id` + `skill_id` scope, so a valid pointer cannot be replayed into
/// another workspace, skill, or algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentPointer<'a> {
    /// Scopes the pointer to one workspace.
    pub workspace_id: &'a str,
    /// Scopes the pointer to one skill.
    pub skill_id: &'a str,
    /// The commit id (`version_id`) `current` points at — rendered as 64 lowercase hex in the JSON.
    pub version_id: [u8; 32],
    /// The anti-rollback generation — `epoch`.
    pub epoch: u64,
    /// The anti-rollback generation — `seq`.
    pub seq: u64,
}

/// The JCS / I-JSON safe-integer bound (2^53 − 1, ECMAScript `Number.MAX_SAFE_INTEGER`). Integers
/// within ±this round-trip identically across conforming JSON-number implementations; beyond it a plain
/// decimal and an ECMAScript-number serializer can diverge, so the trust-root preimage refuses them.
const MAX_SAFE_INT: u64 = (1u64 << 53) - 1;

/// Build the RFC 8785 (JCS) canonical JSON the plane signs and a follower re-verifies.
///
/// The object is `{alg, epoch, seq, skill_id, version_id, workspace_id}` with keys in JCS order
/// (sorted by code unit; for these ASCII keys that is byte order). `alg` is the literal `"Ed25519"`;
/// `version_id` is 64 lowercase hex; `epoch`/`seq` are monotonic counters rendered as plain decimal
/// (identical to ECMAScript `Number::toString` within the JCS/I-JSON safe-integer range, 2^53 − 1).
///
/// The single JCS subtlety we must honor for fixed string values is JSON string escaping; the kernel
/// implementation is cross-validated byte-for-byte against the `json-canon` crate in this crate's tests.
///
/// # Errors
/// [`PreimageError::GenerationOutOfRange`] if `epoch` or `seq` exceeds the JCS/I-JSON safe-integer
/// bound (so the trust root is never canonicalized into bytes a conforming verifier might not match).
pub fn pointer_preimage(pointer: &CurrentPointer) -> Result<String, PreimageError> {
    if pointer.epoch > MAX_SAFE_INT || pointer.seq > MAX_SAFE_INT {
        return Err(PreimageError::GenerationOutOfRange);
    }
    let version_hex = to_hex(&pointer.version_id);
    let members: &mut [(&str, JsonValue)] = &mut [
        ("alg", JsonValue::Str("Ed25519")),
        ("epoch", JsonValue::Uint(pointer.epoch)),
        ("seq", JsonValue::Uint(pointer.seq)),
        ("skill_id", JsonValue::Str(pointer.skill_id)),
        ("version_id", JsonValue::Str(&version_hex)),
        ("workspace_id", JsonValue::Str(pointer.workspace_id)),
    ];
    Ok(canonical_json_object(members))
}

/// Verify a signed `current` pointer with the plane's raw 32-byte public key.
///
/// Returns `false` — never panics — on any malformed input or verification failure (including an
/// out-of-range generation, which therefore could never have been signed).
#[must_use]
pub fn verify_pointer(
    pointer: &CurrentPointer,
    signature: &[u8; 64],
    plane_public_key: &[u8; 32],
) -> bool {
    match pointer_preimage(pointer) {
        Ok(message) => verify_ed25519(message.as_bytes(), signature, plane_public_key),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Internal encoders.
// ---------------------------------------------------------------------------------------------

/// Append a `u32be` length prefix + the raw UTF-8 bytes of `s`.
fn put_lp_str(out: &mut Vec<u8>, s: &str) -> Result<(), PreimageError> {
    let len = u32::try_from(s.len()).map_err(|_| PreimageError::FieldTooLong)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Append a length-prefixed **set** of strings: sort byte-lexicographically + dedup, then emit
/// `u32be`(count) followed by each [`put_lp_str`]. Canonicalizing the set inside the kernel means a
/// client and the plane can never disagree on order or duplicates for a set-valued field. (`str`'s
/// `Ord` compares raw bytes, so the sort IS byte-lexicographic.) The count is the **deduped** length;
/// an empty set is valid (count 0).
///
/// # Errors
/// [`PreimageError::FieldTooLong`] if the deduped count, or any item, exceeds `u32::MAX` bytes.
fn put_lp_str_set(out: &mut Vec<u8>, items: &[&str]) -> Result<(), PreimageError> {
    let mut sorted = items.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let count = u32::try_from(sorted.len()).map_err(|_| PreimageError::FieldTooLong)?;
    out.extend_from_slice(&count.to_be_bytes());
    for &item in &sorted {
        put_lp_str(out, item)?;
    }
    Ok(())
}

/// A JSON value in a flat canonical object: a string or an unsigned integer (the only kinds the
/// pointer preimage needs). Floats are deliberately absent — JCS number canonicalization for them is
/// the hard part we never touch.
enum JsonValue<'a> {
    Str(&'a str),
    Uint(u64),
}

/// Serialize a flat JSON object per RFC 8785 (JCS): sort members by key, no insignificant whitespace,
/// strings minimally escaped, integers as plain decimal. Keys are sorted by **UTF-16 code unit**, as
/// RFC 8785 requires — correct for any key, not only the ASCII keys topos uses today.
fn canonical_json_object(members: &mut [(&str, JsonValue)]) -> String {
    members.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let mut out = String::from("{");
    for (i, (key, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_string(&mut out, key);
        out.push(':');
        match value {
            JsonValue::Str(s) => json_string(&mut out, s),
            JsonValue::Uint(n) => out.push_str(&n.to_string()),
        }
    }
    out.push('}');
    out
}

/// Append a JSON string (with surrounding quotes), escaped per RFC 8785 / ECMAScript `JSON.stringify`:
/// escape `"` and `\`; the named control escapes `\b \t \n \f \r`; every other control char (U+0000–
/// U+001F) as `\u00xx` (lowercase); everything else — including all non-ASCII — verbatim as UTF-8.
fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0A}' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\u{0D}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                // The remaining C0 controls (0x00–0x07, 0x0B, 0x0E–0x1F) as \u00xx, lowercase.
                let byte = c as u32;
                out.push_str("\\u00");
                out.push(hex_lower((byte >> 4) as u8));
                out.push(hex_lower((byte & 0x0F) as u8));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The lowercase hex digit for a nibble (0..=15).
fn hex_lower(nibble: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    HEX[(nibble & 0x0F) as usize] as char
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    // ---- The frozen known-answer vectors (computed once from these encoders, then pinned). A change
    // ---- to any encoding breaks one of these loudly; update only if the change is INTENTIONAL. ----
    //
    // Vector keys (test seeds, NOT real keys): device key seed = bytes 00..1f; plane key seed = 0xAA×32.
    const DEVICE_PK: &str = "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
    const PLANE_PK: &str = "e734ea6c2b6257de72355e472aa05a4c487e6b463c029ed306df2f01b5636b58";
    const COMMIT_PREIMAGE: &str = "544f504f535f434f4d4d49545f563100011111111111111111111111111111111111111111111111111111111111111111222222222222222222222222222222222222222222222222222222222222222200000007645f616c69636500000029496d70726f76652050522074656d706c6174650a0a55736520696d7065726174697665206d6f6f642e";
    const COMMIT_ID: &str = "a10ee836cc1b8290caa8f55ce70c7ff2a281922adf9a94315cbf6c07edfa9225";
    const DEVICEOP_PREIMAGE: &str = "544f504f535f4445564943455f4f505f5349475f56310000000006775f61636d650000000c735f707264657363726962650101f47ac10b58cc4372a5670e02b2c3d4790000000f706b5f616c6963655f6c6170746f700000000000000001000000000000002aa10ee836cc1b8290caa8f55ce70c7ff2a281922adf9a94315cbf6c07edfa92252222222222222222222222222222222222222222222222222222222222222222";
    const DEVICEOP_SIG: &str = "ea4685bbd5f65186f1f307067151ac97016dfc6e618d8b6f73b9d04a89823bc050554c2291b4ee64a22fdeab05671140f949ac95cb3f07f129dd82658d14ea0b";
    const POINTER_PREIMAGE: &str = r#"{"alg":"Ed25519","epoch":1,"seq":42,"skill_id":"s_prdescribe","version_id":"a10ee836cc1b8290caa8f55ce70c7ff2a281922adf9a94315cbf6c07edfa9225","workspace_id":"w_acme"}"#;
    const POINTER_SIG: &str = "e05a3a08c5107ccc30b2e741aaecc75dce6d822f88874f0d63c3b5d95549d1b57399c3860baca7a560e03bfdc89225dd338fc4f059df5d91c509f30187595f06";
    // The device-enrollment possession proof: the registered key (DEVICE_PK) signs the frame. The
    // unsorted offered_skill_ids ["s_prdescribe","s_deploy"] canonicalize to ["s_deploy","s_prdescribe"].
    const ENROLL_PREIMAGE: &str = "544f504f535f4445564943455f454e524f4c4c5f56310000000006775f61636d6533333333333333333333333333333333333333333333333333333333333333330000000b64615f61636d655f3030310000000f706b5f616c6963655f6c6170746f7003a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b80000000200000008735f6465706c6f790000000c735f70726465736372696265";
    const ENROLL_SIG: &str = "df1ff32e9d33dd520241aa141c57c7c5ebeb4a9f0a4dc493c50b3289ea4e0f893c2605390e884cbc9d30b32f4db9358b8c27c0b6f5edde7db8e763414012f80a";
    // The governance-op (an Invite): the owner's device key (DEVICE_PK) signs. Emits a 27-byte tag; the
    // unsorted emails ["bob@…","alice@…"] canonicalize to ["alice@…","bob@…"].
    const GOVERNANCE_PREIMAGE: &str = "544f504f535f474f5645524e414e43455f4f505f5349475f56310000000006775f61636d6501f47ac10b58cc4372a5670e02b2c3d4790000000f706b5f616c6963655f6c6170746f70010000000067748580000000020000000f616c6963654061636d652e746573740000000d626f624061636d652e746573740000000100000008735f6465706c6f79";
    const GOVERNANCE_SIG: &str = "577886a1b76c3673461a2af8b9f71e9dc8854e4c57e69c6a8529fa9fc94009bce0c689bfadd860a25eec57704bb05a2b58410df39b2eb5bbaea05c07c5428008";

    const FIX_PARENTS: [[u8; 32]; 1] = [[0x11u8; 32]];
    const FIX_TREE: [u8; 32] = [0x22u8; 32];
    const FIX_OP_ID: [u8; 16] = [
        0xf4, 0x7a, 0xc1, 0x0b, 0x58, 0xcc, 0x43, 0x72, 0xa5, 0x67, 0x0e, 0x02, 0xb2, 0xc3, 0xd4,
        0x79,
    ];

    fn unhex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("valid hex vector")
    }
    fn arr32(s: &str) -> [u8; 32] {
        unhex(s).try_into().expect("32-byte vector")
    }
    fn arr64(s: &str) -> [u8; 64] {
        unhex(s).try_into().expect("64-byte vector")
    }

    fn fixture_commit() -> Commit<'static> {
        Commit {
            parents: &FIX_PARENTS,
            tree: FIX_TREE,
            author: "d_alice",
            message: "Improve PR template\n\nUse imperative mood.",
        }
    }

    fn fixture_device_op() -> DeviceOpFields<'static> {
        DeviceOpFields {
            workspace_id: "w_acme",
            skill_id: "s_prdescribe",
            op: DeviceOp::PublishDirect,
            op_id: FIX_OP_ID,
            device_key_id: "pk_alice_laptop",
            expected_epoch: 1,
            expected_seq: 42,
            commit_id: arr32(COMMIT_ID),
            bundle_digest: FIX_TREE,
        }
    }

    fn fixture_pointer() -> CurrentPointer<'static> {
        CurrentPointer {
            workspace_id: "w_acme",
            skill_id: "s_prdescribe",
            version_id: arr32(COMMIT_ID),
            epoch: 1,
            seq: 42,
        }
    }

    fn fixture_enroll() -> EnrollFields<'static> {
        EnrollFields {
            workspace_id: "w_acme",
            grant_hash: [0x33; 32],
            device_auth_id: "da_acme_001",
            device_key_id: "pk_alice_laptop",
            device_public_key: arr32(DEVICE_PK),
            // DELIBERATELY unsorted — the kernel sorts to ["s_deploy", "s_prdescribe"].
            offered_skill_ids: &["s_prdescribe", "s_deploy"],
        }
    }

    fn fixture_governance() -> GovernanceOpFields<'static> {
        GovernanceOpFields {
            workspace_id: "w_acme",
            op_id: FIX_OP_ID,
            device_key_id: "pk_alice_laptop",
            op: GovernanceOpKind::Invite {
                role: 1,
                expires_at: 1_735_689_600,
                // DELIBERATELY unsorted — the kernel sorts to ["alice@acme.test", "bob@acme.test"].
                emails: &["bob@acme.test", "alice@acme.test"],
                skills: &["s_deploy"],
            },
        }
    }

    // ---- Commit-id ----

    #[test]
    fn commit_id_known_answer() {
        let commit = fixture_commit();
        assert_eq!(
            crate::digest::to_hex(&commit_preimage(&commit).unwrap()),
            COMMIT_PREIMAGE,
            "commit frame changed — update only if the encoding INTENTIONALLY changed",
        );
        assert_eq!(
            crate::digest::to_hex(&commit_id(&commit).unwrap()),
            COMMIT_ID
        );
    }

    #[test]
    fn commit_parent_count_is_framed_and_capped() {
        // The parent count is the first byte after the 16-byte tag.
        let genesis = Commit {
            parents: &[],
            ..fixture_commit()
        };
        assert_eq!(commit_preimage(&genesis).unwrap()[16], 0);

        let two = [[0xAAu8; 32], [0xBBu8; 32]];
        let merge = Commit {
            parents: &two,
            ..fixture_commit()
        };
        assert_eq!(commit_preimage(&merge).unwrap()[16], 2);

        // A third parent is unrepresentable, not a panic.
        let three = [[0u8; 32], [1u8; 32], [2u8; 32]];
        let bad = Commit {
            parents: &three,
            ..fixture_commit()
        };
        assert_eq!(commit_preimage(&bad), Err(PreimageError::TooManyParents));
        assert_eq!(commit_id(&bad), Err(PreimageError::TooManyParents));
    }

    #[test]
    fn commit_id_binds_every_field() {
        let base = commit_id(&fixture_commit()).unwrap();
        let other_tree = commit_id(&Commit {
            tree: [0x33; 32],
            ..fixture_commit()
        })
        .unwrap();
        let other_author = commit_id(&Commit {
            author: "d_bob",
            ..fixture_commit()
        })
        .unwrap();
        let other_msg = commit_id(&Commit {
            message: "Different",
            ..fixture_commit()
        })
        .unwrap();
        let other_parent = commit_id(&Commit {
            parents: &[[0x99; 32]],
            ..fixture_commit()
        })
        .unwrap();
        assert_ne!(base, other_tree);
        assert_ne!(base, other_author);
        assert_ne!(base, other_msg);
        assert_ne!(base, other_parent);
    }

    // ---- Device-op signature ----

    #[test]
    fn device_op_known_answer_positive() {
        let fields = fixture_device_op();
        assert_eq!(
            crate::digest::to_hex(&device_op_preimage(&fields).unwrap()),
            DEVICEOP_PREIMAGE,
            "device-op frame changed — update only if the encoding INTENTIONALLY changed",
        );
        assert!(
            verify_device_op(&fields, &arr64(DEVICEOP_SIG), &arr32(DEVICE_PK)),
            "the golden device-op signature must verify",
        );
    }

    #[test]
    fn device_op_type_subtype_byte_mapping() {
        // The frozen u8 mapping for every operation (op_type, op_subtype), bytes 0-indexed.
        for (op, ty, sub) in [
            (DeviceOp::PublishDirect, 1u8, 1u8),
            (DeviceOp::PublishPropose, 1, 2),
            (DeviceOp::Revert, 2, 1),
            (DeviceOp::ReviewApprove, 3, 1),
            (DeviceOp::ReviewReject, 3, 2),
        ] {
            assert_eq!(op.op_type(), ty, "op_type for {op:?}");
            assert_eq!(op.op_subtype(), sub, "op_subtype for {op:?}");
        }
    }

    // The six named negative vectors — each tampered input must FAIL to verify the golden signature.
    #[test]
    fn device_op_negative_1_reordered_fields() {
        // A GENUINE field reordering: physically swap the two length-prefixed id chunks
        // (workspace_id, skill_id) in the canonical frame, each carrying its own length prefix. The
        // frame is order-sensitive, so the reordered bytes differ and the golden signature can't verify.
        let canon = unhex(DEVICEOP_PREIMAGE);
        let tag = b"TOPOS_DEVICE_OP_SIG_V1\0".len(); // 23
        let ws = &canon[tag..tag + 4 + 6]; // u32 len + "w_acme"
        let skill = &canon[tag + 10..tag + 10 + 4 + 12]; // u32 len + "s_prdescribe"
        let rest = &canon[tag + 10 + 16..];
        let mut reordered = Vec::new();
        reordered.extend_from_slice(&canon[..tag]);
        reordered.extend_from_slice(skill); // skill before workspace — the reordering
        reordered.extend_from_slice(ws);
        reordered.extend_from_slice(rest);
        assert_ne!(reordered, canon, "the swap must actually change the bytes");
        assert!(!verify_ed25519(
            &reordered,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn device_op_negative_2_wrong_tag() {
        // Flip a byte inside the domain tag: a different context can never verify the same signature.
        let mut bytes = unhex(DEVICEOP_PREIMAGE);
        bytes[0] ^= 0xff;
        assert!(!verify_ed25519(
            &bytes,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn device_op_negative_3_wrong_op_type() {
        // publish -> revert changes the op_type byte.
        let revert = DeviceOpFields {
            op: DeviceOp::Revert,
            ..fixture_device_op()
        };
        assert!(!verify_device_op(
            &revert,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
        // ...and publish-direct -> publish-propose changes only the op_subtype byte (also rejected).
        let propose = DeviceOpFields {
            op: DeviceOp::PublishPropose,
            ..fixture_device_op()
        };
        assert!(!verify_device_op(
            &propose,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn device_op_negative_4_wrong_expected_generation() {
        let bumped_seq = DeviceOpFields {
            expected_seq: 43,
            ..fixture_device_op()
        };
        let bumped_epoch = DeviceOpFields {
            expected_epoch: 2,
            ..fixture_device_op()
        };
        assert!(!verify_device_op(
            &bumped_seq,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
        assert!(!verify_device_op(
            &bumped_epoch,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn device_op_negative_5_wrong_key() {
        // The same bytes, verified against a different (the plane's) public key.
        assert!(!verify_device_op(
            &fixture_device_op(),
            &arr64(DEVICEOP_SIG),
            &arr32(PLANE_PK)
        ));
    }

    #[test]
    fn device_op_negative_6_wrong_digest() {
        let tampered = DeviceOpFields {
            bundle_digest: [0x44; 32],
            ..fixture_device_op()
        };
        assert!(!verify_device_op(
            &tampered,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
        // And a tampered commit_id is likewise rejected.
        let other_commit = DeviceOpFields {
            commit_id: [0x55; 32],
            ..fixture_device_op()
        };
        assert!(!verify_device_op(
            &other_commit,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn device_op_negative_7_device_key_id_and_op_id_are_bound() {
        // device_key_id and op_id are signed fields too. Tampering either — with the SAME public key
        // (so this is distinct from the wrong-key case) — must break verification.
        let other_key_id = DeviceOpFields {
            device_key_id: "pk_evil",
            ..fixture_device_op()
        };
        assert!(!verify_device_op(
            &other_key_id,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
        let other_op_id = DeviceOpFields {
            op_id: [0xAB; 16],
            ..fixture_device_op()
        };
        assert!(!verify_device_op(
            &other_op_id,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    // ---- Device-enrollment possession proof ----

    #[test]
    fn enroll_known_answer_positive() {
        let fields = fixture_enroll();
        assert_eq!(
            crate::digest::to_hex(&enroll_preimage(&fields).unwrap()),
            ENROLL_PREIMAGE,
            "enroll frame changed — update only if the encoding INTENTIONALLY changed",
        );
        assert!(
            verify_enroll(&fields, &arr64(ENROLL_SIG), &arr32(DEVICE_PK)),
            "the golden enrollment possession proof must verify",
        );
    }

    #[test]
    fn enroll_negative_1_wrong_tag() {
        // Flip a byte inside the domain tag: a different context can never verify the same signature.
        let mut bytes = unhex(ENROLL_PREIMAGE);
        bytes[0] ^= 0xff;
        assert!(!verify_ed25519(
            &bytes,
            &arr64(ENROLL_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn enroll_negative_2_binds_every_scalar_field() {
        // Each non-key field is part of the signed frame; tampering any one breaks the golden signature
        // — verified with the ORIGINAL device key, so this is distinct from the wrong-key case below.
        let sig = arr64(ENROLL_SIG);
        let pk = arr32(DEVICE_PK);
        assert!(!verify_enroll(
            &EnrollFields {
                workspace_id: "w_other",
                ..fixture_enroll()
            },
            &sig,
            &pk
        ));
        assert!(!verify_enroll(
            &EnrollFields {
                grant_hash: [0x44; 32],
                ..fixture_enroll()
            },
            &sig,
            &pk
        ));
        assert!(!verify_enroll(
            &EnrollFields {
                device_auth_id: "da_evil",
                ..fixture_enroll()
            },
            &sig,
            &pk
        ));
        assert!(!verify_enroll(
            &EnrollFields {
                device_key_id: "pk_evil",
                ..fixture_enroll()
            },
            &sig,
            &pk
        ));
    }

    #[test]
    fn enroll_negative_3_key_substitution() {
        // The registered key is bound INTO the frame. Tamper the device_public_key FIELD but verify
        // against the original key arg: the frame bytes change, so the proof fails — an attacker cannot
        // swap their own key into a victim's signed enrollment.
        let substituted = EnrollFields {
            device_public_key: [0x77; 32],
            ..fixture_enroll()
        };
        assert!(!verify_enroll(
            &substituted,
            &arr64(ENROLL_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn enroll_negative_4_wrong_verify_key() {
        // Untampered fields, but verified against a different (the plane's) public key.
        assert!(!verify_enroll(
            &fixture_enroll(),
            &arr64(ENROLL_SIG),
            &arr32(PLANE_PK)
        ));
    }

    #[test]
    fn enroll_negative_5_offered_skill_set_add_and_remove() {
        let sig = arr64(ENROLL_SIG);
        let pk = arr32(DEVICE_PK);
        // Adding a skill to the set changes the frame.
        assert!(!verify_enroll(
            &EnrollFields {
                offered_skill_ids: &["s_prdescribe", "s_deploy", "s_extra"],
                ..fixture_enroll()
            },
            &sig,
            &pk
        ));
        // Removing one likewise.
        assert!(!verify_enroll(
            &EnrollFields {
                offered_skill_ids: &["s_deploy"],
                ..fixture_enroll()
            },
            &sig,
            &pk
        ));
    }

    #[test]
    fn enroll_offered_skills_are_a_canonical_set() {
        // POSITIVE: a reordered AND duplicated input canonicalizes to the SAME bytes as the fixture
        // (itself unsorted), so the SAME golden signature verifies — the in-kernel sort+dedup makes the
        // offered skills an order-/duplicate-independent set.
        let canonical = enroll_preimage(&fixture_enroll()).unwrap();
        let reordered = EnrollFields {
            offered_skill_ids: &["s_deploy", "s_prdescribe", "s_deploy"],
            ..fixture_enroll()
        };
        assert_eq!(
            enroll_preimage(&reordered).unwrap(),
            canonical,
            "reorder + dup must yield a byte-identical frame",
        );
        assert!(verify_enroll(
            &reordered,
            &arr64(ENROLL_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn enroll_empty_offered_skill_set_is_valid() {
        // An empty set is valid (count 0), not an error — the tail is just the four zero count bytes.
        let empty = EnrollFields {
            offered_skill_ids: &[],
            ..fixture_enroll()
        };
        assert!(enroll_preimage(&empty).unwrap().ends_with(&[0, 0, 0, 0]));
    }

    #[test]
    fn enroll_domain_separation() {
        // The enroll frame carries its own tag, and its signature does not cross into the device-op
        // frame (nor a device-op signature into enroll).
        assert!(
            enroll_preimage(&fixture_enroll())
                .unwrap()
                .starts_with(DEVICE_ENROLL_TAG)
        );
        // The golden enrollment proof must NOT verify as a device-op...
        assert!(!verify_device_op(
            &fixture_device_op(),
            &arr64(ENROLL_SIG),
            &arr32(DEVICE_PK)
        ));
        // ...and the golden device-op signature must NOT verify as an enrollment.
        assert!(!verify_enroll(
            &fixture_enroll(),
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    // ---- Governance-op signature ----

    #[test]
    fn governance_known_answer_positive() {
        let fields = fixture_governance();
        assert_eq!(
            crate::digest::to_hex(&governance_op_preimage(&fields).unwrap()),
            GOVERNANCE_PREIMAGE,
            "governance frame changed — update only if the encoding INTENTIONALLY changed",
        );
        assert!(
            verify_governance_op(&fields, &arr64(GOVERNANCE_SIG), &arr32(DEVICE_PK)),
            "the golden governance-op signature must verify",
        );
    }

    #[test]
    fn governance_op_type_byte_mapping() {
        // The frozen op_type byte for every governance operation.
        assert_eq!(
            GovernanceOpKind::Invite {
                role: 0,
                expires_at: 0,
                emails: &[],
                skills: &[],
            }
            .op_type(),
            1
        );
        assert_eq!(
            GovernanceOpKind::RosterSet {
                role: 0,
                target: "x",
            }
            .op_type(),
            2
        );
        assert_eq!(GovernanceOpKind::RosterRemove { target: "x" }.op_type(), 3);
        assert_eq!(
            GovernanceOpKind::DeviceRevoke {
                target_device_key_id: "x",
            }
            .op_type(),
            4
        );
    }

    #[test]
    fn governance_negative_1_wrong_tag() {
        let mut bytes = unhex(GOVERNANCE_PREIMAGE);
        bytes[0] ^= 0xff;
        assert!(!verify_ed25519(
            &bytes,
            &arr64(GOVERNANCE_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn governance_negative_2_wrong_op_type() {
        // Re-typing the op (Invite -> DeviceRevoke) flips the op_type byte (and the tail) — the golden
        // invite signature can never verify as a revoke. This is the replay-across-ops guard.
        let revoke = GovernanceOpFields {
            op: GovernanceOpKind::DeviceRevoke {
                target_device_key_id: "pk_alice_laptop",
            },
            ..fixture_governance()
        };
        assert!(!verify_governance_op(
            &revoke,
            &arr64(GOVERNANCE_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn governance_negative_3_wrong_device_key_id() {
        let other = GovernanceOpFields {
            device_key_id: "pk_evil",
            ..fixture_governance()
        };
        assert!(!verify_governance_op(
            &other,
            &arr64(GOVERNANCE_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn governance_negative_4_binds_the_invite_params() {
        // The full canonical Invite param set is bound: role, expiry, op_id, and the email set each
        // break the golden signature when tampered (each verified with the correct key).
        fn invite(
            role: u8,
            expires_at: u64,
            emails: &'static [&'static str],
        ) -> GovernanceOpFields<'static> {
            GovernanceOpFields {
                op: GovernanceOpKind::Invite {
                    role,
                    expires_at,
                    emails,
                    skills: &["s_deploy"],
                },
                ..fixture_governance()
            }
        }
        let sig = arr64(GOVERNANCE_SIG);
        let pk = arr32(DEVICE_PK);
        let base_emails: &'static [&'static str] = &["bob@acme.test", "alice@acme.test"];
        assert!(!verify_governance_op(
            &invite(2, 1_735_689_600, base_emails),
            &sig,
            &pk
        ));
        assert!(!verify_governance_op(
            &invite(1, 1_735_689_601, base_emails),
            &sig,
            &pk
        ));
        assert!(!verify_governance_op(
            &invite(1, 1_735_689_600, &["carol@acme.test", "alice@acme.test"]),
            &sig,
            &pk
        ));
        assert!(!verify_governance_op(
            &GovernanceOpFields {
                op_id: [0xAB; 16],
                ..fixture_governance()
            },
            &sig,
            &pk
        ));
    }

    #[test]
    fn governance_each_kind_binds_its_target() {
        // For the non-Invite kinds (which the golden does not cover), prove the per-kind tail params are
        // bound by showing the FRAME bytes change when a target/role changes — a structural check that
        // needs no separate signature. The differing op_type byte also separates two same-tail kinds.
        fn frame(op: GovernanceOpKind<'static>) -> Vec<u8> {
            governance_op_preimage(&GovernanceOpFields {
                op,
                ..fixture_governance()
            })
            .unwrap()
        }
        assert_ne!(
            frame(GovernanceOpKind::RosterSet {
                role: 1,
                target: "u_bob"
            }),
            frame(GovernanceOpKind::RosterSet {
                role: 1,
                target: "u_carol"
            }),
        );
        assert_ne!(
            frame(GovernanceOpKind::RosterSet {
                role: 1,
                target: "u_bob"
            }),
            frame(GovernanceOpKind::RosterSet {
                role: 2,
                target: "u_bob"
            }),
        );
        assert_ne!(
            frame(GovernanceOpKind::RosterRemove { target: "u_bob" }),
            frame(GovernanceOpKind::RosterRemove { target: "u_carol" }),
        );
        assert_ne!(
            frame(GovernanceOpKind::DeviceRevoke {
                target_device_key_id: "pk_a"
            }),
            frame(GovernanceOpKind::DeviceRevoke {
                target_device_key_id: "pk_b"
            }),
        );
        // Same tail string, different kind ⇒ different op_type byte ⇒ different frame.
        assert_ne!(
            frame(GovernanceOpKind::RosterRemove { target: "u_x" }),
            frame(GovernanceOpKind::DeviceRevoke {
                target_device_key_id: "u_x"
            }),
        );
    }

    #[test]
    fn governance_negative_5_wrong_key() {
        assert!(!verify_governance_op(
            &fixture_governance(),
            &arr64(GOVERNANCE_SIG),
            &arr32(PLANE_PK)
        ));
    }

    #[test]
    fn governance_invite_sets_are_canonical() {
        // POSITIVE: reordered + duplicated emails (and skills) canonicalize to the SAME bytes, so the
        // SAME golden signature verifies — the Invite's emails/skills are order-/dup-independent sets.
        let canonical = governance_op_preimage(&fixture_governance()).unwrap();
        let reordered = GovernanceOpFields {
            op: GovernanceOpKind::Invite {
                role: 1,
                expires_at: 1_735_689_600,
                emails: &["alice@acme.test", "bob@acme.test", "bob@acme.test"],
                skills: &["s_deploy", "s_deploy"],
            },
            ..fixture_governance()
        };
        assert_eq!(
            governance_op_preimage(&reordered).unwrap(),
            canonical,
            "reorder + dup of the invite sets must yield a byte-identical frame",
        );
        assert!(verify_governance_op(
            &reordered,
            &arr64(GOVERNANCE_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn governance_domain_separation() {
        // The governance frame carries its own tag, and its signature does not cross into the enroll or
        // device-op frames — nor either of theirs into governance.
        assert!(
            governance_op_preimage(&fixture_governance())
                .unwrap()
                .starts_with(GOVERNANCE_OP_TAG)
        );
        let gov_sig = arr64(GOVERNANCE_SIG);
        let pk = arr32(DEVICE_PK);
        assert!(!verify_enroll(&fixture_enroll(), &gov_sig, &pk));
        assert!(!verify_device_op(&fixture_device_op(), &gov_sig, &pk));
        // ...and neither the enroll nor the device-op golden verifies as a governance op.
        assert!(!verify_governance_op(
            &fixture_governance(),
            &arr64(ENROLL_SIG),
            &pk
        ));
        assert!(!verify_governance_op(
            &fixture_governance(),
            &arr64(DEVICEOP_SIG),
            &pk
        ));
    }

    // ---- Cross-component identity derivations (the shared impls both halves call) ----

    #[test]
    fn device_key_id_known_answer() {
        // The frozen device key (seed 00..1f → DEVICE_PK) derives this exact id — the SAME value the
        // client signer binds into its frames and the plane re-derives from the registered key.
        assert_eq!(
            device_key_id(&arr32(DEVICE_PK)),
            "dk_56475aa75463474c0285df5dbf2bcab7"
        );
        // Shape: the `dk_` prefix + exactly the first 32 hex chars of sha256(pubkey).
        let full = to_hex(&sha256(&arr32(DEVICE_PK)));
        assert_eq!(
            device_key_id(&arr32(DEVICE_PK)),
            alloc::format!("dk_{}", &full[..32])
        );
    }

    #[test]
    fn governance_role_bytes_and_default_are_frozen() {
        // The exact bytes the invite / roster-set frames bind (Owner=1, Reviewer=2, Member=3) and the
        // shared omitted-role default (Member). A change here re-keys every governance signature.
        assert_eq!(GovernanceRole::Owner.signing_byte(), 1);
        assert_eq!(GovernanceRole::Reviewer.signing_byte(), 2);
        assert_eq!(GovernanceRole::Member.signing_byte(), 3);
        assert_eq!(GovernanceRole::default(), GovernanceRole::Member);
        // The invite no-expiry sentinel both halves bind for "never expires".
        assert_eq!(INVITE_NO_EXPIRY, 0);
    }

    // ---- Signed current pointer (JCS) ----

    #[test]
    fn pointer_known_answer_positive() {
        let pointer = fixture_pointer();
        assert_eq!(
            pointer_preimage(&pointer).unwrap(),
            POINTER_PREIMAGE,
            "pointer JCS changed — update only if the encoding INTENTIONALLY changed",
        );
        assert!(
            verify_pointer(&pointer, &arr64(POINTER_SIG), &arr32(PLANE_PK)),
            "the golden pointer signature must verify",
        );
    }

    #[test]
    fn pointer_generation_is_bounded_to_the_jcs_safe_integer() {
        const MAX: u64 = (1u64 << 53) - 1; // 9007199254740991
        // At the safe-integer bound: built, and byte-identical to json-canon (which also caps here).
        let at_bound = CurrentPointer {
            workspace_id: "w",
            skill_id: "s",
            version_id: [0u8; 32],
            epoch: MAX,
            seq: MAX,
        };
        let kernel = pointer_preimage(&at_bound).unwrap();
        let oracle = json_canon::to_string(&serde_json::json!({
            "alg": "Ed25519",
            "epoch": MAX,
            "seq": MAX,
            "skill_id": "s",
            "version_id": hex::encode([0u8; 32]),
            "workspace_id": "w",
        }))
        .expect("json-canon serializes at the safe-integer bound");
        assert_eq!(kernel, oracle);

        // Above the bound: refused (never an ambiguous encoding), and verify fails closed.
        let over = CurrentPointer {
            epoch: MAX + 1,
            ..at_bound
        };
        assert_eq!(
            pointer_preimage(&over),
            Err(PreimageError::GenerationOutOfRange)
        );
        assert!(!verify_pointer(&over, &[0u8; 64], &arr32(PLANE_PK)));
    }

    #[test]
    fn pointer_negative_vectors() {
        let sig = arr64(POINTER_SIG);
        let plane = arr32(PLANE_PK);
        // wrong scope
        assert!(!verify_pointer(
            &CurrentPointer {
                workspace_id: "w_other",
                ..fixture_pointer()
            },
            &sig,
            &plane
        ));
        assert!(!verify_pointer(
            &CurrentPointer {
                skill_id: "s_other",
                ..fixture_pointer()
            },
            &sig,
            &plane
        ));
        // wrong version_id
        assert!(!verify_pointer(
            &CurrentPointer {
                version_id: [0x66; 32],
                ..fixture_pointer()
            },
            &sig,
            &plane
        ));
        // generation rolled forward/back
        assert!(!verify_pointer(
            &CurrentPointer {
                seq: 43,
                ..fixture_pointer()
            },
            &sig,
            &plane
        ));
        assert!(!verify_pointer(
            &CurrentPointer {
                epoch: 2,
                ..fixture_pointer()
            },
            &sig,
            &plane
        ));
        // wrong key (the device key, not the plane key)
        assert!(!verify_pointer(&fixture_pointer(), &sig, &arr32(DEVICE_PK)));
    }

    /// Cross-validate the kernel's fixed-shape JCS against the `json-canon` crate (a maintained
    /// RFC 8785 implementation) — for the canonical pointer AND for adversarial string values that
    /// exercise JSON escaping: quotes, backslashes, the named control escapes, other C0 controls,
    /// non-ASCII, emoji, and the un-escaped forward slash.
    #[test]
    fn pointer_jcs_matches_json_canon_oracle() {
        let cases = [
            ("w_acme", "s_prdescribe"),
            ("w_\"quote\"", "s_back\\slash"),
            (
                "w_\u{0001}\u{0007}\u{001f}",
                "s_\u{0008}\u{0009}\u{000a}\u{000c}\u{000d}",
            ),
            ("w_unïcodé_Ω", "s_emoji_🚀"),
            ("w_tab\tnl\n", "s_slash/not/escaped"),
        ];
        for (ws, sk) in cases {
            let version_id = arr32(COMMIT_ID);
            let pointer = CurrentPointer {
                workspace_id: ws,
                skill_id: sk,
                version_id,
                epoch: 7,
                seq: 9,
            };
            let kernel = pointer_preimage(&pointer).unwrap();
            let oracle = json_canon::to_string(&serde_json::json!({
                "alg": "Ed25519",
                "epoch": 7,
                "seq": 9,
                "skill_id": sk,
                "version_id": hex::encode(version_id),
                "workspace_id": ws,
            }))
            .expect("json-canon serializes the oracle object");
            assert_eq!(
                kernel, oracle,
                "kernel JCS diverged from json-canon: ws={ws:?} sk={sk:?}"
            );
        }
    }

    // ---- Domain separation + the verify primitive ----

    #[test]
    fn domain_separation_across_the_three_preimages() {
        // Distinct ASCII context tags / leading bytes — no frame can be mistaken for another.
        let commit = commit_preimage(&fixture_commit()).unwrap();
        let device_op = device_op_preimage(&fixture_device_op()).unwrap();
        let pointer = pointer_preimage(&fixture_pointer()).unwrap();
        assert!(commit.starts_with(b"TOPOS_COMMIT_V1\0"));
        assert!(device_op.starts_with(b"TOPOS_DEVICE_OP_SIG_V1\0"));
        assert!(pointer.starts_with('{'));
        // A device-op signature never verifies as a pointer, nor vice versa (different messages/keys).
        assert!(!verify_pointer(
            &fixture_pointer(),
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
        assert!(!verify_device_op(
            &fixture_device_op(),
            &arr64(POINTER_SIG),
            &arr32(PLANE_PK)
        ));
    }

    #[test]
    fn verify_ed25519_rejects_malformed_inputs() {
        let msg = b"hello";
        // An all-zero "key"/"signature" must not verify (and must not panic).
        assert!(!verify_ed25519(msg, &[0u8; 64], &[0u8; 32]));
        // A valid key with a zero signature does not verify.
        assert!(!verify_ed25519(msg, &[0u8; 64], &arr32(DEVICE_PK)));
        // The golden device-op signature does not verify over unrelated bytes.
        assert!(!verify_ed25519(
            msg,
            &arr64(DEVICEOP_SIG),
            &arr32(DEVICE_PK)
        ));
    }

    #[test]
    fn lp_str_writes_a_u32be_length_prefix() {
        // The frozen width: a 4-byte big-endian length, then the raw UTF-8 bytes. (A field longer
        // than u32::MAX — unreachable in practice — would be a typed error, never a truncation/panic.)
        let mut buf = vec![];
        put_lp_str(&mut buf, "abc").unwrap();
        assert_eq!(buf, vec![0x00, 0x00, 0x00, 0x03, b'a', b'b', b'c']);

        let mut empty = vec![];
        put_lp_str(&mut empty, "").unwrap();
        assert_eq!(empty, vec![0x00, 0x00, 0x00, 0x00]);
    }
}
