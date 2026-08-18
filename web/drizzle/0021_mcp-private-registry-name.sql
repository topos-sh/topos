-- ONE NAME, ONE SERVER, INSIDE A WORKSPACE TOO.
--
-- A workspace's registry lane answers `…/servers/{name}` over the servers it runs, and its own
-- private rows are addressed by exactly that name. Two of them under one name would make the
-- lookup a coin flip — the same ambiguity the global partial unique exists to prevent, one scope
-- down. The index is partial for the same reason that one is: a private name is nobody else's
-- business, so it is unique WITHIN the workspace and free everywhere else.
--
-- Nothing existing can violate it: private rows are minted one per create, and the door above
-- this now refuses the second one in the workspace's own words.

CREATE UNIQUE INDEX "mcp_server_private_registry_name" ON "web"."mcp_server" USING btree ("workspace_id","registry_name") WHERE workspace_id is not null;