-- CREATE-ON-FIRST-USE MUST LOSE RACES POLITELY (and a freed name must be placeable again). The
-- channel-placement function creates a channel the first time someone places into a name that does
-- not yet exist. Its check-then-insert (SELECT the name, INSERT if absent) had two ways to reach a
-- caller as a RAW unique_violation — an internal error — instead of a meaningful outcome, and both
-- are the SAME insert failing on the SAME table's two different unique keys:
--
--   1. THE RACE. Two members place into the same brand-new name at once. Both SELECTs miss; both
--      INSERT; the loser blocks on the winner's in-flight `channels_by_name` entry and raises when
--      the winner commits. It should have quietly placed into the channel that now exists.
--   2. THE FREED NAME (no concurrency needed). The table carries TWO unique keys — the PRIMARY KEY
--      `(workspace_id, channel_id)` and the UNIQUE INDEX `channels_by_name (workspace_id, name)` —
--      and a rename moves ONLY the name: `channel_id` is immutable plumbing, `name` the mutable
--      user-facing key (the split the rename feature established). So after `foo` is renamed to
--      `bar`, the row still holds `channel_id = 'foo'` while the NAME `foo` is free. A create under
--      the freed name passes the name-lookup, then collides on the surviving PRIMARY KEY.
--
-- The fix rewrites ONLY the channel-resolution block as a bounded, self-healing retry loop; the
-- member gate, catalog checks, curated-mode role gate, reference insert, and self-heal are untouched.
-- CREATE OR REPLACE keeps the identical signature + outcome vocabulary and preserves the function's
-- grants (0019's default privileges already cover the schema — nothing to grant here). Appended, never
-- editing an applied migration (sqlx checksums the file).
--
-- WHY THE LOOP RESOLVES BOTH — and works for BOTH callers (the READ COMMITTED web tier AND the
-- SERIALIZABLE device-lane transaction runner):
--   * A FREED-NAME collision violates the PRIMARY KEY (`channels_pkey`): the name is genuinely free,
--     only the candidate id is taken. We mint a suffixed candidate id (`foo` -> `foo-2` -> `foo-3` ...)
--     and retry. Minting `foo-2` under the name `foo` is correct and invisible: users address a
--     channel by its NAME; the id is internal plumbing (exactly as a rename already decoupled them).
--   * A NAME collision violates `channels_by_name`: someone else holds the name. Under READ COMMITTED
--     each statement takes a fresh snapshot, so the next turn's SELECT sees the winner's committed row
--     and we place into it as a LOSER (`created` stays false — a race loser returns 'placed', and the
--     curated gate below reads the FOUND row's mode). Under a frozen snapshot (the SERIALIZABLE
--     transaction runner takes ONE snapshot for the whole transaction), that re-SELECT would stay
--     blind and the loop could not make progress — so when a name collision is invisible to our own
--     snapshot we escalate to a serialization failure, which the runner catches and retries from the
--     top with a fresh snapshot that finds the winner. A committed name collision is the ONLY way the
--     re-probe misses (a not-yet-committed conflict would have made our INSERT wait and then succeed,
--     never raise), so the escalation never fires spuriously on the READ COMMITTED path.
-- The loop is bounded so a pathological state can never spin forever.
CREATE OR REPLACE FUNCTION topos_channel_place(p_ws TEXT, p_channel_name TEXT, p_skill TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
    v_channel_id TEXT;
    v_mode TEXT;
    v_created BOOLEAN := false;
    v_candidate TEXT;
    v_suffix INT := 1;   -- 1 => the bare name; 2,3,... => name-2, name-3, ... (bumped on a PK collision)
    v_iter INT := 0;
    v_constraint TEXT;
BEGIN
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    SELECT status INTO v_status FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'active' THEN RETURN 'skill_not_active'; END IF;
    -- Resolve the channel by NAME, creating it on first use — tolerating the two unique-key collisions
    -- above (see the header). Bounded; each turn either finds the channel and places into it, mints it,
    -- bumps a colliding candidate id, or escalates an unresolvable name race for the runner to retry.
    LOOP
        v_iter := v_iter + 1;
        IF v_iter > 32 THEN
            RAISE EXCEPTION 'topos_channel_place: channel % did not settle after % attempts', p_channel_name, v_iter - 1;
        END IF;
        SELECT channel_id, mode INTO v_channel_id, v_mode FROM channels
        WHERE workspace_id = p_ws AND name = p_channel_name;
        EXIT WHEN v_channel_id IS NOT NULL;   -- found (a race winner's, or a pre-existing) -> place into it
        IF p_channel_name !~ '^[a-z0-9][a-z0-9-]*$' OR length(p_channel_name) > 64 THEN
            RETURN 'bad_name';
        END IF;
        v_candidate := CASE WHEN v_suffix = 1 THEN p_channel_name
                            ELSE p_channel_name || '-' || v_suffix::text END;
        BEGIN
            INSERT INTO channels (workspace_id, channel_id, name, mode, builtin, created_by, created_at)
            VALUES (p_ws, v_candidate, p_channel_name, 'open', 0, p_actor, p_created_at);
            v_channel_id := v_candidate;
            v_mode := 'open';
            v_created := true;
            EXIT;
        EXCEPTION WHEN unique_violation THEN
            GET STACKED DIAGNOSTICS v_constraint = CONSTRAINT_NAME;
            IF v_constraint = 'channels_pkey' THEN
                -- The candidate id is taken (a freed name's surviving id, or a suffix race). Bump the
                -- suffix and retry the SAME name against a fresh candidate id.
                v_suffix := v_suffix + 1;
            ELSE
                -- The NAME is taken (`channels_by_name`). If our snapshot can see the holder we loop and
                -- the top SELECT places into it (READ COMMITTED); if not, our snapshot is frozen over a
                -- committed winner (SERIALIZABLE) — escalate so the transaction runner retries fresh.
                PERFORM 1 FROM channels WHERE workspace_id = p_ws AND name = p_channel_name;
                IF NOT FOUND THEN
                    RAISE EXCEPTION 'topos_channel_place: concurrent create of channel %', p_channel_name
                        USING ERRCODE = 'serialization_failure';
                END IF;
            END IF;
        END;
    END LOOP;
    IF v_mode = 'curated' AND v_role NOT IN ('reviewer', 'owner') THEN
        RETURN 'curated_role_required';
    END IF;
    INSERT INTO channel_skills (workspace_id, channel_id, skill_id, added_by, added_at)
    VALUES (p_ws, v_channel_id, p_skill, p_actor, p_created_at)
    ON CONFLICT (workspace_id, channel_id, skill_id) DO NOTHING;
    -- SELF-HEAL: this placement re-entitles the skill for everyone the channel reaches, so no stale
    -- detachment record may strand it (entitlement always wins). Skill-scoped, so the sweep is
    -- bounded by the people who had actually detached THIS skill.
    PERFORM topos_heal_detachments(p_ws, p_skill);
    RETURN CASE WHEN v_created THEN 'created' ELSE 'placed' END;
END
$$;
