-- The contribute authority's schema — proposals (a candidate version offered for review, not yet or never
-- `current`), their GATED object roots, and the approval audit log. Appended (never edits an applied
-- migration's bytes — sqlx checksums the file). Same posture as 0001-0003: STRICT + WITHOUT ROWID, every row
-- `workspace_id`-scoped, content ids the raw 32-byte sha256 BLOBs the kernel passes, width-checked, and the
-- `(epoch, seq)` columns carry the JCS / I-JSON safe-integer ceiling (2^53 - 1) the pointer preimage enforces.

-- PROPOSALS — a `publish --propose` candidate. `status` is the stored fact ({open, accepted, rejected}),
-- transitioned only under the one BEGIN IMMEDIATE write lock (which IS the serialization — there is no
-- SELECT ... FOR UPDATE on SQLite). **`stale` is NEVER stored — it is the derived fact `status='open' AND
-- (base_epoch, base_seq) != current.(epoch, seq)`**: the team moved past the base, so an approve's
-- compare-and-set would CONFLICT and the proposal must be rebased into a NEW proposal.
--
-- `id` IS the opening op_id (the client-minted canonical UUID already bound into the device-op signature and
-- already the idempotency key), so the server mints nothing (a WITHOUT ROWID table cannot autoincrement). It
-- is NOT keyed on `(commit_id, base_es)`: a rejected resubmit of identical bytes is the SAME content-derived
-- `commit_id`, so that would resurrect/collide a terminal row — the surrogate `id` keeps each attempt
-- distinct while the partial index below enforces one LIVE proposal per candidate+base.
--
-- `base_commit_id` is the candidate's first parent (== `current.commit_id` when it was opened, since the
-- first-parent assert passed there). It is the AUTHORITATIVE first-parent source `review --approve` re-asserts
-- against the live `current` — read O(1) here, NEVER re-derived from a git-log walk (which is lossy: it drops
-- parents it cannot map and stops at an unknown first parent, so it must not feed an authority check).
--
-- `proposer` is the rostered PRINCIPAL who opened it (the four-eyes key — under `review_required`, an approve
-- whose principal equals this is rejected — plus the audit "who"). `resolved_by` records the principal who
-- accepted / rejected / withdrew it (NULL while open).
CREATE TABLE proposals (
    workspace_id   TEXT    NOT NULL,
    id             TEXT    NOT NULL,
    skill_id       TEXT    NOT NULL,
    commit_id      BLOB    NOT NULL CHECK (length(commit_id) = 32),
    base_commit_id BLOB    NOT NULL CHECK (length(base_commit_id) = 32),
    base_epoch     INTEGER NOT NULL CHECK (base_epoch >= 0 AND base_epoch <= 9007199254740991),
    base_seq       INTEGER NOT NULL CHECK (base_seq   >= 0 AND base_seq   <= 9007199254740991),
    status         TEXT    NOT NULL CHECK (status IN ('open', 'accepted', 'rejected')),
    proposer       TEXT    NOT NULL,
    resolved_by    TEXT,
    created_at     TEXT    NOT NULL,
    PRIMARY KEY (workspace_id, id),
    FOREIGN KEY (workspace_id, commit_id) REFERENCES skill_commit (workspace_id, commit_id)
) STRICT, WITHOUT ROWID;

-- At most one OPEN proposal per (skill, candidate, base): so `review --approve` resolves to exactly one row,
-- and a re-propose of identical bytes on the same base collides (mapped to an idempotent NEEDS_REVIEW). The
-- predicate excludes terminal rows, so a resubmit AFTER rejection is allowed (a new `id`, same bytes).
CREATE UNIQUE INDEX proposals_one_open
    ON proposals (workspace_id, skill_id, commit_id, base_epoch, base_seq) WHERE status = 'open';

-- The per-skill enumeration (a future review queue; not on the GC/read hot path).
CREATE INDEX proposals_by_skill ON proposals (workspace_id, skill_id, status);

-- PROPOSAL OBJECT ROOTS — the GATED retention/read root, parallel to `commit_object` but for a PENDING
-- proposal. `publish --propose` writes THESE (never `commit_object`, which means "accepted trunk" only), so a
-- proposal's bytes are rooted + readable ONLY while the derived `open AND non-stale` predicate holds: GC's
-- claim and the read-authorization join both gate the proposal arm on that one predicate, so the instant a
-- publish advances `current` (making the proposal stale) the object drops out of BOTH retention and read in
-- the same step — keep == read for proposals, by construction, with no reaper and no edge-deletion event.
-- `review --approve` performs the handoff: it writes the real `commit_object` edges (permanent trunk root)
-- inside the same transaction that flips the status to `accepted`, so retention is continuous across it.
-- CASCADE is hygiene only — proposal rows are never deleted (audit), they transition status.
CREATE TABLE proposal_object (
    workspace_id TEXT NOT NULL,
    proposal_id  TEXT NOT NULL,
    object_id    BLOB NOT NULL CHECK (length(object_id) = 32),
    PRIMARY KEY (workspace_id, proposal_id, object_id),
    FOREIGN KEY (workspace_id, proposal_id) REFERENCES proposals (workspace_id, id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- The inverse (object -> proposals) index: the GC claim's "is X rooted by any open, non-stale proposal?" and
-- the read arm's proposal lookup are both an indexed `WHERE workspace_id = ? AND object_id = ?` probe.
CREATE INDEX proposal_object_by_object ON proposal_object (workspace_id, object_id, proposal_id);

-- APPROVALS — the audit log of who promoted which candidate, on which base. One row per (candidate, base,
-- reviewer); the composite PK makes a replayed approve idempotent (ON CONFLICT DO NOTHING) and leaves room for
-- a future multi-reviewer (`min_approvers`) policy as a pure additive extension — single-approver in v0.
CREATE TABLE approvals (
    workspace_id TEXT    NOT NULL,
    commit_id    BLOB    NOT NULL CHECK (length(commit_id) = 32),
    base_epoch   INTEGER NOT NULL CHECK (base_epoch >= 0 AND base_epoch <= 9007199254740991),
    base_seq     INTEGER NOT NULL CHECK (base_seq   >= 0 AND base_seq   <= 9007199254740991),
    reviewer     TEXT    NOT NULL,
    at           TEXT    NOT NULL,
    PRIMARY KEY (workspace_id, commit_id, base_epoch, base_seq, reviewer),
    FOREIGN KEY (workspace_id, commit_id) REFERENCES skill_commit (workspace_id, commit_id)
) STRICT, WITHOUT ROWID;
