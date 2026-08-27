-- THE GATEWAY'S SECOND CALLER. A proxied MCP call used to resolve against `web.cli_session`
-- alone — a person's enrolled machine. It now also resolves a workspace MACHINE TOKEN and the
-- service session one of its runs appears as, so CI can call tools through the gateway holding
-- the workspace's sign-in instead of a vendor secret of its own.
--
-- Two more tables on the SAME enumerated grant 0028 established, and for the same reason: the
-- gateway is the most-exposed component, so it reads exactly what a proxied call resolves
-- against and nothing else. `machine_token` carries the bearer's SHA-256 (never a plaintext),
-- the identical custody shape as the `cli_session.credential_sha256` this role has always
-- matched bearers against; `service_session` carries a run's display label and its last-seen
-- clock, which is the liveness the gateway must honor.
--
-- Guarded on the role existing (a deployment running no gateway never created it) and
-- idempotent, exactly as 0028.
DO $$
BEGIN
  IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_gateway') THEN
    GRANT SELECT ON
      web.machine_token,
      web.service_session
    TO topos_gateway;
  END IF;
END $$;
