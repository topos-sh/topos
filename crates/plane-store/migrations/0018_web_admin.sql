-- The web-surface admin acts land their guarded functions: skill RENAME with a resolving hint
-- (the old name keeps answering), channel RENAME and DELETE (the existence-admin half curation
-- deliberately left out), the roster acts the web owns (role change, self-serve leave), and the
-- session-side device revoke. Plus the two folds that until now lived only in Rust: catalog-name
-- resolution and the catalog-name mint — ONE implementation each, called by Rust today and by the
-- web tier directly.
-- Appended (never edits an applied migration's bytes — sqlx checksums the file).
--
-- Same posture as 0001-0017: every row `workspace_id`-scoped, mutable times BIGINT epoch
-- MILLISECONDS, audit `created_at` TEXT ISO-8601, principals in the canonical lowercase fold.
-- POLICY LIVES HERE: every policy write with logic is a named `topos_*` SQL function — the
-- membership gate runs IN the function, so the database answer is authoritative for every caller.

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- CATALOG NAME HINTS — the rename redirect store. A rename records "this name used to mean that
-- skill"; resolution consults the LIVE catalog first (a hint can never shadow a live identity),
-- then the hints — so an old address in a log, a doc, or a muscle-memory command keeps resolving
-- until someone claims the name for a new identity. The hint row doubles as the rename's audit
-- record (who renamed away from this name, when).
CREATE TABLE catalog_name_hints (
    workspace_id TEXT NOT NULL,
    name         TEXT NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9-]*$' AND length(name) <= 200),
    skill_id     TEXT NOT NULL,
    renamed_by   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, name),
    FOREIGN KEY (workspace_id, skill_id) REFERENCES catalog (workspace_id, skill_id)
);

CREATE INDEX catalog_name_hints_by_skill ON catalog_name_hints (workspace_id, skill_id);

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- NAME RESOLUTION — the one place a user-facing skill name becomes an identity. Exact catalog
-- match first (any status — archived names stay addressable in history surfaces), then the hint
-- table (`via` says which arm answered, and `name`/`status` carry the LIVE spelling so a caller
-- can redirect). Zero rows = the name resolves to nothing.
CREATE FUNCTION topos_resolve_skill(p_ws TEXT, p_name TEXT)
RETURNS TABLE (skill_id TEXT, name TEXT, status TEXT, via TEXT) LANGUAGE sql STABLE AS $$
    SELECT c.skill_id, c.name, c.status, 'name'
    FROM catalog c WHERE c.workspace_id = p_ws AND c.name = p_name
    UNION ALL
    SELECT c.skill_id, c.name, c.status, 'hint'
    FROM catalog_name_hints h
    JOIN catalog c ON c.workspace_id = h.workspace_id AND c.skill_id = h.skill_id
    WHERE h.workspace_id = p_ws AND h.name = p_name
      AND NOT EXISTS (SELECT 1 FROM catalog c2 WHERE c2.workspace_id = p_ws AND c2.name = p_name)
$$;

-- THE CATALOG-NAME MINT — the fold a registering publish derives a birth name from (display name
-- folded to the agent-skills charset, else the skill id folded, else the literal 'skill'; capped
-- at 64 with no trailing hyphen). Collision handling stays the CALLER's (a registering publish
-- refuses a taken name typed) — this is the pure fold, and the ONE implementation of it.
CREATE FUNCTION topos_mint_catalog_name(p_display_name TEXT, p_skill_id TEXT)
RETURNS TEXT LANGUAGE sql IMMUTABLE AS $$
    SELECT COALESCE(
        NULLIF(btrim(left(btrim(regexp_replace(lower(COALESCE(p_display_name, '')),
                                               '[^a-z0-9]+', '-', 'g'), '-'), 64), '-'), ''),
        NULLIF(btrim(left(btrim(regexp_replace(lower(COALESCE(p_skill_id, '')),
                                               '[^a-z0-9]+', '-', 'g'), '-'), 64), '-'), ''),
        'skill')
$$;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- SKILL RENAME — an owner act on an ACTIVE skill (id-keyed: the immutable custody key, so a
-- concurrent rename can never retarget the act). The old name becomes a hint (the redirect);
-- any hint squatting the NEW name is cleared (the name now names a live identity — resolution
-- would shadow it anyway, but a dead hint must not linger as a false audit trail for the new
-- name). The `-archived-` pattern is refused like the birth mints refuse it: that namespace
-- belongs to the archive rename.
CREATE FUNCTION topos_rename_skill(p_ws TEXT, p_skill TEXT, p_new_name TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
    v_name TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    SELECT status, name INTO v_status, v_name FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'active' THEN RETURN 'not_active'; END IF;
    IF p_new_name IS NULL OR p_new_name !~ '^[a-z0-9][a-z0-9-]*$' OR length(p_new_name) > 64
       OR p_new_name ~ '-archived-' THEN
        RETURN 'bad_name';
    END IF;
    IF p_new_name = v_name THEN RETURN 'renamed'; END IF;
    IF EXISTS (SELECT 1 FROM catalog WHERE workspace_id = p_ws AND name = p_new_name) THEN
        RETURN 'name_taken';
    END IF;
    -- The old name keeps resolving: latest rename wins the hint slot (a name that pointed at an
    -- earlier identity now points here — one name, one meaning at a time).
    INSERT INTO catalog_name_hints (workspace_id, name, skill_id, renamed_by, created_at)
    VALUES (p_ws, v_name, p_skill, p_actor, p_created_at)
    ON CONFLICT (workspace_id, name) DO UPDATE
    SET skill_id = EXCLUDED.skill_id, renamed_by = EXCLUDED.renamed_by, created_at = EXCLUDED.created_at;
    DELETE FROM catalog_name_hints WHERE workspace_id = p_ws AND name = p_new_name;
    UPDATE catalog SET name = p_new_name WHERE workspace_id = p_ws AND skill_id = p_skill;
    RETURN 'renamed';
END
$$;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- CHANNEL RENAME — an owner existence-act. The channel_id (the immutable key) never moves, so
-- references, memberships, and the audit trail survive; only the display key changes. `everyone`
-- refuses typed (the trigger guard is the backstop). No hint table for channels — a channel name
-- is a grouping label, not a distribution address a device pins.
CREATE FUNCTION topos_channel_rename(p_ws TEXT, p_channel_name TEXT, p_new_name TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_channel_id TEXT;
    v_builtin BIGINT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    SELECT channel_id, builtin INTO v_channel_id, v_builtin FROM channels
    WHERE workspace_id = p_ws AND name = p_channel_name;
    IF v_channel_id IS NULL THEN RETURN 'unknown_channel'; END IF;
    IF v_builtin = 1 THEN RETURN 'builtin'; END IF;
    IF p_new_name IS NULL OR p_new_name !~ '^[a-z0-9][a-z0-9-]*$' OR length(p_new_name) > 64 THEN
        RETURN 'bad_name';
    END IF;
    IF p_new_name = p_channel_name THEN RETURN 'renamed'; END IF;
    IF EXISTS (SELECT 1 FROM channels WHERE workspace_id = p_ws AND name = p_new_name) THEN
        RETURN 'name_taken';
    END IF;
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    UPDATE channels SET name = p_new_name WHERE workspace_id = p_ws AND channel_id = v_channel_id;
    RETURN 'renamed';
END
$$;

-- CHANNEL DELETE — an owner existence-act, and a CASCADE by decision: the FKs on channel_skills /
-- channel_members carry no ON DELETE, so this function deletes the references and memberships
-- itself (each DELETE rides the audit trigger — the history says exactly what the deletion
-- unplaced). Deliberately NO person-detach records: a channel deletion is an UPSTREAM withdrawal
-- (the client cleans agent dirs and offers keep-as-yours), never a person's own detach — skills
-- another source still delivers keep flowing via the union. The audit rows keep the channel_id of
-- the deleted channel: history is append-only and survives the row.
CREATE FUNCTION topos_channel_delete(p_ws TEXT, p_channel_name TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_channel_id TEXT;
    v_builtin BIGINT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    SELECT channel_id, builtin INTO v_channel_id, v_builtin FROM channels
    WHERE workspace_id = p_ws AND name = p_channel_name;
    IF v_channel_id IS NULL THEN RETURN 'unknown_channel'; END IF;
    IF v_builtin = 1 THEN RETURN 'builtin'; END IF;
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    DELETE FROM channel_skills WHERE workspace_id = p_ws AND channel_id = v_channel_id;
    DELETE FROM channel_members WHERE workspace_id = p_ws AND channel_id = v_channel_id;
    DELETE FROM channels WHERE workspace_id = p_ws AND channel_id = v_channel_id;
    RETURN 'deleted';
END
$$;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- ROLE CHANGE — an owner act on any seat (invited seats included: a role rides the seat and
-- survives confirmation). The last-owner lockout guards BOTH directions a workspace could lose
-- its last owner here: demoting the sole confirmed owner is refused.
CREATE FUNCTION topos_set_member_role(p_ws TEXT, p_actor TEXT, p_email TEXT, p_role TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_target_role TEXT;
    v_target_status TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    IF p_role NOT IN ('owner', 'reviewer', 'member') THEN RETURN 'bad_role'; END IF;
    SELECT role, status INTO v_target_role, v_target_status FROM workspace_member
    WHERE workspace_id = p_ws AND principal = p_email;
    IF v_target_role IS NULL THEN RETURN 'unknown_member'; END IF;
    IF v_target_role = 'owner' AND v_target_status = 'confirmed' AND p_role <> 'owner'
       AND NOT EXISTS (SELECT 1 FROM workspace_member m
                       WHERE m.workspace_id = p_ws AND m.role = 'owner' AND m.status = 'confirmed'
                         AND m.principal <> p_email) THEN
        RETURN 'sole_owner';
    END IF;
    UPDATE workspace_member SET role = p_role WHERE workspace_id = p_ws AND principal = p_email;
    RETURN 'set';
END
$$;

-- SELF-SERVE LEAVE — the person's own seat delete. A sole confirmed owner cannot leave (transfer
-- ownership first — the workspace must always have an owner). The lapse-detach reconcile runs
-- BEFORE the seat delete (the entitlement union is membership-gated and reads empty after), so
-- the person's devices freeze their copies with honest detach records: "you left; the copies are
-- yours". Invited (never-confirmed) seats just leave — nothing was delivered.
CREATE FUNCTION topos_leave_workspace(p_ws TEXT, p_principal TEXT, p_now BIGINT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
BEGIN
    SELECT role, status INTO v_role, v_status FROM workspace_member
    WHERE workspace_id = p_ws AND principal = p_principal;
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role = 'owner' AND v_status = 'confirmed'
       AND NOT EXISTS (SELECT 1 FROM workspace_member m
                       WHERE m.workspace_id = p_ws AND m.role = 'owner' AND m.status = 'confirmed'
                         AND m.principal <> p_principal) THEN
        RETURN 'sole_owner';
    END IF;
    PERFORM topos_detach_on_removal(p_ws, p_principal, p_now, p_created_at);
    DELETE FROM workspace_member WHERE workspace_id = p_ws AND principal = p_principal;
    RETURN 'left';
END
$$;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- DEVICE REVOKE, session-side — the web twin of the device-lane governance revoke, same role
-- matrix (owner, or the device's own principal signing their device out). A revoke is an instant
-- row flip: the registry row and its credential hash stay (receipts replay; the audit survives),
-- fresh work dies, re-enrollment is the recovery. Idempotent — re-revoking answers 'revoked'.
CREATE FUNCTION topos_revoke_device(p_ws TEXT, p_actor TEXT, p_device TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_principal TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    SELECT principal INTO v_principal FROM device_registry
    WHERE workspace_id = p_ws AND device_key_id = p_device;
    IF v_principal IS NULL THEN RETURN 'unknown_device'; END IF;
    IF v_principal <> p_actor AND v_role IS DISTINCT FROM 'owner' THEN
        RETURN 'owner_or_self_required';
    END IF;
    UPDATE device_registry SET revoked = 1 WHERE workspace_id = p_ws AND device_key_id = p_device;
    RETURN 'revoked';
END
$$;
