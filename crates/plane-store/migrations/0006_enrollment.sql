-- The enrollment + governance issuance schema — the workspace RBAC roster, the opaque invite / grant /
-- device-auth / passcode credentials, and the governance audit + op_id-idempotency store. Appended (never
-- edits an applied migration's bytes — sqlx checksums the file). Same posture as 0001-0005: every row
-- `workspace_id`-scoped, content ids the raw 32-byte sha256 BYTEA the kernel passes, width-checked. Every
-- opaque credential is stored ONLY as its sha256 (the plaintext is HMAC-derived from a 0600 enrollment secret
-- and never persisted), so a database read can never recover a live credential and a revoke is an instant
-- server-side row flip. Time columns are epoch MILLISECONDS (the one server-clock unit, `wire::now_utc()`),
-- matching the read-token expiry this migration adds.

-- WORKSPACE — the billable/addressable object an enrollment stands up. STANDALONE: nothing references it by
-- foreign key (the existing skill_commit/current/roster carry a bare `workspace_id` and seed no workspace
-- row, so the publish/read tests stay green). `deployment_mode` decides the redeem gate (cloud requires
-- a confirmed identity already on the roster; self-host grants membership from the bearer). `verified_domain`
-- is the org-domain claim; its `*_status` is the verification state machine (no domain proof yet — `verified`
-- is operator-asserted in v0).
CREATE TABLE workspace (
    workspace_id           TEXT NOT NULL,
    display_name           TEXT NOT NULL,
    verified_domain        TEXT,
    verified_domain_status TEXT NOT NULL CHECK (verified_domain_status IN ('unverified', 'pending', 'verified')),
    deployment_mode        TEXT NOT NULL CHECK (deployment_mode IN ('cloud', 'self_host')),
    created_at             TEXT NOT NULL,
    PRIMARY KEY (workspace_id)
);

-- ADMIN CLAIM — the self-host first-boot standup token. A getrandom secret printed to the server log on
-- first boot (stored ONLY as its sha256, like every credential), naming the workspace it stands up.
-- `admin_claim` consumes it once (creating the workspace + the first owner + registering the claiming
-- device). A consumed (or absent) token is the uniform denial.
CREATE TABLE admin_claim (
    token_sha256 BYTEA  NOT NULL CHECK (octet_length(token_sha256) = 32),
    workspace_id TEXT   NOT NULL,
    consumed_at  BIGINT,
    created_at   TEXT   NOT NULL,
    PRIMARY KEY (token_sha256)
);

-- WORKSPACE MEMBER — the workspace-level RBAC roster (who has which governance role), DISTINCT from the
-- per-skill `roster` (the read entitlement in 0001). `role` is the governance authority (owner signs
-- invites / roster mutations; revoke is owner-or-self); `status` is the enrollment lifecycle (an invite
-- UPSERTs an `invited` row; redeem flips it `confirmed`). `invited_by` is the audit "who" (NULL for a
-- self-host bearer-granted member). Membership in a SKILL's read roster is granted separately at redeem.
CREATE TABLE workspace_member (
    workspace_id TEXT NOT NULL,
    principal    TEXT NOT NULL,
    role         TEXT NOT NULL CHECK (role IN ('owner', 'reviewer', 'member')),
    status       TEXT NOT NULL CHECK (status IN ('invited', 'confirmed')),
    invited_by   TEXT,
    added_at     TEXT NOT NULL,
    PRIMARY KEY (workspace_id, principal)
);

-- INVITES — the opaque, single-link credential a `/i/<token>` carries. The token is HMAC-derived (so a
-- lost-ack create retry re-derives the IDENTICAL link) and stored ONLY as its sha256 — the PLAINTEXT IS
-- NEVER STORED. There is NO role column on the link itself (the role lives on the pre-seeded
-- `workspace_member` rows the invite UPSERTs): the link carries no authority, it only routes a device into
-- the enrollment flow. `revoked` is the instant server-side kill switch (BIGINT 0/1); `expires_at` (epoch-ms,
-- nullable = never expires) is the soft deadline. `created_by` is the inviting owner principal (audit).
CREATE TABLE invites (
    token_sha256 BYTEA  NOT NULL CHECK (octet_length(token_sha256) = 32),
    workspace_id TEXT   NOT NULL,
    expires_at   BIGINT,
    created_by   TEXT   NOT NULL,
    revoked      BIGINT NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1)),
    created_at   TEXT   NOT NULL,
    PRIMARY KEY (token_sha256)
);

-- The per-workspace enumeration of issued invites (a future revoke/list surface; not on the resolver's hot
-- path, which probes the token_sha256 PK directly).
CREATE INDEX invites_by_ws ON invites (workspace_id);

-- The skills an invite pre-offers, with an optional display `name` (the name is NOT bound into the invite
-- token's HMAC preimage — only the skill ids are — so a rename never forks the deterministic link). CASCADE
-- so revoking/deleting an invite drops its offered-skill rows.
CREATE TABLE invite_skill (
    token_sha256 BYTEA NOT NULL CHECK (octet_length(token_sha256) = 32),
    skill_id     TEXT  NOT NULL,
    name         TEXT,
    PRIMARY KEY (token_sha256, skill_id),
    FOREIGN KEY (token_sha256) REFERENCES invites (token_sha256) ON DELETE CASCADE
);

-- ENROLLMENT GRANTS — the single-use credential `poll_device_auth` issues once a device-auth session is
-- confirmed, and `redeem_enrollment` consumes. HMAC-derived from `(device_code_sha256, workspace_id)` so a
-- re-poll re-derives the SAME grant (idempotent issue) and stored ONLY as its sha256. It BINDS the proven
-- identity end-to-end: the `principal` the session confirmed, the `device_pubkey` + server-derived
-- `device_key_id` the redeem possession-proof must match, the non-secret `device_auth_id` bound into the
-- enroll frame, and the offered skill set (a child table). `consumed_at` is an audit marker only — redeem is
-- naturally idempotent (it re-derives identical read tokens), so a replay re-runs harmlessly.
CREATE TABLE enrollment_grants (
    grant_sha256   BYTEA  NOT NULL CHECK (octet_length(grant_sha256) = 32),
    workspace_id   TEXT   NOT NULL,
    invite_sha256  BYTEA           CHECK (invite_sha256 IS NULL OR octet_length(invite_sha256) = 32),
    principal      TEXT   NOT NULL,
    device_pubkey  BYTEA  NOT NULL CHECK (octet_length(device_pubkey) = 32),
    device_key_id  TEXT   NOT NULL,
    device_auth_id TEXT   NOT NULL,
    expires_at     BIGINT NOT NULL,
    consumed_at    BIGINT,
    created_at     TEXT   NOT NULL,
    PRIMARY KEY (grant_sha256)
);

-- The grant's offered skill set (the skills redeem rosters the principal onto + mints read tokens for). A
-- child table so the redeem loop + the enroll frame's `offered_skill_ids` read it directly. CASCADE hygiene.
CREATE TABLE enrollment_grant_skill (
    grant_sha256 BYTEA NOT NULL CHECK (octet_length(grant_sha256) = 32),
    skill_id     TEXT  NOT NULL,
    PRIMARY KEY (grant_sha256, skill_id),
    FOREIGN KEY (grant_sha256) REFERENCES enrollment_grants (grant_sha256) ON DELETE CASCADE
);

-- DEVICE-AUTH SESSIONS — the RFC-8628-shaped device-authorization flow. `device_code` is the SECRET poll
-- credential (stored ONLY as its sha256, the PK); `user_code` is the short, low-value code a human types on
-- the verification page (stored PLAINTEXT for that lookup) and doubles as the non-secret `device_auth_id`
-- bound into the enroll frame. `status` walks pending -> confirmed -> issued (or denied/expired). On cloud a
-- session starts `pending` (a human must confirm an identity); on self-host it starts `confirmed` with a
-- server-derived device-rooted `confirmed_principal`, so the first poll yields a grant with no human step.
-- `device_pubkey` + `device_key_id` (server-derived, never client-asserted) flow into the issued grant.
CREATE TABLE device_auth_sessions (
    device_code_sha256  BYTEA  NOT NULL CHECK (octet_length(device_code_sha256) = 32),
    user_code           TEXT   NOT NULL,
    workspace_id        TEXT   NOT NULL,
    invite_sha256       BYTEA           CHECK (invite_sha256 IS NULL OR octet_length(invite_sha256) = 32),
    device_pubkey       BYTEA  NOT NULL CHECK (octet_length(device_pubkey) = 32),
    device_key_id       TEXT   NOT NULL,
    machine_name        TEXT   NOT NULL,
    status              TEXT   NOT NULL CHECK (status IN ('pending', 'confirmed', 'issued', 'denied', 'expired')),
    confirmed_principal TEXT,
    expires_at          BIGINT NOT NULL,
    interval_secs       BIGINT NOT NULL,
    last_polled_at      BIGINT,
    created_at          TEXT   NOT NULL,
    PRIMARY KEY (device_code_sha256)
);

-- The verification page looks a session up by `user_code`; it must be unique only among the LIVE
-- (pending/confirmed) sessions — once a session is issued/denied/expired its short code may be reused.
CREATE UNIQUE INDEX device_auth_user_code ON device_auth_sessions (user_code)
    WHERE status IN ('pending', 'confirmed');

-- PASSCODES — the 6-digit second factor the verification page issues to prove control of an email. Stored
-- ONLY as sha256 (the plaintext is mailed once, never persisted, never logged). `attempts` caps brute force
-- (the op locks the row after a small cap). Keyed per (session, principal); CASCADE with the session.
CREATE TABLE passcodes (
    device_code_sha256 BYTEA  NOT NULL CHECK (octet_length(device_code_sha256) = 32),
    principal          TEXT   NOT NULL,
    passcode_sha256    BYTEA  NOT NULL CHECK (octet_length(passcode_sha256) = 32),
    expires_at         BIGINT NOT NULL,
    attempts           BIGINT NOT NULL DEFAULT 0,
    created_at         TEXT   NOT NULL,
    PRIMARY KEY (device_code_sha256, principal),
    FOREIGN KEY (device_code_sha256) REFERENCES device_auth_sessions (device_code_sha256) ON DELETE CASCADE
);

-- WORKSPACE EVENTS — the governance audit log AND the op_id idempotency store, keyed (workspace_id, op_id).
-- `request_sha256` is sha256 of the governance signing preimage (the bound identity of the request): a
-- same-op_id retry with a MATCHING `request_sha256` REPLAYS the receipt; a DIFFERENT one is a DENIED
-- key-reuse (the slot belongs to the original op). `actor` is the acting principal, `gov_op_type` the verb,
-- `target` the affected principal/device, `outcome` the terminal result. NO secret ever lands in `details`.
CREATE TABLE workspace_events (
    workspace_id   TEXT  NOT NULL,
    op_id          TEXT  NOT NULL,
    actor          TEXT  NOT NULL,
    gov_op_type    TEXT  NOT NULL,
    request_sha256 BYTEA NOT NULL CHECK (octet_length(request_sha256) = 32),
    target         TEXT,
    outcome        TEXT  NOT NULL,
    details        TEXT,
    created_at     TEXT  NOT NULL,
    PRIMARY KEY (workspace_id, op_id)
);

-- READ-TOKEN expiry + device binding (both nullable — legacy rows stay NULL, never expire, never bound). A
-- redeem mints a read token bound to the enrolling device, so a per-device revoke (DELETE the device's read
-- tokens) is instant. `resolve_read_token` now enforces `expires_at` against the server clock.
ALTER TABLE read_token ADD COLUMN device_key_id TEXT;
ALTER TABLE read_token ADD COLUMN expires_at BIGINT;
CREATE INDEX read_token_by_device ON read_token (workspace_id, device_key_id);
