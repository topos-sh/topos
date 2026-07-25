ALTER TABLE "web"."login_flow" ADD COLUMN "binding" text DEFAULT 'device' NOT NULL;--> statement-breakpoint
ALTER TABLE "web"."login_flow" ADD COLUMN "auth_code_sha256" "bytea";--> statement-breakpoint
ALTER TABLE "web"."login_flow" ADD CONSTRAINT "login_flow_binding_check" CHECK ("web"."login_flow"."binding" in ('device', 'loopback'));--> statement-breakpoint
ALTER TABLE "web"."login_flow" ADD CONSTRAINT "login_flow_auth_code_sha256_check" CHECK ("web"."login_flow"."auth_code_sha256" is null or octet_length("web"."login_flow"."auth_code_sha256") = 32);--> statement-breakpoint
ALTER TABLE "web"."login_flow" ADD CONSTRAINT "login_flow_auth_code_binding_check" CHECK ("web"."login_flow"."auth_code_sha256" is null or "web"."login_flow"."binding" = 'loopback');