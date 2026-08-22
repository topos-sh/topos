import { defineConfig } from "drizzle-kit";

// `drizzle-kit generate` is offline (schema-diff only); DATABASE_URL is needed only for the
// db-connected commands, so the config must stay valid without env set.
//
// Only the web tier's OWN tables are diffed: schema.auth.ts (Better Auth) + schema.app.ts (the
// policy audit trail + proposal comments). The two read-only mirrors — schema.custody.ts (the
// vault's `plane`) and schema.gateway.ts (the gateway's `gateway`) — are DELIBERATELY excluded:
// those rows belong to the other services, are migrated by their own lineages, and this tier
// never generates DDL against them.
export default defineConfig({
  schema: ["./app/lib/db/schema.auth.ts", "./app/lib/db/schema.app.ts"],
  out: "./drizzle",
  dialect: "postgresql",
  // Must match app/lib/db/migrate.server.ts: the migration ledger is pinned to
  // web.__drizzle_migrations — the web tier's one schema (never the default `drizzle` schema).
  migrations: {
    table: "__drizzle_migrations",
    schema: "web",
  },
  dbCredentials: {
    url: process.env.DATABASE_URL ?? "",
  },
});
