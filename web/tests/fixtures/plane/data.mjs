/**
 * Shared constants for the e2e specs and their custody seeds. Since the seeds are pushed
 * through the fixture vault's `POST /__test/seed` (which derives every content-addressed id
 * and mirrors the rows into `plane.*`), this module carries only the BYTES the specs both
 * seed and assert on — file contents, the hostile payloads, the honest-card triggers. Ids are
 * runtime facts the seed response returns; nothing here hardcodes one.
 */

/** The two SKILL.md revisions the review/history specs diff between. */
export const SKILL_MD_V1 = "# Deploy runbook\n\nStep one: build.\nStep two: ship.\n";
export const SKILL_MD_V2 =
  "# Deploy runbook\n\nStep one: build.\nStep two: test.\nStep three: ship.\n";

/** A quiet docs file that rides along unchanged. */
export const GUIDE_MD =
  "# Guide\n\nHow the runbook is meant to be used:\n\n- read `SKILL.md` first\n- then run `scripts/deploy.sh`\n";

/** The executable script (mode 100755 — the executable-chip subject). */
export const DEPLOY_SH = '#!/bin/sh\nset -eu\necho "deploy"\n';

/** The hostile markdown bundle file — must render INERT everywhere it appears. */
export const XSS_PATH = "notes/hostile.md";
export const XSS_CONTENT =
  '# Payload\n\n<script>alert("xss-e2e")</script>\n\n[click](javascript:alert("xss-e2e"))\n';

/** A marker that must never reach any rendered page (binary bytes never ship as text). */
export const BINARY_MARKER = "E2E_BINARY_SENTINEL";
/** Real binary bytes (a NUL lead-in) carrying the marker. */
export const BINARY_CONTENT_BASE64 = Buffer.concat([
  Buffer.from([0, 1, 2, 3, 0]),
  Buffer.from(BINARY_MARKER, "utf8"),
]).toString("base64");

/** Just past the 1 MiB per-file view budget — the too-large card's subject. */
export const BIG_CONTENT_BASE64 = Buffer.alloc(1024 * 1024 + 64, 0x78).toString("base64");
