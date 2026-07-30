CREATE TABLE "web"."assignment" (
	"workspace_id" text NOT NULL,
	"user_id" text,
	"bundle_id" text,
	"channel_id" text,
	"created_by" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "assignment_target_check" CHECK (("web"."assignment"."bundle_id" is null) <> ("web"."assignment"."channel_id" is null))
);
--> statement-breakpoint
CREATE TABLE "web"."decline" (
	"workspace_id" text NOT NULL,
	"user_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "decline_user_id_bundle_id_unique" UNIQUE("user_id","bundle_id")
);
--> statement-breakpoint
ALTER TABLE "web"."assignment" ADD CONSTRAINT "assignment_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."assignment" ADD CONSTRAINT "assignment_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."assignment" ADD CONSTRAINT "assignment_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."assignment" ADD CONSTRAINT "assignment_channel_fk" FOREIGN KEY ("channel_id","workspace_id") REFERENCES "web"."channel"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."decline" ADD CONSTRAINT "decline_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."decline" ADD CONSTRAINT "decline_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE UNIQUE INDEX "assignment_person_bundle_once" ON "web"."assignment" USING btree ("workspace_id","user_id","bundle_id") WHERE bundle_id is not null and user_id is not null;--> statement-breakpoint
CREATE UNIQUE INDEX "assignment_everyone_bundle_once" ON "web"."assignment" USING btree ("workspace_id","bundle_id") WHERE bundle_id is not null and user_id is null;--> statement-breakpoint
CREATE UNIQUE INDEX "assignment_person_channel_once" ON "web"."assignment" USING btree ("workspace_id","user_id","channel_id") WHERE channel_id is not null and user_id is not null;--> statement-breakpoint
CREATE UNIQUE INDEX "assignment_everyone_channel_once" ON "web"."assignment" USING btree ("workspace_id","channel_id") WHERE channel_id is not null and user_id is null;--> statement-breakpoint
CREATE INDEX "assignment_ws_user_idx" ON "web"."assignment" USING btree ("workspace_id","user_id");--> statement-breakpoint
CREATE INDEX "assignment_bundle_idx" ON "web"."assignment" USING btree ("bundle_id");--> statement-breakpoint
CREATE INDEX "assignment_channel_idx" ON "web"."assignment" USING btree ("channel_id");--> statement-breakpoint
CREATE INDEX "decline_ws_user_idx" ON "web"."decline" USING btree ("workspace_id","user_id");--> statement-breakpoint
-- ── The carry-over: the baseline becomes a row, then every standing stance moves ────────────
--
-- Hand-written from here (drizzle-kit diffs shapes, not data). Order matters: the baseline
-- assignment first, then the person-side stances, then the old table goes.
--
-- 1. THE BASELINE IS NOW A ROW. Every workspace's default channel becomes an assignment to
--    EVERYONE — what used to be a rule inside the delivery query. `created_by` records the
--    workspace's earliest owner (the deterministic choice: the owner seat that has existed
--    longest, tie-broken by user id); a workspace with no owner seat yet — a boot-minted one
--    still awaiting its claim — records 'system', the attribution its birth audit row carries.
INSERT INTO "web"."assignment" ("workspace_id", "user_id", "channel_id", "created_by")
SELECT c."workspace_id", NULL, c."id",
       COALESCE((SELECT s."user_id" FROM "web"."seat" s
                 WHERE s."workspace_id" = c."workspace_id" AND s."role" = 'owner'
                 ORDER BY s."created_at", s."user_id"
                 LIMIT 1), 'system')
FROM "web"."channel" c
WHERE c."is_default";--> statement-breakpoint
-- 2. An INCLUDE of a bundle was the person asking for it themselves — a self-assignment.
--    Pins are dropped: version pinning is a machine-local decision now, so the server keeps no
--    pin column and delivery serves `current`.
INSERT INTO "web"."assignment" ("workspace_id", "user_id", "bundle_id", "created_by", "created_at")
SELECT p."workspace_id", p."user_id", p."bundle_id", p."user_id", p."created_at"
FROM "web"."profile_entry" p
WHERE p."mode" = 'include' AND p."bundle_id" IS NOT NULL
ON CONFLICT DO NOTHING;--> statement-breakpoint
-- 3. An INCLUDE of a channel, likewise.
INSERT INTO "web"."assignment" ("workspace_id", "user_id", "channel_id", "created_by", "created_at")
SELECT p."workspace_id", p."user_id", p."channel_id", p."user_id", p."created_at"
FROM "web"."profile_entry" p
WHERE p."mode" = 'include' AND p."channel_id" IS NOT NULL
ON CONFLICT DO NOTHING;--> statement-breakpoint
-- 4. An EXCLUDE of a bundle is a decline, one for one.
INSERT INTO "web"."decline" ("workspace_id", "user_id", "bundle_id", "created_at")
SELECT p."workspace_id", p."user_id", p."bundle_id", p."created_at"
FROM "web"."profile_entry" p
WHERE p."mode" = 'exclude' AND p."bundle_id" IS NOT NULL
ON CONFLICT DO NOTHING;--> statement-breakpoint
-- 5. An EXCLUDE of a CHANNEL (only ever the default one — the old baseline opt-out) has no
--    successor: declines are per-bundle, so the stance fans out into a decline of each bundle
--    the set carries TODAY. That is the honest reading — "not these" — and it means a bundle
--    added to the set later does arrive, which is the point of dropping channel-level
--    negatives.
INSERT INTO "web"."decline" ("workspace_id", "user_id", "bundle_id", "created_at")
SELECT p."workspace_id", p."user_id", cb."bundle_id", p."created_at"
FROM "web"."profile_entry" p
JOIN "web"."channel_bundle" cb ON cb."channel_id" = p."channel_id"
WHERE p."mode" = 'exclude' AND p."channel_id" IS NOT NULL
ON CONFLICT DO NOTHING;--> statement-breakpoint
DROP TABLE "web"."profile_entry" CASCADE;
