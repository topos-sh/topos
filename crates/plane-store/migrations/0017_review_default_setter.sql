-- The review-required DEFAULT gets its guarded setter — the third workspace-policy knob joins the
-- other two (staleness window, invite policy) behind an owner-gated `topos_*` function, so the web
-- tier's toggle is a database policy decision like every other policy write. Until now the only
-- actor-gated path to this column was none at all: the operator admin-token route sets it without
-- an acting principal (the operator owns the plane — that route stands), and a composing surface
-- had to bring its own lock. The membership gate runs IN the function: the database answer is
-- authoritative for every caller.

CREATE FUNCTION topos_set_review_default(p_ws TEXT, p_actor TEXT, p_required BIGINT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    IF p_required NOT IN (0, 1) THEN RETURN 'bad_value'; END IF;
    INSERT INTO workspace_policy (workspace_id, review_required) VALUES (p_ws, p_required)
    ON CONFLICT (workspace_id) DO UPDATE SET review_required = EXCLUDED.review_required;
    RETURN 'set';
END
$$;
