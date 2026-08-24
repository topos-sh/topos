-- The de-git columns: a version row carries its git commit LOCATOR (the 20-byte OID of the commit
-- object in the object store) and its MESSAGE, so log/version reads answer from Postgres + the
-- store with no ref set anywhere. Additive and NULLABLE: rows written before the object-store
-- import have neither (both lived in the per-workspace bare repo's refs + commit objects); reads
-- that need them FAIL CLOSED on NULL with a typed error naming `topos-plane import-local`, and the
-- import backfills both from the old repo's `refs/topos/versions/*`. Every new write fills both.
ALTER TABLE version
  ADD COLUMN git_commit_oid bytea CHECK (octet_length(git_commit_oid) = 20),
  ADD COLUMN message text;
