-- The trust recalibration: the plane no longer signs pointers, and possession proofs are gone —
-- requests authenticate by credential lookup against live directory rows. Storage follows:
--
-- 1. `current.signed_record` -> `current.record`: the stored pointer document is now the UNSIGNED wire
--    record. Existing rows are rewritten in place with their `signature` block stripped (the document is
--    JSON; jsonb round-trips it — old rows may re-serialize with normalized key order, which nothing
--    byte-compares across the rename).
-- 2. `op_receipts.signed_record` -> `op_receipts.record`, same strip; `op_receipts.key_id` (the plane
--    signing key id) is dropped outright.
-- 3. `op_receipts.method`: the 'device_signed' discriminant is renamed 'device' (nothing is signed; the
--    receipt's actor is the presented device credential's key id).

ALTER TABLE current RENAME COLUMN signed_record TO record;
UPDATE current
   SET record = convert_to((convert_from(record, 'UTF8')::jsonb - 'signature')::text, 'UTF8')
 WHERE record IS NOT NULL;

ALTER TABLE op_receipts RENAME COLUMN signed_record TO record;
UPDATE op_receipts
   SET record = convert_to((convert_from(record, 'UTF8')::jsonb - 'signature')::text, 'UTF8')
 WHERE record IS NOT NULL;

ALTER TABLE op_receipts DROP COLUMN key_id;

ALTER TABLE op_receipts DROP CONSTRAINT op_receipts_method_check;
UPDATE op_receipts SET method = 'device' WHERE method = 'device_signed';
ALTER TABLE op_receipts ALTER COLUMN method SET DEFAULT 'device';
ALTER TABLE op_receipts
  ADD CONSTRAINT op_receipts_method_check CHECK (method IN ('device', 'web_session'));

ALTER TABLE workspace_events DROP CONSTRAINT workspace_events_method_check;
UPDATE workspace_events SET method = 'device' WHERE method = 'device_signed';
ALTER TABLE workspace_events ALTER COLUMN method SET DEFAULT 'device';
ALTER TABLE workspace_events
  ADD CONSTRAINT workspace_events_method_check CHECK (method IN ('device', 'web_session'));
