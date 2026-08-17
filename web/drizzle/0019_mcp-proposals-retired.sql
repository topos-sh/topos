-- NO MORE WORKSPACE REVIEW OF AN MCP SERVER. A server's versions are decided in the catalog now —
-- staff publish global ones, an owner's save publishes a private one — so the proposal path for
-- the kind has nothing left to decide. Every OPEN proposal against a `kind: 'mcp'` bundle is
-- deleted here, with no ceremony and no notice: there is no surface left to resolve it on, and a
-- row nobody can act on is worse than no row. Resolved proposals stay exactly where they are —
-- they are history, and history is not this migration's to rewrite.
--
-- Approval rows go with them by their own cascade.

DELETE FROM "web"."proposal" p
USING "web"."bundle" b
WHERE b.id = p.bundle_id AND b.kind = 'mcp' AND p.status = 'open';
