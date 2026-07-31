ALTER TABLE "web"."login_flow" DROP CONSTRAINT "login_flow_auth_code_sha256_check";--> statement-breakpoint
ALTER TABLE "web"."login_flow" DROP CONSTRAINT "login_flow_auth_code_binding_check";--> statement-breakpoint
ALTER TABLE "web"."login_flow" DROP CONSTRAINT "login_flow_approved_check";--> statement-breakpoint
ALTER TABLE "web"."audit_event" ALTER COLUMN "workspace_id" DROP NOT NULL;--> statement-breakpoint
ALTER TABLE "web"."login_flow" ADD COLUMN "preselect_workspace" text;--> statement-breakpoint
ALTER TABLE "web"."login_flow" DROP COLUMN "requested_workspace";--> statement-breakpoint
ALTER TABLE "web"."login_flow" DROP COLUMN "auth_code_sha256";