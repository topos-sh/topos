import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { bootWorkspace, createScratchDb, type ScratchDb, seedUser } from "./helpers/scratch-db";

/**
 * MACHINE TOKENS against a REAL scratch Postgres: custody (hash stored, plaintext never),
 * resolve (fail-closed across workspaces, service session upserted per reported name, idle
 * rows swept), the applied summary, revoke's cascade — and the guard seam: a token bearer
 * opens the read door and is refused TYPED at every session-only door.
 */

let db: ScratchDb;
let ws = "";
let ws2 = "";

const OWNER = "u_owner";

async function tokens() {
  return await import("@/lib/db/queries.tokens.server");
}
async function guards() {
  return await import("@/lib/auth/guards.server");
}

beforeAll(async () => {
  db = await createScratchDb("machine_tokens");
  await seedUser(db, OWNER, "Owner", "owner@example.com");
  ws = await bootWorkspace();
  ws2 = "w_other";
  await db.q(
    `INSERT INTO web.workspace (id, name, display_name, claimed_at) VALUES ($1, 'other', 'Other', now())`,
    [ws2],
  );
});

afterAll(async () => {
  await db.drop();
});

const actor = { userId: OWNER, display: "Owner" };

describe("custody", () => {
  it("mints hashed — the plaintext appears once and never lands in a row", async () => {
    const t = await tokens();
    const minted = await t.mintMachineToken(ws, "github-actions", actor);
    expect(minted.secret.startsWith(t.MACHINE_TOKEN_PREFIX)).toBe(true);
    const rows = await db.q<{ name: string }>(`SELECT name FROM web.machine_token WHERE id = $1`, [
      minted.tokenId,
    ]);
    expect(rows).toHaveLength(1);
    const leak = await db.q(
      `SELECT 1 FROM web.machine_token WHERE token_sha256 = convert_to($1, 'UTF8')`,
      [minted.secret],
    );
    expect(leak).toHaveLength(0);
    const audit = await db.q<{ kind: string }>(
      `SELECT kind FROM web.audit_event WHERE subject = $1 ORDER BY created_at`,
      [minted.tokenId],
    );
    expect(audit.map((r) => r.kind)).toEqual(["machine_token_minted"]);
  });
});

describe("resolve", () => {
  it("answers its own workspace, fails closed everywhere else", async () => {
    const t = await tokens();
    const minted = await t.mintMachineToken(ws, "ci", actor);
    const hit = await t.tokenActor(ws, minted.secret, null);
    expect(hit?.tokenName).toBe("ci");
    expect(hit?.workspaceId).toBe(ws);
    expect(await t.tokenActor(ws2, minted.secret, null)).toBeNull();
    expect(await t.tokenActor(ws, `${t.MACHINE_TOKEN_PREFIX}nope`, null)).toBeNull();
  });

  it("upserts ONE service session per reported name and bumps last_seen", async () => {
    const t = await tokens();
    const minted = await t.mintMachineToken(ws, "runner", actor);
    const first = await t.tokenActor(ws, minted.secret, "job-1");
    const again = await t.tokenActor(ws, minted.secret, "job-1");
    expect(again?.serviceSessionId).toBe(first?.serviceSessionId);
    const other = await t.tokenActor(ws, minted.secret, "job-2");
    expect(other?.serviceSessionId).not.toBe(first?.serviceSessionId);
    const rows = await db.q(`SELECT 1 FROM web.service_session WHERE token_id = $1`, [
      minted.tokenId,
    ]);
    expect(rows).toHaveLength(2);
  });

  it("sweeps idle service sessions on the next resolve", async () => {
    const t = await tokens();
    const minted = await t.mintMachineToken(ws, "sweeper", actor);
    const stale = await t.tokenActor(ws, minted.secret, "old-run");
    await db.q(
      `UPDATE web.service_session SET last_seen_at = now() - interval '8 days'
                WHERE id = $1`,
      [stale?.serviceSessionId ?? ""],
    );
    await t.tokenActor(ws, minted.secret, "new-run");
    const names = await db.q<{ display_name: string }>(
      `SELECT display_name FROM web.service_session WHERE token_id = $1 ORDER BY display_name`,
      [minted.tokenId],
    );
    expect(names.map((r) => r.display_name)).toEqual(["new-run"]);
  });

  it("keeps the machine's applied summary on the service session", async () => {
    const t = await tokens();
    const minted = await t.mintMachineToken(ws, "reporter", actor);
    const run = await t.tokenActor(ws, minted.secret, null);
    expect(run?.serviceSessionId).toBeDefined();
    await t.serviceReportApplied(run?.serviceSessionId ?? "", [
      { skillId: "b_one", versionId: "a".repeat(64) },
      { skillId: "b_two", versionId: "b".repeat(64) },
    ]);
    const listed = await t.workspaceServiceSessions(ws);
    const row = listed.find((r) => r.serviceSessionId === run?.serviceSessionId);
    expect(row?.appliedCount).toBe(2);
    expect(row?.tokenName).toBe("reporter");
  });
});

describe("revoke", () => {
  it("deletes the token, cascades its service sessions, audits", async () => {
    const t = await tokens();
    const minted = await t.mintMachineToken(ws, "doomed", actor);
    await t.tokenActor(ws, minted.secret, null);
    expect(await t.revokeMachineToken(ws, minted.tokenId, actor)).toBe("revoked");
    expect(await t.revokeMachineToken(ws, minted.tokenId, actor)).toBe("not_found");
    expect(await t.tokenActor(ws, minted.secret, null)).toBeNull();
    const sessions = await db.q(`SELECT 1 FROM web.service_session WHERE token_id = $1`, [
      minted.tokenId,
    ]);
    expect(sessions).toHaveLength(0);
    const audit = await db.q<{ kind: string }>(
      `SELECT kind FROM web.audit_event WHERE subject = $1 ORDER BY created_at`,
      [minted.tokenId],
    );
    expect(audit.map((r) => r.kind)).toEqual(["machine_token_minted", "machine_token_revoked"]);
  });

  it("refuses a foreign workspace's token id", async () => {
    const t = await tokens();
    const minted = await t.mintMachineToken(ws, "held", actor);
    expect(await t.revokeMachineToken(ws2, minted.tokenId, actor)).toBe("not_found");
  });
});

describe("the guard seam", () => {
  const request = (bearer: string) =>
    new Request("https://topos.test/api/v1/x", {
      headers: { authorization: `Bearer ${bearer}` },
    });

  it("requireReadActor opens to a live token as a TokenActor", async () => {
    const [t, g] = [await tokens(), await guards()];
    const minted = await t.mintMachineToken(ws, "reader", actor);
    const got = await g.requireReadActor(request(minted.secret), ws);
    expect(g.isTokenActor(got)).toBe(true);
    if (g.isTokenActor(got)) {
      expect(got.tokenName).toBe("reader");
    }
  });

  it("requireReadActor fails a dead or foreign token to the uniform 404", async () => {
    const [t, g] = [await tokens(), await guards()];
    const minted = await t.mintMachineToken(ws, "gone", actor);
    await t.revokeMachineToken(ws, minted.tokenId, actor);
    const thrown = await g.requireReadActor(request(minted.secret), ws).then(
      () => null,
      (r: unknown) => r,
    );
    expect(thrown).toBeInstanceOf(Response);
    expect((thrown as Response).status).toBe(404);
  });

  it("every session-only door answers a token bearer TYPED, not 404", async () => {
    const g = await guards();
    for (const call of [
      () => g.requireSessionActor(request("tpt_anything"), ws),
      () => g.requireSessionActorPreBody(request("tpt_anything")),
    ]) {
      const thrown = await call().then(
        () => null,
        (r: unknown) => r,
      );
      expect(thrown).toBeInstanceOf(Response);
      expect((thrown as Response).status).toBe(403);
      const body = (await (thrown as Response).json()) as {
        error?: { code?: string };
      };
      expect(body.error?.code).toBe("MACHINE_TOKEN_READ_ONLY");
    }
  });
});
