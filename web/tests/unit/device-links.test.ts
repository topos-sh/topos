import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The DEVICE-LINK model's ceremonies against a REAL scratch Postgres: the born-status rule
 * (one truth table), the approval fence minting registration + the FIRST link, the link
 * describe/apply lane ops (resolution incl. the byte-identical NOT_A_MEMBER for unknown-vs-
 * seatless), the per-request link chain (`deviceActor`), the unlink ceremonies (self / owner
 * remove / approve / reject), and the severing cascades (seat removal · device revocation:
 * links + reported state go together, audit rows cause-tagged).
 */

const SETUP_CODE = "device-links-setup-code";

let db: ScratchDb;
let wsId = "";

async function identity() {
  return import("@/lib/db/identity.server");
}

/** The full ceremony: start → approve (as `userId`) → poll. The device_code IS the credential. */
async function mintDevice(
  userId: string,
  display: string,
  name: string,
): Promise<{ credential: string; deviceId: string }> {
  const id = await identity();
  const flow = await id.startDeviceAuth(name, "");
  const approved = await id.approveDeviceAuth(flow.userCode, { userId, display });
  if (approved === null) {
    throw new Error("approval refused in seed");
  }
  const granted = await id.pollDeviceAuth(flow.deviceCode);
  if (granted.status !== "granted") {
    throw new Error(`poll: ${granted.status}`);
  }
  return { credential: flow.deviceCode, deviceId: granted.deviceId };
}

async function setKnob(value: "off" | "on"): Promise<void> {
  await db.q(`UPDATE web.workspace SET device_approval = $1 WHERE id = $2`, [value, wsId]);
}

beforeAll(async () => {
  db = await createScratchDb("web_devlinks", { TOPOS_SETUP_CODE: SETUP_CODE });
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seedUser(db, "u_out", "Outsider", "out@example.com");
  const id = await identity();
  const claimed = await id.consumeClaim(SETUP_CODE, "u_owner", "Owner");
  if (claimed === null) {
    throw new Error("claim seed failed");
  }
  await seatUser(db, wsId, "u_mem", "member", "u_owner");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("linkBornStatus — the ONE rule, whole truth table", () => {
  it("owner → active regardless of the knob; others follow the knob", async () => {
    const { linkBornStatus } = await identity();
    expect(linkBornStatus("owner", "off")).toBe("active");
    expect(linkBornStatus("owner", "on")).toBe("active");
    expect(linkBornStatus("reviewer", "off")).toBe("active");
    expect(linkBornStatus("reviewer", "on")).toBe("pending");
    expect(linkBornStatus("member", "off")).toBe("active");
    expect(linkBornStatus("member", "on")).toBe("pending");
  });
});

describe("the approval fence mints registration + the FIRST link", () => {
  it("knob off: a member's link is born active; the poll carries it", async () => {
    const id = await identity();
    const flow = await id.startDeviceAuth("mem-box", "");
    await id.approveDeviceAuth(flow.userCode, { userId: "u_mem", display: "Member" });
    const granted = await id.pollDeviceAuth(flow.deviceCode);
    expect(granted.status).toBe("granted");
    const deviceId = granted.status === "granted" ? granted.deviceId : "";
    expect(await id.deviceLinkStatus(deviceId, wsId)).toBe("active");
    const audit = await db.q(
      `SELECT details ->> 'status' AS status FROM web.audit_event
       WHERE kind = 'device_linked' AND subject = $1 AND workspace_id = $2`,
      [deviceId, wsId],
    );
    expect(audit).toEqual([{ status: "active" }]);
  });

  it("knob on: a member's link is born pending; an OWNER's stays active (the actor is the approval)", async () => {
    const id = await identity();
    await setKnob("on");
    try {
      const memFlow = await id.startDeviceAuth("mem-pend-box", "");
      await id.approveDeviceAuth(memFlow.userCode, { userId: "u_mem", display: "Member" });
      const memGrant = await id.pollDeviceAuth(memFlow.deviceCode);
      const memDevice = memGrant.status === "granted" ? memGrant.deviceId : "";
      expect(await id.deviceLinkStatus(memDevice, wsId)).toBe("pending");

      const ownFlow = await id.startDeviceAuth("own-box", "");
      await id.approveDeviceAuth(ownFlow.userCode, { userId: "u_owner", display: "Owner" });
      const ownGrant = await id.pollDeviceAuth(ownFlow.deviceCode);
      const ownDevice = ownGrant.status === "granted" ? ownGrant.deviceId : "";
      expect(await id.deviceLinkStatus(ownDevice, wsId)).toBe("active");
    } finally {
      await setKnob("off");
    }
  });
});

describe("deviceActor — the per-request chain requires the LIVE link", () => {
  it("resolves with the link's status; an unlinked device resolves to NOTHING", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "chain-box");
    const actor = await id.deviceActor(wsId, mem.credential);
    expect(actor?.linkStatus).toBe("active");
    expect(actor?.role).toBe("member");
    // Sever the link (self unlink): the same credential now resolves to nothing — the
    // uniform-404 arm, byte-indistinguishable from a workspace that never existed.
    expect(await id.selfUnlinkDevice({ userId: "u_mem", display: "M" }, mem.deviceId, wsId)).toBe(
      "unlinked",
    );
    expect(await id.deviceActor(wsId, mem.credential)).toBeNull();
  });

  it("a PENDING link resolves WITH its status (the two tolerant routes branch on it)", async () => {
    const id = await identity();
    await setKnob("on");
    try {
      const flow = await id.startDeviceAuth("pend-chain-box", "");
      await id.approveDeviceAuth(flow.userCode, { userId: "u_mem", display: "Member" });
      const actor = await id.deviceActor(wsId, flow.deviceCode);
      expect(actor?.linkStatus).toBe("pending");
    } finally {
      await setKnob("off");
    }
  });
});

describe("the link lane ops (describe / apply)", () => {
  it("describe: seatless caller and unknown workspace answer BYTE-IDENTICAL not_a_member", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "desc-box");
    const person = { userId: "u_out", display: "Outsider", deviceId: mem.deviceId };
    // A seatless person naming the REAL workspace vs anyone naming an INVENTED one: the same
    // single-arm outcome object — nothing to distinguish, no existence oracle.
    const ws = await db.q<{ name: string }>(`SELECT name FROM web.workspace WHERE id = $1`, [wsId]);
    const seatless = await id.describeDeviceLink(person as never, ws[0]?.name ?? "");
    const unknown = await id.describeDeviceLink(person as never, "never-existed");
    expect(seatless).toEqual({ outcome: "not_a_member" });
    expect(unknown).toEqual(seatless);
  });

  it("describe answers standing + born; apply creates idempotently and audits once", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "apply-box");
    const person = { userId: "u_mem", display: "Member", deviceId: mem.deviceId };
    // Unlink first so the describe shows 'none' with a forward look.
    await id.selfUnlinkDevice({ userId: "u_mem", display: "M" }, mem.deviceId, wsId);
    const before = await id.describeDeviceLink(person as never, "");
    expect(before).toMatchObject({
      outcome: "ok",
      workspaceId: wsId,
      role: "member",
      linkStatus: "none",
      born: "active",
    });

    const applied = await id.applyDeviceLink(person as never, "");
    expect(applied).toMatchObject({ outcome: "ok", linkStatus: "active" });
    // Idempotent: a second apply answers ok with the CURRENT status, no duplicate, ONE audit.
    const again = await id.applyDeviceLink(person as never, "");
    expect(again).toMatchObject({ outcome: "ok", linkStatus: "active" });
    const links = await db.q(
      `SELECT 1 FROM web.device_link WHERE device_id = $1 AND workspace_id = $2`,
      [mem.deviceId, wsId],
    );
    expect(links).toHaveLength(1);
    const audits = await db.q(
      `SELECT 1 FROM web.audit_event
       WHERE kind = 'device_linked' AND subject = $1 AND details ->> 'status' = 'active'`,
      [mem.deviceId],
    );
    // One from the approval mint, one from the re-apply after the unlink — not three.
    expect(audits).toHaveLength(2);
  });

  it("apply under the knob: a member's fresh link is born pending; describe forewarns it", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "knob-apply-box");
    const person = { userId: "u_mem", display: "Member", deviceId: mem.deviceId };
    await id.selfUnlinkDevice({ userId: "u_mem", display: "M" }, mem.deviceId, wsId);
    await setKnob("on");
    try {
      const desc = await id.describeDeviceLink(person as never, "");
      expect(desc).toMatchObject({ outcome: "ok", linkStatus: "none", born: "pending" });
      const applied = await id.applyDeviceLink(person as never, "");
      expect(applied).toMatchObject({ outcome: "ok", linkStatus: "pending" });
    } finally {
      await setKnob("off");
    }
  });
});

describe("the unlink ceremonies", () => {
  it("self unlink is SELF-only: a foreign device id answers as an unknown one", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "self-only-box");
    expect(await id.selfUnlinkDevice({ userId: "u_out", display: "O" }, mem.deviceId, wsId)).toBe(
      "unknown_link",
    );
    expect(await id.deviceLinkStatus(mem.deviceId, wsId)).toBe("active");
  });

  it("owner remove severs any link + its reported state; audit carries the cause", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "owner-remove-box");
    await db.q(
      `INSERT INTO web.bundle (id, workspace_id, name) VALUES ('s_orb', $1, 'orb-skill')`,
      [wsId],
    );
    await db.q(
      `INSERT INTO web.device_bundle_state (device_id, bundle_id, applied_version_id)
       VALUES ($1, 's_orb', $2)`,
      [mem.deviceId, "a".repeat(64)],
    );
    expect(
      await id.ownerRemoveDeviceLink({ userId: "u_owner", display: "O" }, wsId, mem.deviceId),
    ).toBe("removed");
    expect(await id.deviceLinkStatus(mem.deviceId, wsId)).toBeNull();
    const state = await db.q(`SELECT 1 FROM web.device_bundle_state WHERE device_id = $1`, [
      mem.deviceId,
    ]);
    expect(state).toHaveLength(0);
    const audit = await db.q(
      `SELECT details ->> 'cause' AS cause FROM web.audit_event
       WHERE kind = 'device_unlinked' AND subject = $1`,
      [mem.deviceId],
    );
    expect(audit).toEqual([{ cause: "owner_removed" }]);
    // Removing what is already gone answers unknown_link — nothing to sever twice.
    expect(
      await id.ownerRemoveDeviceLink({ userId: "u_owner", display: "O" }, wsId, mem.deviceId),
    ).toBe("unknown_link");
  });

  it("approve flips pending → active; reject DELETES the pending row; both audited", async () => {
    const id = await identity();
    await setKnob("on");
    try {
      const a = await mintDevice("u_mem", "Member", "approve-me");
      expect(await id.deviceLinkStatus(a.deviceId, wsId)).toBe("pending");
      expect(
        await id.approveDeviceLink({ userId: "u_owner", display: "O" }, wsId, a.deviceId),
      ).toBe("approved");
      expect(await id.deviceLinkStatus(a.deviceId, wsId)).toBe("active");
      // An approve of a non-pending link is unknown_link (already active, or never asked).
      expect(
        await id.approveDeviceLink({ userId: "u_owner", display: "O" }, wsId, a.deviceId),
      ).toBe("unknown_link");

      const r = await mintDevice("u_mem", "Member", "reject-me");
      expect(await id.rejectDeviceLink({ userId: "u_owner", display: "O" }, wsId, r.deviceId)).toBe(
        "rejected",
      );
      expect(await id.deviceLinkStatus(r.deviceId, wsId)).toBeNull();
      // Relinking later is allowed: the row is gone, not tombstoned.
      const person = { userId: "u_mem", display: "Member", deviceId: r.deviceId };
      expect(await id.applyDeviceLink(person as never, "")).toMatchObject({
        outcome: "ok",
        linkStatus: "pending",
      });
      const kinds = await db.q<{ kind: string }>(
        `SELECT kind FROM web.audit_event
         WHERE subject IN ($1, $2) AND kind IN ('link_approved', 'link_rejected')
         ORDER BY id`,
        [a.deviceId, r.deviceId],
      );
      expect(kinds.map((k) => k.kind)).toEqual(["link_approved", "link_rejected"]);
    } finally {
      await setKnob("off");
    }
  });
});

describe("revocation severs everything", () => {
  it("revokeOwnDevice deletes ALL links + reported state; device_unlinked per link", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "revoke-me");
    await db.q(
      `INSERT INTO web.bundle (id, workspace_id, name) VALUES ('s_rvk', $1, 'rvk-skill')`,
      [wsId],
    );
    await db.q(
      `INSERT INTO web.device_bundle_state (device_id, bundle_id, applied_version_id)
       VALUES ($1, 's_rvk', $2)`,
      [mem.deviceId, "b".repeat(64)],
    );
    expect(await id.revokeOwnDevice({ userId: "u_mem", display: "M" }, mem.deviceId)).toBe(true);
    expect(
      await db.q(`SELECT 1 FROM web.device_link WHERE device_id = $1`, [mem.deviceId]),
    ).toHaveLength(0);
    expect(
      await db.q(`SELECT 1 FROM web.device_bundle_state WHERE device_id = $1`, [mem.deviceId]),
    ).toHaveLength(0);
    const audit = await db.q(
      `SELECT details ->> 'cause' AS cause FROM web.audit_event
       WHERE kind = 'device_unlinked' AND subject = $1`,
      [mem.deviceId],
    );
    expect(audit).toEqual([{ cause: "device_revoked" }]);
    // The credential is dead: the person resolve fails closed too.
    expect(await id.devicePerson(mem.credential)).toBeNull();
  });
});

describe("devicePerson carries the resolved device id", () => {
  it("the link ceremonies act on THIS device, never a client-asserted one", async () => {
    const id = await identity();
    const mem = await mintDevice("u_mem", "Member", "person-box");
    const person = await id.devicePerson(mem.credential);
    expect(person?.deviceId).toBe(mem.deviceId);
    expect(person?.userId).toBe("u_mem");
  });
});
