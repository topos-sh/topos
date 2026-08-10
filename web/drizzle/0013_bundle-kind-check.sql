DO $$
DECLARE
	found text;
BEGIN
	SELECT string_agg(DISTINCT quote_literal("kind"), ', ' ORDER BY quote_literal("kind"))
	INTO found
	FROM "web"."bundle"
	WHERE "kind" NOT IN ('skill', 'mcp');
	IF found IS NOT NULL THEN
		RAISE EXCEPTION 'web.bundle holds bundle kinds this release does not define: %', found
			USING HINT = 'The kind vocabulary is closed to ''skill'' and ''mcp''. A bundle''s kind is birth metadata, so this migration refuses rather than rewrite one: settle each row by hand -- DELETE the bundle, or UPDATE its kind to a defined one -- then run the migration again.';
	END IF;
END
$$;
--> statement-breakpoint
ALTER TABLE "web"."bundle" ADD CONSTRAINT "bundle_kind_check" CHECK ("web"."bundle"."kind" in ('skill', 'mcp'));
