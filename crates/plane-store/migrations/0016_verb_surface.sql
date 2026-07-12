-- The adopted verb surface — the directory schema it needs: the workspace ADDRESS (a URL-safe
-- unique name, so every share link is just a resource address), the workspace policy knobs the
-- verbs read (invite policy, the staleness window), person-scoped invitations as plain roster
-- rows (the tokened invite link machinery retires — the roster is the lock, the address is the
-- door), the notices read-state write (ack), and the token-less enrollment session shape
-- (enroll-by-address + a login intent that re-mints a device's workspace credentials).
-- Appended (never edits an applied migration's bytes — sqlx checksums the file).
--
-- Same posture as 0001-0015: every row `workspace_id`-scoped where one exists, opaque credentials
-- stored ONLY as sha256, deadline/mutable times BIGINT epoch MILLISECONDS, audit `created_at`
-- TEXT ISO-8601, principals in the canonical lowercase fold. POLICY LIVES HERE: every policy
-- write with logic is a named `topos_*` SQL function — the single implementation, called by Rust
-- now and by the web tier at the door cutover.

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- WORKSPACE ADDRESS — `name` is the workspace's URL slug (`<origin>/<name>` IS the share link;
-- links carry nothing, the roster is the lock). Unique across the deployment, same charset as
-- skill/channel names. The full reserved-word list (route prefixes, product words, the
-- `-archived-` pattern) is enforced in the creation ops — one Rust home — since new creations all
-- flow through them; the CHECK pins the charset itself.
ALTER TABLE workspace ADD COLUMN name TEXT
    CHECK (name IS NULL OR (name ~ '^[a-z0-9][a-z0-9-]*$' AND length(name) <= 63));

-- Backfill: slugify the display name; a slug that is empty, collides, carries the archived
-- pattern, or sits on a hot reserved word falls back to a stable id-derived name. Deterministic
-- (ordered by workspace_id), no clocks.
DO $$
DECLARE
    ws RECORD;
    v_slug TEXT;
    v_candidate TEXT;
    v_n BIGINT;
BEGIN
    FOR ws IN SELECT workspace_id, display_name FROM workspace ORDER BY workspace_id LOOP
        v_slug := btrim(left(btrim(regexp_replace(lower(COALESCE(ws.display_name, '')),
                                                  '[^a-z0-9-]+', '-', 'g'), '-'), 63), '-');
        IF v_slug IS NULL OR v_slug = ''
           OR v_slug ~ '-archived-' OR v_slug ~ '^v[0-9]+$'
           OR v_slug IN ('api', 'www', 'admin', 'topos', 'i', 'channels', 'skills',
                         'workspaces', 'everyone', 'auth', 'login', 'verify', 'enroll') THEN
            v_slug := 'ws-' || substr(md5(ws.workspace_id), 1, 8);
        END IF;
        v_candidate := v_slug;
        v_n := 1;
        WHILE EXISTS (SELECT 1 FROM workspace w2 WHERE w2.name = v_candidate) LOOP
            v_n := v_n + 1;
            v_candidate := left(v_slug, 60) || '-' || v_n::TEXT;
        END LOOP;
        UPDATE workspace SET name = v_candidate WHERE workspace_id = ws.workspace_id;
    END LOOP;
END
$$;

ALTER TABLE workspace ALTER COLUMN name SET NOT NULL;
-- One name, one workspace — the address resolves or it does not; nothing else shares the space.
CREATE UNIQUE INDEX workspace_by_name ON workspace (name);

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- WORKSPACE POLICY KNOBS — who may invite, and the one staleness window the fleet page and the
-- client hook read as the same clock. Rows are upserted by the setters; readers COALESCE a
-- missing row to the defaults through the one accessor below.
ALTER TABLE workspace_policy ADD COLUMN invite_policy TEXT NOT NULL DEFAULT 'members'
    CHECK (invite_policy IN ('members', 'owners'));
ALTER TABLE workspace_policy ADD COLUMN staleness_window_ms BIGINT NOT NULL DEFAULT 604800000
    CHECK (staleness_window_ms > 0);

-- The ONE staleness-window home: every reader (delivery, the fleet page) calls this, so a missing
-- policy row cannot fork the default between surfaces.
CREATE FUNCTION topos_staleness_window(p_ws TEXT) RETURNS BIGINT LANGUAGE sql STABLE AS $$
    SELECT COALESCE(
        (SELECT staleness_window_ms FROM workspace_policy WHERE workspace_id = p_ws),
        604800000);
$$;

CREATE FUNCTION topos_invite_policy(p_ws TEXT) RETURNS TEXT LANGUAGE sql STABLE AS $$
    SELECT COALESCE(
        (SELECT invite_policy FROM workspace_policy WHERE workspace_id = p_ws),
        'members');
$$;

-- POLICY SETTERS — owner acts (web-surface class; the functions are the contract the web tier
-- calls). The membership gate runs IN the function: the database answer is authoritative for
-- every caller.
CREATE FUNCTION topos_set_staleness_window(p_ws TEXT, p_actor TEXT, p_window_ms BIGINT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    IF p_window_ms IS NULL OR p_window_ms <= 0 OR p_window_ms > 31622400000 THEN
        RETURN 'bad_window';
    END IF;
    INSERT INTO workspace_policy (workspace_id, staleness_window_ms) VALUES (p_ws, p_window_ms)
    ON CONFLICT (workspace_id) DO UPDATE SET staleness_window_ms = EXCLUDED.staleness_window_ms;
    RETURN 'set';
END
$$;

CREATE FUNCTION topos_set_invite_policy(p_ws TEXT, p_actor TEXT, p_policy TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    IF p_policy NOT IN ('members', 'owners') THEN RETURN 'bad_policy'; END IF;
    INSERT INTO workspace_policy (workspace_id, invite_policy) VALUES (p_ws, p_policy)
    ON CONFLICT (workspace_id) DO UPDATE SET invite_policy = EXCLUDED.invite_policy;
    RETURN 'set';
END
$$;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- INVITATION — a roster write, nothing more: seats each email as an invited member (recording who
-- invited whom), optionally pre-places the person into channels (re-inviting restores placements;
-- per-skill unfollows survive, deliberately — they are the person's own standing mask). The
-- tokened invite link is gone; joining is `follow <address>` + proof of the invited email.
-- Member-level unless the workspace policy restricts inviting to owners. Every CLI invitee starts
-- as a member — roles are raised later, on the web. Never demotes an existing seat.
CREATE FUNCTION topos_invite(p_ws TEXT, p_actor TEXT, p_emails TEXT[], p_channels TEXT[], p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_email TEXT;
    v_channel TEXT;
    v_channel_id TEXT;
    v_builtin BIGINT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF topos_invite_policy(p_ws) = 'owners' AND v_role IS DISTINCT FROM 'owner' THEN
        RETURN 'owner_role_required';
    END IF;
    -- Resolve every named channel BEFORE any write: invitations resolve-all-or-apply-none.
    FOREACH v_channel IN ARRAY p_channels LOOP
        IF NOT EXISTS (SELECT 1 FROM channels c WHERE c.workspace_id = p_ws AND c.name = v_channel) THEN
            RETURN 'unknown_channel';
        END IF;
    END LOOP;
    FOREACH v_email IN ARRAY p_emails LOOP
        INSERT INTO workspace_member (workspace_id, principal, role, status, invited_by, added_at)
        VALUES (p_ws, v_email, 'member', 'invited', p_actor, p_created_at)
        -- Re-inviting never demotes: a confirmed seat keeps its role and status; a still-invited
        -- seat refreshes its inviter attribution.
        ON CONFLICT (workspace_id, principal) DO UPDATE
        SET invited_by = CASE WHEN workspace_member.status = 'confirmed'
                              THEN workspace_member.invited_by ELSE EXCLUDED.invited_by END;
        FOREACH v_channel IN ARRAY p_channels LOOP
            SELECT c.channel_id, c.builtin INTO v_channel_id, v_builtin
            FROM channels c WHERE c.workspace_id = p_ws AND c.name = v_channel;
            -- `everyone` is structural (roster-derived) — a pre-placement into it is already true.
            IF v_builtin = 0 THEN
                PERFORM set_config('topos.actor', p_actor, true);
                PERFORM set_config('topos.created_at', p_created_at, true);
                INSERT INTO channel_members (workspace_id, channel_id, principal, added_by, added_at)
                VALUES (p_ws, v_channel_id, v_email, p_actor, p_created_at)
                ON CONFLICT (workspace_id, channel_id, principal) DO NOTHING;
            END IF;
        END LOOP;
    END LOOP;
    RETURN 'invited';
END
$$;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- NOTICES ACK — the read-state write (the fetch-without-ack already rides delivery): person-scoped,
-- idempotent, only the person's own unacked rows move.
CREATE FUNCTION topos_notices_ack(p_ws TEXT, p_principal TEXT, p_ids TEXT[], p_now BIGINT)
RETURNS TEXT LANGUAGE plpgsql AS $$
BEGIN
    IF topos_member_role(p_ws, p_principal) IS NULL THEN RETURN 'member_required'; END IF;
    UPDATE notices SET acked_at = p_now
    WHERE workspace_id = p_ws AND principal = p_principal AND acked_at IS NULL AND id = ANY(p_ids);
    RETURN 'acked';
END
$$;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- ENROLLMENT — token-less sessions. An 'enroll' session is opened against a workspace ADDRESS
-- (the requested name recorded verbatim; resolution may legitimately fail and the flow still runs
-- to its uniform denial — a name that does not exist and a name that exists but is not yours get
-- one indistinguishable answer). A 'login' session belongs to no workspace at all: its grant
-- re-mints the device's credential in EVERY workspace where the proven identity holds a confirmed
-- seat. The invite-token door retires below.
ALTER TABLE device_auth_sessions ADD COLUMN requested_workspace TEXT
    CHECK (requested_workspace IS NULL OR length(requested_workspace) <= 200);
-- Legacy enroll rows (invite-anchored, all ephemeral) get a placeholder so the rebound CHECK holds.
UPDATE device_auth_sessions SET requested_workspace = workspace_id
    WHERE intent = 'enroll' AND requested_workspace IS NULL;
ALTER TABLE device_auth_sessions DROP CONSTRAINT device_auth_sessions_intent_check;
ALTER TABLE device_auth_sessions ADD CONSTRAINT device_auth_sessions_intent_check
    CHECK (intent IN ('enroll', 'standup', 'login'));
ALTER TABLE device_auth_sessions DROP CONSTRAINT device_auth_ws_bound;
ALTER TABLE device_auth_sessions ADD CONSTRAINT device_auth_ws_bound CHECK
    (workspace_id IS NOT NULL
     OR intent = 'login'
     OR (intent = 'standup' AND status IN ('pending', 'denied', 'expired'))
     OR (intent = 'enroll' AND requested_workspace IS NOT NULL));

-- Grants: a login grant is workspace-less by design; an enroll grant against an unresolved
-- address is workspace-less too (the redeem answers the one uniform denial). The intent
-- discriminant keeps the two redeem doors from crossing.
ALTER TABLE enrollment_grants ALTER COLUMN workspace_id DROP NOT NULL;
ALTER TABLE enrollment_grants ADD COLUMN intent TEXT NOT NULL DEFAULT 'enroll'
    CHECK (intent IN ('enroll', 'login'));

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- THE TOKENED INVITE DIES — links are typed resource addresses carrying nothing; invitation is a
-- roster row (already seeded at mint time by the old door, so dropping the tables loses nothing)
-- plus the workspace address. The one-time admin CLAIM keeps its own disjoint table: it is a
-- genesis ceremony, not an invitation. The standing-door epoch goes with the door.
DROP TABLE invite_skill;
DROP TABLE invites;
ALTER TABLE device_auth_sessions DROP COLUMN invite_sha256;
ALTER TABLE enrollment_grants DROP COLUMN invite_sha256;
ALTER TABLE workspace DROP COLUMN link_epoch;
