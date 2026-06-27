-- The per-workspace storage-authority schema (SQLite-first).
--
-- Every row carries `workspace_id` (the hard tenant scope) and every query binds it — isolation is
-- this database binding, never the directory the git store happens to live in. Tables are STRICT (the
-- column types are enforced) and WITHOUT ROWID (the natural composite key is the row's identity). The
-- two content ids are stored as the raw 32-byte sha256 BLOB the kernel/git layer already pass; a
-- length CHECK pins the width. A future Postgres backend mirrors this schema in its own module.

-- PROVENANCE — which commit belongs to which skill.
-- PRIMARY KEY (workspace_id, commit_id) is the load-bearing constraint: a content-derived commit id
-- maps to exactly ONE skill per workspace, so re-uploading another skill's commit (content-addressing
-- yields the SAME commit id) conflicts at INSERT. The read-authorization join trusts this table
-- directly, so this single-ownership guarantee is what keeps that trust sound.
CREATE TABLE skill_commit (
    workspace_id TEXT NOT NULL,
    commit_id    BLOB NOT NULL CHECK (length(commit_id) = 32),
    skill_id     TEXT NOT NULL,
    PRIMARY KEY (workspace_id, commit_id)
) STRICT, WITHOUT ROWID;

-- Forward-looking skill enumeration (the read join does not need it; the access path is the inverse
-- index on commit_object below).
CREATE INDEX skill_commit_by_skill ON skill_commit (workspace_id, skill_id);

-- REACHABILITY — the objects each commit references (one row per distinct object; a blob at two paths
-- is one edge). The foreign key guarantees every reachability edge has provenance.
CREATE TABLE commit_object (
    workspace_id TEXT NOT NULL,
    commit_id    BLOB NOT NULL CHECK (length(commit_id) = 32),
    object_id    BLOB NOT NULL CHECK (length(object_id) = 32),
    PRIMARY KEY (workspace_id, commit_id, object_id),
    FOREIGN KEY (workspace_id, commit_id) REFERENCES skill_commit (workspace_id, commit_id)
) STRICT, WITHOUT ROWID;

-- The access-join index: the inverse (object -> commits), covering, so read authorization is an
-- index-only probe of `∃ c: skill_commit(w,s,c) ∧ commit_object(w,c,object_id)`.
CREATE INDEX commit_object_by_object ON commit_object (workspace_id, object_id, commit_id);

-- AUTHORIZATION — who may read which skill. Membership = a row exists = read-entitled. Revocation
-- (when enrollment lands) is row deletion; no validity column is added speculatively now, so the
-- read join's contract is exactly "a roster row currently exists".
CREATE TABLE roster (
    workspace_id TEXT NOT NULL,
    skill_id     TEXT NOT NULL,
    principal    TEXT NOT NULL,
    PRIMARY KEY (workspace_id, skill_id, principal)
) STRICT, WITHOUT ROWID;

-- POINTER — the one movable per-skill pointer. CREATED + seedable here but NEVER moved this increment:
-- there is no compare-and-set, no signer, and no receipt yet, so `signed_record` stays NULL until the
-- signer lands. The read path NEVER consults this table — that decoupling is exactly what lets a
-- rostered member read a proposed-but-unpromoted version's bytes.
CREATE TABLE current (
    workspace_id  TEXT    NOT NULL,
    skill_id      TEXT    NOT NULL,
    commit_id     BLOB    NOT NULL CHECK (length(commit_id) = 32),
    epoch         INTEGER NOT NULL,
    seq           INTEGER NOT NULL,
    signed_record BLOB,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, skill_id),
    FOREIGN KEY (workspace_id, commit_id) REFERENCES skill_commit (workspace_id, commit_id)
) STRICT, WITHOUT ROWID;
