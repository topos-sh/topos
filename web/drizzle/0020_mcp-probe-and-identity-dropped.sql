-- TWO TABLES WHOSE QUESTIONS ARE ANSWERED ELSEWHERE NOW.
--
-- `bundle_identity` recorded the registry name a bundle's bytes claimed, so a second bundle could
-- not claim it too. A server's name is a column on the catalog row today, unique among the global
-- ones by index and per workspace by construction — so the claim has nothing left to arbitrate,
-- and every door that used to make one is gone.
--
-- `mcp_probe` recorded what this plane saw when it asked a server's address. The answer belongs to
-- the revision it is about, and lives on it (`mcp_server_revision.probe_outcome` / `probed_at`).
-- The rows here were advisory reports about versions the catalog no longer delivers from; nothing
-- reads them, and a report nobody reads is not worth carrying forward.

DROP TABLE "web"."bundle_identity" CASCADE;--> statement-breakpoint
DROP TABLE "web"."mcp_probe" CASCADE;
