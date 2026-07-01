-- The per-follower, per-skill READ CREDENTIAL — the bearer token a follower presents on the read routes (the
-- `current` pointer fetch carries it as a path segment; the bundle + version GETs as a Bearer header). The
-- resolver hashes the presented token and looks it up HERE to establish the (workspace, skill, principal)
-- scope every later query binds. Appended (never edits an applied migration's bytes — sqlx checksums the
-- file). Same posture as 0001-0004: `workspace_id`-scoped rows.
--
-- ONLY the token's sha256 is stored, NEVER the plaintext: the token is a 0600 secret at rest on the follower,
-- and a database read must never recover a live credential. `token_sha256` is the PRIMARY KEY — the one
-- lookup NOT keyed on `workspace_id`, because the token is precisely what RESOLVES the workspace, so it is
-- globally unique and the resolver is an O(1) probe on the key. Minting (issuing a token to an enrolled
-- follower, and writing its 0600 at-rest file) is the enrollment subsystem's job.
CREATE TABLE read_token (
    workspace_id TEXT  NOT NULL,
    skill_id     TEXT  NOT NULL,
    principal    TEXT  NOT NULL,
    token_sha256 BYTEA NOT NULL CHECK (octet_length(token_sha256) = 32),
    PRIMARY KEY (token_sha256)
);

-- The per-(workspace, skill) enumeration of issued read tokens (a future revoke/rotate surface; not on the
-- resolver's hot path, which probes the PK directly).
CREATE INDEX read_token_by_skill ON read_token (workspace_id, skill_id);
