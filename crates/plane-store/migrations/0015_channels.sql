-- Channels — the distribution unit: the catalog (the first real name→skill mapping), channels +
-- references + person-scoped membership, person-scoped subscriptions (follows / unfollows), per-device
-- exclusions, person-scoped notices with read-state, the fleet applied-state table, per-bundle
-- protection, and the guarded policy functions the Rust plane calls today and the web tier calls at
-- the door cutover. Appended (never edits an applied migration's bytes — sqlx checksums the file).
--
-- Same posture as 0001-0014: every row `workspace_id`-scoped; content ids raw 32-byte sha256 BYTEA,
-- width-checked; mutable/deadline times BIGINT epoch MILLISECONDS; audit `created_at` TEXT ISO-8601;
-- booleans BIGINT 0/1 CHECK-bound; principals stored in the canonical lowercase fold (0010's rule,
-- pinned here with the same `lower(… COLLATE "C")` CHECKs).
--
-- POLICY LIVES HERE: every policy write with logic (curation, membership, protection, lifecycle,
-- subscriptions) is a named `topos_*` SQL function — the single implementation, called by Rust now
-- and by the web tier later; triggers exist ONLY for the append-only channel audit log. The
-- entitlement predicate (what a device should have) is ONE set-returning function extending the
-- confirmed-membership predicate every lane already gates on.

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- CATALOG — name → skill. The first real catalog: until now a skill was only its `skill_id` text.
-- `skill_id` stays the IMMUTABLE internal key every custody table already uses (client-minted at
-- birth, never changes); `name` is the mutable user-facing key (unique per workspace) so
-- rename-on-archive and id-keyed follows/channel references touch ONLY this table, never custody.
-- `display_name` absorbs `current.display_name` (advisory, last-writer-wins, dropped below).
-- `protection` is the per-bundle gate: NULL = follow the workspace default (the cascade —
-- `workspace_policy.review_required` IS that default), 'open'/'reviewed' = explicitly pinned.
-- `status` is the lifecycle: active → archived → deleted (delete keeps the row as a tombstone
-- under its archived name; content is dropped, audit survives).
CREATE TABLE catalog (
    workspace_id TEXT   NOT NULL,
    skill_id     TEXT   NOT NULL,
    name         TEXT   NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9-]*$' AND length(name) <= 200),
    display_name TEXT,
    status       TEXT   NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived', 'deleted')),
    protection   TEXT            CHECK (protection IS NULL OR protection IN ('open', 'reviewed')),
    -- The name the skill carried before an archive renamed it — recorded so unarchive restores it
    -- EXACTLY, with no name parsing (a skill legitimately named `foo-archived-2026-07-11` would
    -- defeat any suffix-stripping regex). NULL unless archived.
    base_name    TEXT,
    archived_at  BIGINT,
    deleted_at   BIGINT,
    created_at   TEXT   NOT NULL,
    PRIMARY KEY (workspace_id, skill_id)
);

-- One name, one skill — per workspace, across every status: an archived rename frees the base name
-- precisely BECAUSE the renamed entry still occupies its suffixed name, and a delete tombstones
-- under the archived name (which therefore stays reserved).
CREATE UNIQUE INDEX catalog_by_name ON catalog (workspace_id, name);

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- CHANNELS — named groups: members × skill REFERENCES + audit. Plain rows; no versioning, nothing
-- signs. `channel_id` is the immutable key (the name at birth); `name` is the mutable display key
-- (owners rename on the web, later). `everyone` is STRUCTURAL: `builtin = 1`, membership derived
-- from the confirmed workspace roster — it has NO channel_members rows, cannot be joined, left,
-- deleted, or renamed (mode stays mutable: an org can mark #everyone curated). Enforced below by
-- trigger guards — invariants in the database, not convention.
CREATE TABLE channels (
    workspace_id TEXT   NOT NULL,
    channel_id   TEXT   NOT NULL,
    name         TEXT   NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9-]*$' AND length(name) <= 200),
    mode         TEXT   NOT NULL DEFAULT 'open' CHECK (mode IN ('open', 'curated')),
    builtin      BIGINT NOT NULL DEFAULT 0 CHECK (builtin IN (0, 1)),
    created_by   TEXT,
    created_at   TEXT   NOT NULL,
    PRIMARY KEY (workspace_id, channel_id)
);

CREATE UNIQUE INDEX channels_by_name ON channels (workspace_id, name);

-- The references a channel holds (Gmail labels, not folders): a skill in three channels is one
-- skill, delivered once — the entitlement union DISTINCTs over these.
CREATE TABLE channel_skills (
    workspace_id TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    skill_id     TEXT NOT NULL,
    added_by     TEXT NOT NULL,
    added_at     TEXT NOT NULL,
    PRIMARY KEY (workspace_id, channel_id, skill_id),
    FOREIGN KEY (workspace_id, channel_id) REFERENCES channels (workspace_id, channel_id),
    FOREIGN KEY (workspace_id, skill_id)   REFERENCES catalog  (workspace_id, skill_id)
);

-- The entitlement union probes by member+skill through the channel; this inverse index serves the
-- per-skill "which channels deliver it" attribution without a table scan.
CREATE INDEX channel_skills_by_skill ON channel_skills (workspace_id, skill_id);

-- PERSON-scoped channel membership (subscriptions belong to the person; devices hold state, never
-- subscriptions). Self-serve join/leave; `added_by` NULL = self-serve, else the pre-placing inviter.
CREATE TABLE channel_members (
    workspace_id TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    principal    TEXT NOT NULL CHECK (principal = lower(principal COLLATE "C")),
    added_by     TEXT,
    added_at     TEXT NOT NULL,
    PRIMARY KEY (workspace_id, channel_id, principal),
    FOREIGN KEY (workspace_id, channel_id) REFERENCES channels (workspace_id, channel_id)
);

CREATE INDEX channel_members_by_principal ON channel_members (workspace_id, principal);

-- The append-only channel audit — TRIGGER-emitted on every curation/membership/existence write, so
-- no write path can skip it (the one sanctioned trigger use: audit emission, no business logic).
-- `actor` rides the transaction-local `topos.actor` setting the guarded functions set; a write that
-- bypassed a guarded function is still recorded, attributed 'unattributed'. IDENTITY is the one
-- deliberate departure from "no serial columns": an audit log needs a total order and has no
-- natural content key.
CREATE TABLE channel_events (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    event        TEXT NOT NULL,
    skill_id     TEXT,
    principal    TEXT,
    actor        TEXT NOT NULL,
    created_at   TEXT NOT NULL
);

CREATE INDEX channel_events_by_channel ON channel_events (workspace_id, channel_id, id);

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- PERSON-scoped subscriptions. `skill_follows` = direct follows (survive a channel dropping the
-- skill); `skill_unfollows` = the standing negative mask (subtracted from the whole union, so an
-- unfollowed skill stays out however many channels deliver it). The guarded functions keep the two
-- mutually exclusive per (person, skill).
CREATE TABLE skill_follows (
    workspace_id TEXT NOT NULL,
    principal    TEXT NOT NULL CHECK (principal = lower(principal COLLATE "C")),
    skill_id     TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, principal, skill_id),
    FOREIGN KEY (workspace_id, skill_id) REFERENCES catalog (workspace_id, skill_id)
);

CREATE INDEX skill_follows_by_skill ON skill_follows (workspace_id, skill_id);

CREATE TABLE skill_unfollows (
    workspace_id TEXT NOT NULL,
    principal    TEXT NOT NULL CHECK (principal = lower(principal COLLATE "C")),
    skill_id     TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, principal, skill_id),
    FOREIGN KEY (workspace_id, skill_id) REFERENCES catalog (workspace_id, skill_id)
);

-- PERSON-SCOPED DETACHMENT RECORDS — the who-acts signal the delivery response carries. A row here
-- means "this PERSON's own act stopped delivering this skill to them" (an explicit unfollow, a
-- channel they left, a membership that was removed), so every device FREEZES the copy in place
-- rather than cleaning it. Distinct from `skill_unfollows` (the standing negative MASK subtracted
-- from the union): a detachment is a RECORD of a lapse, never a mask — a later channel join or
-- curation placement re-entitles the skill and the record is cleared (entitlement always wins).
-- Person-scoped, so it is correct for a device that has never reported (a fleet row need not exist).
CREATE TABLE skill_detachments (
    workspace_id TEXT NOT NULL,
    principal    TEXT NOT NULL CHECK (principal = lower(principal COLLATE "C")),
    skill_id     TEXT NOT NULL,
    cause        TEXT NOT NULL CHECK (cause IN ('unfollow', 'channel_leave', 'membership_removed')),
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, principal, skill_id),
    FOREIGN KEY (workspace_id, skill_id) REFERENCES catalog (workspace_id, skill_id)
);

-- Per-DEVICE exclusions — "not on this device": written by `remove` on a followed skill, cleared by
-- `follow` on that device; every other device keeps receiving.
CREATE TABLE device_exclusions (
    workspace_id  TEXT NOT NULL,
    device_key_id TEXT NOT NULL,
    skill_id      TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    PRIMARY KEY (workspace_id, device_key_id, skill_id),
    FOREIGN KEY (workspace_id, skill_id) REFERENCES catalog (workspace_id, skill_id)
);

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- NOTICES — person-scoped, with read-state. The delivery response carries the unacked rows; the
-- silent hook fetches without acking; an interactive session acks by id (the ack write is a later
-- surface). `kind` is an open vocabulary ('verdict', 'proposal_closed', …new classes ride free);
-- the typed columns cover the emitters that exist today (verdicts with reasons, auto-closures).
CREATE TABLE notices (
    workspace_id TEXT  NOT NULL,
    id           TEXT  NOT NULL,
    principal    TEXT  NOT NULL CHECK (principal = lower(principal COLLATE "C")),
    kind         TEXT  NOT NULL,
    skill_id     TEXT,
    version_id   BYTEA          CHECK (version_id IS NULL OR octet_length(version_id) = 32),
    actor        TEXT,
    outcome      TEXT,
    reason       TEXT,
    message      TEXT,
    created_at   TEXT  NOT NULL,
    acked_at     BIGINT,
    PRIMARY KEY (workspace_id, id)
);

-- The delivery read: this person's unacked notices, oldest first.
CREATE INDEX notices_unacked ON notices (workspace_id, principal, created_at) WHERE acked_at IS NULL;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- FLEET — the applied-state report (device × skill × applied version) the update call upserts; the
-- dashboard's staleness window reads `device_registry.last_report_at`. `detached = 1` is the FINAL
-- DETACH RECORD (skill + last-applied version, frozen at unfollow/lapse time): the snapshot upsert
-- never touches a detached row, so the fleet page can name its blind spots ("detached — last known
-- state") instead of hiding them; a re-follow re-attaches (flips it live again). Removed members'
-- rows stay audit-retained — removal deletes the seat, never this history.
CREATE TABLE device_skill_state (
    workspace_id   TEXT   NOT NULL,
    device_key_id  TEXT   NOT NULL,
    skill_id       TEXT   NOT NULL,
    applied_commit BYTEA          CHECK (applied_commit IS NULL OR octet_length(applied_commit) = 32),
    reported_at    BIGINT NOT NULL,
    detached       BIGINT NOT NULL DEFAULT 0 CHECK (detached IN (0, 1)),
    detached_at    BIGINT,
    PRIMARY KEY (workspace_id, device_key_id, skill_id)
);

CREATE INDEX device_skill_state_by_skill ON device_skill_state (workspace_id, skill_id);

ALTER TABLE device_registry ADD COLUMN last_report_at BIGINT;

-- ────────────────────────────────────────────────────────────────────────────────────────────────
-- VERSION PURGE — the leak tool's tombstone: one version's bytes dropped, its hash kept in history
-- with who/when. The columns live on the provenance row (the version IS the skill_commit row); the
-- purge function below un-roots the version's `commit_object` edges so the shipped GC reclaims the
-- blobs no live version still roots. Deliberately NOT the object-denylist `tombstones` table:
-- content-addressed bytes may legitimately reappear in a future version — purge drops storage, it
-- does not ban byte patterns.
ALTER TABLE skill_commit ADD COLUMN purged_at BIGINT;
ALTER TABLE skill_commit ADD COLUMN purged_by TEXT;

-- Proposals gain the circumstantial terminal state: 'closed' = no human verdict — the skill was
-- archived or the base/candidate purged (reason in `resolved_reason`). Distinct from 'rejected'
-- (a reviewer's decision, carrying their reason back to the author). Every open/staleness/GC
-- predicate keys on 'open' and is untouched.
ALTER TABLE proposals DROP CONSTRAINT proposals_status_check;
ALTER TABLE proposals ADD CONSTRAINT proposals_status_check
    CHECK (status IN ('open', 'accepted', 'rejected', 'closed'));

-- ════════════════════════════════════════════════════════════════════════════════════════════════
-- TRIGGERS — the channel audit log + the structural-`everyone` guards. No business logic here:
-- emission and invariant protection only.
-- ════════════════════════════════════════════════════════════════════════════════════════════════

-- Actor/time attribution for trigger-emitted audit rows: the guarded functions set the
-- transaction-local `topos.actor` / `topos.created_at`; a bypassing write is still recorded.
CREATE FUNCTION topos_audit_actor() RETURNS TEXT LANGUAGE sql STABLE AS $$
    SELECT COALESCE(NULLIF(current_setting('topos.actor', true), ''), 'unattributed')
$$;

CREATE FUNCTION topos_audit_created_at() RETURNS TEXT LANGUAGE sql STABLE AS $$
    SELECT COALESCE(NULLIF(current_setting('topos.created_at', true), ''),
                    to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'))
$$;

CREATE FUNCTION topos_channel_audit() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME = 'channels' THEN
        IF TG_OP = 'INSERT' THEN
            INSERT INTO channel_events (workspace_id, channel_id, event, actor, created_at)
            VALUES (NEW.workspace_id, NEW.channel_id, 'channel_created', topos_audit_actor(), topos_audit_created_at());
            RETURN NEW;
        ELSIF TG_OP = 'UPDATE' THEN
            IF NEW.mode IS DISTINCT FROM OLD.mode THEN
                INSERT INTO channel_events (workspace_id, channel_id, event, actor, created_at)
                VALUES (NEW.workspace_id, NEW.channel_id, 'mode_' || NEW.mode, topos_audit_actor(), topos_audit_created_at());
            END IF;
            IF NEW.name IS DISTINCT FROM OLD.name THEN
                INSERT INTO channel_events (workspace_id, channel_id, event, actor, created_at)
                VALUES (NEW.workspace_id, NEW.channel_id, 'channel_renamed', topos_audit_actor(), topos_audit_created_at());
            END IF;
            RETURN NEW;
        ELSE
            INSERT INTO channel_events (workspace_id, channel_id, event, actor, created_at)
            VALUES (OLD.workspace_id, OLD.channel_id, 'channel_deleted', topos_audit_actor(), topos_audit_created_at());
            RETURN OLD;
        END IF;
    ELSIF TG_TABLE_NAME = 'channel_skills' THEN
        IF TG_OP = 'INSERT' THEN
            INSERT INTO channel_events (workspace_id, channel_id, event, skill_id, actor, created_at)
            VALUES (NEW.workspace_id, NEW.channel_id, 'skill_added', NEW.skill_id, topos_audit_actor(), topos_audit_created_at());
            RETURN NEW;
        ELSE
            INSERT INTO channel_events (workspace_id, channel_id, event, skill_id, actor, created_at)
            VALUES (OLD.workspace_id, OLD.channel_id, 'skill_removed', OLD.skill_id, topos_audit_actor(), topos_audit_created_at());
            RETURN OLD;
        END IF;
    ELSE
        IF TG_OP = 'INSERT' THEN
            INSERT INTO channel_events (workspace_id, channel_id, event, principal, actor, created_at)
            VALUES (NEW.workspace_id, NEW.channel_id, 'member_joined', NEW.principal, topos_audit_actor(), topos_audit_created_at());
            RETURN NEW;
        ELSE
            INSERT INTO channel_events (workspace_id, channel_id, event, principal, actor, created_at)
            VALUES (OLD.workspace_id, OLD.channel_id, 'member_left', OLD.principal, topos_audit_actor(), topos_audit_created_at());
            RETURN OLD;
        END IF;
    END IF;
END
$$;

CREATE TRIGGER channels_audit AFTER INSERT OR UPDATE OR DELETE ON channels
    FOR EACH ROW EXECUTE FUNCTION topos_channel_audit();
CREATE TRIGGER channel_skills_audit AFTER INSERT OR DELETE ON channel_skills
    FOR EACH ROW EXECUTE FUNCTION topos_channel_audit();
CREATE TRIGGER channel_members_audit AFTER INSERT OR DELETE ON channel_members
    FOR EACH ROW EXECUTE FUNCTION topos_channel_audit();

-- The structural-`everyone` invariants, held in the database itself: undeletable, unrenameable
-- (mode stays mutable), and never carrying membership rows (its membership IS the roster).
CREATE FUNCTION topos_guard_builtin_channel() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF OLD.builtin = 1 THEN
            RAISE EXCEPTION 'the everyone channel is structural and cannot be deleted';
        END IF;
        RETURN OLD;
    END IF;
    IF NEW.builtin = 1 AND NEW.name IS DISTINCT FROM OLD.name THEN
        RAISE EXCEPTION 'the everyone channel is structural and cannot be renamed';
    END IF;
    IF NEW.builtin IS DISTINCT FROM OLD.builtin THEN
        RAISE EXCEPTION 'builtin is immutable';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER channels_guard_builtin BEFORE UPDATE OR DELETE ON channels
    FOR EACH ROW EXECUTE FUNCTION topos_guard_builtin_channel();

CREATE FUNCTION topos_guard_builtin_membership() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM channels c WHERE c.workspace_id = NEW.workspace_id
               AND c.channel_id = NEW.channel_id AND c.builtin = 1) THEN
        RAISE EXCEPTION 'the everyone channel derives its membership from the roster';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER channel_members_guard_builtin BEFORE INSERT ON channel_members
    FOR EACH ROW EXECUTE FUNCTION topos_guard_builtin_membership();

-- ════════════════════════════════════════════════════════════════════════════════════════════════
-- GUARDED POLICY FUNCTIONS — the one policy implementation. Role gates read `workspace_member`
-- INSIDE the function (defense in depth: callers gate too, but the database answer is authoritative
-- for every caller, Rust today and TS at the cutover). Outcome codes are TEXT — the callers map
-- them to their surfaces' typed errors.
-- ════════════════════════════════════════════════════════════════════════════════════════════════

-- The confirmed role, or NULL: the one role probe every function shares.
CREATE FUNCTION topos_member_role(p_ws TEXT, p_principal TEXT) RETURNS TEXT LANGUAGE sql STABLE AS $$
    SELECT role FROM workspace_member
    WHERE workspace_id = p_ws AND principal = p_principal AND status = 'confirmed'
$$;

-- Every workspace is born with `everyone` (idempotent — genesis paths, standup doors, and the
-- backfill below all converge here).
CREATE FUNCTION topos_ensure_everyone(p_ws TEXT, p_created_at TEXT) RETURNS VOID LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO channels (workspace_id, channel_id, name, mode, builtin, created_by, created_at)
    VALUES (p_ws, 'everyone', 'everyone', 'open', 1, NULL, p_created_at)
    ON CONFLICT (workspace_id, channel_id) DO NOTHING;
END
$$;

-- THE ENTITLEMENT UNION CORE — person-scoped, the ONE SQL home every consumer reads (the delivery
-- function below, the lapse-detach reconcile, the re-attach): DISTINCT union of roster-derived
-- `everyone` ∪ followed channels ∪ direct follows − unfollowed skills. Membership gates the WHOLE
-- union (access is a confirmed seat — deleting it silences every source at once); `everyone` needs
-- no membership rows (builtin ⇒ every confirmed member). Device-scoped subtraction (exclusions)
-- and current/status joins live in topos_entitled_skills — this core is the person's standing
-- subscription surface.
CREATE FUNCTION topos_person_entitled(p_ws TEXT, p_principal TEXT)
RETURNS TABLE (skill_id TEXT, via_channels TEXT[], direct BIGINT) LANGUAGE sql STABLE AS $$
    WITH sources AS (
        SELECT cs.skill_id, ch.name AS via, 0::BIGINT AS direct
        FROM channel_skills cs
        JOIN channels ch ON ch.workspace_id = cs.workspace_id AND ch.channel_id = cs.channel_id
        WHERE cs.workspace_id = p_ws
          AND (ch.builtin = 1 OR EXISTS (
                SELECT 1 FROM channel_members cm
                WHERE cm.workspace_id = cs.workspace_id AND cm.channel_id = cs.channel_id
                  AND cm.principal = p_principal))
        UNION ALL
        SELECT f.skill_id, NULL, 1::BIGINT
        FROM skill_follows f
        WHERE f.workspace_id = p_ws AND f.principal = p_principal
    )
    SELECT s.skill_id,
           COALESCE(array_agg(s.via ORDER BY s.via) FILTER (WHERE s.via IS NOT NULL), '{}'),
           MAX(s.direct)
    FROM sources s
    WHERE EXISTS (SELECT 1 FROM workspace_member m
                  WHERE m.workspace_id = p_ws AND m.principal = p_principal AND m.status = 'confirmed')
      AND NOT EXISTS (SELECT 1 FROM skill_unfollows u
                      WHERE u.workspace_id = p_ws AND u.principal = p_principal AND u.skill_id = s.skill_id)
    GROUP BY s.skill_id
$$;

-- THE DELIVERY PREDICATE — what THIS DEVICE should have: the person's union, minus this device's
-- exclusions, active catalog entries only, skipping current-less skills. Effective protection is
-- resolved here too (per-bundle pin, else the workspace default, else open) so the client's consent
-- posture and the publish gate read the same cascade.
CREATE FUNCTION topos_entitled_skills(p_ws TEXT, p_principal TEXT, p_device TEXT)
RETURNS TABLE (
    skill_id TEXT, name TEXT, display_name TEXT, protection TEXT,
    commit_id BYTEA, epoch BIGINT, seq BIGINT, updated_at BIGINT, bundle_digest BYTEA,
    via_channels TEXT[], direct BIGINT
) LANGUAGE sql STABLE AS $$
    SELECT e.skill_id, cat.name, cat.display_name,
           COALESCE(cat.protection,
                    CASE WHEN wp.review_required = 1 THEN 'reviewed' ELSE 'open' END,
                    'open'),
           cur.commit_id, cur.epoch, cur.seq, cur.updated_at, sc.bundle_digest,
           e.via_channels, e.direct
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

-- The effective per-bundle protection on its own — the publish/revert gate's read (same cascade as
-- the delivery read; an unregistered skill answers the workspace default).
CREATE FUNCTION topos_effective_protection(p_ws TEXT, p_skill TEXT) RETURNS TEXT LANGUAGE sql STABLE AS $$
    SELECT COALESCE(
        (SELECT cat.protection FROM catalog cat WHERE cat.workspace_id = p_ws AND cat.skill_id = p_skill),
        (SELECT CASE WHEN wp.review_required = 1 THEN 'reviewed' ELSE 'open' END
         FROM workspace_policy wp WHERE wp.workspace_id = p_ws),
        'open')
$$;

-- The person's currently-entitled skill ids, as an array — the before/after snapshots every
-- person-scoped event takes so the lapse reconcile below detaches EXACTLY what that event lapsed
-- (never what an unrelated upstream act — a curator's unplace, an archive — removed: those are
-- UPSTREAM withdrawals, whose contract is that the client CLEANS the agent dirs, not freezes them;
-- misrecording one as a person detach would freeze withdrawn bytes on the fleet forever).
CREATE FUNCTION topos_entitled_ids(p_ws TEXT, p_principal TEXT) RETURNS TEXT[] LANGUAGE sql STABLE AS $$
    SELECT COALESCE(array_agg(e.skill_id ORDER BY e.skill_id), '{}')
    FROM topos_person_entitled(p_ws, p_principal) e
$$;

-- LAPSE-DETACH — the reconcile behind every entitlement-losing PERSON event (unfollow, channel
-- leave, membership removal). `p_lapsed` is the EXACT set that event lapsed (the caller's
-- before − after), so an unrelated skill the person still receives — or one an UPSTREAM act
-- removed — is never mislabelled a person detach. Writes the person-scoped detachment RECORD (the
-- who-acts signal the delivery response carries; correct even for a device that never reported) and
-- freezes the matching fleet rows at their last-applied version (the dashboard's blind-spot list).
CREATE FUNCTION topos_detach_lapsed(p_ws TEXT, p_principal TEXT, p_lapsed TEXT[], p_cause TEXT,
                                    p_now BIGINT, p_created_at TEXT) RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE
    n BIGINT;
BEGIN
    IF p_lapsed IS NULL OR cardinality(p_lapsed) = 0 THEN
        RETURN 0;
    END IF;
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

-- RE-ATTACH — the inverse, and the SELF-HEAL: any skill the person is entitled to again (their own
-- follow/join, but equally a curator re-placing it or their membership being restored) has its
-- detachment record dropped and its fleet rows revived. Entitlement always wins over a stale
-- detachment — so no record can strand a live subscription.
CREATE FUNCTION topos_reattach(p_ws TEXT, p_principal TEXT) RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE
    n BIGINT;
BEGIN
    DELETE FROM skill_detachments d
    WHERE d.workspace_id = p_ws AND d.principal = p_principal
      AND EXISTS (SELECT 1 FROM topos_person_entitled(p_ws, p_principal) e WHERE e.skill_id = d.skill_id);
    UPDATE device_skill_state st
    SET detached = 0, detached_at = NULL
    FROM device_registry dr
    WHERE st.workspace_id = p_ws
      AND dr.workspace_id = st.workspace_id AND dr.device_key_id = st.device_key_id
      AND dr.principal = p_principal
      AND st.detached = 1
      AND EXISTS (SELECT 1 FROM topos_person_entitled(p_ws, p_principal) e WHERE e.skill_id = st.skill_id);
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END
$$;

-- SELF-HEAL, skill-scoped — the inverse of a lapse for ONE skill, across everyone who had detached
-- it: any principal now entitled to it again (a curator re-placed it, an owner unarchived it) has
-- its detachment record dropped and its fleet rows revived. Entitlement always wins over a stale
-- record, so no upstream re-entitlement can strand a subscription behind a detach the person never
-- meant to be permanent.
CREATE FUNCTION topos_heal_detachments(p_ws TEXT, p_skill TEXT) RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE
    n BIGINT;
BEGIN
    DELETE FROM skill_detachments d
    WHERE d.workspace_id = p_ws AND d.skill_id = p_skill
      AND EXISTS (SELECT 1 FROM topos_person_entitled(p_ws, d.principal) e WHERE e.skill_id = p_skill);
    UPDATE device_skill_state st
    SET detached = 0, detached_at = NULL
    FROM device_registry dr
    WHERE st.workspace_id = p_ws AND st.skill_id = p_skill AND st.detached = 1
      AND dr.workspace_id = st.workspace_id AND dr.device_key_id = st.device_key_id
      AND EXISTS (SELECT 1 FROM topos_person_entitled(p_ws, dr.principal) e WHERE e.skill_id = p_skill);
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END
$$;

-- MEMBERSHIP REMOVAL's lapse reconcile — called by the roster-removal transactions BEFORE the seat
-- is deleted (the entitlement union is membership-gated, so it reads empty once the seat is gone).
-- Everything the person received lapses at once: their devices freeze the copies in place and the
-- fleet page names the blind spot ("removed — last known state"). The credential rows stay: re-adding
-- the member re-enables the same devices (the git/GitHub model), and `topos_reattach` revives them.
CREATE FUNCTION topos_detach_on_removal(p_ws TEXT, p_principal TEXT, p_now BIGINT, p_created_at TEXT)
RETURNS BIGINT LANGUAGE plpgsql AS $$
BEGIN
    RETURN topos_detach_lapsed(p_ws, p_principal, topos_entitled_ids(p_ws, p_principal),
                               'membership_removed', p_now, p_created_at);
END
$$;

-- CURATION: place a skill reference — creating the channel on FIRST use (member-level self-serve;
-- founder-resolved). Gates: confirmed member; the skill active in the catalog; on a CURATED channel
-- reviewer+ (symmetric with removal); `everyone` is placeable like any channel (its skill list is
-- curated; only its membership is structural).
CREATE FUNCTION topos_channel_place(p_ws TEXT, p_channel_name TEXT, p_skill TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
    v_channel_id TEXT;
    v_mode TEXT;
    v_created BOOLEAN := false;
BEGIN
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    SELECT status INTO v_status FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'active' THEN RETURN 'skill_not_active'; END IF;
    SELECT channel_id, mode INTO v_channel_id, v_mode FROM channels
    WHERE workspace_id = p_ws AND name = p_channel_name;
    IF v_channel_id IS NULL THEN
        IF p_channel_name !~ '^[a-z0-9][a-z0-9-]*$' OR length(p_channel_name) > 64 THEN
            RETURN 'bad_name';
        END IF;
        INSERT INTO channels (workspace_id, channel_id, name, mode, builtin, created_by, created_at)
        VALUES (p_ws, p_channel_name, p_channel_name, 'open', 0, p_actor, p_created_at);
        v_channel_id := p_channel_name;
        v_mode := 'open';
        v_created := true;
    END IF;
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

-- CURATION: remove a skill reference — symmetric with place (member on open, reviewer+ on curated).
-- The channel and the skill keep existing either way.
CREATE FUNCTION topos_channel_unplace(p_ws TEXT, p_channel_name TEXT, p_skill TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_channel_id TEXT;
    v_mode TEXT;
    n BIGINT;
BEGIN
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    SELECT channel_id, mode INTO v_channel_id, v_mode FROM channels
    WHERE workspace_id = p_ws AND name = p_channel_name;
    IF v_channel_id IS NULL THEN RETURN 'unknown_channel'; END IF;
    IF v_mode = 'curated' AND v_role NOT IN ('reviewer', 'owner') THEN
        RETURN 'curated_role_required';
    END IF;
    DELETE FROM channel_skills
    WHERE workspace_id = p_ws AND channel_id = v_channel_id AND skill_id = p_skill;
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n = 0 THEN RETURN 'not_placed'; END IF;
    RETURN 'removed';
END
$$;

-- MEMBERSHIP: join a channel — always self-serve for a confirmed member; `everyone` refuses (you
-- are already in it, structurally).
CREATE FUNCTION topos_channel_join(p_ws TEXT, p_channel_name TEXT, p_principal TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_channel_id TEXT;
    v_builtin BIGINT;
BEGIN
    PERFORM set_config('topos.actor', p_principal, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    IF topos_member_role(p_ws, p_principal) IS NULL THEN RETURN 'member_required'; END IF;
    SELECT channel_id, builtin INTO v_channel_id, v_builtin FROM channels
    WHERE workspace_id = p_ws AND name = p_channel_name;
    IF v_channel_id IS NULL THEN RETURN 'unknown_channel'; END IF;
    IF v_builtin = 1 THEN RETURN 'builtin'; END IF;
    INSERT INTO channel_members (workspace_id, channel_id, principal, added_by, added_at)
    VALUES (p_ws, v_channel_id, p_principal, NULL, p_created_at)
    ON CONFLICT (workspace_id, channel_id, principal) DO NOTHING;
    PERFORM topos_reattach(p_ws, p_principal);
    RETURN 'joined';
END
$$;

-- MEMBERSHIP: leave a channel — self-serve; `everyone` cannot be left (it mirrors the roster;
-- unfollow its skills individually). Leaving runs the lapse-detach reconcile: skills this person
-- now receives from NO source get their final per-device detach records (reference counting via
-- the union — a skill another followed channel still references stays live).
CREATE FUNCTION topos_channel_leave(p_ws TEXT, p_channel_name TEXT, p_principal TEXT, p_now BIGINT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_channel_id TEXT;
    v_builtin BIGINT;
    v_before TEXT[];
    n BIGINT;
BEGIN
    PERFORM set_config('topos.actor', p_principal, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    SELECT channel_id, builtin INTO v_channel_id, v_builtin FROM channels
    WHERE workspace_id = p_ws AND name = p_channel_name;
    IF v_channel_id IS NULL THEN RETURN 'unknown_channel'; END IF;
    IF v_builtin = 1 THEN RETURN 'builtin'; END IF;
    -- The BEFORE snapshot: the lapse reconcile below detaches exactly (before − after), so leaving
    -- a channel freezes only the skills THIS leave cost the person — a skill another followed
    -- channel still delivers stays live (reference counting via the union), and a skill an upstream
    -- act removed is NOT mislabelled a person detach.
    v_before := topos_entitled_ids(p_ws, p_principal);
    DELETE FROM channel_members
    WHERE workspace_id = p_ws AND channel_id = v_channel_id AND principal = p_principal;
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n = 0 THEN RETURN 'not_member'; END IF;
    PERFORM topos_detach_lapsed(
        p_ws, p_principal,
        ARRAY(SELECT unnest(v_before) EXCEPT SELECT unnest(topos_entitled_ids(p_ws, p_principal))),
        'channel_leave', p_now, p_created_at);
    RETURN 'left';
END
$$;

-- SUBSCRIPTION: direct-follow a skill — clears the person's unfollow mask, this device's exclusion
-- (when a device is named), and re-attaches. A direct follow survives any channel dropping the
-- skill. Archived skills refuse (a freed name is a NEW identity; the old one is out of circulation).
CREATE FUNCTION topos_follow_skill(p_ws TEXT, p_principal TEXT, p_skill TEXT, p_device TEXT, p_created_at TEXT)
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
    IF p_device IS NOT NULL THEN
        DELETE FROM device_exclusions WHERE workspace_id = p_ws AND device_key_id = p_device AND skill_id = p_skill;
    END IF;
    PERFORM topos_reattach(p_ws, p_principal);
    RETURN 'followed';
END
$$;

-- SUBSCRIPTION: unfollow a skill — the standing negative mask (delivery ends on ALL the person's
-- devices, whatever channels still reference it) + the final detach records. `follow` re-attaches.
CREATE FUNCTION topos_unfollow_skill(p_ws TEXT, p_principal TEXT, p_skill TEXT, p_now BIGINT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_before TEXT[];
BEGIN
    -- The membership gate, IN the function (defense in depth): the callers gate too, but the database
    -- answer must be authoritative for EVERY caller — the web tier calls these same functions
    -- directly at the door cutover, and an ungated one would let a caller bug write durable mask
    -- rows for a principal with no seat, silently killing a member's delivery.
    IF topos_member_role(p_ws, p_principal) IS NULL THEN RETURN 'member_required'; END IF;
    IF NOT EXISTS (SELECT 1 FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill) THEN
        RETURN 'unknown_skill';
    END IF;
    v_before := topos_entitled_ids(p_ws, p_principal);
    INSERT INTO skill_unfollows (workspace_id, principal, skill_id, created_at)
    VALUES (p_ws, p_principal, p_skill, p_created_at)
    ON CONFLICT (workspace_id, principal, skill_id) DO NOTHING;
    DELETE FROM skill_follows WHERE workspace_id = p_ws AND principal = p_principal AND skill_id = p_skill;
    -- Exactly what THIS unfollow lapsed (the mask subtracts the skill from every source at once);
    -- an unrelated skill an upstream act removed keeps its upstream-withdrawal semantics.
    PERFORM topos_detach_lapsed(
        p_ws, p_principal,
        ARRAY(SELECT unnest(v_before) EXCEPT SELECT unnest(topos_entitled_ids(p_ws, p_principal))),
        'unfollow', p_now, p_created_at);
    RETURN 'unfollowed';
END
$$;

-- DEVICE EXCLUSION: "not on this device" — written by `remove` on a followed skill; other devices
-- keep receiving; `follow` on this device lifts it (above).
CREATE FUNCTION topos_exclude_device(p_ws TEXT, p_device TEXT, p_skill TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
BEGIN
    -- The device must be a REGISTERED, non-revoked device of a CONFIRMED member (the same predicate
    -- every lane gates on) — the exclusion is that device's own row, and no caller may mint one for
    -- a device (or a person) that does not hold a seat.
    IF NOT EXISTS (
        SELECT 1 FROM device_registry dr
        WHERE dr.workspace_id = p_ws AND dr.device_key_id = p_device AND dr.revoked = 0
          AND topos_member_role(p_ws, dr.principal) IS NOT NULL
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

-- PROTECTION SETTERS — the `protect` verb's backend, one function per kind. Tightening (open →
-- reviewed / curated) takes reviewer+; loosening back to open widens what members can do and takes
-- an owner (target level decides the gate — pinning 'open' explicitly is also an owner act, since
-- it opts the bundle out of a future default tightening). Pending proposals survive a loosening
-- untouched — they still await their verdict.
CREATE FUNCTION topos_protect_skill(p_ws TEXT, p_skill TEXT, p_level TEXT, p_actor TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
BEGIN
    IF p_level NOT IN ('open', 'reviewed') THEN RETURN 'bad_level'; END IF;
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF p_level = 'open' THEN
        IF v_role <> 'owner' THEN RETURN 'owner_role_required'; END IF;
    ELSE
        IF v_role NOT IN ('reviewer', 'owner') THEN RETURN 'reviewer_role_required'; END IF;
    END IF;
    UPDATE catalog SET protection = p_level WHERE workspace_id = p_ws AND skill_id = p_skill AND status = 'active';
    IF NOT FOUND THEN RETURN 'unknown_skill'; END IF;
    RETURN 'set';
END
$$;

CREATE FUNCTION topos_protect_channel(p_ws TEXT, p_channel_name TEXT, p_mode TEXT, p_actor TEXT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
BEGIN
    IF p_mode NOT IN ('open', 'curated') THEN RETURN 'bad_level'; END IF;
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS NULL THEN RETURN 'member_required'; END IF;
    IF p_mode = 'open' THEN
        IF v_role <> 'owner' THEN RETURN 'owner_role_required'; END IF;
    ELSE
        IF v_role NOT IN ('reviewer', 'owner') THEN RETURN 'reviewer_role_required'; END IF;
    END IF;
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    UPDATE channels SET mode = p_mode WHERE workspace_id = p_ws AND name = p_channel_name;
    IF NOT FOUND THEN RETURN 'unknown_channel'; END IF;
    RETURN 'set';
END
$$;

-- LIFECYCLE: archive — out of circulation, not out of history. Renames the catalog entry
-- (`<name>-archived-<date>`, a counter on same-day repeats) FREEING the base name (id-keyed
-- follows/references make a reused name a new identity); removes the skill from every channel
-- (audit rides the trigger); auto-closes open proposals with author notices. Delivery excludes it
-- via the status join. Owner-gated (web-surface class).
CREATE FUNCTION topos_archive_skill(p_ws TEXT, p_skill TEXT, p_actor TEXT, p_date TEXT, p_now BIGINT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
    v_name TEXT;
    v_new_name TEXT;
    v_counter BIGINT := 1;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    SELECT status, name INTO v_status, v_name FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'active' THEN RETURN 'not_active'; END IF;
    PERFORM set_config('topos.actor', p_actor, true);
    PERFORM set_config('topos.created_at', p_created_at, true);
    v_new_name := v_name || '-archived-' || p_date;
    WHILE EXISTS (SELECT 1 FROM catalog WHERE workspace_id = p_ws AND name = v_new_name) LOOP
        v_counter := v_counter + 1;
        v_new_name := v_name || '-archived-' || p_date || '-' || v_counter::TEXT;
    END LOOP;
    -- `base_name` records the pre-archive name EXACTLY, so unarchive restores it without parsing
    -- (a skill legitimately named `<x>-archived-<date>` would defeat any suffix-stripping regex).
    UPDATE catalog SET status = 'archived', name = v_new_name, base_name = v_name, archived_at = p_now
    WHERE workspace_id = p_ws AND skill_id = p_skill;
    DELETE FROM channel_skills WHERE workspace_id = p_ws AND skill_id = p_skill;
    -- Auto-close open proposals, notifying each author (no verdict — circumstances closed them).
    INSERT INTO notices (workspace_id, id, principal, kind, skill_id, version_id, actor, outcome, reason, created_at)
    SELECT p_ws, gen_random_uuid()::TEXT, pr.proposer, 'proposal_closed', p_skill, pr.commit_id,
           p_actor, 'closed', 'skill archived', p_created_at
    FROM proposals pr WHERE pr.workspace_id = p_ws AND pr.skill_id = p_skill AND pr.status = 'open';
    UPDATE proposals SET status = 'closed', resolved_by = p_actor,
           resolved_reason = 'skill archived', resolved_at = p_created_at
    WHERE workspace_id = p_ws AND skill_id = p_skill AND status = 'open';
    RETURN 'archived';
END
$$;

-- LIFECYCLE: unarchive — renames back if the base name is still free, else a typed refusal (keep
-- the suffix or rename on the web). Channel placements are NOT restored (curation moved on).
CREATE FUNCTION topos_unarchive_skill(p_ws TEXT, p_skill TEXT, p_actor TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
    v_base TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    SELECT status, base_name INTO v_status, v_base FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'archived' THEN RETURN 'not_archived'; END IF;
    -- The RECORDED pre-archive name (never parsed back out of the archived spelling).
    IF v_base IS NULL THEN RETURN 'not_archived'; END IF;
    IF EXISTS (SELECT 1 FROM catalog WHERE workspace_id = p_ws AND name = v_base) THEN
        RETURN 'name_taken';
    END IF;
    UPDATE catalog SET status = 'active', name = v_base, base_name = NULL, archived_at = NULL
    WHERE workspace_id = p_ws AND skill_id = p_skill;
    PERFORM topos_heal_detachments(p_ws, p_skill);
    RETURN 'unarchived';
END
$$;

-- LIFECYCLE: delete — archive-first required; the catalog row becomes a tombstone under its
-- archived name (the base name stays free); content reclamation is the caller's custody half
-- (un-rooting + GC). Deletion cannot recall bytes — device copies remain, and the fleet page says
-- so via the retained state rows.
CREATE FUNCTION topos_delete_skill(p_ws TEXT, p_skill TEXT, p_actor TEXT, p_now BIGINT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_status TEXT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    SELECT status INTO v_status FROM catalog WHERE workspace_id = p_ws AND skill_id = p_skill;
    IF v_status IS NULL THEN RETURN 'unknown_skill'; END IF;
    IF v_status <> 'archived' THEN RETURN 'not_archived'; END IF;
    UPDATE catalog SET status = 'deleted', deleted_at = p_now
    WHERE workspace_id = p_ws AND skill_id = p_skill;
    RETURN 'deleted';
END
$$;

-- VERSION PURGE (rows half) — the leak tool: tombstone ONE version (who, when — the hash stays in
-- history), refuse while it is `current` (publish or revert first), auto-close proposals based on
-- it or carrying it with author notices. The caller's custody half un-roots the version's
-- `commit_object` edges in the same transaction so only blobs unreachable from any live version
-- drop out (the shipped GC keep-set does the rest).
CREATE FUNCTION topos_purge_version_rows(p_ws TEXT, p_skill TEXT, p_commit BYTEA, p_actor TEXT, p_now BIGINT, p_created_at TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
    v_role TEXT;
    v_owner TEXT;
    v_purged BIGINT;
BEGIN
    v_role := topos_member_role(p_ws, p_actor);
    IF v_role IS DISTINCT FROM 'owner' THEN RETURN 'owner_role_required'; END IF;
    SELECT skill_id INTO v_owner FROM skill_commit WHERE workspace_id = p_ws AND commit_id = p_commit;
    IF v_owner IS NULL OR v_owner <> p_skill THEN RETURN 'unknown_version'; END IF;
    SELECT purged_at INTO v_purged FROM skill_commit WHERE workspace_id = p_ws AND commit_id = p_commit;
    IF v_purged IS NOT NULL THEN RETURN 'already_purged'; END IF;
    IF EXISTS (SELECT 1 FROM current c WHERE c.workspace_id = p_ws AND c.skill_id = p_skill
               AND c.commit_id = p_commit) THEN
        RETURN 'is_current';
    END IF;
    UPDATE skill_commit SET purged_at = p_now, purged_by = p_actor
    WHERE workspace_id = p_ws AND commit_id = p_commit;
    INSERT INTO notices (workspace_id, id, principal, kind, skill_id, version_id, actor, outcome, reason, created_at)
    SELECT p_ws, gen_random_uuid()::TEXT, pr.proposer, 'proposal_closed', p_skill, pr.commit_id,
           p_actor, 'closed', 'a version it rests on was purged', p_created_at
    FROM proposals pr WHERE pr.workspace_id = p_ws AND pr.skill_id = p_skill AND pr.status = 'open'
      AND (pr.commit_id = p_commit OR pr.base_commit_id = p_commit);
    UPDATE proposals SET status = 'closed', resolved_by = p_actor,
           resolved_reason = 'a version it rests on was purged', resolved_at = p_created_at
    WHERE workspace_id = p_ws AND skill_id = p_skill AND status = 'open'
      AND (commit_id = p_commit OR base_commit_id = p_commit);
    RETURN 'purged';
END
$$;

-- ════════════════════════════════════════════════════════════════════════════════════════════════
-- BACKFILL + LIFT — dev/test seeds only (nothing is enrolled in production; the wire shapes are a
-- clean break under the standing grant). Deterministic: no clocks, stable ordering.
-- ════════════════════════════════════════════════════════════════════════════════════════════════

-- Catalog rows for every skill holding a `current` pointer. Name derivation: the advisory
-- display_name folded to the charset, else the skill id folded (`_` → `-`); collisions dedupe
-- deterministically by skill_id order with a numeric suffix.
INSERT INTO catalog (workspace_id, skill_id, name, display_name, status, created_at)
SELECT c.workspace_id, c.skill_id,
       CASE WHEN rn = 1 THEN base_name ELSE base_name || '-' || rn::TEXT END,
       c.display_name, 'active', 'migration-0015'
FROM (
    SELECT cur.workspace_id, cur.skill_id, cur.display_name,
           candidate AS base_name,
           ROW_NUMBER() OVER (PARTITION BY cur.workspace_id, candidate ORDER BY cur.skill_id) AS rn
    FROM current cur
    CROSS JOIN LATERAL (
        -- Fold to the charset, then CAP the base at 64 (the runtime mint's birth limit) so neither
        -- a long display name nor a `-N` dedupe suffix can overflow `catalog.name`'s 200-char CHECK
        -- and fail the migration. `btrim` runs again after the cap: truncation can leave a trailing
        -- hyphen, which the charset CHECK's `^[a-z0-9]` … rule would otherwise reject.
        SELECT COALESCE(
            NULLIF(btrim(left(btrim(regexp_replace(lower(COALESCE(cur.display_name, '')),
                                                   '[^a-z0-9-]+', '-', 'g'), '-'), 64), '-'), ''),
            NULLIF(btrim(left(btrim(replace(cur.skill_id, '_', '-'), '-'), 64), '-'), ''),
            'skill'
        ) AS candidate
    ) names
) c;

-- The display name now lives on the catalog; the pointer row's advisory copy is retired.
ALTER TABLE current DROP COLUMN display_name;

-- Every existing workspace is born (retroactively) with `everyone` …
INSERT INTO channels (workspace_id, channel_id, name, mode, builtin, created_by, created_at)
SELECT DISTINCT ws, 'everyone', 'everyone', 'open', 1, NULL, 'migration-0015'
FROM (
    SELECT workspace_id AS ws FROM workspace
    UNION SELECT workspace_id FROM current
    UNION SELECT workspace_id FROM workspace_member
    UNION SELECT workspace_id FROM roster
) w
ON CONFLICT (workspace_id, channel_id) DO NOTHING;

-- … delivering all currently-published skills (existing followers keep receiving exactly what the
-- per-skill roster delivered, now via the union).
INSERT INTO channel_skills (workspace_id, channel_id, skill_id, added_by, added_at)
SELECT workspace_id, 'everyone', skill_id, 'migration-0015', 'migration-0015'
FROM catalog
ON CONFLICT (workspace_id, channel_id, skill_id) DO NOTHING;

-- The LIFT: the per-skill `roster` rows (interim follow-state since the
-- workspace credential landed — they gated nothing) become person-scoped DIRECT follows; rows
-- for skills that never published (no catalog entry) carry no deliverable state and are dropped
-- with the table.
INSERT INTO skill_follows (workspace_id, principal, skill_id, created_at)
SELECT r.workspace_id, r.principal, r.skill_id, 'migration-0015'
FROM roster r
JOIN catalog cat ON cat.workspace_id = r.workspace_id AND cat.skill_id = r.skill_id
ON CONFLICT (workspace_id, principal, skill_id) DO NOTHING;

DROP TABLE roster;
