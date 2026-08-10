-- A bundle's SECOND name — the one its machine consumers resolve — recorded as a row whose
-- primary key refuses a second claimant. Replaces a per-workspace advisory lock plus a capped
-- scan of every published document, which is this index written the long way.
--
-- DELIBERATELY NO SQL BACKFILL, and this is not an oversight. The names being recorded live in
-- `server.json` INSIDE THE VAULT: `plane` stores object hashes and reachability edges, never the
-- bytes or even the paths, and the bytes themselves sit in a git object store behind an HTTP
-- lane. No statement in this file can read one. The backfill therefore runs in the app tier
-- immediately after these migrations and before the first request is served
-- (app/lib/db/bundle-identity.server.ts) — where the bytes are reachable, and still ordered
-- ahead of any publish that could claim a name an existing server serves. It is idempotent, so
-- a bundle it cannot read today is picked up on the next boot rather than skipped for good.

CREATE TABLE "web"."bundle_identity" (
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"kind" text NOT NULL,
	"identity" text NOT NULL,
	CONSTRAINT "bundle_identity_workspace_id_kind_identity_pk" PRIMARY KEY("workspace_id","kind","identity")
);
--> statement-breakpoint
ALTER TABLE "web"."bundle_identity" ADD CONSTRAINT "bundle_identity_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "bundle_identity_bundle_idx" ON "web"."bundle_identity" USING btree ("workspace_id","bundle_id");