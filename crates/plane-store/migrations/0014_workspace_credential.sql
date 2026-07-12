-- The workspace credential — ONE membership credential per (principal × workspace × device), replacing
-- the per-skill read tokens. Appended (never edits an applied migration's bytes — sqlx checksums the
-- file). Same posture as every credential since 0005: the plaintext is HMAC-derived from the 0600
-- enrollment secret and NEVER persisted; only its sha256 is stored, so a database read can never recover
-- a live credential.
--
-- The credential lives ON the device's registry row — one row, one device, one credential — so the two
-- revocation stories are both a row-write on directory tables: a device revoke flips `revoked` (the row
-- still RESOLVES, so a since-revoked device's lost-ack retry can still replay its stored receipt, and the
-- authoritative in-transaction check denies fresh work), and a member removal deletes the
-- `workspace_member` row, which every read/write gate joins against — access dies with the row.
--
-- The UNIQUE index is the resolver: a presented credential's sha256 probes it O(1). The column is
-- nullable because a registry row is only ever credentialed by the redeem/claim mint — a row predating
-- this migration (none exist in any real deployment; a deliberate pre-1.0 clean break) simply never
-- resolves and re-enrolls.
ALTER TABLE device_registry ADD COLUMN credential_sha256 BYTEA
    CHECK (credential_sha256 IS NULL OR octet_length(credential_sha256) = 32);
CREATE UNIQUE INDEX device_registry_by_credential ON device_registry (credential_sha256)
    WHERE credential_sha256 IS NOT NULL;

-- CLEAN BREAK (deliberate: nothing enrolled in production, pre-1.0, single-deployment
-- reality): the per-skill read-token table and the grant's per-skill offer rows are dropped outright —
-- no compatibility window, no dual-read path. Reads now authorize by the membership join
-- (workspace_member) plus the same lane-blind reachability they always had; the per-skill `roster`
-- table STAYS (the genesis self-seat still writes it as interim follow-state) but no longer gates
-- anything.
DROP TABLE read_token;
DROP TABLE enrollment_grant_skill;
