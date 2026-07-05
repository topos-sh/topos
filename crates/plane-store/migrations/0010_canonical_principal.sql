-- CANONICAL PRINCIPAL FORM — one mailbox, one identity.
--
-- Principals (emails, and the device-rooted `dev.dk_…` ids) were stored byte-exact, so `alice@x`
-- and `Alice@x` were two identities: a lowercased invite seat could never match a mixed-case
-- device-confirmed principal at the redeem roster gate ("invited but can't join"), a mixed-case
-- owner seat denied its own lowercased web session, and the owned-workspace cap counted one human
-- once per casing. From this migration on, every principal is stored and compared in the kernel's
-- canonical ASCII-lowercase fold (the parse boundary folds; the CHECKs below pin it), and this
-- migration folds the durable rows that already exist.
--
-- Collation note: `COLLATE "C"` everywhere — the principal charset is ASCII-only, and the fold
-- must stay the ASCII fold regardless of the database's LC_CTYPE (a locale-sensitive lower() could
-- emit non-ASCII bytes that every later parse would reject).
--
-- Case-variant DUPLICATE rows are possible today (an invite of `alice@x` + `Alice@x` seats two
-- rows), so the two FOLD-TARGET tables whose PRIMARY KEY contains the principal dedupe
-- deterministically BEFORE folding — a bare fold would abort the whole migration on the PK
-- collision:
--
--   roster           — rows are identical up to case (PK-only table): keep one variant, lossless.
--   workspace_member — keep the strongest seat: confirmed beats invited, then owner > reviewer >
--                      member, then the earliest added_at (ISO-8601 text: lexicographic order is
--                      chronological), then the smallest principal bytes as the final determinism
--                      tiebreak. A confirmed owner can never lose its seat to a case-variant
--                      duplicate (rule 1 keeps it), so no workspace is orphaned.
--
-- Tables holding a principal WITHOUT it in a unique key (read_token, device_registry, admin_claim,
-- genesis_requests, proposals) fold in place; if two case-variant identities existed there they
-- MERGE silently — intended: they were always the same mailbox. Ephemeral flow tables
-- (device_auth_sessions, passcodes, enrollment_grants — minutes-scale TTL) are NOT rewritten: an
-- in-flight mixed-case enrollment crossing the deploy denies at the roster gate and is re-run
-- fresh. Audit columns (workspace_events.actor/target, invited_by, invites.created_by,
-- approvals.reviewer — the approvals audit log's PK carries a principal, but it is never an
-- identity compare) are NOT rewritten — the ledger keeps what was recorded.

-- roster: lossless dedupe (keep the smallest byte variant per folded key), then fold.
DELETE FROM roster a
USING roster b
WHERE a.workspace_id = b.workspace_id
  AND a.skill_id = b.skill_id
  AND lower(a.principal COLLATE "C") = lower(b.principal COLLATE "C")
  AND a.principal COLLATE "C" > b.principal COLLATE "C";

UPDATE roster
SET principal = lower(principal COLLATE "C")
WHERE principal <> lower(principal COLLATE "C");

-- workspace_member: keep the strongest seat per folded key (see the header), then fold.
DELETE FROM workspace_member a
USING workspace_member b
WHERE a.workspace_id = b.workspace_id
  AND lower(a.principal COLLATE "C") = lower(b.principal COLLATE "C")
  AND (
    CASE a.status WHEN 'confirmed' THEN 0 ELSE 1 END,
    CASE a.role WHEN 'owner' THEN 0 WHEN 'reviewer' THEN 1 ELSE 2 END,
    a.added_at,
    a.principal COLLATE "C"
  ) > (
    CASE b.status WHEN 'confirmed' THEN 0 ELSE 1 END,
    CASE b.role WHEN 'owner' THEN 0 WHEN 'reviewer' THEN 1 ELSE 2 END,
    b.added_at,
    b.principal COLLATE "C"
  );

UPDATE workspace_member
SET principal = lower(principal COLLATE "C")
WHERE principal <> lower(principal COLLATE "C");

-- Principal columns outside any unique key: fold in place (case-variant identities merge).
UPDATE read_token
SET principal = lower(principal COLLATE "C")
WHERE principal <> lower(principal COLLATE "C");

UPDATE device_registry
SET principal = lower(principal COLLATE "C")
WHERE principal <> lower(principal COLLATE "C");

UPDATE admin_claim
SET owner_email = lower(owner_email COLLATE "C")
WHERE owner_email IS NOT NULL
  AND owner_email <> lower(owner_email COLLATE "C");

UPDATE genesis_requests
SET owner_principal = lower(owner_principal COLLATE "C")
WHERE owner_principal <> lower(owner_principal COLLATE "C");

UPDATE proposals
SET proposer = lower(proposer COLLATE "C")
WHERE proposer <> lower(proposer COLLATE "C");

-- Pin the invariant where the identity lives: every writer folds at the parse boundary, and these
-- CHECKs make a future non-folding write path a loud constraint violation instead of a silent
-- second identity.
ALTER TABLE workspace_member
    ADD CONSTRAINT workspace_member_principal_canonical
    CHECK (principal = lower(principal COLLATE "C"));

ALTER TABLE roster
    ADD CONSTRAINT roster_principal_canonical
    CHECK (principal = lower(principal COLLATE "C"));
