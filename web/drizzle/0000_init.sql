-- The one web-lineage migration: the whole app-owned schema, born designed. The schema
-- itself normally pre-exists (the initdb creates it, owned by the app role); IF NOT EXISTS
-- covers scratch databases that run the lineage without the initdb.
CREATE SCHEMA IF NOT EXISTS "web";
--> statement-breakpoint
CREATE TABLE "web"."account" (
	"id" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"provider_id" text NOT NULL,
	"user_id" text NOT NULL,
	"password" text,
	"access_token" text,
	"refresh_token" text,
	"id_token" text,
	"access_token_expires_at" timestamp with time zone,
	"refresh_token_expires_at" timestamp with time zone,
	"scope" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "web"."session" (
	"id" text PRIMARY KEY NOT NULL,
	"token" text NOT NULL,
	"user_id" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	"ip_address" text,
	"user_agent" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "session_token_unique" UNIQUE("token")
);
--> statement-breakpoint
CREATE TABLE "web"."user" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"email" text NOT NULL,
	"email_verified" boolean DEFAULT false NOT NULL,
	"image" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "user_email_unique" UNIQUE("email"),
	CONSTRAINT "user_email_lowercase" CHECK ("web"."user"."email" = lower("web"."user"."email"))
);
--> statement-breakpoint
CREATE TABLE "web"."verification" (
	"id" text PRIMARY KEY NOT NULL,
	"identifier" text NOT NULL,
	"value" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "web"."approval" (
	"proposal_id" text NOT NULL,
	"reviewer" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "approval_proposal_id_reviewer_pk" PRIMARY KEY("proposal_id","reviewer")
);
--> statement-breakpoint
CREATE TABLE "web"."audit_event" (
	"id" bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY (sequence name "web"."audit_event_id_seq" INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1 CACHE 1),
	"workspace_id" text NOT NULL,
	"actor_user_id" text,
	"actor_device_id" text,
	"actor_display" text NOT NULL,
	"kind" text NOT NULL,
	"subject" text,
	"outcome" text NOT NULL,
	"details" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "web"."bundle" (
	"id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"kind" text DEFAULT 'skill' NOT NULL,
	"name" text NOT NULL,
	"display_name" text,
	"status" text DEFAULT 'active' NOT NULL,
	"protection" text,
	"base_name" text,
	"archived_at" timestamp with time zone,
	"deleted_at" timestamp with time zone,
	"created_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "bundle_workspace_id_name_unique" UNIQUE("workspace_id","name"),
	CONSTRAINT "bundle_id_workspace_id_unique" UNIQUE("id","workspace_id"),
	CONSTRAINT "bundle_name_check" CHECK ("web"."bundle"."name" ~ '^[a-z0-9][a-z0-9-]*$' and length("web"."bundle"."name") <= 200),
	CONSTRAINT "bundle_status_check" CHECK ("web"."bundle"."status" in ('active', 'archived', 'deleted')),
	CONSTRAINT "bundle_protection_check" CHECK ("web"."bundle"."protection" is null or "web"."bundle"."protection" in ('open', 'reviewed')),
	CONSTRAINT "bundle_deleted_check" CHECK (("web"."bundle"."status" = 'deleted') = ("web"."bundle"."deleted_at" is not null)),
	CONSTRAINT "bundle_archived_check" CHECK ("web"."bundle"."status" <> 'archived' or "web"."bundle"."archived_at" is not null),
	CONSTRAINT "bundle_base_name_check" CHECK ("web"."bundle"."base_name" is null or "web"."bundle"."status" <> 'active')
);
--> statement-breakpoint
CREATE TABLE "web"."bundle_detachment" (
	"user_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"cause" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "bundle_detachment_user_id_bundle_id_pk" PRIMARY KEY("user_id","bundle_id"),
	CONSTRAINT "bundle_detachment_cause_check" CHECK ("web"."bundle_detachment"."cause" in ('unfollow', 'channel_leave', 'membership_removed'))
);
--> statement-breakpoint
CREATE TABLE "web"."bundle_name_hint" (
	"workspace_id" text NOT NULL,
	"old_name" text NOT NULL,
	"bundle_id" text NOT NULL,
	"renamed_by" text,
	"renamed_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "bundle_name_hint_workspace_id_old_name_pk" PRIMARY KEY("workspace_id","old_name")
);
--> statement-breakpoint
CREATE TABLE "web"."bundle_subscription" (
	"user_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"state" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "bundle_subscription_user_id_bundle_id_pk" PRIMARY KEY("user_id","bundle_id"),
	CONSTRAINT "bundle_subscription_state_check" CHECK ("web"."bundle_subscription"."state" in ('following', 'unfollowed'))
);
--> statement-breakpoint
CREATE TABLE "web"."channel" (
	"id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"name" text NOT NULL,
	"mode" text DEFAULT 'open' NOT NULL,
	"is_default" boolean DEFAULT false NOT NULL,
	"created_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "channel_workspace_id_name_unique" UNIQUE("workspace_id","name"),
	CONSTRAINT "channel_id_workspace_id_unique" UNIQUE("id","workspace_id"),
	CONSTRAINT "channel_mode_check" CHECK ("web"."channel"."mode" in ('open', 'curated'))
);
--> statement-breakpoint
CREATE TABLE "web"."channel_bundle" (
	"channel_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"added_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "channel_bundle_channel_id_bundle_id_pk" PRIMARY KEY("channel_id","bundle_id")
);
--> statement-breakpoint
CREATE TABLE "web"."channel_member" (
	"channel_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"user_id" text NOT NULL,
	"added_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "channel_member_channel_id_user_id_pk" PRIMARY KEY("channel_id","user_id")
);
--> statement-breakpoint
CREATE TABLE "web"."channel_optout" (
	"channel_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"user_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "channel_optout_channel_id_user_id_pk" PRIMARY KEY("channel_id","user_id")
);
--> statement-breakpoint
CREATE TABLE "web"."device" (
	"id" text PRIMARY KEY NOT NULL,
	"user_id" text NOT NULL,
	"display_name" text NOT NULL,
	"credential_sha256" "bytea" NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_seen_at" timestamp with time zone,
	"revoked_at" timestamp with time zone,
	CONSTRAINT "device_credential_sha256_unique" UNIQUE("credential_sha256"),
	CONSTRAINT "device_credential_sha256_check" CHECK (octet_length("web"."device"."credential_sha256") = 32)
);
--> statement-breakpoint
CREATE TABLE "web"."device_auth_session" (
	"id" text PRIMARY KEY NOT NULL,
	"user_code" text NOT NULL,
	"device_code_sha256" "bytea" NOT NULL,
	"requested_name" text NOT NULL,
	"status" text DEFAULT 'pending' NOT NULL,
	"approved_by" text,
	"device_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	CONSTRAINT "device_auth_session_device_code_sha256_unique" UNIQUE("device_code_sha256"),
	CONSTRAINT "device_auth_session_device_code_sha256_check" CHECK (octet_length("web"."device_auth_session"."device_code_sha256") = 32),
	CONSTRAINT "device_auth_session_status_check" CHECK ("web"."device_auth_session"."status" in ('pending', 'approved', 'denied')),
	CONSTRAINT "device_auth_session_approved_check" CHECK ("web"."device_auth_session"."status" <> 'approved' or "web"."device_auth_session"."device_id" is not null)
);
--> statement-breakpoint
CREATE TABLE "web"."device_bundle_state" (
	"device_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"applied_version_id" text NOT NULL,
	"reported_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "device_bundle_state_device_id_bundle_id_pk" PRIMARY KEY("device_id","bundle_id")
);
--> statement-breakpoint
CREATE TABLE "web"."device_exclusion" (
	"device_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "device_exclusion_device_id_bundle_id_pk" PRIMARY KEY("device_id","bundle_id")
);
--> statement-breakpoint
CREATE TABLE "web"."invitation" (
	"id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"email" text NOT NULL,
	"role" text DEFAULT 'member' NOT NULL,
	"status" text DEFAULT 'pending' NOT NULL,
	"invited_by" text,
	"accepted_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"expires_at" timestamp with time zone,
	"accepted_at" timestamp with time zone,
	CONSTRAINT "invitation_email_check" CHECK ("web"."invitation"."email" = lower("web"."invitation"."email")),
	CONSTRAINT "invitation_role_check" CHECK ("web"."invitation"."role" in ('owner', 'reviewer', 'member')),
	CONSTRAINT "invitation_status_check" CHECK ("web"."invitation"."status" in ('pending', 'accepted', 'revoked')),
	CONSTRAINT "invitation_accepted_check" CHECK (("web"."invitation"."status" = 'accepted') = ("web"."invitation"."accepted_at" is not null))
);
--> statement-breakpoint
CREATE TABLE "web"."notice" (
	"id" bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY (sequence name "web"."notice_id_seq" INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1 CACHE 1),
	"user_id" text NOT NULL,
	"workspace_id" text NOT NULL,
	"kind" text NOT NULL,
	"payload" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"acked_at" timestamp with time zone
);
--> statement-breakpoint
CREATE TABLE "web"."op_receipt" (
	"workspace_id" text NOT NULL,
	"device_id" text NOT NULL,
	"op_id" uuid NOT NULL,
	"request_sha256" "bytea" NOT NULL,
	"outcome" jsonb NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "op_receipt_workspace_id_device_id_op_id_pk" PRIMARY KEY("workspace_id","device_id","op_id"),
	CONSTRAINT "op_receipt_request_sha256_check" CHECK (octet_length("web"."op_receipt"."request_sha256") = 32)
);
--> statement-breakpoint
CREATE TABLE "web"."proposal" (
	"id" text PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"candidate_version_id" text NOT NULL,
	"proposed_by" text,
	"status" text DEFAULT 'open' NOT NULL,
	"resolved_by" text,
	"resolved_reason" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"resolved_at" timestamp with time zone,
	CONSTRAINT "proposal_status_check" CHECK ("web"."proposal"."status" in ('open', 'approved', 'rejected', 'withdrawn')),
	CONSTRAINT "proposal_resolved_check" CHECK (("web"."proposal"."status" = 'open') = ("web"."proposal"."resolved_at" is null))
);
--> statement-breakpoint
CREATE TABLE "web"."proposal_comment" (
	"id" uuid PRIMARY KEY NOT NULL,
	"workspace_id" text NOT NULL,
	"bundle_id" text NOT NULL,
	"version_id" text NOT NULL,
	"author_user_id" text,
	"author_display" text NOT NULL,
	"body" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "proposal_comment_body_check" CHECK (char_length("web"."proposal_comment"."body") between 1 and 4000)
);
--> statement-breakpoint
CREATE TABLE "web"."seat" (
	"workspace_id" text NOT NULL,
	"user_id" text NOT NULL,
	"role" text NOT NULL,
	"invited_by" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "seat_workspace_id_user_id_pk" PRIMARY KEY("workspace_id","user_id"),
	CONSTRAINT "seat_role_check" CHECK ("web"."seat"."role" in ('owner', 'reviewer', 'member'))
);
--> statement-breakpoint
CREATE TABLE "web"."workspace" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"display_name" text NOT NULL,
	"claim_code_sha256" "bytea",
	"claimed_at" timestamp with time zone,
	"invite_policy" text DEFAULT 'members' NOT NULL,
	"protection_default" text DEFAULT 'open' NOT NULL,
	"staleness_window_ms" bigint DEFAULT 604800000 NOT NULL,
	"registration" text DEFAULT 'invite_only' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "workspace_name_unique" UNIQUE("name"),
	CONSTRAINT "workspace_name_check" CHECK ("web"."workspace"."name" ~ '^[a-z0-9][a-z0-9-]*$' and length("web"."workspace"."name") <= 100),
	CONSTRAINT "workspace_claim_code_sha256_check" CHECK ("web"."workspace"."claim_code_sha256" is null or octet_length("web"."workspace"."claim_code_sha256") = 32),
	CONSTRAINT "workspace_invite_policy_check" CHECK ("web"."workspace"."invite_policy" in ('members', 'owners')),
	CONSTRAINT "workspace_protection_default_check" CHECK ("web"."workspace"."protection_default" in ('open', 'reviewed')),
	CONSTRAINT "workspace_registration_check" CHECK ("web"."workspace"."registration" in ('invite_only', 'open')),
	CONSTRAINT "workspace_claim_state_check" CHECK (("web"."workspace"."claimed_at" is null) <> ("web"."workspace"."claim_code_sha256" is null))
);
--> statement-breakpoint
ALTER TABLE "web"."account" ADD CONSTRAINT "account_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."session" ADD CONSTRAINT "session_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."approval" ADD CONSTRAINT "approval_proposal_id_proposal_id_fk" FOREIGN KEY ("proposal_id") REFERENCES "web"."proposal"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."approval" ADD CONSTRAINT "approval_reviewer_user_id_fk" FOREIGN KEY ("reviewer") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."audit_event" ADD CONSTRAINT "audit_event_actor_user_id_user_id_fk" FOREIGN KEY ("actor_user_id") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."audit_event" ADD CONSTRAINT "audit_event_actor_device_id_device_id_fk" FOREIGN KEY ("actor_device_id") REFERENCES "web"."device"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle" ADD CONSTRAINT "bundle_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle" ADD CONSTRAINT "bundle_created_by_user_id_fk" FOREIGN KEY ("created_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_detachment" ADD CONSTRAINT "bundle_detachment_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_detachment" ADD CONSTRAINT "bundle_detachment_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_name_hint" ADD CONSTRAINT "bundle_name_hint_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_name_hint" ADD CONSTRAINT "bundle_name_hint_bundle_id_bundle_id_fk" FOREIGN KEY ("bundle_id") REFERENCES "web"."bundle"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_name_hint" ADD CONSTRAINT "bundle_name_hint_renamed_by_user_id_fk" FOREIGN KEY ("renamed_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_subscription" ADD CONSTRAINT "bundle_subscription_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."bundle_subscription" ADD CONSTRAINT "bundle_subscription_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel" ADD CONSTRAINT "channel_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel" ADD CONSTRAINT "channel_created_by_user_id_fk" FOREIGN KEY ("created_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_bundle" ADD CONSTRAINT "channel_bundle_added_by_user_id_fk" FOREIGN KEY ("added_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_bundle" ADD CONSTRAINT "channel_bundle_channel_fk" FOREIGN KEY ("channel_id","workspace_id") REFERENCES "web"."channel"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_bundle" ADD CONSTRAINT "channel_bundle_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_member" ADD CONSTRAINT "channel_member_added_by_user_id_fk" FOREIGN KEY ("added_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_member" ADD CONSTRAINT "channel_member_channel_fk" FOREIGN KEY ("channel_id","workspace_id") REFERENCES "web"."channel"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_member" ADD CONSTRAINT "channel_member_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_optout" ADD CONSTRAINT "channel_optout_channel_fk" FOREIGN KEY ("channel_id","workspace_id") REFERENCES "web"."channel"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."channel_optout" ADD CONSTRAINT "channel_optout_seat_fk" FOREIGN KEY ("workspace_id","user_id") REFERENCES "web"."seat"("workspace_id","user_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device" ADD CONSTRAINT "device_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_auth_session" ADD CONSTRAINT "device_auth_session_approved_by_user_id_fk" FOREIGN KEY ("approved_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_auth_session" ADD CONSTRAINT "device_auth_session_device_id_device_id_fk" FOREIGN KEY ("device_id") REFERENCES "web"."device"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_bundle_state" ADD CONSTRAINT "device_bundle_state_device_id_device_id_fk" FOREIGN KEY ("device_id") REFERENCES "web"."device"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_bundle_state" ADD CONSTRAINT "device_bundle_state_bundle_id_bundle_id_fk" FOREIGN KEY ("bundle_id") REFERENCES "web"."bundle"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_exclusion" ADD CONSTRAINT "device_exclusion_device_id_device_id_fk" FOREIGN KEY ("device_id") REFERENCES "web"."device"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."device_exclusion" ADD CONSTRAINT "device_exclusion_bundle_id_bundle_id_fk" FOREIGN KEY ("bundle_id") REFERENCES "web"."bundle"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_invited_by_user_id_fk" FOREIGN KEY ("invited_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."invitation" ADD CONSTRAINT "invitation_accepted_by_user_id_fk" FOREIGN KEY ("accepted_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."notice" ADD CONSTRAINT "notice_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."notice" ADD CONSTRAINT "notice_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."op_receipt" ADD CONSTRAINT "op_receipt_device_id_device_id_fk" FOREIGN KEY ("device_id") REFERENCES "web"."device"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."proposal" ADD CONSTRAINT "proposal_proposed_by_user_id_fk" FOREIGN KEY ("proposed_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."proposal" ADD CONSTRAINT "proposal_resolved_by_user_id_fk" FOREIGN KEY ("resolved_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."proposal" ADD CONSTRAINT "proposal_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."proposal_comment" ADD CONSTRAINT "proposal_comment_author_user_id_user_id_fk" FOREIGN KEY ("author_user_id") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."proposal_comment" ADD CONSTRAINT "proposal_comment_bundle_fk" FOREIGN KEY ("bundle_id","workspace_id") REFERENCES "web"."bundle"("id","workspace_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."seat" ADD CONSTRAINT "seat_workspace_id_workspace_id_fk" FOREIGN KEY ("workspace_id") REFERENCES "web"."workspace"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."seat" ADD CONSTRAINT "seat_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "web"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "web"."seat" ADD CONSTRAINT "seat_invited_by_user_id_fk" FOREIGN KEY ("invited_by") REFERENCES "web"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "account_user_idx" ON "web"."account" USING btree ("user_id");--> statement-breakpoint
CREATE INDEX "session_user_idx" ON "web"."session" USING btree ("user_id");--> statement-breakpoint
CREATE INDEX "session_expires_idx" ON "web"."session" USING btree ("expires_at");--> statement-breakpoint
CREATE INDEX "verification_identifier_idx" ON "web"."verification" USING btree ("identifier");--> statement-breakpoint
CREATE INDEX "verification_expires_idx" ON "web"."verification" USING btree ("expires_at");--> statement-breakpoint
CREATE INDEX "approval_reviewer_idx" ON "web"."approval" USING btree ("reviewer");--> statement-breakpoint
CREATE INDEX "audit_ws_time" ON "web"."audit_event" USING btree ("workspace_id","created_at");--> statement-breakpoint
CREATE INDEX "audit_actor_user" ON "web"."audit_event" USING btree ("actor_user_id") WHERE actor_user_id is not null;--> statement-breakpoint
CREATE INDEX "audit_actor_device" ON "web"."audit_event" USING btree ("actor_device_id") WHERE actor_device_id is not null;--> statement-breakpoint
CREATE INDEX "bundle_detachment_ws_idx" ON "web"."bundle_detachment" USING btree ("workspace_id","bundle_id");--> statement-breakpoint
CREATE INDEX "bundle_name_hint_bundle_idx" ON "web"."bundle_name_hint" USING btree ("bundle_id");--> statement-breakpoint
CREATE INDEX "bundle_subscription_bundle_idx" ON "web"."bundle_subscription" USING btree ("bundle_id");--> statement-breakpoint
CREATE UNIQUE INDEX "channel_one_default" ON "web"."channel" USING btree ("workspace_id") WHERE is_default;--> statement-breakpoint
CREATE INDEX "channel_bundle_bundle_idx" ON "web"."channel_bundle" USING btree ("bundle_id");--> statement-breakpoint
CREATE INDEX "channel_member_user_idx" ON "web"."channel_member" USING btree ("user_id","workspace_id");--> statement-breakpoint
CREATE INDEX "channel_optout_user_idx" ON "web"."channel_optout" USING btree ("user_id","workspace_id");--> statement-breakpoint
CREATE INDEX "device_user_idx" ON "web"."device" USING btree ("user_id");--> statement-breakpoint
CREATE UNIQUE INDEX "device_auth_live_code" ON "web"."device_auth_session" USING btree ("user_code") WHERE status = 'pending';--> statement-breakpoint
CREATE INDEX "device_auth_expires_idx" ON "web"."device_auth_session" USING btree ("expires_at");--> statement-breakpoint
CREATE INDEX "device_bundle_state_bundle_idx" ON "web"."device_bundle_state" USING btree ("bundle_id");--> statement-breakpoint
CREATE INDEX "device_exclusion_bundle_idx" ON "web"."device_exclusion" USING btree ("bundle_id");--> statement-breakpoint
CREATE UNIQUE INDEX "invitation_pending_once" ON "web"."invitation" USING btree ("email","workspace_id") WHERE status = 'pending';--> statement-breakpoint
CREATE INDEX "notice_inbox" ON "web"."notice" USING btree ("user_id","workspace_id") WHERE acked_at is null;--> statement-breakpoint
CREATE INDEX "notice_ws_idx" ON "web"."notice" USING btree ("workspace_id");--> statement-breakpoint
CREATE INDEX "op_receipt_retention_idx" ON "web"."op_receipt" USING btree ("created_at");--> statement-breakpoint
CREATE INDEX "proposal_open" ON "web"."proposal" USING btree ("workspace_id","bundle_id") WHERE status = 'open';--> statement-breakpoint
CREATE UNIQUE INDEX "proposal_one_open_per_candidate" ON "web"."proposal" USING btree ("workspace_id","bundle_id","candidate_version_id") WHERE status = 'open';--> statement-breakpoint
CREATE INDEX "proposal_comment_thread_idx" ON "web"."proposal_comment" USING btree ("workspace_id","bundle_id","version_id","created_at");--> statement-breakpoint
CREATE INDEX "seat_user_idx" ON "web"."seat" USING btree ("user_id");--> statement-breakpoint
-- Revocation is FINAL — trigger-enforced. A bug-guard: no ordinary app code path can
-- un-revoke a device; rotation is revoke + re-enroll.
CREATE FUNCTION "web"."revocation_is_final"() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'device.revoked_at is set-once: % stays revoked', OLD.id;
END $$;--> statement-breakpoint
CREATE TRIGGER "device_revoke_monotonic"
  BEFORE UPDATE OF "revoked_at" ON "web"."device"
  FOR EACH ROW
  WHEN (OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at)
  EXECUTE FUNCTION "web"."revocation_is_final"();
