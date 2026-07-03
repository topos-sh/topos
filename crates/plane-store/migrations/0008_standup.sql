-- First-boot workspace standup — the schema for the two self-serve doors (an un-enrolled publish's
-- standup device flow, and a direct create-workspace call) plus the hardened one-time admin claim.
-- Appended (never edits an applied migration's bytes — sqlx checksums the file). Same posture as
-- 0001-0007: every row workspace-scoped where one exists, opaque credentials stored ONLY as sha256,
-- deadline columns BIGINT epoch-ms, audit `created_at` TEXT ISO-8601.

-- DEVICE-AUTH SESSIONS grow an `intent`: 'enroll' (the existing invite-anchored flow) or 'standup'
-- (a session born with NO workspace — the workspace is created when a signed-in human approves it).
-- `workspace_id` therefore becomes nullable, but ONLY a standup session that has not been approved may
-- lack one: the CHECK pins every enroll session — and every confirmed/issued standup session — to a
-- workspace, so the grant-issue path never sees a NULL workspace.
ALTER TABLE device_auth_sessions ALTER COLUMN workspace_id DROP NOT NULL;
ALTER TABLE device_auth_sessions ADD COLUMN intent TEXT NOT NULL DEFAULT 'enroll'
    CHECK (intent IN ('enroll', 'standup'));
ALTER TABLE device_auth_sessions ADD CONSTRAINT device_auth_ws_bound CHECK
    (workspace_id IS NOT NULL OR (intent = 'standup' AND status IN ('pending', 'denied', 'expired')));

-- ADMIN CLAIM rows gain the mint-time facts the redeem trusts (the request's display name is
-- disclosure-only): the workspace display name, an expiry (nullable — legacy/test rows never expire;
-- expiry applies only to the FIRST consumption, a consumed-replay probe answers before it), and the
-- owner email a cloud-mode mint binds (the seated owner principal; absent ⇒ device-rooted).
ALTER TABLE admin_claim ADD COLUMN display_name TEXT;
ALTER TABLE admin_claim ADD COLUMN expires_at BIGINT;
ALTER TABLE admin_claim ADD COLUMN owner_email TEXT;

-- GENESIS REQUESTS — the create-workspace idempotency ledger, keyed by sha256(request_id). A replay of
-- the SAME request by the SAME owner returns the workspace it already created; the same request id under
-- a DIFFERENT owner is denied (the slot belongs to the original owner). One row per created workspace.
CREATE TABLE genesis_requests (
    request_sha256  BYTEA NOT NULL CHECK (octet_length(request_sha256) = 32),
    owner_principal TEXT  NOT NULL,
    workspace_id    TEXT  NOT NULL,
    created_at      TEXT  NOT NULL,
    PRIMARY KEY (request_sha256)
);
