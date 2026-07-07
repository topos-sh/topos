-- The session-authenticated review lane — schema for browser-side review approve/reject on a hosted
-- composition. Appended (never edits an applied migration's bytes — sqlx checksums the file). Same posture
-- as 0001-0011: workspace-scoped rows, BIGINT counters, audit `created_at` TEXT ISO-8601.

-- OP RECEIPTS gain the lane discriminant. The receipt slot's owner column was named for the only writer that
-- existed (a signing device); the session lane records the acting principal's verified EMAIL in the same
-- slot, so the column is renamed to what it always was — the acting identity. The rename rewrites no tuples
-- and the PK constraint keeps its name (`op_receipts_pkey`), so the serializable runner's convergent-23505
-- set is untouched.
ALTER TABLE op_receipts RENAME COLUMN device_key_id TO actor;

-- Which LEG wrote the receipt: a device-signed op ('device_signed', actor = the signing device key id) or a
-- web-session review op ('web_session', actor = the acting principal's verified email). Existing rows are all
-- device-signed (the session lane did not exist before this migration); the kept DEFAULT is the backfill, and
-- new writes set the column explicitly on both legs.
ALTER TABLE op_receipts ADD COLUMN method TEXT NOT NULL DEFAULT 'device_signed'
    CHECK (method IN ('device_signed', 'web_session'));

-- The session lane's full-request identity (a domain-tagged sha256 over the whole request, reason included)
-- — what makes a divergent retry under a reused request id fail closed. Device-lane rows carry NULL (their
-- request identity is the signed device-op frame).
ALTER TABLE op_receipts ADD COLUMN request_sha256 BYTEA
    CHECK (request_sha256 IS NULL OR octet_length(request_sha256) = 32);

-- Reserved: a future step-up attestation over the approval (schema only; no code path writes it yet).
ALTER TABLE op_receipts ADD COLUMN step_up_attestation TEXT;

-- The lane-blind replay probe reads receipts BY (workspace, op id) — without an index that is a sequential
-- scan, and under SERIALIZABLE a seq scan takes a relation-level predicate lock that would make every write
-- conflict with every concurrent write in the workspace.
CREATE INDEX op_receipts_by_ws_op ON op_receipts (workspace_id, op_id);

-- PROPOSALS gain the resolution facts the review surfaces disclose: the mandatory reason a session reject
-- carries (device rejects write NULL — the CLI keeps its surface), and when the proposal was resolved.
-- Pre-existing resolved rows keep NULL for both (rendered as optional).
ALTER TABLE proposals ADD COLUMN resolved_reason TEXT;
ALTER TABLE proposals ADD COLUMN resolved_at TEXT;
