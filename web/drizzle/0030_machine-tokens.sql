-- HEADLESS ACCESS, in two tables. A machine token is a workspace's own read-only credential
-- (owner-minted, shown once, stored hashed); a service session is one machine seen using it —
-- ephemeral, idle rows deleted lazily, listed apart from people's machines. No user, no seat:
-- a token is not a person, and every write lane refuses it.

CREATE TABLE "web"."machine_token" (
	"id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"name" text NOT NULL,
	"token_sha256" "bytea" NOT NULL,
	"created_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_used_at" timestamp with time zone,
	CONSTRAINT "machine_token_token_sha256_unique" UNIQUE("token_sha256"),
	CONSTRAINT "machine_token_sha256_check" CHECK (octet_length("web"."machine_token"."token_sha256") = 32)
);
--> statement-breakpoint
CREATE TABLE "web"."service_session" (
	"id" text PRIMARY KEY NOT NULL,
	"token_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"display_name" text NOT NULL,
	"applied" jsonb,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_seen_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "web"."machine_token" ADD CONSTRAINT "machine_token_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."machine_token" ADD CONSTRAINT "machine_token_created_by_user_id_fk" FOREIGN KEY ("created_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."service_session" ADD CONSTRAINT "service_session_token_id_machine_token_id_fk" FOREIGN KEY ("token_id") REFERENCES "web"."machine_token"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."service_session" ADD CONSTRAINT "service_session_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "machine_token_workspace_idx" ON "web"."machine_token" USING btree ("workspace_id");--> statement-breakpoint
CREATE UNIQUE INDEX "service_session_token_name_idx" ON "web"."service_session" USING btree ("token_id","display_name");--> statement-breakpoint
CREATE INDEX "service_session_workspace_idx" ON "web"."service_session" USING btree ("workspace_id");