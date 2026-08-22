-- The gateway role reads a FIXED, ENUMERATED set of web tables — never a blanket. This lives in
-- the web lineage, not initdb, for one reason: initdb runs before any web table exists (fresh
-- volume), so the only initdb-time way to cover them is ALTER DEFAULT PRIVILEGES, which grants
-- EVERY future web table — including the Better Auth identity store (account tokens, session
-- tokens, emails). The gateway is the most-exposed component (the one public listener, holding the
-- master key); it must not also be able to read the identity store. Here the tables exist and
-- topos_web owns them, so the grant is exactly the eight the gateway's store.ts resolves against.
--
-- Guarded on the role existing: a deployment that runs no gateway never created topos_gateway, and
-- this migration is then a no-op. Idempotent — GRANT is, and re-running only re-affirms.
DO $$
BEGIN
  IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_gateway') THEN
    GRANT USAGE ON SCHEMA web TO topos_gateway;
    GRANT SELECT ON
      web.cli_session,
      web.workspace,
      web.bundle,
      web.bundle_mcp,
      web.mcp_server,
      web.mcp_server_revision,
      web.mcp_tool_policy,
      web.mcp_tool_selection
    TO topos_gateway;
  END IF;
END $$;
