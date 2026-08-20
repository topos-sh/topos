-- THE GENERIC MCP CATALOG IS SOURCED FROM A COMMITTED FILE NOW, reconciled by a boot sync. So the
-- registry-as-upstream apparatus comes off the schema: the `source` origin taxonomy, the
-- candidate/published/rejected/revoked/superseded status vocabulary, and the read-API framing that
-- keyed on a `registry_name`. What remains is ONE storage model for every server, told apart only
-- by `workspace_id` (null = public, from the file or staff; set = a workspace's own).
--
-- Maturity is the POINTER, not a column: `current_revision_id` names the one people receive, every
-- other revision is a proposal or history (told apart by `seq` against the current), and
-- `dismissed_at` is the one terminal state a staff member can put a proposal in.
--
-- No back-compat (pre-1.0): this reshapes IN PLACE and lets the boot sync reseed the public catalog
-- from the file. Connections and pins are preserved — only the sweep-era vocabulary is dropped.

-- ── mcp_server ──────────────────────────────────────────────────────────────────────────────

-- A `candidate` was a row pulled in for evaluation and offered to nobody; the vocabulary is gone,
-- so any such row is DELISTED (off offer) rather than deleted — it may carry a name a person knows.
UPDATE "web"."mcp_server" SET "status" = 'delisted' WHERE "status" = 'candidate';--> statement-breakpoint
ALTER TABLE "web"."mcp_server" DROP CONSTRAINT "mcp_server_status_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server" ALTER COLUMN "status" SET DEFAULT 'active';--> statement-breakpoint
ALTER TABLE "web"."mcp_server" ADD CONSTRAINT "mcp_server_status_check" CHECK ("web"."mcp_server"."status" in ('active', 'delisted'));--> statement-breakpoint

-- `registry_name` → `name`: the reverse-DNS identity, no longer the key of a read API that is gone.
DROP INDEX "web"."mcp_server_global_registry_name";--> statement-breakpoint
DROP INDEX "web"."mcp_server_private_registry_name";--> statement-breakpoint
ALTER TABLE "web"."mcp_server" DROP CONSTRAINT "mcp_server_registry_name_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server" RENAME COLUMN "registry_name" TO "name";--> statement-breakpoint
ALTER TABLE "web"."mcp_server" ADD CONSTRAINT "mcp_server_name_check" CHECK ("web"."mcp_server"."name" is null or "web"."mcp_server"."name" ~ '^[^/]+/[^/]+$');--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_server_public_name" ON "web"."mcp_server" USING btree ("name") WHERE workspace_id is null;--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_server_private_name" ON "web"."mcp_server" USING btree ("workspace_id","name") WHERE workspace_id is not null;--> statement-breakpoint

-- The ONE bit that decides how a public row tracks the file: false = tracks it (advance current to
-- each new file version); true = a staff member has edited or promoted it, so a file version lands
-- as a non-current proposal. A self-hosted install has no panel, so it never sets this.
ALTER TABLE "web"."mcp_server" ADD COLUMN "manually_curated" boolean DEFAULT false NOT NULL;--> statement-breakpoint

-- BACKFILL curation from the old provenance BEFORE dropping `source` and the decision stamps: a
-- public server a staff member ever TOUCHED is a decision the file must not silently undo. Marking
-- it curated makes the boot sync treat future file versions as proposals rather than promoting one
-- over the staff choice. "Touched" is any revision that is not the seed's own row (an accepted
-- upstream candidate, a correction), OR any revision a staff member decided against (`decided_at`)
-- or pulled back (`revoked_at`) — even when the current fell back to the seed afterward. A server
-- with only its untouched seed revision stays uncurated and tracks the file.
UPDATE "web"."mcp_server" ms SET "manually_curated" = true
WHERE ms."workspace_id" IS NULL
  AND EXISTS (
    SELECT 1 FROM "web"."mcp_server_revision" r
    WHERE r."server_id" = ms."id"
      AND (r."source" <> 'seed' OR r."decided_at" IS NOT NULL OR r."revoked_at" IS NOT NULL)
  );--> statement-breakpoint

-- ── mcp_server_revision ─────────────────────────────────────────────────────────────────────

-- The one terminal state that remains: a staff-declined proposal. A `rejected` candidate carried
-- exactly this meaning, so its decision stamp becomes the dismissal.
ALTER TABLE "web"."mcp_server_revision" ADD COLUMN "dismissed_at" timestamp with time zone;--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" ADD COLUMN "dismissed_by" text;--> statement-breakpoint
UPDATE "web"."mcp_server_revision"
  SET "dismissed_at" = COALESCE("decided_at", now()), "dismissed_by" = COALESCE("decided_by", 'Topos')
  WHERE "status" = 'rejected';--> statement-breakpoint

-- The status vocabulary, the `source` origin, and the decide/revoke stamps come off — the pointer
-- carries maturity, and the promotion stamp (`published_at`/`published_by`) stays as the record of
-- when a revision last became current. The version-uniqueness index no longer keys on `source`.
DROP INDEX "web"."mcp_server_revision_upstream_version";--> statement-breakpoint
DROP INDEX "web"."mcp_server_revision_server_idx";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_status_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_source_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_published_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_rejected_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_decided_by_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_revoked_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP COLUMN "status";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP COLUMN "source";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP COLUMN "decided_at";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP COLUMN "decided_by";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" DROP COLUMN "revoked_at";--> statement-breakpoint

-- The new index covers ANY non-null version, where the old one covered only source='registry'. So a
-- server that legitimately carried two revisions of one version (a staff/owner edit that kept the
-- number) would break the index build. Make all but the newest such revision truly VERSIONLESS
-- FIRST — the honest state of a document whose version another revision now owns, and exempt from
-- the index. Both sides are rewritten together so the extracted columns never disagree with the
-- stored document: the version and its `$schema` (that schema requires a version) come off the
-- document, and `schema_version` is nulled with `upstream_version`. This never touches a server
-- with one revision per version.
UPDATE "web"."mcp_server_revision" r
SET "upstream_version" = NULL,
    "schema_version" = NULL,
    "document" = r."document" - 'version' - '$schema'
WHERE r."upstream_version" IS NOT NULL
  AND EXISTS (
    SELECT 1 FROM "web"."mcp_server_revision" r2
    WHERE r2."server_id" = r."server_id"
      AND r2."upstream_version" = r."upstream_version"
      AND r2."seq" > r."seq"
  );--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_server_revision_upstream_version" ON "web"."mcp_server_revision" USING btree ("server_id","upstream_version") WHERE upstream_version is not null;--> statement-breakpoint
CREATE INDEX "mcp_server_revision_server_idx" ON "web"."mcp_server_revision" USING btree ("server_id");--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" ADD CONSTRAINT "mcp_server_revision_dismissed_by_check" CHECK (("web"."mcp_server_revision"."dismissed_at" is null) = ("web"."mcp_server_revision"."dismissed_by" is null));
