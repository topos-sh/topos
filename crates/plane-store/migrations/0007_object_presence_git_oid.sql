-- The version-metadata read resolves a version's tree leaves `git_oid -> object_id` with
-- `WHERE workspace_id = ? AND status = 'present' AND git_oid = ANY(?)`. Without this index that probe
-- falls back to the `(workspace_id, status)` index and re-scans every present row per request — cost that
-- grows with the workspace's lifetime object count instead of the requested version's tree.
CREATE INDEX object_presence_by_git_oid ON object_presence (workspace_id, git_oid);
