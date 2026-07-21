ALTER TABLE "web"."invitation" DROP CONSTRAINT "invitation_status_check";--> statement-breakpoint
ALTER TABLE "web"."device_auth_session" ADD COLUMN "invite_token_sha256" "bytea";--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD COLUMN "token_sha256" "bytea";--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD COLUMN "hint_bundle_id" text;--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD COLUMN "hint_channel_id" text;--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_token_sha256_unique" UNIQUE("token_sha256");--> statement-breakpoint
ALTER TABLE "web"."device_auth_session" ADD CONSTRAINT "device_auth_session_invite_token_sha256_check" CHECK ("web"."device_auth_session"."invite_token_sha256" is null or octet_length("web"."device_auth_session"."invite_token_sha256") = 32);--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_token_sha256_check" CHECK ("web"."invitation"."token_sha256" is null or octet_length("web"."invitation"."token_sha256") = 32);--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_hint_one_check" CHECK ("web"."invitation"."hint_bundle_id" is null or "web"."invitation"."hint_channel_id" is null);--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_status_check" CHECK ("web"."invitation"."status" in ('pending', 'accepted', 'revoked', 'declined'));--> statement-breakpoint
-- Workspace coherence for the hint references, hand-appended (drizzle-kit cannot express a
-- per-column SET NULL): the composite FK pins the hinted bundle/channel to the invitation's
-- OWN workspace, and deleting the hinted thing clears ONLY the hint column — the invitation
-- (and its NOT NULL workspace_id) survive. MATCH SIMPLE semantics: a NULL hint disarms the FK.
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_hint_bundle_fk"
  FOREIGN KEY ("hint_bundle_id","workspace_id")
  REFERENCES "web"."bundle"("id","workspace_id")
  ON DELETE SET NULL ("hint_bundle_id");--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_hint_channel_fk"
  FOREIGN KEY ("hint_channel_id","workspace_id")
  REFERENCES "web"."channel"("id","workspace_id")
  ON DELETE SET NULL ("hint_channel_id");
