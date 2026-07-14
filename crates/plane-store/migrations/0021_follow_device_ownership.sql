-- A DEVICE EXCLUSION IS THAT DEVICE'S OWN ROW — both directions, mint AND clear, are fenced to the
-- CALLER's own live device. The "not on this device" marker is per-device state: `remove` mints it,
-- a `follow` on the same device lifts it. Neither act may reach another person's device rows —
-- membership alone must not be enough.
--
-- Before this migration the fence held on NEITHER side:
--   * `topos_follow_skill` (0015) cleared whatever `(device, skill)` exclusion row the caller named,
--     checking only that the CALLER is a member — a buggy or malicious caller could erase ANOTHER
--     person's exclusion by naming that person's device id.
--   * `topos_exclude_device` (0015) took no caller identity at all: it checked the named device was
--     a registered, live device of SOME confirmed member — so any caller could mint an exclusion for
--     ANOTHER person's device, silently stopping that device's delivery of a followed skill.
--
-- The fence, both sides now: the named device must be a REGISTERED, non-revoked `device_registry`
-- row in this workspace whose `principal` IS the acting caller. On the clear, a foreign, unknown, or
-- revoked device SILENTLY SKIPS the delete — the follow is still the member's own legitimate act, so
-- it returns 'followed' either way (a new refusal code would ripple through the exhaustive outcome
-- maps the Rust and web tiers keep; the skip needs none). On the mint, the same predicate folds into
-- the existing 'member_required' refusal — the mint has always refused there, the fence only narrows
-- WHOSE device qualifies. All legitimate callers pass the bearer-resolved calling device (the device
-- lane acts as itself), so no legitimate call changes behavior.
--
-- `topos_follow_skill` is CREATE OR REPLACE (same signature, outcome codes verbatim: 'member_required'
-- / 'unknown_skill' / 'skill_not_active' / 'followed'), which keeps its grants. `topos_exclude_device`
-- gains a caller parameter, so it is DROP + CREATE; 0019's default privileges re-cover EXECUTE for the
-- web role (functions the migrating role creates in this schema). Appended, never editing an applied
-- migration's bytes (sqlx checksums the file).

CREATE OR REPLACE FUNCTION topos_follow_skill(p_ws TEXT, p_principal TEXT, p_skill TEXT, p_device TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_status TEXT;
BEGIN
    IF topos_member_role(p_ws, p_principal) IS NULL THEN RETURN 'member_required'; END IF;
    SELECT status INTO v_status FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'active' THEN RETURN 'skill_not_active'; END IF;
    INSERT INTO skill_follows (workspace_id, principal, skill_id, created_at)
    VALUES (p_ws, p_principal, p_skill, p_created_at)
    ON CONFLICT (workspace_id, principal, skill_id) DO NOTHING;
    DELETE FROM skill_unfollows WHERE workspace_id = p_ws AND principal = p_principal AND skill_id = p_skill;
    -- The exclusion is the DEVICE's own row — clear it only for the caller's own live device (the
    -- mint's fence, mirrored). A foreign/unknown/revoked device skips silently; the follow still lands.
    IF p_device IS NOT NULL AND EXISTS (
        SELECT 1 FROM device_registry dr
        WHERE dr.workspace_id = p_ws AND dr.device_key_id = p_device
          AND dr.principal = p_principal AND dr.revoked = 0
    ) THEN
        DELETE FROM device_exclusions WHERE workspace_id = p_ws AND device_key_id = p_device AND skill_id = p_skill;
    END IF;
    PERFORM topos_reattach(p_ws, p_principal);
    RETURN 'followed';
END
$$;

-- The mint, re-keyed on the acting caller: the named device must be the CALLER's own live device.
-- Parameter order mirrors `topos_follow_skill` (ws, principal, skill, device, created_at).
DROP FUNCTION topos_exclude_device(TEXT, TEXT, TEXT, TEXT);

CREATE FUNCTION topos_exclude_device(p_ws TEXT, p_principal TEXT, p_skill TEXT, p_device TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
BEGIN
    IF topos_member_role(p_ws, p_principal) IS NULL THEN RETURN 'member_required'; END IF;
    -- The exclusion is that device's own row: only the caller's OWN registered, non-revoked device
    -- qualifies. A foreign, unknown, or revoked device folds into the same refusal the mint has
    -- always answered — no oracle distinguishes "not yours" from "not a member".
    IF NOT EXISTS (
        SELECT 1 FROM device_registry dr
        WHERE dr.workspace_id = p_ws AND dr.device_key_id = p_device
          AND dr.principal = p_principal AND dr.revoked = 0
    ) THEN
        RETURN 'member_required';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill) THEN
        RETURN 'unknown_skill';
    END IF;
    INSERT INTO device_exclusions (workspace_id, device_key_id, skill_id, created_at)
    VALUES (p_ws, p_device, p_skill, p_created_at)
    ON CONFLICT (workspace_id, device_key_id, skill_id) DO NOTHING;
    RETURN 'excluded';
END
$$;
