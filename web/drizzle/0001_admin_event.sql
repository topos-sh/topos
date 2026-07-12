CREATE TABLE "admin_event" (
	"id" uuid PRIMARY KEY DEFAULT gen_random_uuid() NOT NULL,
	"workspace_id" text NOT NULL,
	"kind" text NOT NULL,
	"subject" text NOT NULL,
	"detail" text,
	"set_by" text NOT NULL,
	"set_at" timestamp with time zone DEFAULT now() NOT NULL,
	"outcome" text NOT NULL,
	CONSTRAINT "admin_event_outcome_check" CHECK ("admin_event"."outcome" in ('ok', 'denied', 'error'))
);
--> statement-breakpoint
CREATE INDEX "admin_event_workspace_idx" ON "admin_event" USING btree ("workspace_id","set_at");