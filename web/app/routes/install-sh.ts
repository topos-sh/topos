/**
 * GET /install.sh — the same installer bytes `/install` serves, at the name shell muscle memory
 * expects (`curl …/install.sh | sh`). A pure alias: one loader, so the bytes and headers cannot
 * drift from the canonical `/install`.
 */
export { loader } from "./install";
