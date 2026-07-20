/**
 * GET /.well-known/skills/index.json — the earlier well-known spelling some skill clients
 * probe, kept as a pure alias of the canonical /.well-known/agent-skills/index.json (the
 * install-sh.ts posture): one loader, so the bytes and headers cannot drift.
 */
export { loader } from "./agent-skills-index";
