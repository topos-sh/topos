-- The gateway schema — owned by role `topos_gateway`, migrated by the gateway service at boot.
--
-- CREDENTIAL CUSTODY SPLITS IN TWO: `credential` is metadata the web app may read (it renders
-- "using your account" lines from it, with no lane call), `credential_secret` is ciphertext NO
-- other role is ever granted. The grants below are the boundary; a grants test logs in as
-- `topos_web` and proves the split.
--
-- No cross-schema foreign keys in either direction: `workspace_id`, `server_id`, `user_id`,
-- `session_id` are the OPAQUE ids the web schema minted. Deletion coherence is the calling
-- tier's job (a revoked connection stops resolving; a deleted credential row cascades only its
-- own secret).

CREATE SCHEMA IF NOT EXISTS gateway;

-- One sign-in to one server, held for a workspace: `user_id` NULL is the workspace service
-- account. DELETE on revoke — never tombstoned (history is the audit ledger, web-side).
CREATE TABLE gateway.credential (
  id text PRIMARY KEY,
  workspace_id text NOT NULL,
  server_id text NOT NULL,
  user_id text,
  auth_kind text NOT NULL,
  -- Attribution as display text — the row outlives whichever account acted.
  created_by text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  last_refreshed_at timestamptz,
  CONSTRAINT credential_id_shape_check CHECK (id ~ '^cred_[0-9a-f]{32}$'),
  CONSTRAINT credential_auth_kind_check CHECK (auth_kind IN ('oauth', 'manual')),
  -- One personal credential per person per server; NULLs are distinct, so the service-account
  -- slot needs its own partial unique below.
  CONSTRAINT credential_slot_unique UNIQUE (workspace_id, server_id, user_id)
);

-- One workspace service account per server.
CREATE UNIQUE INDEX credential_workspace_slot
  ON gateway.credential (workspace_id, server_id)
  WHERE user_id IS NULL;

-- The ciphertext, apart from the metadata so the grant boundary can run between them. The
-- envelope's AAD binds credential id + workspace id: a ciphertext copied onto another row will
-- not decrypt.
CREATE TABLE gateway.credential_secret (
  credential_id text PRIMARY KEY REFERENCES gateway.credential (id) ON DELETE CASCADE,
  ciphertext bytea NOT NULL
);

-- The per-workspace data key, wrapped by the master key (increment 1 backend: a key file). A
-- copied wrapped key unwraps only under its own workspace id — the wrap's AAD.
CREATE TABLE gateway.workspace_key (
  workspace_id text PRIMARY KEY,
  wrapped_key bytea NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- What live tools/list responses advertised — the web's tool-selection checklist reads it.
-- `currently_offered` flips false when a later listing no longer carries the name.
CREATE TABLE gateway.observed_tool (
  workspace_id text NOT NULL,
  server_id text NOT NULL,
  name text NOT NULL,
  description text,
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz NOT NULL DEFAULT now(),
  currently_offered boolean NOT NULL DEFAULT true,
  PRIMARY KEY (workspace_id, server_id, name)
);

-- One row per proxied request. NEVER arguments, results, or header values — the columns are the
-- whole vocabulary, and the usage page renders nothing more.
CREATE TABLE gateway.usage_event (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  workspace_id text NOT NULL,
  server_id text NOT NULL,
  session_id text NOT NULL,
  user_id text NOT NULL,
  -- NULL for non-tool methods (initialize, lists, notifications).
  tool_name text,
  method text NOT NULL,
  outcome text NOT NULL,
  duration_ms integer NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT usage_event_outcome_check CHECK (
    outcome IN ('ok', 'denied_tool', 'no_credential', 'unauthorized', 'upstream_error')
  )
);

CREATE INDEX usage_event_workspace_created_idx ON gateway.usage_event (workspace_id, created_at);

-- Short-TTL authorize-ceremony rows, deleted on completion (single-use: the callback consumes
-- the state row before it exchanges). `return_to` is where the person lands afterwards,
-- same-origin-checked against the web's public URL at mint AND at redemption. `resource` is the
-- RFC 8707 audience the token request must repeat.
CREATE TABLE gateway.oauth_flow (
  id text PRIMARY KEY,
  state text NOT NULL UNIQUE,
  workspace_id text NOT NULL,
  server_id text NOT NULL,
  user_id text,
  code_verifier text NOT NULL,
  client_id text NOT NULL,
  token_endpoint text NOT NULL,
  resource text NOT NULL,
  return_to text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz NOT NULL,
  CONSTRAINT oauth_flow_id_shape_check CHECK (id ~ '^oaf_[0-9a-f]{32}$')
);

-- THE READ BOUNDARY, explicit per table (never default privileges): the web renders sign-in
-- state, the tools checklist, and the usage page from these three. `credential_secret`,
-- `workspace_key`, and `oauth_flow` get NO grant, ever — the grants test proves the refusal by
-- logging in as `topos_web`.
GRANT USAGE ON SCHEMA gateway TO topos_web;
GRANT SELECT ON gateway.credential TO topos_web;
GRANT SELECT ON gateway.observed_tool TO topos_web;
GRANT SELECT ON gateway.usage_event TO topos_web;
