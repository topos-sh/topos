-- A CALL WITH NO PERSON BEHIND IT. The gateway now serves a second kind of caller: a workspace
-- MACHINE TOKEN (CI, a VM, a sandbox), which is not a person and never resolves to one — it holds
-- the workspace's own credential so a build machine needs no vendor secret of its own.
--
-- `user_id NOT NULL` was written when a session was always somebody's. Left standing, the first
-- machine call would fail the whole buffered usage flush (one INSERT per batch, so every other
-- workspace's rows in that batch would be dropped with it), and the alternative — writing some
-- stand-in id — would put a person's shape on a row that has no person. NULL is the honest value,
-- and the web's usage table reads it as the machine it was.
ALTER TABLE gateway.usage_event ALTER COLUMN user_id DROP NOT NULL;
