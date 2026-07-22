CREATE TABLE "web"."mail_event" (
	"id" bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY (sequence name "web"."mail_event_id_seq" INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1 CACHE 1),
	"kind" text NOT NULL,
	"recipient" text NOT NULL,
	"outcome" text NOT NULL,
	"code" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "mail_event_kind_check" CHECK ("web"."mail_event"."kind" in ('magic-link', 'invite', 'auth-verify', 'auth-reset')),
	CONSTRAINT "mail_event_outcome_check" CHECK ("web"."mail_event"."outcome" in ('ok', 'failed')),
	CONSTRAINT "mail_event_code_check" CHECK ("web"."mail_event"."code" is null or "web"."mail_event"."code" in ('unconfigured', 'send_failed')),
	CONSTRAINT "mail_event_code_on_failure_check" CHECK ("web"."mail_event"."outcome" = 'failed' or "web"."mail_event"."code" is null)
);
--> statement-breakpoint
CREATE INDEX "mail_event_time_idx" ON "web"."mail_event" USING btree ("created_at");