-- AUTHORSHIP IS PER VERSION. `device_owner` alone could not carry it: a machine id is not a
-- person, so one laptop signing in as someone else re-pointed the mapping and relabelled every
-- version that machine had ever published. `version_author` records the acting person against the
-- one version they published, written in the same transaction as the accepted write and never
-- rewritten.
--
-- `device_owner` stays as the fallback for versions written before this table, and becomes what it
-- always should have been: an append-only OBSERVATION. The person joins its key, so a second
-- person on the same machine adds a row instead of taking the first one's place — a device with
-- two rows names nobody, and its pre-table versions keep showing the id they were signed with.
-- `last_seen_at` goes with the updates that used to move it.

CREATE TABLE "web"."version_author" (
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"version_id" text NOT NULL,
	"user_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "version_author_bundle_id_version_id_pk" PRIMARY KEY("bundle_id","version_id")
);
--> statement-breakpoint
ALTER TABLE "web"."device_owner" DROP CONSTRAINT "device_owner_workspace_id_device_id_pk";--> statement-breakpoint
ALTER TABLE "web"."device_owner" ADD CONSTRAINT "device_owner_workspace_id_device_id_user_id_pk" PRIMARY KEY("workspace_id","device_id","user_id");--> statement-breakpoint
ALTER TABLE "web"."version_author" ADD CONSTRAINT "version_author_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."version_author" ADD CONSTRAINT "version_author_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "version_author_user_idx" ON "web"."version_author" USING btree ("user_id");--> statement-breakpoint
ALTER TABLE "web"."device_owner" DROP COLUMN "last_seen_at";