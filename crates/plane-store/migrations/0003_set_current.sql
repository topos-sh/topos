-- The pointer-move write's schema — durable all-outcome receipts, the workspace review-policy, and the
-- device registry the in-transaction authorization resolves against. Appended (never edits an applied
-- migration's bytes — sqlx checksums the file). Same posture as 0001/0002: STRICT + WITHOUT ROWID, every
-- row `workspace_id`-scoped, content ids the raw 32-byte sha256 BLOBs the kernel passes, width-checked.
-- The `(epoch, seq)` columns carry the JCS / I-JSON safe-integer ceiling (2^53 − 1) the pointer preimage
-- enforces, so a generation a follower could never verify can never be stored.

-- ALL-OUTCOME RECEIPTS — durable idempotency for the pointer-move, keyed (workspace_id, device_key_id,
-- op_id). EVERY terminal outcome (OK + every typed failure, in-txn AND pre-txn) writes exactly one row.
-- The BOUND IDENTITY (command, skill_id, commit_id, bundle_digest, expected_(epoch,seq)) is the value, not
-- the key: a retry whose op_id matches but whose bound identity DIFFERS is a DENIED key-reuse, not a new
-- op, so it must be comparable — and `expected_es` in the KEY would hand a replayed op_id + a different es
-- a fresh slot, defeating once-only execution. `device_key_id` is NOT a foreign key to device_registry —
-- receipts OUTLIVE device revocation, so a since-revoked device's committed OK still replays. `signed_record`
-- keeps the OK outcome's signed pointer so a retry re-serves the ORIGINAL signature byte-for-byte even after
-- `current` has advanced to a later version. `created_at` is STORED (recomputing it would make retries differ).
CREATE TABLE op_receipts (
    workspace_id   TEXT    NOT NULL,
    device_key_id  TEXT    NOT NULL,
    op_id          TEXT    NOT NULL,
    -- the bound identity (a same-op_id retry must carry the SAME of these, else it is a DENIED key-reuse)
    command        TEXT    NOT NULL,
    skill_id       TEXT    NOT NULL,
    commit_id      BLOB             CHECK (commit_id IS NULL OR length(commit_id) = 32),
    bundle_digest  BLOB             CHECK (bundle_digest IS NULL OR length(bundle_digest) = 32),
    expected_epoch INTEGER NOT NULL CHECK (expected_epoch >= 0 AND expected_epoch <= 9007199254740991),
    expected_seq   INTEGER NOT NULL CHECK (expected_seq   >= 0 AND expected_seq   <= 9007199254740991),
    -- the outcome + its replay payload
    outcome        TEXT    NOT NULL CHECK (outcome IN (
                       'OK', 'APPROVAL_REQUIRED', 'NEEDS_REVIEW', 'CONFLICT', 'DIVERGED', 'DENIED',
                       'UNAVAILABLE', 'AMBIGUOUS_NAME', 'KEY_REPIN_REQUIRED', 'RETRYABLE_FAILURE',
                       'PERMANENT_FAILURE')),
    current_epoch  INTEGER          CHECK (current_epoch IS NULL OR (current_epoch >= 0 AND current_epoch <= 9007199254740991)),
    current_seq    INTEGER          CHECK (current_seq   IS NULL OR (current_seq   >= 0 AND current_seq   <= 9007199254740991)),
    signed_record  BLOB,
    key_id         TEXT,
    created_at     TEXT    NOT NULL,
    details        TEXT,
    PRIMARY KEY (workspace_id, device_key_id, op_id)
) STRICT, WITHOUT ROWID;

-- WORKSPACE POLICY — the off-by-default review-required gate, an authoritative ROW the pointer-move txn
-- reads (a cheap preflight reads it before ingest; the in-txn read is the source of truth, since policy may
-- flip between the two). Fixture-seeded in v0 — there is no public set-policy verb yet, which is what keeps
-- the typed-fail gate honest (the APPROVAL_REQUIRED dead-end is unreachable by a real user until the
-- propose remedy ships alongside a policy-enable surface).
CREATE TABLE workspace_policy (
    workspace_id    TEXT    NOT NULL,
    review_required INTEGER NOT NULL DEFAULT 0 CHECK (review_required IN (0, 1)),
    PRIMARY KEY (workspace_id)
) STRICT, WITHOUT ROWID;

-- DEVICE REGISTRY — (workspace_id, device_key_id) -> (public_key, principal, revoked). The pointer-move's
-- in-transaction authorization resolves a device to its NON-REVOKED public key BOUND TO a principal, then
-- verifies the device-op signature and checks the principal is rostered — all inside the one BEGIN
-- IMMEDIATE write transaction, so a revoke committed BEFORE the promotion is serialized AHEAD of it and
-- blocks the move. Fixture-seeded in v0; real device issuance + revocation routes land later behind the
-- frozen enrollment port. `public_key` is the raw 32-byte Ed25519 verifying key.
CREATE TABLE device_registry (
    workspace_id  TEXT    NOT NULL,
    device_key_id TEXT    NOT NULL,
    public_key    BLOB    NOT NULL CHECK (length(public_key) = 32),
    principal     TEXT    NOT NULL,
    revoked       INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1)),
    PRIMARY KEY (workspace_id, device_key_id)
) STRICT, WITHOUT ROWID;

-- REVERT support (Option B). `revert --to <good>` builds a forward commit `{tree: good.tree, parents:
-- [current]}`, which needs good's `bundle_digest` to re-derive the new version_id — but the git commit
-- object does NOT persist it (it is only an INPUT to the version_id/render check, never stored). So the
-- pointer-move records the server-rehashed digest here, on the commit's provenance row, at promote/record
-- time; revert reads it O(1). Nullable: legacy rows written before the pointer-move carry NULL and cannot be
-- revert targets (a typed failure), which is correct — every version born through the pointer-move path
-- carries its digest. A length CHECK pins the width for the non-NULL case.
ALTER TABLE skill_commit ADD COLUMN bundle_digest BLOB CHECK (bundle_digest IS NULL OR length(bundle_digest) = 32);
