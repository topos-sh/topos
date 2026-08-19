-- A REVISION ID'S SHAPE, HELD BY THE DATABASE.
--
-- The id travels: a machine reports back the revision it holds, and the door that receives that
-- report accepts the minted shape (`mcpr_` and 32 lowercase hex) and nothing else. A row written
-- with any other spelling therefore breaks the machine that receives it — the applied report is
-- atomic, so its WHOLE report is refused and every bundle on it goes with it, on every update.
--
-- The catalog's seed already writes the minted shape and the previous migration remapped the
-- databases that had not; this is the rule itself, so the next writer that is not the minter —
-- a repair run by hand, a future importer — meets it here instead of in somebody's log a week
-- later.

ALTER TABLE "web"."mcp_server_revision" ADD CONSTRAINT "mcp_server_revision_id_shape_check" CHECK ("web"."mcp_server_revision"."id" ~ '^mcpr_[0-9a-f]{32}$');
