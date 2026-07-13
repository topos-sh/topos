-- THE CATALOG LEARNS WHAT KIND OF BUNDLE A NAME POINTS AT. The vault's custody layer is generic —
-- it holds content-addressed bundles and never asks what they are — and the directory's catalog is
-- where a bundle's user-facing identity lives (name, status, protection). This migration adds the
-- missing half of that identity: a `kind` tag, `'skill'` for everything that exists today. A future
-- bundle kind (prompt packs, configs, agent definitions) is a new `kind` value plus surface work —
-- the custody tables and functions below the catalog need nothing.
--
-- `kind` is display metadata on every read surface: clients render it and MUST NOT branch on it
-- (the consent, protection, and delivery machinery is kind-blind by design). The vocabulary is
-- deliberately open (like `notices.kind`), bounded only by a slug-shape CHECK.

-- Add-with-default backfills every existing row in one statement; the column is NOT NULL from birth.
ALTER TABLE catalog
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'skill'
    CHECK (kind ~ '^[a-z][a-z0-9-]*$' AND length(kind) <= 40);

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- THE DELIVERY PREDICATE gains the catalog's `kind` (a RETURNS TABLE shape change, so drop-and-
-- recreate — CREATE OR REPLACE cannot change a function's return type). Body otherwise verbatim
-- from its previous definition; every consumer selects columns by name, so the appended column is
-- additive.
DROP FUNCTION topos_entitled_skills(TEXT, TEXT, TEXT);
CREATE FUNCTION topos_entitled_skills(p_ws TEXT, p_principal TEXT, p_device TEXT)
RETURNS TABLE (
    skill_id TEXT, name TEXT, display_name TEXT, protection TEXT,
    commit_id BYTEA, epoch BIGINT, seq BIGINT, updated_at BIGINT, bundle_digest BYTEA,
    via_channels TEXT[], direct BIGINT, kind TEXT
) LANGUAGE sql STABLE AS $$
    SELECT e.skill_id, cat.name, cat.display_name,
           COALESCE(cat.protection,
                    CASE WHEN wp.review_required = 1 THEN 'reviewed' ELSE 'open' END,
                    'open'),
           cur.commit_id, cur.epoch, cur.seq, cur.updated_at, sc.bundle_digest,
           e.via_channels, e.direct, cat.kind
    FROM topos_person_entitled(p_ws, p_principal) e
    JOIN catalog cat ON cat.workspace_id = p_ws AND cat.skill_id = e.skill_id AND cat.status = 'active'
    JOIN current cur ON cur.workspace_id = p_ws AND cur.skill_id = e.skill_id
    JOIN skill_commit sc ON sc.workspace_id = p_ws AND sc.commit_id = cur.commit_id
    LEFT JOIN workspace_policy wp ON wp.workspace_id = p_ws
    WHERE NOT EXISTS (SELECT 1 FROM device_exclusions dx
                      WHERE dx.workspace_id = p_ws AND dx.device_key_id = p_device
                        AND dx.skill_id = e.skill_id)
    ORDER BY cat.name
$$;

-- ─────────────────────────────────────────────────────────────────────────────────────────────────
-- THE DELIVERY BODY rides `kind` on every skill object (same return type, so CREATE OR REPLACE —
-- existing grants survive). Body otherwise verbatim from its previous definition.
CREATE OR REPLACE FUNCTION topos_delivery(p_ws TEXT, p_person TEXT, p_device TEXT)
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
                       'kind', e.kind,
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
-- Grants. The recreated function loses its grants at DROP; the migrating role's default privileges
-- re-cover it, but the grant is recorded explicitly so the role's shape is readable from the
-- migrations alone. Role-guarded like every grant block: a bare test database (no roles) skips it
-- and fails closed. The new `kind` column itself needs nothing — `topos_web` holds table-level
-- SELECT on `catalog`, and `kind` is display-only (no web-tier write).
DO $$
DECLARE
    v_schema TEXT := current_schema();
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'topos_web') THEN
        RAISE NOTICE 'role topos_web absent - skipping web-tier grants';
        RETURN;
    END IF;
    EXECUTE format('GRANT EXECUTE ON FUNCTION %I.topos_entitled_skills(TEXT, TEXT, TEXT) TO topos_web',
                   v_schema);
END
$$;
