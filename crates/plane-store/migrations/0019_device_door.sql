-- THE DEVICE DOOR MOVES TO THE COMPOSING SURFACE. The row-op half of the device lane (delivery,
-- report, subscriptions, curation, protection, notices, invitations, the describe reads) is now
-- served by the web tier calling the guarded `topos_*` functions directly under its scoped role —
-- the same one-implementation rule the channel era established. This migration adds the three
-- functions that lane still lacked, then records the web role's grants NEXT TO THE SCHEMA THEY
-- BOUND (from here on, a migration that adds a table or column a guarded function writes carries
-- the matching grant delta in the same file).
--
-- Callers of everything here are plain READ COMMITTED autocommit connections (the web tier), so
-- each function is one self-contained transaction body: single-statement snapshots where a
-- consistent read matters, an explicit row fence where a read-then-write invariant does.

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- THE FRONT DOOR: resolve a presented workspace credential to its acting identity. The sha256 is
-- computed HERE (Postgres' built-in) so the surface tier holds no digest code at all; the lookup IS
-- the authentication (0014's partial-unique index), the workspace binding keeps a credential from
-- crossing workspaces, and the confirmed-seat join is the ONE membership predicate every lane gates
-- on. Every miss — unknown credential, revoked device, unknown workspace, unseated or unconfirmed
-- principal — is the same empty set, which the caller folds to its uniform not-found.
CREATE FUNCTION topos_device_actor(p_ws TEXT, p_credential TEXT)
RETURNS TABLE (person TEXT, device_key_id TEXT, role TEXT) LANGUAGE sql STABLE AS $$
    SELECT dr.principal, dr.device_key_id, m.role
    FROM device_registry dr
    JOIN workspace_member m
      ON m.workspace_id = dr.workspace_id AND m.principal = dr.principal AND m.status = 'confirmed'
    WHERE dr.workspace_id = p_ws
      AND dr.credential_sha256 = sha256(convert_to(p_credential, 'UTF8'))
      AND dr.revoked = 0
$$;

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- THE DELIVERY READ, whole. One SQL statement = one snapshot: the entitled set, the person's
-- detached set, this device's exclusions, and the notices feed MUST come from a single consistent
-- read — a subscription change landing between two independent reads could leave a skill in
-- NEITHER list, and the client reads "delivered nowhere, detached nowhere" as an UPSTREAM
-- withdrawal, cleaning agent dirs for a skill the person still subscribes to. (The Rust
-- orchestration this replaces held a REPEATABLE READ transaction open for the same reason; a
-- single statement gets the same guarantee for free.)
--
-- Returns the COMPLETE `WireDelivery` body (the wire logic lives in ONE place), or NULL when the
-- membership gate refuses — the caller has already run `topos_device_actor`, but this function
-- re-runs the gate itself like every guarded function, so a direct call discloses nothing.
-- Shape notes pinned by the wire contract: `excluded` is OMITTED when empty; a notice's optional
-- fields and a skill's absent display_name are omitted, never null (jsonb_strip_nulls); `detached`,
-- `notices`, and `skills` are always present, possibly empty; hashes are 64-char lowercase hex.
CREATE FUNCTION topos_delivery(p_ws TEXT, p_person TEXT, p_device TEXT)
RETURNS JSONB LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN NOT EXISTS (
               SELECT 1 FROM device_registry dr
               JOIN workspace_member m ON m.workspace_id = dr.workspace_id
                    AND m.principal = dr.principal AND m.status = 'confirmed'
               WHERE dr.workspace_id = p_ws AND dr.device_key_id = p_device
                 AND dr.principal = p_person AND dr.revoked = 0
           ) THEN NULL
    ELSE jsonb_strip_nulls(jsonb_build_object(
        'schema_version', 1,
        'workspace_id', p_ws,
        'skills', COALESCE((
            SELECT jsonb_agg(jsonb_build_object(
                       'skill_id', e.skill_id,
                       'name', e.name,
                       'display_name', e.display_name,
                       'protection', e.protection,
                       'version_id', encode(e.commit_id, 'hex'),
                       'bundle_digest', encode(e.bundle_digest, 'hex'),
                       'generation', jsonb_build_object('epoch', e.epoch, 'seq', e.seq),
                       'updated_at', e.updated_at,
                       'via', jsonb_build_object(
                           'channels', to_jsonb(e.via_channels),
                           'direct', e.direct <> 0)
                   ) ORDER BY e.name)
            FROM topos_entitled_skills(p_ws, p_person, p_device) e), '[]'::jsonb),
        'detached', COALESCE((
            SELECT jsonb_agg(d.skill_id ORDER BY d.skill_id)
            FROM (SELECT u.skill_id FROM skill_unfollows u
                  WHERE u.workspace_id = p_ws AND u.principal = p_person
                  UNION
                  SELECT dt.skill_id FROM skill_detachments dt
                  WHERE dt.workspace_id = p_ws AND dt.principal = p_person) d
            WHERE NOT EXISTS (SELECT 1 FROM topos_entitled_skills(p_ws, p_person, p_device) e
                              WHERE e.skill_id = d.skill_id)), '[]'::jsonb),
        'excluded', NULLIF(COALESCE((
            SELECT jsonb_agg(dx.skill_id ORDER BY dx.skill_id)
            FROM device_exclusions dx
            JOIN catalog cat ON cat.workspace_id = dx.workspace_id AND cat.skill_id = dx.skill_id
            WHERE dx.workspace_id = p_ws AND dx.device_key_id = p_device
              AND cat.status = 'active'), '[]'::jsonb), '[]'::jsonb),
        'notices', COALESCE((
            SELECT jsonb_agg(jsonb_build_object(
                       'id', n.id,
                       'kind', n.kind,
                       'skill_id', n.skill_id,
                       'skill_name', cat.name,
                       'version_id', encode(n.version_id, 'hex'),
                       'actor', n.actor,
                       'outcome', n.outcome,
                       'reason', n.reason,
                       'message', n.message,
                       'created_at', n.created_at
                   ) ORDER BY n.created_at, n.id)
            FROM notices n
            LEFT JOIN catalog cat ON cat.workspace_id = n.workspace_id AND cat.skill_id = n.skill_id
            WHERE n.workspace_id = p_ws AND n.principal = p_person AND n.acked_at IS NULL), '[]'::jsonb),
        -- The verbatim `open ∧ base == current` staleness clause (the tracked copy family) over the
        -- entitled ids — a staled proposal drops out of the count exactly as it drops out of
        -- read/retention/listing.
        'proposals_awaiting', (
            SELECT COUNT(*)
            FROM proposals p
            JOIN current c ON c.workspace_id = p.workspace_id AND c.skill_id = p.skill_id
            WHERE p.workspace_id = p_ws AND p.status = 'open'
              AND c.epoch = p.base_epoch AND c.seq = p.base_seq
              AND p.skill_id IN (SELECT e.skill_id FROM topos_entitled_skills(p_ws, p_person, p_device) e)),
        'staleness_window_ms', topos_staleness_window(p_ws)
    )) END
$$;

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- THE APPLIED-STATE REPORT: one snapshot upsert per device — refresh the reported rows, drop the
-- non-detached rows the snapshot no longer names, stamp the device's `last_report_at` (the ONE
-- staleness clock). A report is CLIENT-ASSERTED data, so every named skill is re-checked against
-- the server's own entitlement predicate in the INSERT's join: only truly-delivered skills are
-- recorded, an ENTITLED reported skill revives its row (`detached = 0` — what heals a fleet row a
-- lapse froze before a curator re-placed the skill), and a DETACHED skill is by definition not
-- entitled, so no client can revive a detach record the plane is deliberately holding.
--
-- Callers are READ COMMITTED, so the function fences ITSELF: the device's registry row is locked
-- FOR UPDATE first (the same discipline as `topos_set_member_role` / `topos_leave_workspace`), so
-- two racing reports from one device serialize and a concurrent detach reconcile — which joins the
-- same registry row — cannot interleave between this function's three writes.
--
-- Returns 'ok', or NULL when the gate refuses (unknown/revoked device, mismatched person, no
-- confirmed seat) — the caller folds NULL to its uniform not-found.
CREATE FUNCTION topos_report_applied(p_ws TEXT, p_person TEXT, p_device TEXT, p_now BIGINT,
                                     p_skill_ids TEXT[], p_commits BYTEA[])
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_principal TEXT;
BEGIN
    SELECT dr.principal INTO v_principal
    FROM device_registry dr
    WHERE dr.workspace_id = p_ws AND dr.device_key_id = p_device AND dr.revoked = 0
    FOR UPDATE;
    IF v_principal IS NULL OR v_principal <> p_person THEN RETURN NULL; END IF;
    IF NOT EXISTS (SELECT 1 FROM workspace_member m
                   WHERE m.workspace_id = p_ws AND m.principal = p_person
                     AND m.status = 'confirmed') THEN
        RETURN NULL;
    END IF;
    UPDATE device_registry SET last_report_at = p_now
    WHERE workspace_id = p_ws AND device_key_id = p_device;
    INSERT INTO device_skill_state (workspace_id, device_key_id, skill_id, applied_commit, reported_at)
    SELECT p_ws, p_device, r.skill_id, r.applied_commit, p_now
    FROM UNNEST(p_skill_ids, p_commits) AS r(skill_id, applied_commit)
    JOIN topos_entitled_skills(p_ws, p_person, p_device) e ON e.skill_id = r.skill_id
    ON CONFLICT (workspace_id, device_key_id, skill_id) DO UPDATE
      SET applied_commit = excluded.applied_commit, reported_at = excluded.reported_at,
          detached = 0, detached_at = NULL;
    DELETE FROM device_skill_state
    WHERE workspace_id = p_ws AND device_key_id = p_device AND detached = 0
      AND skill_id <> ALL(p_skill_ids);
    RETURN 'ok';
END
$$;

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- MAKE THE REPORT/DETACH FENCE REAL. `topos_report_applied` above FOR UPDATE-locks the reporting
-- device's registry row so the applied-state upsert can't be raced. But the lapse-detach reconcile
-- (`topos_detach_lapsed`, the ONE funnel behind unfollow / channel-leave / member-removal) writes
-- `device_skill_state` via `UPDATE … FROM device_registry` — which locks only `device_skill_state`,
-- never the registry row it joins. So a report and a concurrent detach did NOT actually exclude:
-- at READ COMMITTED (both the web tier and the now-autocommit Rust wrapper), a report whose
-- entitlement snapshot predated the detach's commit could `ON CONFLICT DO UPDATE detached = 0` and
-- REVIVE a detach record the plane is deliberately holding — then its DELETE arm could erase the
-- frozen "last known state" the fleet page names as its blind spot. (The old code ran the report
-- under SERIALIZABLE + 40001-retry, which caught this; the rewrite dropped that.)
--
-- Fix: the reconcile takes the SAME lock the report does — it FOR UPDATE-locks the person's
-- registry rows (deterministic order, so two reconciles never deadlock) before its writes. The
-- report holds the reporting device's row (one of the person's), the reconcile wants all of them,
-- so the two mutually exclude on that shared row; whichever runs second re-reads a post-commit
-- snapshot and converges (a report after a detach sees the skill no longer entitled and skips it; a
-- detach after a report re-applies detached = 1). Lock order is acyclic — the report acquires no
-- second registry row after its fence. Behaviour is otherwise byte-identical to 0015's body.
CREATE OR REPLACE FUNCTION topos_detach_lapsed(p_ws TEXT, p_principal TEXT, p_lapsed TEXT[],
                                               p_cause TEXT, p_now BIGINT, p_created_at TEXT)
RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE
    n BIGINT;
BEGIN
    IF p_lapsed IS NULL OR cardinality(p_lapsed) = 0 THEN
        RETURN 0;
    END IF;
    -- Fence against a concurrent applied-state report on any of this person's devices (see above).
    PERFORM 1 FROM device_registry
    WHERE workspace_id = p_ws AND principal = p_principal
    ORDER BY device_key_id
    FOR UPDATE;
    INSERT INTO skill_detachments (workspace_id, principal, skill_id, cause, created_at)
    SELECT p_ws, p_principal, s, p_cause, p_created_at FROM unnest(p_lapsed) AS s
    ON CONFLICT (workspace_id, principal, skill_id) DO NOTHING;
    UPDATE device_skill_state st
    SET detached = 1, detached_at = p_now
    FROM device_registry dr
    WHERE st.workspace_id = p_ws
      AND dr.workspace_id = st.workspace_id AND dr.device_key_id = st.device_key_id
      AND dr.principal = p_principal
      AND st.detached = 0
      AND st.skill_id = ANY(p_lapsed);
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END
$$;

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- REVOCATION IS ONE-WAY, enforced below any grant. `revoked` may go 0→1 (the revoke ceremony) but
-- NEVER 1→0. The web role holds UPDATE(revoked) so it can run `topos_revoke_device`; that same
-- column grant would otherwise let a compromised web tier resurrect a revoked device — and since
-- the workspace credential never expires, the device revoke is the ONLY per-device kill. A trigger
-- makes the column monotonic in the database, so a device revoke stays effective-immediately AND
-- durable (baseline #2) for the DEVICE, matching how a member removal (a row DELETE) is durable for
-- the SEAT. This constrains no legitimate path: the vault only ever writes revoked=1 (the revoke) or
-- INSERTs a fresh device at 0 (the trigger is UPDATE-only); a revoked device's re-enrollment keeps
-- the flag (redeem's ON CONFLICT touches only credential_sha256) and is refused. A last_report_at
-- update leaves revoked unchanged, so it passes.
CREATE FUNCTION topos_revoked_monotonic() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.revoked = 1 AND NEW.revoked = 0 THEN
        RAISE EXCEPTION 'device_registry.revoked is one-way: a revoked device cannot be un-revoked';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER device_registry_revoked_monotonic
    BEFORE UPDATE ON device_registry
    FOR EACH ROW EXECUTE FUNCTION topos_revoked_monotonic();

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- THE WEB ROLE'S GRANTS — the row/byte rule enforced by grants, not convention. `topos_web` gets
-- broad SELECT, EXECUTE on the guarded functions, and DML on EXACTLY the tables those functions
-- touch — UPDATE at COLUMN grain, so the role cannot reach a column no guarded function writes. A
-- table-wide UPDATE on `device_registry` would let a compromised web tier rewrite a device's
-- credential hash and then drive the device lane as that device, which no guarded function can do;
-- the column list below is the whole reach: `revoked` (the revoke ceremony) and `last_report_at`
-- (the fleet clock). Never `credential_sha256`, never `public_key`, never `principal`.
--
-- Role creation stays with provisioning (a migration runs as the schema owner, which cannot
-- CREATE ROLE): the e2e bootstrap, the compose initdb script, and production provisioning all
-- create `topos_plane` + `topos_web` BEFORE first boot. When `topos_web` does not exist (a bare
-- plane-only test database), the block is skipped — a deployment that later adds the role without
-- re-running these grants fails CLOSED (the web tier cannot read), never open.
--
-- Schema-agnostic on purpose: deployments migrate into schema `plane`, the in-crate test databases
-- into `public` — the schema-scoped statements read `current_schema()`, the table grants resolve
-- through the migrator's own search_path, and the default-privileges owner is the migrating role
-- itself (`topos_plane` everywhere it matters).
DO $$
DECLARE
    v_schema TEXT := current_schema();
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'topos_web') THEN
        RAISE NOTICE 'role topos_web absent - skipping web-tier grants';
        RETURN;
    END IF;
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO topos_web', v_schema);
    EXECUTE format('GRANT SELECT ON ALL TABLES IN SCHEMA %I TO topos_web', v_schema);
    EXECUTE format('GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA %I TO topos_web', v_schema);
    EXECUTE format('ALTER DEFAULT PRIVILEGES FOR ROLE %I IN SCHEMA %I GRANT SELECT ON TABLES TO topos_web',
                   current_user, v_schema);
    EXECUTE format('ALTER DEFAULT PRIVILEGES FOR ROLE %I IN SCHEMA %I GRANT EXECUTE ON FUNCTIONS TO topos_web',
                   current_user, v_schema);
    -- INSERT — the rows the guarded functions create.
    GRANT INSERT ON channel_events, channel_members, workspace_member, workspace_policy TO topos_web;
    GRANT INSERT ON skill_detachments, skill_follows, skill_unfollows, device_exclusions TO topos_web;
    GRANT INSERT ON channels, channel_skills, device_skill_state TO topos_web;
    -- DELETE — the rows they retract.
    GRANT DELETE ON channel_members, channel_skills, channels, workspace_member TO topos_web;
    GRANT DELETE ON skill_follows, skill_unfollows, skill_detachments, device_exclusions TO topos_web;
    GRANT DELETE ON device_skill_state TO topos_web;
    -- UPDATE — COLUMN grain, exactly what the guarded functions write.
    GRANT UPDATE (review_required, invite_policy, staleness_window_ms) ON workspace_policy TO topos_web;
    GRANT UPDATE (role, invited_by) ON workspace_member TO topos_web;
    GRANT UPDATE (acked_at) ON notices TO topos_web;
    GRANT UPDATE (name, mode) ON channels TO topos_web;
    GRANT UPDATE (protection) ON catalog TO topos_web;
    GRANT UPDATE (applied_commit, reported_at, detached, detached_at) ON device_skill_state TO topos_web;
    GRANT UPDATE (revoked, last_report_at) ON device_registry TO topos_web;
END
$$;
