-- One advisory probe result per published MCP version: what this plane saw when it asked the
-- endpoint, once, after the publish had already landed. Additive and empty at birth — a version
-- with no row here reads as "not checked yet", which is what every version published before this
-- migration is. Nothing backfills: a probe is an observation with a timestamp, and inventing one
-- for a version nobody asked about would be inventing the observation.
--
-- `outcome` is a small vocabulary of its own, constrained here so a typo cannot become a state.

CREATE TABLE "web"."mcp_probe" (
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"version_id" text NOT NULL,
	"outcome" text NOT NULL,
	"detail" text,
	"probed_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "mcp_probe_bundle_id_version_id_pk" PRIMARY KEY("bundle_id","version_id"),
	CONSTRAINT "mcp_probe_outcome_check" CHECK ("web"."mcp_probe"."outcome" IN ('responding', 'sign_in_required', 'not_verifiable', 'not_responding'))
);
--> statement-breakpoint
ALTER TABLE "web"."mcp_probe" ADD CONSTRAINT "mcp_probe_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;