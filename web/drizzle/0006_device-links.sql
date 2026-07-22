-- Deliberately NO backfill: a link is only ever born through a ceremony (the /verify approval,
-- the invitation accept, the credentialed link op), so a deployment holding device rows that
-- predate this table re-links through those same ceremonies — pre-release, with no deployment
-- carrying such rows, that set is empty by construction.
CREATE TABLE "web"."device_link" (
	"id" text PRIMARY KEY NOT NULL,
	"device_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"status" text DEFAULT 'pending' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "device_link_device_id_workspace_id_unique" UNIQUE("device_id","workspace_id"),
	CONSTRAINT "device_link_status_check" CHECK ("web"."device_link"."status" in ('pending', 'active'))
);
--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD COLUMN "device_approval" text DEFAULT 'off' NOT NULL;--> statement-breakpoint
ALTER TABLE "web"."device_link" ADD CONSTRAINT "device_link_device_id_device_id_fk" FOREIGN KEY ("device_id") REFERENCES "web"."device"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_link" ADD CONSTRAINT "device_link_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "device_link_workspace_idx" ON "web"."device_link" USING btree ("workspace_id");--> statement-breakpoint
CREATE INDEX "device_link_device_idx" ON "web"."device_link" USING btree ("device_id");--> statement-breakpoint
ALTER TABLE "web"."workspace" ADD CONSTRAINT "workspace_device_approval_check" CHECK ("web"."workspace"."device_approval" in ('off', 'on'));