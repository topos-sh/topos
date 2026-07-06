-- PER-SKILL DISPLAY NAME — an unsigned, advisory human name on the pointer row.
--
-- The author's skill-folder name, stored last-writer-wins and served for display only: a follower may
-- name its local folder by it, and the web dashboard shows it in place of the raw skill id. It is
-- ADVISORY METADATA — deliberately NOT part of the byte-exact bundle digest, the candidate, or any
-- device-op signing preimage: the trust stays on the bytes, so a rename never changes a version id, a
-- digest, or a signature. NULLABLE — a pre-existing skill (or a publish that carried no name) has none,
-- and every downstream read falls back to the skill id when it is NULL/absent.
ALTER TABLE current ADD COLUMN display_name TEXT;
