/**
 * The full Drizzle schema: Better Auth's tables + the app-owned directory (schema `web`),
 * plus the read-only mirror of the vault's custody-state tables (schema `plane`).
 */
export * from "./schema.app";
export * from "./schema.auth";
export * from "./schema.custody";
