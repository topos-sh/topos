import { and, asc, eq, sql } from "drizzle-orm";
import type { OwnerActor } from "@/lib/auth/guards.server";
import { auditInTx } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { bundleStatusInTx } from "@/lib/db/queries.channels.server";
import type { FeedActor } from "@/lib/db/queries.lane.server";
import { bundle, channel, decline, seat } from "@/lib/db/schema.app";

/**
 * The FEED data access layer — the two row kinds that decide what the server says a person
 * should have.
 *
 *  · An ASSIGNMENT is the one positive row: a bundle or a channel, aimed at one person or at
 *    EVERYONE. It is born two ways and looks identical either way — a curator aims it at
 *    someone, or the person adds it to their own feed. There are no strengths: every
 *    assignment is declinable, and the workspace baseline is just the default channel assigned
 *    to everyone.
 *  · A DECLINE is the one negative row: this person does not want this BUNDLE, whatever
 *    assigns it. Keyed to bundle identity, so it survives new versions and channel reshuffles.
 *    There is no channel-level decline — a set is declined one bundle at a time.
 *
 * Two affordances that look alike are deliberately distinct: UNPICKING deletes an assignment
 * the person made themselves (undoing their own act), while DECLINING records a standing "not
 * this one" that also holds against everyone-assignments and curator assignments. Unpicking
 * something a channel still carries leaves it delivered — declining is the way to stop it.
 *
 * Every function is actor-first and derives its workspace FROM the actor; the self-service ops
 * take the structural [`FeedActor`] both branded actors satisfy (a feed is personal — the only
 * authorization is being that person), and the curator ops take an OwnerActor and emit an
 * audit row, because aiming something at someone else has reach.
 */

// ── The person's own view ────────────────────────────────────────────────────────────────────

export interface FeedAssignmentView {
  kind: "skill" | "channel";
  /** The catalog kind for bundles ('skill' today); channel rows carry none. */
  bundleKind?: string;
  /** The bundle or channel id — the immutable key every form posts. */
  targetId: string;
  name: string;
  /** Who it reaches: this person by name, or the whole workspace. */
  audience: "you" | "everyone";
  /** The person placed it themselves, so they can take it back (an unpick). */
  own: boolean;
}

export interface FeedView {
  assignments: FeedAssignmentView[];
  /** The bundles this person has switched off, name-sorted (they stay visible, dimmed). */
  declines: { skillId: string; name: string }[];
}

/** The person's own feed rows in this workspace, resolved to names (name-sorted per group). */
export async function feedOf(actor: FeedActor): Promise<FeedView> {
  const ws = actor.workspaceId;
  const rows = await getDb().execute(sql`
    SELECT a.user_id, a.created_by, a.bundle_id, a.channel_id,
           b.name AS bundle_name, b.kind AS bundle_kind, c.name AS channel_name
    FROM web.assignment a
    LEFT JOIN web.bundle b ON b.id = a.bundle_id
    LEFT JOIN web.channel c ON c.id = a.channel_id
    WHERE a.workspace_id = ${ws} AND (a.user_id = ${actor.userId} OR a.user_id IS NULL)
    ORDER BY COALESCE(b.name, c.name)
  `);
  const assignments: FeedAssignmentView[] = (rows.rows as Record<string, unknown>[]).map((r) => ({
    kind: r.bundle_name !== null ? ("skill" as const) : ("channel" as const),
    ...(r.bundle_name !== null ? { bundleKind: r.bundle_kind as string } : {}),
    targetId: (r.bundle_id ?? r.channel_id) as string,
    name: (r.bundle_name ?? r.channel_name) as string,
    audience: r.user_id === null ? ("everyone" as const) : ("you" as const),
    own: r.user_id === actor.userId && r.created_by === actor.userId,
  }));
  const declined = await getDb()
    .select({ skillId: decline.bundleId, name: bundle.name })
    .from(decline)
    .innerJoin(bundle, and(eq(bundle.id, decline.bundleId), eq(bundle.workspaceId, ws)))
    .where(and(eq(decline.workspaceId, ws), eq(decline.userId, actor.userId)))
    .orderBy(asc(bundle.name));
  return { assignments, declines: declined };
}

// ── The grouped read behind the assignments page ─────────────────────────────────────────────

/**
 * Who put a thing in this person's feed. It is the assignment's own shape, read out loud: an
 * everyone-row reaches the whole workspace, a person-row placed by the person themselves is
 * their own pick, and a person-row placed by someone else names that someone.
 */
export type FeedAttribution =
  | { by: "everyone" }
  | { by: "you" }
  | { by: "person"; display: string };

/** A bundle as the assignments page draws it: identity, the version the workspace serves, and
 * whether this person has switched it off (a declined row stays on the page, dimmed). */
export interface AssignedBundle {
  bundleId: string;
  name: string;
  displayName: string | null;
  /** The workspace's `current`, or null while nothing has been published to the name. */
  versionId: string | null;
  declined: boolean;
}

/** One assigned channel and the bundles it carries TODAY (the set is re-read, never frozen). */
export interface AssignedChannel {
  channelId: string;
  name: string;
  isDefault: boolean;
  attribution: FeedAttribution;
  bundles: AssignedBundle[];
}

/**
 * Everything effectively this person's, grouped by WHAT PUTS IT THERE — the shape the page
 * reads back to them. A bundle two things carry appears under both; that is the truth, and it
 * is what makes "turn this off" (one negative row, holding against all of them) legible.
 */
export interface AssignedView {
  /** The workspace baseline: the default channel, when an assignment reaches this person with
   * it. Null on a workspace whose baseline row was withdrawn. */
  baseline: AssignedChannel | null;
  /** Every other assigned channel, name-sorted. */
  channels: AssignedChannel[];
  /** Bundles aimed straight at this person — by a curator, or at everyone. */
  assigned: (AssignedBundle & { attribution: FeedAttribution })[];
  /** Bundles this person added themselves: theirs to take back, unlike everything above. */
  picked: AssignedBundle[];
}

/** The raw assignment row this read groups, channel identity and attribution joined in. */
interface RawAssignment {
  user_id: string | null;
  created_by: string;
  bundle_id: string | null;
  channel_id: string | null;
  channel_name: string | null;
  is_default: boolean | null;
  created_by_display: string | null;
}

/** One bundle's display facts — the same three columns both bundle reads below select. */
interface RawBundle {
  id: string;
  name: string;
  display_name: string | null;
  version_id: string | null;
}

export async function assignedView(actor: FeedActor): Promise<AssignedView> {
  const ws = actor.workspaceId;
  const db = getDb();
  const [assignmentRows, declineRows] = await Promise.all([
    db.execute(sql`
      SELECT a.user_id, a.created_by, a.bundle_id, a.channel_id,
             c.name AS channel_name, c.is_default,
             -- The display rule (app/lib/person-display.ts): a blank name falls back to email.
             COALESCE(NULLIF(btrim(u.name), ''), u.email) AS created_by_display
      FROM web.assignment a
      LEFT JOIN web.channel c ON c.id = a.channel_id AND c.workspace_id = ${ws}
      LEFT JOIN web."user" u ON u.id = a.created_by
      WHERE a.workspace_id = ${ws} AND (a.user_id = ${actor.userId} OR a.user_id IS NULL)
      ORDER BY c.name NULLS LAST
    `),
    db.execute(sql`
      SELECT bundle_id FROM web.decline
      WHERE workspace_id = ${ws} AND user_id = ${actor.userId}
    `),
  ]);
  const declined = new Set((declineRows.rows as { bundle_id: string }[]).map((r) => r.bundle_id));
  const raws = assignmentRows.rows as unknown as RawAssignment[];

  const attributionOf = (r: RawAssignment): FeedAttribution => {
    if (r.user_id === null) {
      return { by: "everyone" };
    }
    if (r.created_by === actor.userId) {
      return { by: "you" };
    }
    // A curator whose account is gone still placed the row; the display degrades, never the fact.
    return { by: "person", display: r.created_by_display ?? "former member" };
  };

  // Channels first. The same channel can be assigned twice (to everyone AND to this person);
  // the WIDER row wins the attribution, because that is the one that would survive an unpick.
  const channels = new Map<string, Omit<AssignedChannel, "bundles">>();
  for (const r of raws) {
    if (r.channel_id === null) {
      continue;
    }
    const attribution = attributionOf(r);
    const held = channels.get(r.channel_id);
    if (
      held !== undefined &&
      !(attribution.by === "everyone" && held.attribution.by !== "everyone")
    ) {
      continue;
    }
    channels.set(r.channel_id, {
      channelId: r.channel_id,
      name: r.channel_name ?? r.channel_id,
      isDefault: r.is_default ?? false,
      attribution,
    });
  }

  const directRows = raws.filter((r) => r.bundle_id !== null);
  const channelIds = [...channels.keys()];
  const directIds = [...new Set(directRows.map((r) => r.bundle_id as string))];

  const [channelBundleRows, directBundleRows] = await Promise.all([
    channelIds.length === 0
      ? Promise.resolve({ rows: [] })
      : db.execute(sql`
          SELECT cb.channel_id, b.id, b.name, b.display_name, cp.version_id
          FROM web.channel_bundle cb
          JOIN web.bundle b
            ON b.id = cb.bundle_id AND b.workspace_id = ${ws} AND b.status = 'active'
          LEFT JOIN plane.current_pointer cp
            ON cp.workspace_id = ${ws} AND cp.bundle_id = b.id
          WHERE cb.workspace_id = ${ws} AND cb.channel_id = ANY(${pgTextArray(channelIds)})
          ORDER BY b.name
        `),
    directIds.length === 0
      ? Promise.resolve({ rows: [] })
      : db.execute(sql`
          SELECT b.id, b.name, b.display_name, cp.version_id
          FROM web.bundle b
          LEFT JOIN plane.current_pointer cp
            ON cp.workspace_id = ${ws} AND cp.bundle_id = b.id
          WHERE b.workspace_id = ${ws} AND b.status = 'active'
            AND b.id = ANY(${pgTextArray(directIds)})
          ORDER BY b.name
        `),
  ]);

  const toBundle = (r: RawBundle): AssignedBundle => ({
    bundleId: r.id,
    name: r.name,
    displayName: r.display_name,
    versionId: r.version_id,
    declined: declined.has(r.id),
  });

  const byChannel = new Map<string, AssignedBundle[]>();
  for (const raw of channelBundleRows.rows as unknown as (RawBundle & { channel_id: string })[]) {
    const list = byChannel.get(raw.channel_id) ?? [];
    list.push(toBundle(raw));
    byChannel.set(raw.channel_id, list);
  }
  const directById = new Map(
    (directBundleRows.rows as unknown as RawBundle[]).map((r) => [r.id, toBundle(r)]),
  );

  const assembled: AssignedChannel[] = [...channels.values()]
    .map((c) => ({ ...c, bundles: byChannel.get(c.channelId) ?? [] }))
    .sort((a, b) => a.name.localeCompare(b.name));

  const assigned: (AssignedBundle & { attribution: FeedAttribution })[] = [];
  const picked: AssignedBundle[] = [];
  for (const r of directRows) {
    const bundleRow = directById.get(r.bundle_id as string);
    // An archived or deleted bundle keeps its assignment row but has left the library: the feed
    // stops carrying it (feedDemandSql agrees), so the page must not draw it either.
    if (bundleRow === undefined) {
      continue;
    }
    if (r.user_id === actor.userId && r.created_by === actor.userId) {
      picked.push(bundleRow);
    } else {
      assigned.push({ ...bundleRow, attribution: attributionOf(r) });
    }
  }

  return {
    baseline: assembled.find((c) => c.isDefault) ?? null,
    channels: assembled.filter((c) => !c.isDefault),
    assigned,
    picked,
  };
}

/**
 * Whether the WORKSPACE-WIDE assignment exists on a target — the one fact the curator arm on a
 * channel or skill page is a toggle over. It is a plain row probe, so a member reads the same
 * answer an owner does; only the arm that flips it is gated.
 */
export async function assignedToEveryone(
  actor: FeedActor,
  target: { bundleId: string } | { channelId: string },
): Promise<boolean> {
  const ws = actor.workspaceId;
  const match =
    "bundleId" in target
      ? sql`a.bundle_id = ${target.bundleId}`
      : sql`a.channel_id = ${target.channelId}`;
  const rows = await getDb().execute(sql`
    SELECT 1 FROM web.assignment a
    WHERE a.workspace_id = ${ws} AND a.user_id IS NULL AND ${match}
    LIMIT 1
  `);
  return rows.rows.length > 0;
}

// ── The person's own acts (self-scoped; no role gate, no audit — nobody else is touched) ────

export type AddToMineOutcome = "added" | "unknown_skill" | "skill_not_active";

/**
 * "Add to mine": a self-assignment of a bundle — the person asking for it in their own name.
 * It ALSO clears any decline on the same bundle, because asking for a thing and holding it
 * back are contradictory stances and the newer one is the person's real intent. Idempotent.
 * Archived bundles refuse (a freed name is a NEW identity; the old one is out of circulation).
 */
export async function addToMine(actor: FeedActor, bundleId: string): Promise<AddToMineOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    if (status !== "active") {
      return "skill_not_active";
    }
    await tx.execute(sql`
      INSERT INTO web.assignment (workspace_id, user_id, bundle_id, created_by)
      VALUES (${ws}, ${actor.userId}, ${bundleId}, ${actor.userId})
      ON CONFLICT DO NOTHING
    `);
    await tx.execute(sql`
      DELETE FROM web.decline
      WHERE workspace_id = ${ws} AND user_id = ${actor.userId} AND bundle_id = ${bundleId}
    `);
    return "added";
  });
}

export type UnpickOutcome = "unpicked" | "not_picked" | "unknown_skill";

/**
 * Take back an assignment this person made themselves — the inverse of "add to mine", and
 * NOTHING more: an assignment someone else aimed at them (or at everyone) is untouched, so a
 * bundle a channel still carries keeps arriving. Declining is the affordance for that.
 */
export async function unpickBundle(actor: FeedActor, bundleId: string): Promise<UnpickOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    if ((await bundleStatusInTx(tx, ws, bundleId)) === null) {
      return "unknown_skill";
    }
    const deleted = await tx.execute(sql`
      DELETE FROM web.assignment
      WHERE workspace_id = ${ws} AND user_id = ${actor.userId} AND bundle_id = ${bundleId}
        AND created_by = ${actor.userId}
      RETURNING bundle_id
    `);
    return deleted.rows.length > 0 ? "unpicked" : "not_picked";
  });
}

export type DeclineOutcome = "declined" | "unknown_skill";

/**
 * Switch a bundle OFF for this person, whatever assigns it: the one negative row, keyed to the
 * bundle's identity so it holds through new versions and through the set being re-curated.
 * Idempotent. The bundle stays in the workspace library — this is a personal stance, never a
 * removal.
 */
export async function declineBundle(actor: FeedActor, bundleId: string): Promise<DeclineOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    if ((await bundleStatusInTx(tx, ws, bundleId)) === null) {
      return "unknown_skill";
    }
    await tx.execute(sql`
      INSERT INTO web.decline (workspace_id, user_id, bundle_id)
      VALUES (${ws}, ${actor.userId}, ${bundleId})
      ON CONFLICT DO NOTHING
    `);
    return "declined";
  });
}

/** Clear the decline — the bundle flows again from whatever still assigns it. Idempotent. */
export async function undeclineBundle(actor: FeedActor, bundleId: string): Promise<"cleared"> {
  await getDb()
    .delete(decline)
    .where(
      and(
        eq(decline.workspaceId, actor.workspaceId),
        eq(decline.userId, actor.userId),
        eq(decline.bundleId, bundleId),
      ),
    );
  return "cleared";
}

export type SelfChannelOutcome =
  | "assigned"
  | "unassigned"
  | "not_assigned"
  /** The workspace baseline: assigned to everyone, so one person cannot un-assign it. Skills
   * from it are declined one at a time. */
  | "baseline"
  | "unknown_channel";

/** Carry a channel yourself — a self-assignment of the set. Idempotent. */
export async function assignChannelToSelf(
  actor: FeedActor,
  channelId: string,
): Promise<SelfChannelOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelRowInTx(tx, ws, channelId);
    if (row === undefined) {
      return "unknown_channel";
    }
    if (row.isDefault) {
      // Already everyone's; there is nothing to add and nothing to report as changed.
      return "baseline";
    }
    await tx.execute(sql`
      INSERT INTO web.assignment (workspace_id, user_id, channel_id, created_by)
      VALUES (${ws}, ${actor.userId}, ${channelId}, ${actor.userId})
      ON CONFLICT DO NOTHING
    `);
    return "assigned";
  });
}

/**
 * Stop carrying a channel — deletes THIS person's own assignment of the set. The baseline is
 * refused typed: it reaches everyone, and one person's window onto it is per-bundle declines.
 */
export async function unassignChannelFromSelf(
  actor: FeedActor,
  channelId: string,
): Promise<SelfChannelOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelRowInTx(tx, ws, channelId);
    if (row === undefined) {
      return "unknown_channel";
    }
    if (row.isDefault) {
      return "baseline";
    }
    const deleted = await tx.execute(sql`
      DELETE FROM web.assignment
      WHERE workspace_id = ${ws} AND user_id = ${actor.userId} AND channel_id = ${channelId}
      RETURNING channel_id
    `);
    return deleted.rows.length > 0 ? "unassigned" : "not_assigned";
  });
}

/**
 * Switch off every bundle a channel carries TODAY — the convenience behind "I don't want this
 * set" when the set is not the person's to un-assign (the baseline) or when they want the
 * stance to survive the set being re-curated. Per-bundle by construction: bundles added later
 * still arrive.
 */
export async function declineChannelContents(
  actor: FeedActor,
  channelId: string,
): Promise<{ outcome: "declined"; count: number } | { outcome: "unknown_channel" }> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelRowInTx(tx, ws, channelId);
    if (row === undefined) {
      return { outcome: "unknown_channel" };
    }
    const inserted = await tx.execute(sql`
      INSERT INTO web.decline (workspace_id, user_id, bundle_id)
      SELECT ${ws}, ${actor.userId}, cb.bundle_id
      FROM web.channel_bundle cb
      JOIN web.bundle b ON b.id = cb.bundle_id AND b.workspace_id = ${ws} AND b.status = 'active'
      WHERE cb.workspace_id = ${ws} AND cb.channel_id = ${channelId}
      ON CONFLICT DO NOTHING
      RETURNING bundle_id
    `);
    return { outcome: "declined", count: inserted.rows.length };
  });
}

// ── Curator assignments (aimed at someone else, or at everyone — audited) ───────────────────

/** Who an assignment reaches: one seated person, or the whole workspace. */
export type Audience = { userId: string } | { everyone: true };

export type AssignOutcome =
  | "assigned"
  | "unassigned"
  | "not_assigned"
  | "unknown_member"
  | "unknown_skill"
  | "skill_not_active"
  | "unknown_channel";

/** Aim a bundle at a person or at everyone. Idempotent; the audit row records the audience. */
export async function assignBundle(
  actor: OwnerActor,
  bundleId: string,
  audience: Audience,
): Promise<AssignOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    if (status !== "active") {
      return "skill_not_active";
    }
    const userId = await resolveAudienceTx(tx, ws, audience);
    if (userId === undefined) {
      return "unknown_member";
    }
    await tx.execute(sql`
      INSERT INTO web.assignment (workspace_id, user_id, bundle_id, created_by)
      VALUES (${ws}, ${userId}, ${bundleId}, ${actor.userId})
      ON CONFLICT DO NOTHING
    `);
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "assigned",
      subject: bundleId,
      outcome: "ok",
      details: { audience: userId ?? "everyone" },
    });
    return "assigned";
  });
}

/** Aim a channel at a person or at everyone — the same act one set wider. */
export async function assignChannel(
  actor: OwnerActor,
  channelId: string,
  audience: Audience,
): Promise<AssignOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    if ((await channelRowInTx(tx, ws, channelId)) === undefined) {
      return "unknown_channel";
    }
    const userId = await resolveAudienceTx(tx, ws, audience);
    if (userId === undefined) {
      return "unknown_member";
    }
    await tx.execute(sql`
      INSERT INTO web.assignment (workspace_id, user_id, channel_id, created_by)
      VALUES (${ws}, ${userId}, ${channelId}, ${actor.userId})
      ON CONFLICT DO NOTHING
    `);
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "assigned",
      subject: channelId,
      outcome: "ok",
      details: { audience: userId ?? "everyone" },
    });
    return "assigned";
  });
}

/**
 * Withdraw an assignment — by target and audience, the exact inverse of the two above. It
 * withdraws the OFFER only: bytes already on a machine stay there until that machine
 * reconciles, and a person who also assigned the thing to themselves keeps it.
 */
export async function unassign(
  actor: OwnerActor,
  target: { bundleId: string } | { channelId: string },
  audience: Audience,
): Promise<AssignOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const userId = await resolveAudienceTx(tx, ws, audience);
    if (userId === undefined) {
      return "unknown_member";
    }
    const subject = "bundleId" in target ? target.bundleId : target.channelId;
    const targetMatch =
      "bundleId" in target
        ? sql`a.bundle_id = ${target.bundleId}`
        : sql`a.channel_id = ${target.channelId}`;
    const audienceMatch = userId === null ? sql`a.user_id IS NULL` : sql`a.user_id = ${userId}`;
    const deleted = await tx.execute(sql`
      DELETE FROM web.assignment a
      WHERE a.workspace_id = ${ws} AND ${targetMatch} AND ${audienceMatch}
      RETURNING a.workspace_id
    `);
    if (deleted.rows.length === 0) {
      return "not_assigned";
    }
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "unassigned",
      subject,
      outcome: "ok",
      details: { audience: userId ?? "everyone" },
    });
    return "unassigned";
  });
}

// ── Shared helpers ───────────────────────────────────────────────────────────────────────────

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/** A REAL `text[]` value — an `ARRAY[...]` constructor with every element its own bind
 * parameter, so no hand-rolled literal ever has to escape a value. (A bare JS array in a
 * template renders as a parenthesised list, which is not an array at all.) */
function pgTextArray(values: string[]) {
  return values.length === 0
    ? sql`ARRAY[]::text[]`
    : sql`ARRAY[${sql.join(
        values.map((v) => sql`${v}`),
        sql`, `,
      )}]::text[]`;
}

/** The id-keyed channel resolve every feed op runs — workspace-scoped, default flag included. */
async function channelRowInTx(tx: Tx, ws: string, channelId: string) {
  const rows = await tx
    .select({ id: channel.id, isDefault: channel.isDefault })
    .from(channel)
    .where(and(eq(channel.workspaceId, ws), eq(channel.id, channelId)))
    .limit(1);
  return rows[0];
}

/**
 * The audience a curator named, resolved to the column value: `null` for everyone, the user id
 * for a person — and `undefined` when the named person holds no seat here, which is the honest
 * refusal (an assignment is seat-anchored; aiming one at a stranger would fail the FK anyway).
 */
async function resolveAudienceTx(
  tx: Tx,
  ws: string,
  audience: Audience,
): Promise<string | null | undefined> {
  if ("everyone" in audience) {
    return null;
  }
  const rows = await tx
    .select({ userId: seat.userId })
    .from(seat)
    .where(and(eq(seat.workspaceId, ws), eq(seat.userId, audience.userId)))
    .limit(1);
  return rows[0]?.userId;
}
