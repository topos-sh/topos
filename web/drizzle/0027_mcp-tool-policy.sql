-- WHICH TOOLS OF A CONNECTED SERVER A WORKSPACE'S AGENTS MAY USE. Purely additive: no row means
-- `all`, which is the answer every existing connection already had, so nothing changes for a
-- workspace that never opens the panel. The policy hangs off the CONNECTION (bundle_mcp's unique
-- workspace/server pair), so disconnecting takes the policy — and its selections — with it.

CREATE TABLE "web"."mcp_tool_policy" (
	"workspace_id" text NOT NULL,
	"server_id" text NOT NULL,
	"mode" text DEFAULT 'all' NOT NULL,
	"updated_by" text,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "mcp_tool_policy_workspace_id_server_id_pk" PRIMARY KEY("workspace_id","server_id"),
	CONSTRAINT "mcp_tool_policy_mode_check" CHECK ("web"."mcp_tool_policy"."mode" in ('all', 'selected'))
);
--> statement-breakpoint
CREATE TABLE "web"."mcp_tool_selection" (
	"workspace_id" text NOT NULL,
	"server_id" text NOT NULL,
	"tool_name" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "mcp_tool_selection_workspace_id_server_id_tool_name_pk" PRIMARY KEY("workspace_id","server_id","tool_name")
);
--> statement-breakpoint
ALTER TABLE "web"."mcp_tool_policy" ADD CONSTRAINT "mcp_tool_policy_updated_by_user_id_fk" FOREIGN KEY ("updated_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_tool_policy" ADD CONSTRAINT "mcp_tool_policy_connection_fk" FOREIGN KEY ("workspace_id","server_id") REFERENCES "web"."bundle_mcp"("workspace_id","server_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."mcp_tool_selection" ADD CONSTRAINT "mcp_tool_selection_policy_fk" FOREIGN KEY ("workspace_id","server_id") REFERENCES "web"."mcp_tool_policy"("workspace_id","server_id") ON DELETE cascade ON UPDATE no action;