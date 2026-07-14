-- A FOLLOW CLEARS ONLY THE CALLER'S OWN DEVICE EXCLUSION. `topos_follow_skill` lifts a device's
-- "not on this device" marker as a convenience — a `remove` then a `follow` on the same device
-- un-does the removal. But the marker is that DEVICE's own row, and clearing one must be as fenced
-- as minting one: `topos_exclude_device` (0015) refuses unless the named device is a REGISTERED,
-- non-revoked device whose owner holds a confirmed seat — no caller may mint an exclusion for a
-- device (or a person) that is not its own. The clear had no such fence: it deleted whatever
-- `(device, skill)` row the caller named, so a member could erase ANOTHER person's exclusion by
-- naming that person's device id. Membership alone must not reach another person's device rows.
--
-- The fix scopes the DELETE to the caller's OWN live device: it fires only when the named device is
-- a registered, non-revoked `device_registry` row in this workspace whose `principal` is the
-- following caller. A foreign, unknown, or revoked device SILENTLY SKIPS the delete — the follow is
-- still the member's own legitimate act, so it returns 'followed' either way. (A new refusal code
-- would ripple through the exhaustive outcome maps the Rust and web tiers keep; the skip needs
-- none.) All callers pass the bearer-resolved calling device, so no legitimate follow changes.
--
-- CREATE OR REPLACE keeps the existing grants; the signature, body, and outcome codes
-- ('member_required' / 'unknown_skill' / 'skill_not_active' / 'followed') are otherwise verbatim.
-- Appended (never edits an applied migration's bytes — sqlx checksums the file).

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
