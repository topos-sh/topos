-- HOW A CONNECTED SERVER IS ROUTED, in three layers. Purely additive, and every default is the
-- standing behavior: the workspace switch is born 'on', a connection's `gateway_policy` is born
-- NULL (route through a deployed gateway once a sign-in stands, directly otherwise), and no
-- opt-out row exists until a member chooses direct for their own machines. The opt-out anchors
-- to the SEAT (removal cascades it; a re-invite starts clean) and to the CONNECTION (a
-- disconnect takes it along), exactly as declines and the tool policy do.

CREATE TABLE "web"."mcp_gateway_optout" (
	"workspace_id" text NOT NULL,
	"server_id" text NOT NULL,
	"user_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "mcp_gateway_optout_workspace_id_server_id_user_id_pk" PRIMARY KEY("workspace_id","server_id","user_id")
);
--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" ADD COLUMN "gateway_policy" text;--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD COLUMN "mcp_gateway" text DEFAULT 'on' NOT NULL;--> statement-breakpoint
ALTER TABLE "web"."mcp_gateway_optout" ADD CONSTRAINT "mcp_gateway_optout_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_gateway_optout" ADD CONSTRAINT "mcp_gateway_optout_connection_fk" FOREIGN KEY ("workspace_id","server_id") REFERENCES "web"."bundle_mcp"("workspace_id","server_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_mcp" ADD CONSTRAINT "bundle_mcp_gateway_policy_check" CHECK ("web"."bundle_mcp"."gateway_policy" is null or "web"."bundle_mcp"."gateway_policy" in ('direct', 'required'));--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD CONSTRAINT "workspace_mcp_gateway_check" CHECK ("web"."workspace"."mcp_gateway" in ('off', 'on'));