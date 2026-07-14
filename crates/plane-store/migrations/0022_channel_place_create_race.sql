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
-- SERIALIZABLE device-lane transaction runner). After a unique_violation the handler asks ONE
-- question — can our own snapshot SEE the conflicting row? — and triages on the answer:
--   * A FREED-NAME collision violates the PRIMARY KEY (`channels_pkey`): the name is genuinely free,
--     only the candidate id is taken (a rename freed the name but its immutable id survives). When
--     the holder is VISIBLE we mint a suffixed candidate id in ONE jump past the highest suffixed
--     survivor (`foo` with `foo`..`foo-32` all taken settles as `foo-33` on the second iteration,
--     never the 33rd — accumulated survivors can NEVER exhaust the bound). Minting `foo-2` under the
--     name `foo` is correct and invisible: users address a channel by its NAME; the id is internal
--     plumbing (exactly as a rename already decoupled them).
--   * A NAME collision violates `channels_by_name`: someone else holds the name. When the holder is
--     VISIBLE we loop and the top SELECT places into it as the race LOSER (`created` stays false —
--     a loser returns 'placed', and the curated gate below reads the FOUND row's mode).
--   * When the conflicting row is INVISIBLE the triage splits on the transaction's isolation:
--       - At READ COMMITTED every statement takes a fresh snapshot, so an invisible conflict means
--         the holder VANISHED between our failed INSERT and the probe (the winner was renamed away
--         or deleted) — the collision has resolved itself, so we simply loop and retry. The web
--         tier has no retry runner, so this path must never raise.
--       - Under a FROZEN snapshot (the SERIALIZABLE device-lane runner takes ONE snapshot for the
--         whole transaction) the loop could never make progress against a committed winner it
--         cannot read — so we escalate to a serialization failure (SQLSTATE 40001), which the
--         runner already catches and retries from the top with a fresh snapshot.
-- The iteration bound survives ONLY as a runaway backstop against pathological live contention
-- (every burned iteration requires a FRESH committed conflict inside the statement window);
-- accumulated state cannot reach it, so exhaustion is a genuine internal fault and raises as one.
CREATE OR REPLACE FUNCTION topos_channel_place(p_ws TEXT, p_channel_name TEXT, p_skill TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
    v_channel_id TEXT;
    v_mode TEXT;
    v_created BOOLEAN := false;
    v_candidate TEXT;
    v_iter INT := 0;
    v_constraint TEXT;
    v_next BIGINT;
BEGIN
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    SELECT status INTO v_status FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'active' THEN RETURN 'skill_not_active'; END IF;
    -- Resolve the channel by NAME, creating it on first use — tolerating the two unique-key collisions
    -- above (see the header). Each turn either finds the channel and places into it, mints it, jumps a
    -- colliding candidate id past the visible survivors, retries a vanished conflict, or escalates a
    -- frozen-snapshot race for the runner to retry. The bound is a live-contention backstop only.
    v_candidate := p_channel_name;
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
                -- The candidate id is taken. Visible holder -> one jump past the highest suffixed
                -- survivor (the name charset is [a-z0-9-], so the name is regex-inert as a pattern).
                PERFORM 1 FROM channels WHERE workspace_id = p_ws AND channel_id = v_candidate;
                IF FOUND THEN
                    SELECT COALESCE(MAX(CAST(substring(channel_id FROM length(p_channel_name) + 2) AS BIGINT)), 1) + 1
                    INTO v_next FROM channels
                    WHERE workspace_id = p_ws AND channel_id ~ ('^' || p_channel_name || '-[0-9]+$');
                    v_candidate := p_channel_name || '-' || v_next::text;
                ELSIF current_setting('transaction_isolation') <> 'read committed' THEN
                    -- Invisible under a frozen snapshot: a committed racer we can never read.
                    RAISE EXCEPTION 'topos_channel_place: concurrent create of channel %', p_channel_name
                        USING ERRCODE = 'serialization_failure';
                END IF;
                -- Invisible at READ COMMITTED: the holder vanished since the INSERT — retry as-is.
            ELSE
                -- The NAME is taken (`channels_by_name`). Visible holder -> loop; the top SELECT
                -- places into it. Vanished at READ COMMITTED (winner renamed away or deleted) ->
                -- the name is free again, loop retries the create. Invisible under a frozen
                -- snapshot -> escalate for the transaction runner to retry fresh.
                PERFORM 1 FROM channels WHERE workspace_id = p_ws AND name = p_channel_name;
                IF NOT FOUND AND current_setting('transaction_isolation') <> 'read committed' THEN
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
