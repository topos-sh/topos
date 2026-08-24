-- WHICH PERSON A MACHINE PUBLISHES AS. A version's author is part of its identity — the client
-- derives the version id from (parents, tree, author, message) and verifies the server landed
-- exactly that id — so the commit frame carries the machine's own `d_…` id and can never be
-- rewritten into a name. The session that sent the candidate knows the person, so the pairing is
-- written down here and resolved at render time. One row per (workspace, machine); it outlives
-- the session that taught it, and dies with the workspace or the person.

CREATE TABLE "web"."device_owner" (
	"workspace_id" text NOT NULL,
	"device_id" text NOT NULL,
	"user_id" text NOT NULL,
	"first_seen_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_seen_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "device_owner_workspace_id_device_id_pk" PRIMARY KEY("workspace_id","device_id")
);
--> statement-breakpoint
ALTER TABLE "web"."device_owner" ADD CONSTRAINT "device_owner_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_owner" ADD CONSTRAINT "device_owner_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "device_owner_user_idx" ON "web"."device_owner" USING btree ("user_id");