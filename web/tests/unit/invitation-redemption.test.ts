import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  bootWorkspace,
  createScratchDb,
  placeInDefault,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedChannel,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The tokened invitation ceremonies: mint (only the hash stored; re-inviting supersedes),
 * the GET-safe view, the FOR-UPDATE-fenced accept (email-bound, unverified-squat-fenced,
 * hint effects after the seat, race-safe), the session-less decline, and the device-flow
 * weave (the flow row carries the token; approval accepts inside its own fence; the granted
 * poll decorates the hint). Every dead-token read answers the same null — the constant page.
 */

let db: ScratchDb;
let ws: string;

/** The inviter's member actor (seeded once). */
const INVITER = "u_inviter";

beforeAll(async () => {
  db = await createScratchDb("invredeem");
  ws = await bootWorkspace();
  await seedUser(db, INVITER, "Inviter", "inviter@acme.test");
  await seatUser(db, ws, INVITER, "owner");
  await seedBundle(db, ws, "s_deploy", "deploy");
  await placeInDefault(db, ws, "s_deploy");
  await seedChannel(db, ws, "c_eng", "eng");
}, 60_000);

afterAll(async () => {
  await db?.drop();
});

/** Mint ONE invitation via the roster ceremony; return its plaintext token. */
async function invite(
  email: string,
  hint: { bundleId?: string; channelId?: string } = {},
): Promise<string> {
  const { createInvitations } = await import("@/lib/db/queries.roster.server");
  const outcome = await createInvitations(asMember(ws, INVITER, "owner"), [email], "members", hint);
  if (outcome.outcome !== "invited") {
    throw new Error(`invite refused: ${outcome.outcome}`);
  }
  const minted = outcome.minted[0];
  if (minted === undefined) {
    throw new Error("nothing minted");
  }
  return minted.token;
}

/** A verified account for `email` (the invited persona most tests act as). */
async function verifiedUser(id: string, email: string): Promise<void> {
  await seedUser(db, id, "", email);
  await db.q(`UPDATE web."user" SET email_verified = true WHERE id = $1`, [id]);
}

describe("mint + view", () => {
  it("stores only the token hash; the view resolves a live token and nothing else", async () => {
    const token = await invite("viewer@x.test", { bundleId: "s_deploy" });
    const rows = await db.q(
      `SELECT token_sha256 = sha256(convert_to($1, 'UTF8')) AS hashed, hint_bundle_id
       FROM web.invitation WHERE email = 'viewer@x.test' AND status = 'pending'`,
      [token],
    );
    expect(rows[0]?.hashed).toBe(true);
    expect(rows[0]?.hint_bundle_id).toBe("s_deploy");

    const { invitationByToken } = await import("@/lib/db/identity.server");
    const view = await invitationByToken(token);
    expect(view).not.toBeNull();
    expect(view?.email).toBe("viewer@x.test");
    expect(view?.role).toBe("member");
    expect(view?.inviterDisplay).toBe("Inviter");
    expect(view?.hint).toEqual({ kind: "skill", name: "deploy" });
    // The delivered summary: the default channel's one active bundle.
    expect(view?.deliveredCount).toBe(1);
    expect(view?.viaChannels).toEqual(["everyone"]);
    // An invented token answers the same null every dead state does.
    expect(await invitationByToken("not-a-token")).toBeNull();
  });

  it("re-inviting mints a fresh token and kills the old link", async () => {
    const first = await invite("reinvite@x.test");
    const second = await invite("reinvite@x.test");
    expect(second).not.toBe(first);
    const { invitationByToken } = await import("@/lib/db/identity.server");
    expect(await invitationByToken(first)).toBeNull();
    expect(await invitationByToken(second)).not.toBeNull();
  });

  it("an expired invitation is the same constant miss", async () => {
    const token = await invite("expired@x.test");
    await db.q(
      `UPDATE web.invitation SET expires_at = now() - interval '1 minute'
       WHERE email = 'expired@x.test'`,
      [],
    );
    const { invitationByToken, acceptInvitationByToken } = await import("@/lib/db/identity.server");
    expect(await invitationByToken(token)).toBeNull();
    await verifiedUser("u_expired", "expired@x.test");
    const result = await acceptInvitationByToken(
      token,
      { userId: "u_expired", display: "E" },
      { mailboxProven: false },
    );
    expect(result.outcome).toBe("gone");
  });

  it("a revoked invitation's token is dead", async () => {
    const token = await invite("revoked@x.test");
    const { revokeInvitation } = await import("@/lib/db/queries.roster.server");
    const rows = await db.q(`SELECT id FROM web.invitation WHERE email = 'revoked@x.test'`, []);
    await revokeInvitation(asMember(ws, INVITER, "owner"), rows[0]?.id as string);
    const { invitationByToken } = await import("@/lib/db/identity.server");
    expect(await invitationByToken(token)).toBeNull();
  });
});

describe("accept", () => {
  it("binds to the invited email's account and lands seat + hint in one transaction", async () => {
    const token = await invite("accept@x.test", { bundleId: "s_deploy" });
    await verifiedUser("u_accept", "accept@x.test");
    const { acceptInvitationByToken } = await import("@/lib/db/identity.server");
    const result = await acceptInvitationByToken(
      token,
      { userId: "u_accept", display: "A" },
      { mailboxProven: false },
    );
    expect(result.outcome).toBe("accepted");
    if (result.outcome !== "accepted") {
      return;
    }
    expect(result.workspaceId).toBe(ws);
    expect(result.hint).toEqual({ kind: "skill", name: "deploy" });
    expect(result.alreadyMember).toBe(false);
    const seat = await db.q(
      `SELECT role, invited_by FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_accept'`,
      [ws],
    );
    expect(seat[0]?.role).toBe("member");
    expect(seat[0]?.invited_by).toBe(INVITER);
    // The hint effect: the direct follow, written AFTER the seat, same transaction.
    const sub = await db.q(
      `SELECT state FROM web.bundle_subscription WHERE user_id = 'u_accept' AND bundle_id = 's_deploy'`,
      [],
    );
    expect(sub[0]?.state).toBe("following");
    // Consumed: the row flips accepted; the audit trail carries the act.
    const inv = await db.q(
      `SELECT status, accepted_by FROM web.invitation WHERE email = 'accept@x.test'`,
      [],
    );
    expect(inv[0]?.status).toBe("accepted");
    expect(inv[0]?.accepted_by).toBe("u_accept");
    const audit = await db.q(
      `SELECT count(*)::int AS n FROM web.audit_event
       WHERE kind = 'invitation_accepted' AND subject = 'accept@x.test'`,
      [],
    );
    expect(audit[0]?.n).toBe(1);
  });

  it("a channel hint joins the channel", async () => {
    const token = await invite("chan@x.test", { channelId: "c_eng" });
    await verifiedUser("u_chan", "chan@x.test");
    const { acceptInvitationByToken } = await import("@/lib/db/identity.server");
    const result = await acceptInvitationByToken(
      token,
      { userId: "u_chan", display: "C" },
      { mailboxProven: false },
    );
    expect(result.outcome).toBe("accepted");
    if (result.outcome === "accepted") {
      expect(result.hint).toEqual({ kind: "channel", name: "eng" });
    }
    const member = await db.q(
      `SELECT 1 FROM web.channel_member WHERE channel_id = 'c_eng' AND user_id = 'u_chan'`,
      [],
    );
    expect(member.length).toBe(1);
  });

  it("an archived hint lands the seat and simply drops the hint", async () => {
    await seedBundle(db, ws, "s_gone", "gone-skill", { status: "archived" });
    const token = await invite("hintgone@x.test", { bundleId: "s_gone" });
    await verifiedUser("u_hintgone", "hintgone@x.test");
    const { acceptInvitationByToken } = await import("@/lib/db/identity.server");
    const result = await acceptInvitationByToken(
      token,
      { userId: "u_hintgone", display: "H" },
      { mailboxProven: false },
    );
    expect(result.outcome).toBe("accepted");
    if (result.outcome === "accepted") {
      expect(result.hint).toBeNull();
    }
    const sub = await db.q(
      `SELECT 1 FROM web.bundle_subscription WHERE user_id = 'u_hintgone'`,
      [],
    );
    expect(sub.length).toBe(0);
  });

  it("two racing accepts serialize — exactly one consumes", async () => {
    const token = await invite("race@x.test");
    await verifiedUser("u_race", "race@x.test");
    const { acceptInvitationByToken } = await import("@/lib/db/identity.server");
    const actor = { userId: "u_race", display: "R" };
    const [a, b] = await Promise.all([
      acceptInvitationByToken(token, actor, { mailboxProven: false }),
      acceptInvitationByToken(token, actor, { mailboxProven: false }),
    ]);
    const outcomes = [a.outcome, b.outcome].sort();
    expect(outcomes).toEqual(["accepted", "gone"]);
    // One seat, one accepted row — never two consumptions.
    const seats = await db.q(
      `SELECT count(*)::int AS n FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_race'`,
      [ws],
    );
    expect(seats[0]?.n).toBe(1);
  });

  it("the wrong account never accepts — and consumes nothing", async () => {
    const token = await invite("intended@x.test");
    await verifiedUser("u_intruder", "someoneelse@x.test");
    const { acceptInvitationByToken, invitationByToken } = await import("@/lib/db/identity.server");
    const result = await acceptInvitationByToken(
      token,
      { userId: "u_intruder", display: "I" },
      { mailboxProven: false },
    );
    expect(result.outcome).toBe("wrong_account");
    expect(await invitationByToken(token)).not.toBeNull();
  });

  it("the unverified-squat fence demands the round-trip; the token-proven path passes and proves", async () => {
    const token = await invite("squat@x.test");
    await seedUser(db, "u_squat", "", "squat@x.test"); // email_verified defaults false
    const { acceptInvitationByToken } = await import("@/lib/db/identity.server");
    const refused = await acceptInvitationByToken(
      token,
      { userId: "u_squat", display: "S" },
      { mailboxProven: false },
    );
    expect(refused.outcome).toBe("unverified");
    // The token-holding path (the account-mint arm) IS the mailbox proof: the fence passes and
    // the account's address flips verified inside the same transaction.
    const proven = await acceptInvitationByToken(
      token,
      { userId: "u_squat", display: "S" },
      { mailboxProven: true },
    );
    expect(proven.outcome).toBe("accepted");
    const user = await db.q(`SELECT email_verified FROM web."user" WHERE id = 'u_squat'`, []);
    expect(user[0]?.email_verified).toBe(true);
  });
});

describe("decline", () => {
  it("is recorded, session-less, re-invitable — and supersedes on re-invite", async () => {
    const token = await invite("decline@x.test");
    const { declineInvitationByToken, invitationByToken } = await import(
      "@/lib/db/identity.server"
    );
    expect(await declineInvitationByToken(token)).toBe("declined");
    // Dead afterwards — and declining again answers the same constant miss.
    expect(await invitationByToken(token)).toBeNull();
    expect(await declineInvitationByToken(token)).toBe("gone");
    const rows = await db.q(`SELECT status FROM web.invitation WHERE email = 'decline@x.test'`, []);
    expect(rows[0]?.status).toBe("declined");
    // Re-inviting supersedes the declined record with a fresh pending row.
    const fresh = await invite("decline@x.test");
    const after = await db.q(
      `SELECT status FROM web.invitation WHERE email = 'decline@x.test' ORDER BY status`,
      [],
    );
    expect(after.map((r) => r.status)).toEqual(["pending"]);
    expect(await invitationByToken(fresh)).not.toBeNull();
  });
});

describe("the invitation page's branch resolve", () => {
  it("resolves every visitor arm from the one sanctioned place", async () => {
    const token = await invite("branch@x.test");
    const { invitationPageView } = await import("@/lib/db/identity.server");
    // Anonymous, no account under the address.
    expect((await invitationPageView(token, null))?.branch).toBe("anon_new");
    // Anonymous, the address has an account.
    await seedUser(db, "u_branch", "", "branch@x.test");
    expect((await invitationPageView(token, null))?.branch).toBe("anon_existing");
    // Signed in as the invited address, unverified mailbox.
    expect((await invitationPageView(token, "u_branch"))?.branch).toBe("match_unverified");
    // Verified → the one-click arm.
    await db.q(`UPDATE web."user" SET email_verified = true WHERE id = 'u_branch'`, []);
    expect((await invitationPageView(token, "u_branch"))?.branch).toBe("match");
    // A different signed-in account → the switch page.
    expect((await invitationPageView(token, INVITER))?.branch).toBe("other");
    // Already seated → the redirect arm.
    await seatUser(db, ws, "u_branch", "member");
    expect((await invitationPageView(token, "u_branch"))?.branch).toBe("member");
    // A dead token resolves null whoever asks.
    expect(await invitationPageView("dead-token", "u_branch")).toBeNull();
  });
});

describe("the registration ceremony", () => {
  it("admits the link's account mint on a gated deployment — and only inside the ceremony", async () => {
    const { registrationDecision, withInvitationCeremony, assertRegistrationAllowed } =
      await import("@/lib/auth/registration.server");
    // The pure row: the invitation ceremony admits with every other fact closed.
    expect(
      registrationDecision({
        policy: "gated",
        tenancy: "single",
        inClaimCeremony: false,
        inInvitationCeremony: true,
        registrationKnob: "invite_only",
        pendingInvitation: false,
        mailArmed: false,
      }),
    ).toBe("allow");
    // The live hook: inside the wrapper the same email that refuses outside is admitted.
    await expect(
      withInvitationCeremony(() => assertRegistrationAllowed("nobody@x.test")),
    ).resolves.toBeUndefined();
    await expect(assertRegistrationAllowed("nobody@x.test")).rejects.toThrow();
  });
});

describe("the device-flow weave", () => {
  it("records the token; approval accepts the invitation inside its own fence; the poll carries the hint", async () => {
    const token = await invite("weave@x.test", { bundleId: "s_deploy" });
    await verifiedUser("u_weave", "weave@x.test");
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startDeviceAuth("weave laptop", "", token);
    // The pending views disclose the invitation's workspace to the code/challenge holder.
    const pending = await identity.pendingDeviceAuth(flow.userCode);
    expect(pending?.inviteWorkspace?.name).toBeDefined();
    // The challenge lookup resolves the same flow with zero typing.
    const challenge = await db.q(
      `SELECT encode(device_code_sha256, 'hex') AS hex FROM web.device_auth_session
       WHERE user_code = $1`,
      [flow.userCode],
    );
    const byChallenge = await identity.pendingDeviceAuthByChallenge(challenge[0]?.hex as string);
    expect(byChallenge?.userCode).toBe(flow.userCode);
    // The SEATLESS invited person approves: the weave accepts the invitation first, so the
    // seat requirement passes in the same transaction.
    const approved = await identity.approveDeviceAuth(flow.userCode, {
      userId: "u_weave",
      display: "W",
    });
    expect(approved).not.toBeNull();
    const seat = await db.q(
      `SELECT 1 FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_weave'`,
      [ws],
    );
    expect(seat.length).toBe(1);
    // The granted poll decorates the hint the invitation named.
    const poll = await identity.pollDeviceAuth(flow.deviceCode);
    expect(poll.status).toBe("granted");
    if (poll.status === "granted") {
      expect(poll.hint).toEqual({ kind: "skill", name: "deploy" });
    }
  });

  it("a wrong-account approver gains nothing from a token-carrying flow", async () => {
    const token = await invite("weave2@x.test");
    await verifiedUser("u_notmine", "other2@x.test");
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startDeviceAuth("laptop", "", token);
    // Seatless AND not the addressee: the weave refuses the accept, the seat check refuses the
    // approval — the same uniform null an expired code gets. The invitation stays live.
    const approved = await identity.approveDeviceAuth(flow.userCode, {
      userId: "u_notmine",
      display: "N",
    });
    expect(approved).toBeNull();
    expect(await identity.invitationByToken(token)).not.toBeNull();
  });

  it("a failed approval rolls back the invitation accept — never a split-brain commit", async () => {
    // The invited person IS the addressee (the accept would seat them), but the flow's device
    // code has already been made unresolvable (past expiry) so the approval's own liveness gate
    // refuses under the lock. The accept must NOT commit while the poll reports refused.
    const token = await invite("rollback@x.test");
    await verifiedUser("u_rollback", "rollback@x.test");
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startDeviceAuth("laptop", "", token);
    // Expire the flow after minting (the FOR-UPDATE liveness gate then finds no live row → the
    // whole approval — accept included — rolls back).
    await db.q(
      `UPDATE web.device_auth_session SET expires_at = now() - interval '1 minute'
       WHERE user_code = $1`,
      [flow.userCode],
    );
    const approved = await identity.approveDeviceAuth(flow.userCode, {
      userId: "u_rollback",
      display: "R",
    });
    expect(approved).toBeNull();
    // The invitation is STILL pending (never consumed) and no seat was written.
    expect(await identity.invitationByToken(token)).not.toBeNull();
    const seat = await db.q(
      `SELECT 1 FROM web.seat s JOIN web."user" u ON u.id = s.user_id WHERE u.email = 'rollback@x.test'`,
      [],
    );
    expect(seat.length).toBe(0);
  });

  it("devicePerson resolves a credential seat-lessly, fail-closed", async () => {
    const identity = await import("@/lib/db/identity.server");
    await seedUser(db, "u_dev", "Dev", "dev@x.test");
    const { seedDevice } = await import("./helpers/scratch-db");
    await seedDevice(db, "dk_person", "u_dev");
    // The seeded credential hash derives from the id itself.
    const person = await identity.devicePerson("dk_person");
    expect(person?.userId).toBe("u_dev");
    expect(person?.email).toBe("dev@x.test");
    expect(await identity.devicePerson("dk_bogus")).toBeNull();
  });
});
