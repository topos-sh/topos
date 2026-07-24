import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { request as pwRequest } from "@playwright/test";
import { Client } from "pg";
import { BASE_URL, E2E_ADMIN_URL, E2E_PASSWORD, PLANE_PORT } from "./env";

/**
 * The specs' arrangement toolbox. Three lanes, each the honest one for its layer:
 *  - IDENTITY rows ride the REAL auth flow where the flow is the subject (the mail-sink spec)
 *    and superuser SQL where it is mere arrangement (seating an already-created account);
 *  - WEB catalog rows (bundles, channels, proposals) are superuser SQL — directory arrangement;
 *  - CUSTODY state goes through the fixture vault's `POST /__test/seed`, which derives every
 *    content-addressed id and mirrors the rows into `plane.*`, so the app's HTTP reads and DB
 *    joins agree by construction. A seed RESETS the whole custody world — each spec file seeds
 *    everything it needs and never leans on another file's custody.
 */

const HERE = path.dirname(fileURLToPath(import.meta.url));
/** The dev outbox the app appends every rendered mail to (APP_ENV=test records always). */
export const OUTBOX_FILE = path.resolve(HERE, "..", "..", ".outbox.jsonl");
export const INVITE_EMAILS_FILE = path.resolve(HERE, "..", "..", ".invite-emails.jsonl");

// ── Superuser SQL ────────────────────────────────────────────────────────────────────────────

export async function adminQuery<T = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<T[]> {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    const { rows } = await db.query(sql, params);
    return rows as T[];
  } finally {
    await db.end();
  }
}

/** The single-tenant workspace row (boot-minted; claimed by auth.setup.ts). */
export async function theWorkspace(): Promise<{ id: string; name: string; displayName: string }> {
  const rows = await adminQuery<{ id: string; name: string; display_name: string }>(
    `select id, name, display_name from web.workspace limit 1`,
  );
  const row = rows[0];
  if (row === undefined) {
    throw new Error("no workspace row — did auth.setup run?");
  }
  return { id: row.id, name: row.name, displayName: row.display_name };
}

// ── Identities ───────────────────────────────────────────────────────────────────────────────

/**
 * Ensure `email` has an ACCOUNT (via the real sign-up flow — registration is open after
 * setup; idempotent) and return its user row. Accounts are NOT seats.
 */
export async function ensureAccount(email: string): Promise<{ userId: string; display: string }> {
  const ctx = await pwRequest.newContext({ baseURL: BASE_URL });
  try {
    await ctx.post("/api/auth/sign-up/email", {
      data: { email, password: E2E_PASSWORD, name: email.split("@")[0] ?? "user" },
      headers: { origin: BASE_URL },
      failOnStatusCode: false, // already exists on a re-run — sign-in still works
    });
  } finally {
    await ctx.dispose();
  }
  const rows = await adminQuery<{ id: string; name: string }>(
    `select id, name from web."user" where email = $1`,
    [email.toLowerCase()],
  );
  const row = rows[0];
  if (row === undefined) {
    throw new Error(`account for ${email} did not land`);
  }
  return { userId: row.id, display: row.name };
}

/** Ensure an account AND a seat with `role` in the one workspace (arrangement, not subject). */
export async function ensureSeatedUser(
  email: string,
  role: "owner" | "reviewer" | "member",
): Promise<{ userId: string; display: string }> {
  const account = await ensureAccount(email);
  const ws = await theWorkspace();
  await adminQuery(
    `insert into web.seat (workspace_id, user_id, role) values ($1, $2, $3)
     on conflict (workspace_id, user_id) do update set role = excluded.role`,
    [ws.id, account.userId, role],
  );
  return account;
}

/**
 * Mint a SESSION row for a user with a KNOWN plaintext credential (the hash is computed in
 * Postgres, like the product's own mint) in the install's one workspace — active by default
 * (the born state the real ceremony mints under an off knob; pass "pending" for the
 * approval-queue seeds). The credential then drives `/api/v1` as that user.
 */
export async function mintSession(
  userId: string,
  sessionId: string,
  displayName: string,
  credential: string,
  status: "active" | "pending" = "active",
): Promise<void> {
  const ws = await theWorkspace();
  await adminQuery(
    `insert into web.cli_session (id, workspace_id, user_id, display_name, credential_sha256, status)
     values ($1, $2, $3, $4, sha256(convert_to($5, 'UTF8')), $6)
     on conflict (id) do update set status = excluded.status`,
    [sessionId, ws.id, userId, displayName, credential, status],
  );
}

// ── Web catalog rows ─────────────────────────────────────────────────────────────────────────

/** Upsert one ACTIVE bundle row (the catalog identity custody keys join against). */
export async function ensureBundle(args: {
  id: string;
  name: string;
  displayName?: string | null;
  protection?: "open" | "reviewed" | null;
  createdBy?: string | null;
}): Promise<void> {
  const ws = await theWorkspace();
  await adminQuery(
    `insert into web.bundle (id, workspace_id, name, display_name, protection, created_by)
     values ($1, $2, $3, $4, $5, $6)
     on conflict (id) do update
       set name = excluded.name, display_name = excluded.display_name,
           protection = excluded.protection,
           status = 'active', base_name = null, archived_at = null, deleted_at = null`,
    [
      args.id,
      ws.id,
      args.name,
      args.displayName ?? null,
      args.protection ?? null,
      args.createdBy ?? null,
    ],
  );
}

/** Open one proposal row for a committed candidate (the review pages' subject). */
export async function ensureProposal(args: {
  id: string;
  bundleId: string;
  candidateVersionId: string;
  proposedBy?: string | null;
  status?: "open" | "approved" | "rejected" | "withdrawn";
  resolvedBy?: string | null;
  resolvedReason?: string | null;
}): Promise<void> {
  const ws = await theWorkspace();
  const status = args.status ?? "open";
  const resolvedAt = status === "open" ? null : new Date();
  await adminQuery(
    `insert into web.proposal
       (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status, resolved_by, resolved_reason, resolved_at)
     values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
     on conflict (id) do update
       set status = excluded.status, resolved_by = excluded.resolved_by,
           resolved_reason = excluded.resolved_reason, resolved_at = excluded.resolved_at,
           candidate_version_id = excluded.candidate_version_id`,
    [
      args.id,
      ws.id,
      args.bundleId,
      args.candidateVersionId,
      args.proposedBy ?? null,
      status,
      args.resolvedBy ?? null,
      args.resolvedReason ?? null,
      resolvedAt,
    ],
  );
}

// ── The fixture vault ────────────────────────────────────────────────────────────────────────

export interface SeedVersionSpec {
  files: { path: string; mode?: string; content?: string; content_base64?: string }[];
  /** Index into THIS bundle's versions array (the first-parent chain); absent = genesis. */
  parent?: number;
  author?: string;
  message?: string;
  created_at_ms?: number;
  purged?: boolean;
  /** Paths whose object bytes the fixture drops AFTER id derivation (the fetch-failed card). */
  drop_objects?: string[];
}

export interface SeedBundleSpec {
  ws: string;
  bundle: string;
  versions: SeedVersionSpec[];
  /** Index of the pointer target; null/absent = nothing published. */
  current?: number | null;
  generation?: number;
  moved_by?: string;
}

export interface SeededVersion {
  version_id: string;
  bundle_digest: string;
  /** path → content-addressed object id. */
  objects: Record<string, string>;
}

export interface SeededBundle {
  ws: string;
  bundle: string;
  versions: SeededVersion[];
  current: { version_id: string; generation: number } | null;
}

/** Seed the fixture vault (RESETS all custody state + recorded calls unless reset:false). */
export async function seedCustody(
  bundles: SeedBundleSpec[],
  opts: { reset?: boolean } = {},
): Promise<SeededBundle[]> {
  const ctx = await pwRequest.newContext();
  try {
    const res = await ctx.post(`http://127.0.0.1:${PLANE_PORT}/__test/seed`, {
      data: { reset: opts.reset ?? true, bundles },
    });
    if (!res.ok()) {
      throw new Error(`custody seed failed: ${res.status()} ${await res.text()}`);
    }
    const body = (await res.json()) as { bundles: SeededBundle[] };
    return body.bundles;
  } finally {
    await ctx.dispose();
  }
}

export interface RecordedCustodyCall {
  route: string;
  method: string;
  path: string;
  ws: string;
  bundle: string;
  body: Record<string, unknown>;
}

/** The fixture's recorded custody WRITE calls, optionally filtered. */
export async function custodyCalls(
  filter: { route?: string; bundle?: string } = {},
): Promise<RecordedCustodyCall[]> {
  const ctx = await pwRequest.newContext();
  try {
    const res = await ctx.get(`http://127.0.0.1:${PLANE_PORT}/__test/calls`);
    const calls = (await res.json()) as RecordedCustodyCall[];
    return calls.filter(
      (c) =>
        (filter.route === undefined || c.route === filter.route) &&
        (filter.bundle === undefined || c.bundle === filter.bundle),
    );
  } finally {
    await ctx.dispose();
  }
}

// ── The dev outbox ───────────────────────────────────────────────────────────────────────────

export interface OutboxMail {
  at: string;
  kind: string;
  to: string;
  subject: string;
  text: string;
  html?: string;
}

/**
 * The NEWEST outbox line matching (kind, to), polled — other flows append unrelated lines to
 * the same file, so match by recipient and take the last.
 */
export async function latestMail(kind: string, to: string): Promise<OutboxMail> {
  for (let attempt = 0; attempt < 50; attempt++) {
    try {
      const raw = await fs.readFile(OUTBOX_FILE, "utf8");
      const lines = raw.split("\n").filter((line) => line.trim().length > 0);
      for (let i = lines.length - 1; i >= 0; i--) {
        const entry = JSON.parse(lines[i] as string) as OutboxMail;
        if (entry.kind === kind && entry.to === to) {
          return entry;
        }
      }
    } catch {
      // file not written yet — keep polling
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw new Error(`no ${kind} mail for ${to} appeared in ${OUTBOX_FILE}`);
}
