-- A SERVER WITH NO KNOWN VERSION CARRIES NONE — no fabricated number to fill a column.
--
-- The catalog was seeded from a curated list that held only a URL and a sign-in tier, never a
-- version. The first seed stamped a placeholder `1.0.0` on every one so the column could be NOT
-- NULL, which both broke semver precedence against a real-but-lower upstream version and published
-- a fake version to the world through the feed. `upstream_version` becomes nullable, and the
-- placeholder is undone: null column, no `version` in the stored document.

ALTER TABLE "web"."mcp_server_revision" ALTER COLUMN "upstream_version" DROP NOT NULL;--> statement-breakpoint

-- Undo the seed's fabricated version, in place and only where it was fabricated. The predicate is
-- narrow on purpose: a row is a seed placeholder only when it came FROM the seed, its column reads
-- the placeholder `1.0.0`, AND its document carries that same `1.0.0` — so a registry document is
-- never touched (the fabrication only ever happened in the seed) and an owner's server a person
-- deliberately versioned `1.0.0` is left exactly as they wrote it. Idempotent: once nulled, the
-- `upstream_version = '1.0.0'` clause no longer matches, so a re-run is a no-op.
--
-- The seed documents also declared the official 2025-12-11 `$schema`, which REQUIRES a `version` —
-- so a document that dropped its version while keeping that line would be schema-invalid, and the
-- feed and delivery both serve the document verbatim. These are hand-authored editorial rows, not
-- registry entries, so the honest shape is to make NO schema claim: the `$schema` line and the
-- extracted `schema_version` column come off together with the version.
UPDATE "web"."mcp_server_revision"
SET "upstream_version" = NULL,
    "schema_version" = NULL,
    "document" = "document" - 'version' - '$schema'
WHERE "source" = 'seed'
  AND "upstream_version" = '1.0.0'
  AND "document"->>'version' = '1.0.0';
