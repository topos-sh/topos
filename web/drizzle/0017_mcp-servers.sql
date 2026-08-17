-- MCP servers as ROWS: the catalog (`mcp_server`), its append-only version history
-- (`mcp_server_revision`), and one workspace's use of one server (`bundle_mcp`, 1:1 with the
-- `kind: 'mcp'` bundle that carries it). Additive and empty at birth — the catalog's own rows
-- arrive in the seed beside this file, and existing connections are recorded by the app-tier
-- backfill that follows the migrations, where the documents they name are reachable.
--
-- Two keys carry rules a comment would otherwise have to be trusted for:
--   · `mcp_server_global_registry_name` is PARTIAL — one global row per registry name, while
--     private rows (workspace_id set) collide with nobody, including each other.
--   · `mcp_server_revision_upstream_version` is PARTIAL — one document per version FROM
--     UPSTREAM; a staff correction or an owner's edit is a new revision of the same version
--     string by design.
-- The pointer keys are composite ((current_revision_id, id) and (pinned_revision_id, server_id)),
-- so a pointer or a pin can only ever name a revision of the server it belongs to.

CREATE TABLE "web"."bundle_mcp" (
	"bundle_id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"server_id" text NOT NULL,
	"pinned_revision_id" text,
	"created_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "bundle_mcp_workspace_server_unique" UNIQUE("workspace_id","server_id")
);
--> statement-breakpoint
CREATE TABLE "web"."mcp_server" (
	"id" text PRIMARY KEY NOT NULL,
	"workspace_id" text,
	"registry_name" text,
	"display_name" text NOT NULL,
	"description" text,
	"website_url" text,
	"icon" text,
	"auth_mode" text,
	"auth_note" text,
	"scope_menu" jsonb,
	"status" text DEFAULT 'candidate' NOT NULL,
	"current_revision_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "mcp_server_auth_mode_check" CHECK ("web"."mcp_server"."auth_mode" is null or "web"."mcp_server"."auth_mode" in ('none', 'oauth', 'manual')),
	CONSTRAINT "mcp_server_status_check" CHECK ("web"."mcp_server"."status" in ('candidate', 'active', 'delisted')),
	CONSTRAINT "mcp_server_registry_name_check" CHECK ("web"."mcp_server"."registry_name" is null or "web"."mcp_server"."registry_name" ~ '^[^/]+/[^/]+$')
);
--> statement-breakpoint
CREATE TABLE "web"."mcp_server_revision" (
	"id" text PRIMARY KEY NOT NULL,
	"server_id" text NOT NULL,
	"seq" integer NOT NULL,
	"status" text DEFAULT 'candidate' NOT NULL,
	"upstream_version" text NOT NULL,
	"schema_version" text,
	"document" jsonb NOT NULL,
	"transport" text,
	"url" text,
	"probe_outcome" text,
	"probed_at" timestamp with time zone,
	"protocol_versions" jsonb,
	"verification" jsonb,
	"source" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"published_at" timestamp with time zone,
	"published_by" text,
	"decided_at" timestamp with time zone,
	"decided_by" text,
	"revoked_at" timestamp with time zone,
	CONSTRAINT "mcp_server_revision_seq_unique" UNIQUE("server_id","seq"),
	CONSTRAINT "mcp_server_revision_id_server_unique" UNIQUE("id","server_id"),
	CONSTRAINT "mcp_server_revision_status_check" CHECK ("web"."mcp_server_revision"."status" in ('candidate', 'published', 'rejected', 'revoked')),
	CONSTRAINT "mcp_server_revision_source_check" CHECK ("web"."mcp_server_revision"."source" in ('registry', 'staff', 'owner', 'seed')),
	CONSTRAINT "mcp_server_revision_seq_check" CHECK ("web"."mcp_server_revision"."seq" >= 1),
	CONSTRAINT "mcp_server_revision_probe_outcome_check" CHECK ("web"."mcp_server_revision"."probe_outcome" is null or "web"."mcp_server_revision"."probe_outcome" in ('responding', 'sign_in_required', 'not_verifiable', 'not_responding')),
	CONSTRAINT "mcp_server_revision_probed_check" CHECK (("web"."mcp_server_revision"."probe_outcome" is null) = ("web"."mcp_server_revision"."probed_at" is null)),
	CONSTRAINT "mcp_server_revision_remote_check" CHECK (("web"."mcp_server_revision"."url" is null) = ("web"."mcp_server_revision"."transport" is null)),
	CONSTRAINT "mcp_server_revision_url_check" CHECK ("web"."mcp_server_revision"."url" is null or "web"."mcp_server_revision"."url" ~ '^https://'),
	CONSTRAINT "mcp_server_revision_published_check" CHECK (("web"."mcp_server_revision"."status" in ('published', 'revoked')) = ("web"."mcp_server_revision"."published_at" is not null)),
	CONSTRAINT "mcp_server_revision_published_by_check" CHECK (("web"."mcp_server_revision"."published_at" is null) = ("web"."mcp_server_revision"."published_by" is null)),
	CONSTRAINT "mcp_server_revision_rejected_check" CHECK (("web"."mcp_server_revision"."status" = 'rejected') = ("web"."mcp_server_revision"."decided_at" is not null)),
	CONSTRAINT "mcp_server_revision_decided_by_check" CHECK (("web"."mcp_server_revision"."decided_at" is null) = ("web"."mcp_server_revision"."decided_by" is null)),
	CONSTRAINT "mcp_server_revision_revoked_check" CHECK (("web"."mcp_server_revision"."status" = 'revoked') = ("web"."mcp_server_revision"."revoked_at" is not null))
);
--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" ADD CONSTRAINT "bundle_mcp_server_id_mcp_server_id_fk" FOREIGN KEY ("server_id") REFERENCES "web"."mcp_server"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" ADD CONSTRAINT "bundle_mcp_created_by_user_id_fk" FOREIGN KEY ("created_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" ADD CONSTRAINT "bundle_mcp_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" ADD CONSTRAINT "bundle_mcp_pinned_revision_fk" FOREIGN KEY ("pinned_revision_id","server_id") REFERENCES "web"."mcp_server_revision"("id","server_id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_server" ADD CONSTRAINT "mcp_server_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_server" ADD CONSTRAINT "mcp_server_current_revision_fk" FOREIGN KEY ("current_revision_id","id") REFERENCES "web"."mcp_server_revision"("id","server_id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" ADD CONSTRAINT "mcp_server_revision_server_id_mcp_server_id_fk" FOREIGN KEY ("server_id") REFERENCES "web"."mcp_server"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "bundle_mcp_server_idx" ON "web"."bundle_mcp" USING btree ("server_id");--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_server_global_registry_name" ON "web"."mcp_server" USING btree ("registry_name") WHERE workspace_id is null;--> statement-breakpoint
CREATE INDEX "mcp_server_workspace_idx" ON "web"."mcp_server" USING btree ("workspace_id");--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_server_revision_upstream_version" ON "web"."mcp_server_revision" USING btree ("server_id","upstream_version") WHERE source = 'registry';--> statement-breakpoint
CREATE INDEX "mcp_server_revision_server_idx" ON "web"."mcp_server_revision" USING btree ("server_id","status");