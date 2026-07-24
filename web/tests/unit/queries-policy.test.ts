import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  asOwner,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The WORKSPACE-POLICY DAL (queries.policy.server.ts) against a REAL scratch Postgres — the
 * knobs are plain columns on the ONE `web.workspace` row now, their DEFAULTs the canonical
 * fallbacks, and every setter lands its audit row in the same transaction. The OwnerActor
 * BRAND is the gate (there is no per-call role re-check to probe); what this suite pins is the
 * value validation, the bounds, and the write+audit pairing.
 */

let db: ScratchDb;
let wsId = "";

const DEFAULT_WINDOW_MS = 604_800_000; // 7 days — the schema default.
const MAX_WINDOW_MS = 31_622_400_000; // 366 days — the setter's ceiling (inclusive).

async function q() {
  return import("@/lib/db/queries.policy.server");
}

async function auditRows(kind: string): Promise<{ subject: string; outcome: string }[]> {
  return db.q(
    `SELECT subject, outcome FROM web.audit_event WHERE workspace_id = $1 AND kind = $2 ORDER BY id`,
    [wsId, kind],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_policy");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("workspacePolicyOf (the reads)", () => {
  it("answers the boot-minted defaults — no reader re-derives them anywhere", async () => {
    const queries = await q();
    expect(await queries.workspacePolicyOf(asMember(wsId, "u_owner"))).toEqual({
      stalenessWindowMs: DEFAULT_WINDOW_MS,
      protectionDefault: "open",
      registration: "invite_only",
      sessionApproval: "off",
    });
    expect(await queries.stalenessWindowOf(asMember(wsId, "u_owner"))).toBe(DEFAULT_WINDOW_MS);
  });
});

describe("setStalenessWindow (bounded: 1ms .. 366 days)", () => {
  it("accepts the bounds inclusive and refuses everything outside, integers only", async () => {
    const queries = await q();
    const owner = asOwner(wsId, "u_owner", "Owner");
    expect(await queries.setStalenessWindow(owner, 1)).toBe("set");
    expect(await queries.stalenessWindowOf(asMember(wsId, "u_owner"))).toBe(1);
    expect(await queries.setStalenessWindow(owner, MAX_WINDOW_MS)).toBe("set");
    expect(await queries.stalenessWindowOf(asMember(wsId, "u_owner"))).toBe(MAX_WINDOW_MS);

    expect(await queries.setStalenessWindow(owner, 0)).toBe("bad_window");
    expect(await queries.setStalenessWindow(owner, -1)).toBe("bad_window");
    expect(await queries.setStalenessWindow(owner, MAX_WINDOW_MS + 1)).toBe("bad_window");
    expect(await queries.setStalenessWindow(owner, 1.5)).toBe("bad_window");
    expect(await queries.setStalenessWindow(owner, Number.NaN)).toBe("bad_window");
    // The last accepted value stands; the refusals wrote nothing.
    expect(await queries.stalenessWindowOf(asMember(wsId, "u_owner"))).toBe(MAX_WINDOW_MS);
    expect(await auditRows("policy_staleness")).toEqual([
      { subject: "1", outcome: "ok" },
      { subject: String(MAX_WINDOW_MS), outcome: "ok" },
    ]);
  });
});

describe("setRegistration (the open-sign-up knob)", () => {
  it("sets 'open' and back to 'invite_only', audited; refuses any other value", async () => {
    const queries = await q();
    const owner = asOwner(wsId, "u_owner", "Owner");
    expect(await queries.setRegistration(owner, "open")).toBe("set");
    expect((await queries.workspacePolicyOf(asMember(wsId, "u_owner"))).registration).toBe("open");
    expect(await queries.setRegistration(owner, "invite_only")).toBe("set");
    expect((await queries.workspacePolicyOf(asMember(wsId, "u_owner"))).registration).toBe(
      "invite_only",
    );

    expect(await queries.setRegistration(owner, "closed")).toBe("bad_value");
    expect(await auditRows("policy_registration")).toEqual([
      { subject: "open", outcome: "ok" },
      { subject: "invite_only", outcome: "ok" },
    ]);
  });
});

describe("setSessionApproval (the session-approval knob)", () => {
  it("sets 'on' and back to 'off', audited; refuses any other value", async () => {
    const queries = await q();
    const owner = asOwner(wsId, "u_owner", "Owner");
    expect(await queries.setSessionApproval(owner, "on")).toBe("set");
    expect((await queries.workspacePolicyOf(asMember(wsId, "u_owner"))).sessionApproval).toBe("on");
    expect(await queries.setSessionApproval(owner, "off")).toBe("set");
    expect((await queries.workspacePolicyOf(asMember(wsId, "u_owner"))).sessionApproval).toBe(
      "off",
    );

    expect(await queries.setSessionApproval(owner, "maybe")).toBe("bad_value");
    expect(await auditRows("policy_session_approval")).toEqual([
      { subject: "on", outcome: "ok" },
      { subject: "off", outcome: "ok" },
    ]);
  });
});
