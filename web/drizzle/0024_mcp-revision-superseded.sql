-- A REVISION MAY BE SUPERSEDED — overtaken, not decided against.
--
-- Accepting one version of a server publishes it and, in the same transaction, moves the server's
-- OTHER pending candidate revisions out of the queue. They were never rejected — nobody looked at
-- them and said no — so reusing `rejected` would put words in a person's mouth. `superseded` is the
-- honest terminal state: neither published nor a candidate, so the feed, the versions history and
-- delivery (all of which filter on `published`/`candidate`) skip it, while the staff history still
-- shows it as superseded. This widens the status CHECK to admit it.
ALTER TABLE "web"."mcp_server_revision" DROP CONSTRAINT "mcp_server_revision_status_check";--> statement-breakpoint
ALTER TABLE "web"."mcp_server_revision" ADD CONSTRAINT "mcp_server_revision_status_check" CHECK ("web"."mcp_server_revision"."status" in ('candidate', 'published', 'rejected', 'revoked', 'superseded'));