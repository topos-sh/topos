-- The web-session roster leg — schema for the session-authorized membership ops a hosted
-- composition drives (invite / remove / rotate-the-standing-door / read-roster).
-- Appended (never edits an applied migration's bytes — sqlx checksums the file). Same posture as
-- 0001-0008: workspace-scoped rows, opaque credentials stored ONLY as sha256, BIGINT counters,
-- audit `created_at` TEXT ISO-8601.

-- The WORKSPACE gains the standing-door epoch: the membership-door invite token derives
-- deterministically from (secret, workspace_id, link_epoch), so the door is re-showable at any
-- time without ever storing a plaintext token. "Reset link" = bump the epoch (revoking the prior
-- door's invite row); epoch 0 is the birth state, where a create-page-born workspace's door is
-- its genesis self-invite (re-derivable through genesis_requests) until the first rotation.
ALTER TABLE workspace ADD COLUMN link_epoch BIGINT NOT NULL DEFAULT 0;

-- WORKSPACE EVENTS gain the issuance-method discriminant: which LEG acted — a device-signed
-- governance op ('device_signed', where `actor` is the signing device key id) or a web-session
-- roster op ('web_session', where `actor` is the acting principal's verified email). Existing
-- rows are all device-signed (the session leg did not exist before this migration); new writes
-- set the column explicitly on both legs.
ALTER TABLE workspace_events ADD COLUMN method TEXT NOT NULL DEFAULT 'device_signed'
    CHECK (method IN ('device_signed', 'web_session'));

-- The genesis-door lookup (resolve a workspace's create-time self-invite for the door family)
-- reads genesis_requests BY WORKSPACE — without an index that is a plane-wide sequential scan,
-- and under SERIALIZABLE a seq scan takes a relation-level predicate lock that would make every
-- epoch-0 session op conflict with every concurrent create_workspace.
CREATE INDEX genesis_requests_by_ws ON genesis_requests (workspace_id);
