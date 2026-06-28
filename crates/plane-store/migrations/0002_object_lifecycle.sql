-- The DB-authoritative object-lifecycle schema (the garbage-collection fence over the git object store).
--
-- The database is the SINGLE authority for every object's byte status; the git store holds dumb bytes and
-- always TRAILS the database. No git ref is ever used for reachability, and no operation stats the store to
-- decide presence — `object_presence` is the sole presence authority. Every row carries `workspace_id` and
-- every query binds it (isolation is this binding, never a directory). Tables are STRICT + WITHOUT ROWID;
-- content ids are the raw 32-byte sha256 BLOBs the kernel/git layer pass, width-checked. This layer keeps
-- everything in the git store (`location` is always `git`); the size-routed large-object store is the next
-- step and the `location`/`size`/`git_oid` columns are shaped for it.

-- BYTE STATUS — the fenced state machine, one row per (workspace, object).
--   absent  : the bytes are not installed (represented by EITHER no row OR a row in this state after a GC)
--   present : the bytes are durably installed at their final path and verifiable (the ONLY readable/reusable
--             state; set only AFTER the durable install)
--   deleting: a GC has claimed the object for unlink (a NON-RESURRECTABLE fence — nothing returns it to
--             present; a migrate that meets it waits for absent, then re-copies fresh)
--   unavailable: terminal; the bytes are denylisted (a purged secret) and may never be re-added
-- `location` records WHICH physical store holds the bytes (only `git` this layer; the others are for the
-- later large-object store). `git_oid` is the physical LOCATOR for a `git` object — the 20-byte git object
-- id of the loose blob — set in the same transaction that flips the row to `present`, so a GC can always
-- find the bytes to unlink (the database, not a ref, owns the locator). `size` is OPERATIONAL only
-- (accounting + the later size-routing); it NEVER enters the canonical manifest, the digest, or any id.
-- `status_updated_at` stamps every transition so the recovery sweep can finalize only a STALE `deleting`
-- (one a crashed GC left behind) without racing a live GC.
CREATE TABLE object_presence (
    workspace_id      TEXT    NOT NULL,
    object_id         BLOB    NOT NULL CHECK (length(object_id) = 32),
    status            TEXT    NOT NULL CHECK (status IN ('present', 'deleting', 'absent', 'unavailable')),
    location          TEXT    NOT NULL CHECK (location IN ('git', 'large-local', 'large-remote')),
    size              INTEGER NOT NULL,
    git_oid           BLOB             CHECK (git_oid IS NULL OR length(git_oid) = 20),
    status_updated_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, object_id)
) STRICT, WITHOUT ROWID;

-- The candidate scan reads `WHERE workspace_id = ? AND status = 'present'`; without this index it is a full
-- per-workspace partition scan every GC cycle.
CREATE INDEX object_presence_by_status ON object_presence (workspace_id, status);

-- UPLOAD QUARANTINE — an in-flight upload's staging objdir, which the GC scanner NEVER touches (it lives
-- outside the per-workspace store the scanner walks). A janitor sweeps an abandoned/expired one whole. The
-- stored `objdir` is reference metadata only — the janitor REBUILDS the deletion path from the validated
-- (workspace_id, op_id), never trusting this string.
CREATE TABLE upload_quarantine (
    workspace_id TEXT    NOT NULL,
    op_id        TEXT    NOT NULL,
    objdir       TEXT    NOT NULL,
    expires_at   INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, op_id)
) STRICT, WITHOUT ROWID;

-- PROMOTION LEASE — a GC ROOT naming a candidate commit, inserted BEFORE migration so the GC keep-set
-- protects every object the commit needs (even an old, already-present one a dedup-skip would otherwise
-- leave exposed) before any byte moves into the main store. `expires_at` NULL means NON-EXPIRING: the
-- finite value guards a crashed/abandoned migrate (then GC-reclaimable), and a SUCCESSFUL migrate sets it
-- NULL so the migrated version stays rooted until the later pointer-move consumes the lease.
CREATE TABLE promotion_lease (
    workspace_id TEXT    NOT NULL,
    op_id        TEXT    NOT NULL,
    commit_id    BLOB    NOT NULL CHECK (length(commit_id) = 32),
    expires_at   INTEGER,
    PRIMARY KEY (workspace_id, op_id)
) STRICT, WITHOUT ROWID;

-- The lease's FULL explicit object-id set (a child table so the GC's per-candidate "is X named by any LIVE
-- lease?" is an indexed EXISTS, not an app-side decode). CASCADE so releasing a lease drops its rows.
CREATE TABLE promotion_lease_object (
    workspace_id TEXT NOT NULL,
    op_id        TEXT NOT NULL,
    object_id    BLOB NOT NULL CHECK (length(object_id) = 32),
    PRIMARY KEY (workspace_id, op_id, object_id),
    FOREIGN KEY (workspace_id, op_id) REFERENCES promotion_lease (workspace_id, op_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- The claim-step spare check probes `WHERE workspace_id = ? AND object_id = ?` across leases; index the inverse.
CREATE INDEX promotion_lease_object_by_object ON promotion_lease_object (workspace_id, object_id);

-- TOMBSTONES — the purge denylist + unavailability evidence. A blob here may never be re-introduced: ingest
-- and the install transition both reject a candidate blob on this list (a best-effort early guard this layer;
-- the serializing, race-proof check lands with the pointer-move write). The dedicated purge force-unlink is
-- a later step — storage merely supports the table + the denylist check from day one.
CREATE TABLE tombstones (
    workspace_id TEXT    NOT NULL,
    blob_id      BLOB    NOT NULL CHECK (length(blob_id) = 32),
    reason       TEXT    NOT NULL,
    at           INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, blob_id)
) STRICT, WITHOUT ROWID;
