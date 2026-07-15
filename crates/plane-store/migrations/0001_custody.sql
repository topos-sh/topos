-- The vault's ONE schema: byte custody only, identity-free by design — pinned twice, by
-- GRANTS (the vault role cannot read the app's schema; the app cannot write this one) and by
-- the CI vocabulary gate (no identity table or column ever appears here). workspace_id and
-- bundle_id arrive as OPAQUE strings from the app over the internal lane (no cross-schema
-- FKs); attribution is a pass-through display string stored verbatim. The git object store
-- (one repo per workspace) lives on disk beside these tables.
--
-- Opaque ids get shape CHECKs (charset/length — never meaning), belt-and-suspenders under
-- the app's own validation. Content ids in the internal bookkeeping tables stay the raw
-- 32-byte sha256 BYTEA the kernel/git layer pass, width-checked; the app-facing tables
-- (version / current_pointer / upload) carry them as opaque text.

-- ── The app-facing custody state ─────────────────────────────────────────────────────────────

-- A version IS the hash of its bytes (content-addressed). Versions are never deleted while
-- pointed-at; a byte purge tombstones the row (purged_at) and the hash stays.
CREATE TABLE version (
  workspace_id   text        NOT NULL CHECK (workspace_id ~ '^[A-Za-z0-9._-]{1,128}$'),
  bundle_id      text        NOT NULL CHECK (bundle_id    ~ '^[A-Za-z0-9._-]{1,128}$'),
  version_id     text        NOT NULL CHECK (version_id   ~ '^[A-Za-z0-9._-]{1,128}$'),
  commit_id      text        NOT NULL CHECK (commit_id    ~ '^[A-Za-z0-9._-]{1,128}$'),
  -- The first parent of this version's commit frame (NULL = genesis). Persisted so the pointer
  -- move can enforce the lineage fence WITHOUT re-reading the git commit frame: a promote (the
  -- approve path) is refused unless the candidate's first parent is exactly the version the
  -- pointer names, so approving a proposal whose base has since advanced conflicts instead of
  -- silently fast-forwarding over the intervening version.
  first_parent   text        CHECK (first_parent IS NULL OR first_parent ~ '^[A-Za-z0-9._-]{1,128}$'),
  author_display text        NOT NULL CHECK (char_length(author_display) <= 200),
  created_at     timestamptz NOT NULL DEFAULT now(),
  purged_at      timestamptz,                   -- byte purge tombstone; the hash stays
  PRIMARY KEY (workspace_id, bundle_id, version_id)
);

-- The movable 'current', CAS-fenced by generation: every move compares the caller's expected
-- generation and advances it by one, so a lost race is a typed conflict, never a silent
-- overwrite. RESTRICT is intent, not just default: versions are tombstoned (purged_at),
-- never deleted while pointed-at.
CREATE TABLE current_pointer (
  workspace_id     text        NOT NULL,
  bundle_id        text        NOT NULL,
  version_id       text        NOT NULL,
  generation       bigint      NOT NULL DEFAULT 1,
  moved_by_display text        NOT NULL CHECK (char_length(moved_by_display) <= 200),
  moved_at         timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, bundle_id),
  FOREIGN KEY (workspace_id, bundle_id, version_id)
    REFERENCES version (workspace_id, bundle_id, version_id) ON DELETE RESTRICT
);

-- Upload staging/quarantine bookkeeping. A staging row past its window is janitor-reclaimed
-- (the partial index below); 'committed'/'aborted' rows are the audit trail of the ingest.
CREATE TABLE upload (
  id           text        PRIMARY KEY,
  workspace_id text        NOT NULL CHECK (workspace_id ~ '^[A-Za-z0-9._-]{1,128}$'),
  bundle_id    text        NOT NULL CHECK (bundle_id    ~ '^[A-Za-z0-9._-]{1,128}$'),
  digest       text        NOT NULL CHECK (digest       ~ '^[A-Za-z0-9._-]{1,128}$'),
  state        text        NOT NULL DEFAULT 'staging'
                           CHECK (state IN ('staging','quarantined','committed','aborted')),
  created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX upload_bundle_idx ON upload (workspace_id, bundle_id);
CREATE INDEX upload_gc_idx     ON upload (created_at) WHERE state = 'staging'; -- GC sweep

-- ── Custody-internal bookkeeping (reachability, presence, GC fences) ────────────────────────

-- REACHABILITY — the objects each version references (one row per distinct object; a blob at
-- two paths is one edge). The FK guarantees every reachability edge has provenance; the
-- inverse index makes read confinement ("is this object reachable from this bundle?") an
-- indexed probe, so no object is ever served by bare hash.
CREATE TABLE version_object (
  workspace_id text  NOT NULL,
  bundle_id    text  NOT NULL,
  version_id   text  NOT NULL,
  object_id    bytea NOT NULL CHECK (octet_length(object_id) = 32),
  PRIMARY KEY (workspace_id, bundle_id, version_id, object_id),
  FOREIGN KEY (workspace_id, bundle_id, version_id)
    REFERENCES version (workspace_id, bundle_id, version_id)
);
CREATE INDEX version_object_by_object ON version_object (workspace_id, object_id);

-- The consent digest of a version's file tree (path + mode + content) — what a human
-- approves and what every client re-hashes after every fetch. Kept beside (not on) the
-- version row: it is derived custody bookkeeping, recomputed from the bytes at ingest.
CREATE TABLE version_digest (
  workspace_id  text  NOT NULL,
  bundle_id     text  NOT NULL,
  version_id    text  NOT NULL,
  bundle_digest text  NOT NULL CHECK (bundle_digest ~ '^[A-Za-z0-9._-]{1,128}$'),
  PRIMARY KEY (workspace_id, bundle_id, version_id),
  FOREIGN KEY (workspace_id, bundle_id, version_id)
    REFERENCES version (workspace_id, bundle_id, version_id)
);

-- BYTE STATUS — the fenced state machine, one row per (workspace, object). The database is
-- the SINGLE authority for every object's byte status; the git store holds dumb bytes and
-- always trails it. No git ref is ever used for reachability, and no operation stats the
-- store to decide presence.
--   present    : durably installed at its final path and verifiable (the ONLY readable state)
--   deleting   : a GC has acquired the object for unlink — a NON-RESURRECTABLE fence
--   absent     : not installed (either no row, or this state after a GC)
--   unavailable: terminal; the bytes are denylisted (a purged secret) and may never return
-- `location` records WHICH physical store holds the bytes; `git_oid` is the physical locator
-- for a `git` object, set in the same transaction that flips the row to `present`. `size` is
-- operational only (accounting + size-routing); it never enters any id. `status_updated_at`
-- stamps every transition so the recovery sweep can finalize only a STALE `deleting` without
-- racing a live GC.
CREATE TABLE object_presence (
  workspace_id      text   NOT NULL,
  object_id         bytea  NOT NULL CHECK (octet_length(object_id) = 32),
  status            text   NOT NULL CHECK (status IN ('present', 'deleting', 'absent', 'unavailable')),
  location          text   NOT NULL CHECK (location IN ('git', 'large-local', 'large-remote')),
  size              bigint NOT NULL,
  git_oid           bytea           CHECK (git_oid IS NULL OR octet_length(git_oid) = 20),
  status_updated_at bigint NOT NULL,
  PRIMARY KEY (workspace_id, object_id)
);
CREATE INDEX object_presence_by_status ON object_presence (workspace_id, status);

-- PROMOTION LEASE — a GC ROOT naming an in-flight candidate, inserted BEFORE any byte moves
-- so the keep-set protects every object the candidate needs (even an already-present one a
-- dedup-skip would otherwise leave exposed). A finite expires_at guards a crashed ingest
-- (then GC-reclaimable); a successful ingest sets it NULL so the version stays rooted until
-- its row lands.
CREATE TABLE promotion_lease (
  workspace_id text   NOT NULL,
  op_id        text   NOT NULL,
  commit_id    bytea  NOT NULL CHECK (octet_length(commit_id) = 32),
  expires_at   bigint,
  PRIMARY KEY (workspace_id, op_id)
);

-- The lease's FULL explicit object-id set (a child table so the GC's per-candidate "is X
-- named by any LIVE lease?" is an indexed EXISTS). CASCADE so releasing a lease drops it.
CREATE TABLE promotion_lease_object (
  workspace_id text  NOT NULL,
  op_id        text  NOT NULL,
  object_id    bytea NOT NULL CHECK (octet_length(object_id) = 32),
  PRIMARY KEY (workspace_id, op_id, object_id),
  FOREIGN KEY (workspace_id, op_id) REFERENCES promotion_lease (workspace_id, op_id) ON DELETE CASCADE
);
CREATE INDEX promotion_lease_object_by_object ON promotion_lease_object (workspace_id, object_id);

-- TOMBSTONES — the purge denylist + unavailability evidence. A blob here may never be
-- re-introduced: ingest and the install transition both reject a candidate blob on this
-- list.
CREATE TABLE tombstones (
  workspace_id text   NOT NULL,
  blob_id      bytea  NOT NULL CHECK (octet_length(blob_id) = 32),
  reason       text   NOT NULL,
  at           bigint NOT NULL,
  PRIMARY KEY (workspace_id, blob_id)
);
