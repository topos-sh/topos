-- THE CATALOG'S SEEDED IDS TAKE THE SHAPE EVERY MINTED ONE HAS.
--
-- A revision id TRAVELS: a machine reports back the revision it holds, and the door that receives
-- that report accepts the minted shape and nothing else (`mcpr_` and 32 lowercase hex). The rows
-- the catalog was born with carried a readable slug instead, so an install holding one had its
-- WHOLE applied report refused — the report is atomic, so every bundle on that machine went with
-- it, on every update, forever.
--
-- The seed itself now writes the right shape, so a database provisioned from scratch never holds
-- a wrong one. This is for the databases that already ran it: the same derivation, so both paths
-- land on exactly the same ids and there is one catalog, not two.
--
-- The four foreign keys stand down for the length of the remap because the ids being rewritten
-- are the keys they point at. Nothing else about them changes, and they are put back byte for
-- byte. A row already in the minted shape is untouched, which is what makes this a no-op on a
-- fresh database.

ALTER TABLE "web"."bundle_mcp" DROP CONSTRAINT "bundle_mcp_server_id_mcp_server_id_fk";--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" DROP CONSTRAINT "bundle_mcp_pinned_revision_fk";--> statement-breakpoint
ALTER TABLE "web"."mcp_server" DROP CONSTRAINT "mcp_server_current_revision_fk";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_server_id_mcp_server_id_fk";--> statement-breakpoint

UPDATE "web"."bundle_mcp" SET "server_id" = 'mcps_' || md5(substring("server_id" from 6))
  WHERE "server_id" !~ '^mcps_[0-9a-f]{32}$';--> statement-breakpoint
UPDATE "web"."bundle_mcp" SET "pinned_revision_id" = 'mcpr_' || md5(substring("pinned_revision_id" from 6))
  WHERE "pinned_revision_id" IS NOT NULL AND "pinned_revision_id" !~ '^mcpr_[0-9a-f]{32}$';--> statement-breakpoint
UPDATE "web"."mcp_server" SET "current_revision_id" = 'mcpr_' || md5(substring("current_revision_id" from 6))
  WHERE "current_revision_id" IS NOT NULL AND "current_revision_id" !~ '^mcpr_[0-9a-f]{32}$';--> statement-breakpoint
UPDATE "web"."mcp_server_revision" SET "server_id" = 'mcps_' || md5(substring("server_id" from 6))
  WHERE "server_id" !~ '^mcps_[0-9a-f]{32}$';--> statement-breakpoint
UPDATE "web"."mcp_server_revision" SET "id" = 'mcpr_' || md5(substring("id" from 6))
  WHERE "id" !~ '^mcpr_[0-9a-f]{32}$';--> statement-breakpoint
UPDATE "web"."mcp_server" SET "id" = 'mcps_' || md5(substring("id" from 6))
  WHERE "id" !~ '^mcps_[0-9a-f]{32}$';--> statement-breakpoint

ALTER TABLE "web"."bundle_mcp" ADD CONSTRAINT "bundle_mcp_server_id_mcp_server_id_fk" FOREIGN KEY ("server_id") REFERENCES "web"."mcp_server"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" ADD CONSTRAINT "bundle_mcp_pinned_revision_fk" FOREIGN KEY ("pinned_revision_id","server_id") REFERENCES "web"."mcp_server_revision"("id","server_id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_server" ADD CONSTRAINT "mcp_server_current_revision_fk" FOREIGN KEY ("current_revision_id","id") REFERENCES "web"."mcp_server_revision"("id","server_id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" ADD CONSTRAINT "mcp_server_revision_server_id_mcp_server_id_fk" FOREIGN KEY ("server_id") REFERENCES "web"."mcp_server"("id") ON DELETE cascade ON UPDATE no action;
