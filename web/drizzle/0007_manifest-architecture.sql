CREATE TABLE "web"."bundle_upstream" (
	"bundle_id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"host" text NOT NULL,
	"repo" text NOT NULL,
	"path" text DEFAULT '' NOT NULL,
	"license" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_checked_at" timestamp with time zone,
	"last_seen_commit" text,
	CONSTRAINT "bundle_upstream_repo_check" CHECK ("web"."bundle_upstream"."repo" ~ '^[^/]+/[^/]+$')
);
--> statement-breakpoint
CREATE TABLE "web"."cli_session" (
	"id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"user_id" text NOT NULL,
	"display_name" text NOT NULL,
	"credential_sha256" "bytea" NOT NULL,
	"status" text DEFAULT 'active' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_seen_at" timestamp with time zone,
	CONSTRAINT "cli_session_credential_sha256_unique" UNIQUE("credential_sha256"),
	CONSTRAINT "cli_session_status_check" CHECK ("web"."cli_session"."status" in ('pending', 'active')),
	CONSTRAINT "cli_session_credential_sha256_check" CHECK (octet_length("web"."cli_session"."credential_sha256") = 32)
);
--> statement-breakpoint
CREATE TABLE "web"."login_flow" (
	"id" text PRIMARY KEY NOT NULL,
	"user_code" text NOT NULL,
	"flow_code_sha256" "bytea" NOT NULL,
	"requested_name" text NOT NULL,
	"requested_workspace" text DEFAULT '' NOT NULL,
	"approved_workspace_id" text,
	"invite_token_sha256" "bytea",
	"status" text DEFAULT 'pending' NOT NULL,
	"approved_by" text,
	"session_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	CONSTRAINT "login_flow_flow_code_sha256_unique" UNIQUE("flow_code_sha256"),
	CONSTRAINT "login_flow_flow_code_sha256_check" CHECK (octet_length("web"."login_flow"."flow_code_sha256") = 32),
	CONSTRAINT "login_flow_invite_token_sha256_check" CHECK ("web"."login_flow"."invite_token_sha256" is null or octet_length("web"."login_flow"."invite_token_sha256") = 32),
	CONSTRAINT "login_flow_status_check" CHECK ("web"."login_flow"."status" in ('pending', 'approved', 'denied')),
	CONSTRAINT "login_flow_approved_check" CHECK ("web"."login_flow"."status" <> 'approved' or "web"."login_flow"."session_id" is not null)
);
--> statement-breakpoint
CREATE TABLE "web"."profile_entry" (
	"workspace_id" text NOT NULL,
	"user_id" text NOT NULL,
	"mode" text NOT NULL,
	"bundle_id" text,
	"channel_id" text,
	"pin" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "profile_entry_mode_check" CHECK ("web"."profile_entry"."mode" in ('include', 'exclude')),
	CONSTRAINT "profile_entry_target_check" CHECK (("web"."profile_entry"."bundle_id" is null) <> ("web"."profile_entry"."channel_id" is null)),
	CONSTRAINT "profile_entry_pin_check" CHECK ("web"."profile_entry"."pin" is null or ("web"."profile_entry"."bundle_id" is not null and "web"."profile_entry"."mode" = 'include'))
);
--> statement-breakpoint
CREATE TABLE "web"."session_bundle_state" (
	"session_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"applied_version_id" text NOT NULL,
	"reported_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "session_bundle_state_session_id_bundle_id_pk" PRIMARY KEY("session_id","bundle_id")
);
--> statement-breakpoint
CREATE TABLE "web"."version_upstream" (
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"version_id" text NOT NULL,
	"commit" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "version_upstream_bundle_id_version_id_pk" PRIMARY KEY("bundle_id","version_id")
);
--> statement-breakpoint
ALTER TABLE "web"."bundle_detachment" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."bundle_subscription" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."channel_member" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."channel_optout" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."device" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."device_auth_session" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."device_bundle_state" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."device_exclusion" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
ALTER TABLE "web"."device_link" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
DROP TABLE "web"."bundle_detachment" CASCADE;--> statement-breakpoint
DROP TABLE "web"."bundle_subscription" CASCADE;--> statement-breakpoint
DROP TABLE "web"."channel_member" CASCADE;--> statement-breakpoint
DROP TABLE "web"."channel_optout" CASCADE;--> statement-breakpoint
DROP TABLE "web"."device" CASCADE;--> statement-breakpoint
DROP TABLE "web"."device_auth_session" CASCADE;--> statement-breakpoint
DROP TABLE "web"."device_bundle_state" CASCADE;--> statement-breakpoint
DROP TABLE "web"."device_exclusion" CASCADE;--> statement-breakpoint
DROP TABLE "web"."device_link" CASCADE;--> statement-breakpoint
ALTER TABLE "web"."workspace" DROP CONSTRAINT "workspace_device_approval_check";--> statement-breakpoint
-- Already gone: DROP TABLE "web"."device" CASCADE above took these dependent FKs with it.
ALTER TABLE "web"."audit_event" DROP CONSTRAINT IF EXISTS "audit_event_actor_device_id_device_id_fk";
--> statement-breakpoint
ALTER TABLE "web"."op_receipt" DROP CONSTRAINT IF EXISTS "op_receipt_device_id_device_id_fk";
--> statement-breakpoint
DROP INDEX "web"."audit_actor_device";--> statement-breakpoint
-- Hand-adjusted from the generated output: op_receipt rows are short-retention idempotency
-- slots keyed by the acting principal; the principal table is being replaced wholesale, so the
-- slots are purged (losing a replay window, never data) and the column ordering fixed — the
-- generated SQL added the new PK before the column it names existed.
DELETE FROM "web"."op_receipt";--> statement-breakpoint
ALTER TABLE "web"."op_receipt" DROP CONSTRAINT "op_receipt_workspace_id_device_id_op_id_pk";--> statement-breakpoint
ALTER TABLE "web"."audit_event" ADD COLUMN "actor_session_id" text;--> statement-breakpoint
ALTER TABLE "web"."op_receipt" ADD COLUMN "session_id" text NOT NULL;--> statement-breakpoint
ALTER TABLE "web"."op_receipt" ADD CONSTRAINT "op_receipt_workspace_id_session_id_op_id_pk" PRIMARY KEY("workspace_id","session_id","op_id");--> statement-breakpoint
-- The device table's revocation-finality trigger died with the table; sessions are DELETED,
-- never revoked-in-place, so the guard function has no successor. Drop the orphan.
DROP FUNCTION IF EXISTS "web"."revocation_is_final"();--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD COLUMN "session_approval" text DEFAULT 'off' NOT NULL;--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD COLUMN "session_max_age_ms" bigint;--> statement-breakpoint
ALTER TABLE "web"."bundle_upstream" ADD CONSTRAINT "bundle_upstream_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."cli_session" ADD CONSTRAINT "cli_session_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."login_flow" ADD CONSTRAINT "login_flow_approved_by_user_id_fk" FOREIGN KEY ("approved_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."login_flow" ADD CONSTRAINT "login_flow_session_id_cli_session_id_fk" FOREIGN KEY ("session_id") REFERENCES "web"."cli_session"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."profile_entry" ADD CONSTRAINT "profile_entry_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."profile_entry" ADD CONSTRAINT "profile_entry_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."profile_entry" ADD CONSTRAINT "profile_entry_channel_fk" FOREIGN KEY ("channel_id","workspace_id") REFERENCES "web"."channel"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."session_bundle_state" ADD CONSTRAINT "session_bundle_state_session_id_cli_session_id_fk" FOREIGN KEY ("session_id") REFERENCES "web"."cli_session"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."session_bundle_state" ADD CONSTRAINT "session_bundle_state_bundle_id_bundle_id_fk" FOREIGN KEY ("bundle_id") REFERENCES "web"."bundle"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."version_upstream" ADD CONSTRAINT "version_upstream_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "bundle_upstream_ws_idx" ON "web"."bundle_upstream" USING btree ("workspace_id");--> statement-breakpoint
CREATE INDEX "cli_session_workspace_idx" ON "web"."cli_session" USING btree ("workspace_id");--> statement-breakpoint
CREATE INDEX "cli_session_user_idx" ON "web"."cli_session" USING btree ("user_id");--> statement-breakpoint
CREATE UNIQUE INDEX "login_flow_live_code" ON "web"."login_flow" USING btree ("user_code") WHERE status = 'pending';--> statement-breakpoint
CREATE INDEX "login_flow_expires_idx" ON "web"."login_flow" USING btree ("expires_at");--> statement-breakpoint
CREATE UNIQUE INDEX "profile_entry_bundle_once" ON "web"."profile_entry" USING btree ("user_id","bundle_id") WHERE bundle_id is not null;--> statement-breakpoint
CREATE UNIQUE INDEX "profile_entry_channel_once" ON "web"."profile_entry" USING btree ("user_id","channel_id") WHERE channel_id is not null;--> statement-breakpoint
CREATE INDEX "profile_entry_ws_user_idx" ON "web"."profile_entry" USING btree ("workspace_id","user_id");--> statement-breakpoint
CREATE INDEX "profile_entry_bundle_idx" ON "web"."profile_entry" USING btree ("bundle_id");--> statement-breakpoint
CREATE INDEX "session_bundle_state_bundle_idx" ON "web"."session_bundle_state" USING btree ("bundle_id");--> statement-breakpoint
ALTER TABLE "web"."audit_event" ADD CONSTRAINT "audit_event_actor_session_id_cli_session_id_fk" FOREIGN KEY ("actor_session_id") REFERENCES "web"."cli_session"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."op_receipt" ADD CONSTRAINT "op_receipt_session_id_cli_session_id_fk" FOREIGN KEY ("session_id") REFERENCES "web"."cli_session"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "audit_actor_session" ON "web"."audit_event" USING btree ("actor_session_id") WHERE actor_session_id is not null;--> statement-breakpoint
ALTER TABLE "web"."audit_event" DROP COLUMN "actor_device_id";--> statement-breakpoint
ALTER TABLE "web"."op_receipt" DROP COLUMN "device_id";--> statement-breakpoint
ALTER TABLE "web"."workspace" DROP COLUMN "device_approval";--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD CONSTRAINT "workspace_session_approval_check" CHECK ("web"."workspace"."session_approval" in ('off', 'on'));--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD CONSTRAINT "workspace_session_max_age_check" CHECK ("web"."workspace"."session_max_age_ms" is null or "web"."workspace"."session_max_age_ms" > 0);