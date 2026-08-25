import { and, eq, inArray, like, sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import type { MemberActor, ReadActor, SessionActor } from "@/lib/auth/guards.server";
import { auditInTx, mintProposalId } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import {
  bundle,
  channel,
  channelBundle,
  deviceOwner,
  notice,
  opReceipt,
  proposal,
  versionAuthor,
  workspace,
} from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";
import { planeVersion } from "@/lib/db/schema.custody";
import type { McpGateRefusal } from "@/lib/mcp/publish-gate.server";
import { personAttribution } from "@/lib/person-display";

/**
 * The custody-op ORCHESTRATION's data half — everything the publish/propose/review/revert
 * routes read and write in the app's own rows: the op-receipt idempotency slots, the genesis
 * registration (bundle row + placement), the proposal rows, and the
 * verdict notices. The byte half lives in app/lib/plane/custody.server.ts; the routes sequence
 * the two (vault call first, then the final row transaction carrying the receipt).
 */

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/** The transaction handle a final-transaction body is handed — the DAL's own type, exported so
 *  an orchestration layer can name it without importing the raw driver. */
export type FinalTx = Tx;

// ── Op receipts (session-op idempotency) ──────────────────────────────────────────────────────

export type ReceiptLookup =
  | { kind: "miss" }
  | { kind: "replay"; outcome: unknown }
  | { kind: "key_reuse" };

/**
 * The replay probe: same (workspace, session, op_id) + same request bytes replays the stored
 * outcome VERBATIM; the same key with DIFFERENT bytes is a refused key reuse. The request hash
 * is computed IN Postgres (this tier computes no digest).
 */
export async function findReceipt(
  actor: SessionActor,
  opId: string,
  rawBody: string,
): Promise<ReceiptLookup> {
  const rows = await getDb().execute(sql`
    SELECT outcome, (request_sha256 = sha256(convert_to(${rawBody}, 'UTF8'))) AS body_match
    FROM ${opReceipt}
    WHERE workspace_id = ${actor.workspaceId} AND session_id = ${actor.sessionId}
      AND op_id = ${opId}::uuid
  `);
  const row = rows.rows[0] as { outcome: unknown; body_match: boolean } | undefined;
  if (row === undefined) {
    return { kind: "miss" };
  }
  return row.body_match ? { kind: "replay", outcome: row.outcome } : { kind: "key_reuse" };
}

/** Insert the terminal outcome's receipt slot (same-transaction with the op's row writes). */
export async function insertReceiptInTx(
  tx: Tx,
  actor: SessionActor,
  opId: string,
  rawBody: string,
  outcome: unknown,
): Promise<void> {
  await tx.execute(sql`
    INSERT INTO ${opReceipt} (workspace_id, session_id, op_id, request_sha256, outcome)
    VALUES (${actor.workspaceId}, ${actor.sessionId}, ${opId}::uuid,
            sha256(convert_to(${rawBody}, 'UTF8')), ${JSON.stringify(outcome)}::jsonb)
    ON CONFLICT (workspace_id, session_id, op_id) DO NOTHING
  `);
}

// ── The publish gate's reads ─────────────────────────────────────────────────────────────────

export interface PublishTarget {
  bundleId: string;
  name: string;
  status: string;
  /** The catalog kind the bundle was born with — fixed at genesis (a publish naming a
   * different one is refused before any custody write). */
  kind: string;
  /** The RESOLVED protection: the per-bundle pin, else the workspace default. */
  protection: "open" | "reviewed";
}

/** The publish/revert gate's read: the bundle row + the resolved protection cascade. A
 * MemberActor suffices (a SessionActor IS one structurally) — the review pages share it. */
export async function publishTargetOf(
  // Only the workspace scope is read — structural, so member, session, and token actors all pass.
  actor: ReadActor,
  bundleId: string,
): Promise<PublishTarget | undefined> {
  const rows = await getDb()
    .select({
      bundleId: bundle.id,
      name: bundle.name,
      status: bundle.status,
      kind: bundle.kind,
      protection: sql<string>`COALESCE(${bundle.protection}, ${workspace.protectionDefault}, 'open')`,
    })
    .from(bundle)
    .innerJoin(workspace, eq(workspace.id, bundle.workspaceId))
    .where(and(eq(bundle.workspaceId, actor.workspaceId), eq(bundle.id, bundleId)))
    .limit(1);
  const row = rows[0];
  return row === undefined
    ? undefined
    : { ...row, protection: row.protection as PublishTarget["protection"] };
}

/**
 * Whether a bundle id is already registered to a DIFFERENT workspace.
 *
 * A genesis publish keeps the CLIENT-SUPPLIED id, and `bundle.id` is unique across the whole
 * catalog — so a caller who names another workspace's id reaches the genesis arm (their own
 * workspace has no such row) while every id-keyed write below still addresses the foreign row.
 * The upstream row is the sharp one: its arbiter is the bundle id ALONE, so its upsert would
 * rewrite the other workspace's provenance in place. Refusing here — before the vault ingest,
 * so no orphan bytes land either — closes that at the root.
 *
 * A same-workspace id is NOT this: `publishTargetOf` is workspace-scoped and status-agnostic,
 * so an id already in the caller's own catalog never reaches the genesis arm at all, and a
 * retried genesis still converges on its own row.
 */
export async function bundleIdHeldElsewhere(
  actor: MemberActor,
  bundleId: string,
): Promise<boolean> {
  const rows = await getDb()
    .select({ workspaceId: bundle.workspaceId })
    .from(bundle)
    .where(eq(bundle.id, bundleId))
    .limit(1);
  const row = rows[0];
  return row !== undefined && row.workspaceId !== actor.workspaceId;
}

// ── Genesis registration ─────────────────────────────────────────────────────────────────────

/**
 * The birth-name fold (the one implementation, ported from SQL): the display name folded to
 * the agent-skills charset, else the bundle id folded, else the literal 'skill'; capped at 64
 * with no leading/trailing hyphen.
 */
export function mintCatalogName(displayName: string | null, bundleId: string): string {
  const fold = (input: string): string =>
    input
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 64)
      .replace(/^-+|-+$/g, "");
  const fromDisplay = displayName === null ? "" : fold(displayName);
  if (fromDisplay.length > 0) {
    return fromDisplay;
  }
  const fromId = fold(bundleId);
  return fromId.length > 0 ? fromId : "skill";
}

/**
 * Catalog names reserved for the CLI's own artifacts — the directories it owns inside an agent's
 * skills root: `topos` (the built-in skill) and `topos-mcp` (the plugin directory an MCP-capable
 * harness is served through). A workspace bundle that minted either name would compete for a
 * directory the client already governs.
 *
 * The reservation binds at EVERY door that writes a catalog name, not just the genesis mint —
 * a rename reaches the same namespace by a different route. Server-side on purpose: a reserved
 * name answers byte-identically to a taken one everywhere, so the list is never enumerable
 * through a form.
 */
const RESERVED_BUNDLE_NAMES = new Set(["topos", "topos-mcp"]);

/** Is `name` reserved for the client's own directories? (see [`RESERVED_BUNDLE_NAMES`]) */
export function isReservedBundleName(name: string): boolean {
  return RESERVED_BUNDLE_NAMES.has(name);
}

// ── The bundle cap (`bundles`) ──────────────────────────────────────────────────────────────

/** The bundle cap's one refusal — the same shape every genesis door already renders. */
const BUNDLE_LIMIT_REFUSAL: McpGateRefusal = {
  code: "BUNDLE_LIMIT_REACHED",
  message: "This workspace is at its bundle limit.",
};

async function activeBundleCount(executor: Pick<Tx, "execute">, ws: string): Promise<number> {
  const rows = await executor.execute(
    sql`SELECT count(*)::int AS n FROM ${bundle}
        WHERE workspace_id = ${ws} AND status = 'active'`,
  );
  return (rows.rows[0] as { n: number } | undefined)?.n ?? 0;
}

/** Whether an ACTIVE row with this id already stands in this workspace — such a "creation"
 * adds no row (the registration no-ops on the id), so the cap must not refuse its retry. */
async function activeBundleExists(
  executor: Pick<Tx, "execute">,
  ws: string,
  bundleId: string,
): Promise<boolean> {
  const rows = await executor.execute(
    sql`SELECT 1 FROM ${bundle}
        WHERE workspace_id = ${ws} AND id = ${bundleId} AND status = 'active' LIMIT 1`,
  );
  return rows.rows.length > 0;
}

/**
 * The bundle cap (`bundles` — active catalog rows), consulted ONLY where an ACTIVE row may be
 * ADDED: a new identity's genesis, and unarchive. New versions of existing bundles never come
 * through here, and a retry naming an id that already stands ACTIVE here is no growth — the
 * registration no-ops on it, so refusing would turn a lost-ack retry of a landed publish into
 * `BUNDLE_LIMIT_REACHED`. A no-op without a limit (the OSS default). Two spellings of one
 * check:
 *  - `bundleCapRefusal` runs BEFORE the vault call — the cheap read that keeps the common
 *    refusal from leaving ingested bytes behind;
 *  - `bundleCapRefusalInTx` is the AUTHORITY, inside the registration transaction under an
 *    advisory lock, so two concurrent geneses at the boundary serialize instead of both
 *    slipping past the count.
 */
export async function bundleCapRefusal(
  actor: PublishActor,
  bundleId: string,
): Promise<McpGateRefusal | null> {
  const entitlements = await composition.entitlements.forWorkspace(actor.workspaceId);
  const limit = entitlements.limit("bundles");
  if (limit === null) {
    return null;
  }
  const db = getDb();
  if (await activeBundleExists(db, actor.workspaceId, bundleId)) {
    return null;
  }
  return (await activeBundleCount(db, actor.workspaceId)) >= limit ? BUNDLE_LIMIT_REFUSAL : null;
}

export async function bundleCapRefusalInTx(
  tx: Tx,
  actor: PublishActor,
  bundleId: string,
): Promise<McpGateRefusal | null> {
  const entitlements = await composition.entitlements.forWorkspace(actor.workspaceId);
  const limit = entitlements.limit("bundles");
  if (limit === null) {
    return null;
  }
  await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${`bundles:${actor.workspaceId}`}))`);
  if (await activeBundleExists(tx, actor.workspaceId, bundleId)) {
    return null;
  }
  return (await activeBundleCount(tx, actor.workspaceId)) >= limit ? BUNDLE_LIMIT_REFUSAL : null;
}

// ── The history window (`history-days`) ─────────────────────────────────────────────────────

/**
 * The workspace's revert-reach window in days, or null when none is set (the OSS default —
 * unlimited). Nothing is ever deleted under a window: rows older than the cutoff stay listed
 * (annotated) and only the REVERT doors refuse them, so a wider window restores access whole.
 */
export async function historyWindowDays(workspaceId: string): Promise<number | null> {
  const entitlements = await composition.entitlements.forWorkspace(workspaceId);
  return entitlements.limit("history-days");
}

/**
 * Whether a revert target sits OUTSIDE the workspace's history window: the version's own
 * recorded creation time (the custody mirror) against now minus the window. No window, or a
 * version the mirror does not hold, is `false` — the revert flow's own refusals (unknown
 * version, purged target) stay the authority on what exists.
 */
export async function versionOutsideHistoryWindow(
  actor: PublishActor,
  bundleId: string,
  versionId: string,
): Promise<boolean> {
  const days = await historyWindowDays(actor.workspaceId);
  if (days === null) {
    return false;
  }
  const rows = await getDb()
    .select({ createdAt: planeVersion.createdAt })
    .from(planeVersion)
    .where(
      and(
        eq(planeVersion.workspaceId, actor.workspaceId),
        eq(planeVersion.bundleId, bundleId),
        eq(planeVersion.versionId, versionId),
      ),
    )
    .limit(1);
  const createdAt = rows[0]?.createdAt;
  if (createdAt === undefined) {
    return false;
  }
  return createdAt.getTime() < Date.now() - days * 24 * 60 * 60 * 1000;
}

/**
 * The creation times of a set of versions (the history page's lock annotation read) — one
 * query over the custody mirror; versions the mirror does not hold are simply absent.
 */
export async function versionCreatedAtMap(
  actor: PublishActor,
  bundleId: string,
  versionIds: string[],
): Promise<Map<string, Date>> {
  if (versionIds.length === 0) {
    return new Map();
  }
  const rows = await getDb()
    .select({ versionId: planeVersion.versionId, createdAt: planeVersion.createdAt })
    .from(planeVersion)
    .where(
      and(
        eq(planeVersion.workspaceId, actor.workspaceId),
        eq(planeVersion.bundleId, bundleId),
        inArray(planeVersion.versionId, versionIds),
      ),
    );
  return new Map(rows.map((r) => [r.versionId, r.createdAt]));
}

// ── Addressing a version by what a person can copy ───────────────────────────────────────────

/** The shortest prefix a version id may be addressed by — git's own object-prefix floor. */
export const VERSION_PREFIX_MIN = 8;

const VERSION_REF = new RegExp(`^[0-9a-f]{${VERSION_PREFIX_MIN},64}$`);

/** Is `typed` shaped like a version address at all — a full id or a long-enough prefix? The cheap
 * gate a page runs before it asks the database anything. */
export function isVersionRef(typed: string): boolean {
  return VERSION_REF.test(typed);
}

/**
 * Resolve a version id AS TYPED IN A URL against ONE bundle's versions, by git's object-prefix
 * rule: a full 64-hex id addresses itself, and a prefix of at least eight hex characters
 * addresses the one version that starts with it.
 *
 * Every id this product shows a person is the 12-hex SHORT form — the link text on a version, the
 * History rows, `topos log`, and every CLI receipt — so a URL assembled from what the app itself
 * printed has to open. Only the full 64-hex form did, which made a hand-built version URL a 404
 * over an id the same page had just rendered.
 *
 * An ambiguous prefix, a prefix nothing matches, and a token of the wrong shape all answer `null`;
 * the caller turns that into the uniform 404, so a probe learns nothing a member could not list
 * anyway. A FULL id comes back WITHOUT asking the mirror: whether the vault still holds those
 * bytes is the vault's answer to give (the page's own "no readable version" card), not this
 * lookup's.
 */
export async function resolveVersionRef(
  actor: PublishActor,
  bundleId: string,
  typed: string,
): Promise<string | null> {
  if (!isVersionRef(typed)) {
    return null;
  }
  if (typed.length === 64) {
    return typed;
  }
  // `typed` is hex-only by the gate above, so it carries no LIKE metacharacter. Two rows is all
  // the question needs: one is the answer, two is an ambiguity, and neither reads further.
  const rows = await getDb()
    .select({ versionId: planeVersion.versionId })
    .from(planeVersion)
    .where(
      and(
        eq(planeVersion.workspaceId, actor.workspaceId),
        eq(planeVersion.bundleId, bundleId),
        like(planeVersion.versionId, `${typed}%`),
      ),
    )
    .limit(2);
  return rows.length === 1 ? (rows[0]?.versionId ?? null) : null;
}

// ── Who authored a version (the display-time key) ───────────────────────────────────────────

/**
 * The commit-author string a client signs with: `d_` + 32 lowercase hex. Anything else is a
 * person-shaped attribution already (a web ceremony records the actor's own display), so it is
 * neither recorded here nor resolved on the way out.
 */
const DEVICE_AUTHOR = /^d_[0-9a-f]{32}$/;

/** Is `author` a machine id rather than a person? */
export function isDeviceAuthor(author: string): boolean {
  return DEVICE_AUTHOR.test(author);
}

/**
 * Record WHO published this version — the acting person against the one version they published.
 *
 * Called from inside the transaction that lands an ACCEPTED write (a publish, a proposal's
 * ingest, a revert's forward commit) and nowhere else, so a refused op records nothing and
 * authorship is bound to a version rather than to a machine. The row is written once and never
 * rewritten: the same machine signing in as someone else later authors ITS OWN versions and
 * relabels none of the ones already here.
 *
 * The `device_owner` row beside it is the fallback for versions written BEFORE this table
 * existed — an append-only observation, first write per person wins, and a machine seen as two
 * people ends up naming neither (see [`versionAuthorDisplays`]).
 */
export async function recordVersionAuthorInTx(
  tx: Tx,
  actor: PublishActor,
  bundleId: string,
  versionId: string,
  author: string,
): Promise<void> {
  await tx
    .insert(versionAuthor)
    .values({ workspaceId: actor.workspaceId, bundleId, versionId, userId: actor.userId })
    .onConflictDoNothing({ target: [versionAuthor.bundleId, versionAuthor.versionId] });
  if (!isDeviceAuthor(author)) {
    return;
  }
  await tx
    .insert(deviceOwner)
    .values({ workspaceId: actor.workspaceId, deviceId: author, userId: actor.userId })
    .onConflictDoNothing();
}

/** One version as the display resolver reads it: its id and the author custody recorded. */
export interface RecordedAuthor {
  versionId: string;
  author: string;
}

/**
 * WHO to show as each version's author, keyed by version id.
 *
 * Two sources, in order. `version_author` is the authority: it names the person who published
 * that exact version, so a machine that has since changed hands cannot touch it. A version with
 * no such row predates the table, and only then does the machine-level fallback speak — and only
 * when it is unambiguous: a device this workspace has seen publish as exactly ONE person names
 * them, a device seen as two names nobody. Versions absent from the returned map keep the author
 * they were signed with, which for a machine id means the id itself.
 */
export async function versionAuthorDisplays(
  // Every surface that renders an author reads it: the session lane's log (a SessionActor or a
  // machine token) and the web's own bundle pages (a MemberActor). Only the workspace scope is
  // used — this read asks no other question of the actor.
  actor: ReadActor | MemberActor,
  bundleId: string,
  versions: readonly RecordedAuthor[],
): Promise<Map<string, string>> {
  const display = new Map<string, string>();
  if (versions.length === 0) {
    return display;
  }
  const db = getDb();
  const ids = [...new Set(versions.map((v) => v.versionId))];
  const authored = await db
    .select({ versionId: versionAuthor.versionId, name: user.name, email: user.email })
    .from(versionAuthor)
    .innerJoin(user, eq(user.id, versionAuthor.userId))
    .where(
      and(
        eq(versionAuthor.workspaceId, actor.workspaceId),
        eq(versionAuthor.bundleId, bundleId),
        inArray(versionAuthor.versionId, ids),
      ),
    );
  for (const row of authored) {
    display.set(row.versionId, personAttribution(row.name, row.email));
  }

  // The fallback, for versions this app has no author row for at all.
  const orphans = versions.filter((v) => !display.has(v.versionId) && isDeviceAuthor(v.author));
  const devices = [...new Set(orphans.map((v) => v.author))];
  if (devices.length === 0) {
    return display;
  }
  const observed = await db
    .select({ deviceId: deviceOwner.deviceId, name: user.name, email: user.email })
    .from(deviceOwner)
    .innerJoin(user, eq(user.id, deviceOwner.userId))
    .where(
      and(eq(deviceOwner.workspaceId, actor.workspaceId), inArray(deviceOwner.deviceId, devices)),
    );
  const byDevice = new Map<string, string | null>();
  for (const row of observed) {
    // A SECOND person on the same machine settles it as unknowable — never a coin toss.
    byDevice.set(
      row.deviceId,
      byDevice.has(row.deviceId) ? null : personAttribution(row.name, row.email),
    );
  }
  for (const orphan of orphans) {
    const person = byDevice.get(orphan.author);
    if (person != null) {
      display.set(orphan.versionId, person);
    }
  }
  return display;
}

/** One version's author as a person, or the author custody recorded when nothing names them. */
export function displayAuthor(
  versionId: string,
  author: string,
  people: Map<string, string>,
): string {
  return people.get(versionId) ?? author;
}

export interface GenesisRegistration {
  bundleId: string;
  name: string;
  /** The placement's outcome, when a channel was named — or when the DEFAULT `everyone`
   * placement was withheld by its curated mode (`curated_role_required`). */
  placement?: "placed" | "curated_role_required" | "channel_not_found";
}

/**
 * NO CHANNEL AT ALL — the third thing a genesis destination can say. `toChannel` reads three
 * ways: a channel NAME places into that channel, `null` takes the workspace default, and this
 * places NOWHERE — the bundle lands in the catalog and reaches nobody until someone puts it in a
 * channel later. A symbol rather than a reserved word, so no name typed into a form or sent over
 * a wire can ever spell it.
 */
export const NO_CHANNEL: unique symbol = Symbol("no-channel");

/** Where a genesis publish's REACH goes: a named channel · the default · nowhere. */
export type GenesisDestination = string | null | typeof NO_CHANNEL;

/**
 * Register a NEW bundle at its genesis publish, inside the caller's final transaction: the
 * bundle row (name minted from the display name with suffix-on-collision — `name`, `name-2`,
 * `name-3`…; `kind` is the catalog tag the publish declared, 'skill' when it declared none —
 * birth-only, never rewritten) and an EXCLUSIVE placement. The author gets no standing row of
 * their own: publishing places the bundle, it does not assign it to the publisher.
 * Placement is exclusive because `--to` is the targeting mechanism: with NO `--to` the bundle
 * lands in the default `everyone` channel; with a `--to` channel named (`everyone` included — no
 * string-match bypass) it lands in THAT channel alone; with `NO_CHANNEL` it lands in none, which
 * is a destination a caller ASKS for and never a fallback this function picks. EVERY placement is
 * gated by the channel's mode (custody is never curation-blocked; REACH is — including the default
 * channel, including genesis): a curated channel withholds a member's placement with
 * `curated_role_required`, riding the receipt details independent of the version gate. A
 * withheld placement leaves the bundle in NO channel (catalog-only) — the disclosure rides the
 * receipt, and the bundle is registered either way.
 */
/** The actor shape BOTH publish doors satisfy: the session lane's SessionActor and the web
 * add-from-GitHub page's MemberActor (sessionId rides into audit when present). */
export interface PublishActor {
  readonly userId: string;
  readonly display: string;
  readonly workspaceId: string;
  readonly role: "owner" | "reviewer" | "member";
  readonly sessionId?: string;
}

export async function registerGenesisBundleInTx(
  tx: Tx,
  actor: PublishActor,
  bundleId: string,
  displayName: string | null,
  toChannel: GenesisDestination,
  kind: string | null = null,
): Promise<GenesisRegistration> {
  const ws = actor.workspaceId;
  const base = mintCatalogName(displayName, bundleId);
  let name = base;
  for (let n = 2; ; n++) {
    // `topos` is reserved for the CLI's built-in skill — treated exactly like a taken name
    // (suffix-on-collision, no oracle, no new refusal shape), so no workspace skill can ever
    // shadow the built-in in an agent's skill dirs.
    const reserved = RESERVED_BUNDLE_NAMES.has(name);
    const taken = reserved
      ? []
      : await tx
          .select({ id: bundle.id })
          .from(bundle)
          .where(and(eq(bundle.workspaceId, ws), eq(bundle.name, name)))
          .limit(1);
    if (!reserved && taken.length === 0) {
      break;
    }
    name = `${base.slice(0, 60)}-${n}`;
  }
  // Idempotent on the bundle id: a same-id genesis that races its own retry no-ops the insert
  // (the row already stands with this id; every downstream write below keys on the id, not the
  // name, so they converge). A distinct bundle folding to the SAME catalog name concurrently
  // still trips the name-unique constraint and self-heals on the next retry's suffix pick.
  await tx
    .insert(bundle)
    .values({
      id: bundleId,
      workspaceId: ws,
      name,
      displayName,
      // What this bundle IS, recorded once at birth ('skill' when the publish declares
      // nothing) — the catalog tag every later publish is checked against.
      kind: kind ?? "skill",
      createdBy: actor.userId,
    })
    .onConflictDoNothing({ target: bundle.id });
  // EXCLUSIVE placement: the default `everyone` channel ONLY when no `--to` was named (`--to`
  // targets a subset; adding `everyone` too would deliver to the whole workspace anyway,
  // defeating the targeting). REACH is curation-gated even here: a CURATED default channel
  // withholds a member's placement (`curated_role_required` rides the receipt details — the
  // same outcome a named curated `--to` answers), while the publish itself — custody — still
  // lands. The member never asked for a channel, so the default's refusal never fails the op.
  let placement: GenesisRegistration["placement"];
  if (toChannel === null) {
    // FOR UPDATE — the same pin the named `--to` placement takes: a concurrent open→curated
    // flip of the default channel must conflict here, never slip a member's placement past
    // the curation gate mid-transaction.
    const everyone = await tx
      .select({ id: channel.id, mode: channel.mode })
      .from(channel)
      .where(and(eq(channel.workspaceId, ws), eq(channel.isDefault, true)))
      .limit(1)
      .for("update");
    if (everyone[0] !== undefined) {
      if (everyone[0].mode === "curated" && actor.role === "member") {
        placement = "curated_role_required";
      } else {
        await tx
          .insert(channelBundle)
          .values({ channelId: everyone[0].id, workspaceId: ws, bundleId, addedBy: actor.userId })
          .onConflictDoNothing();
      }
    }
  }
  // A named `--to` — `everyone` included — rides the ONE gated path every channel placement
  // runs (the old `everyone` string-match bypassed the mode gate). `NO_CHANNEL` falls through
  // both arms with nothing written and nothing to report: no reach was asked for, so there is
  // no outcome to disclose.
  if (typeof toChannel === "string") {
    placement = await placeIntoChannelInTx(tx, actor, bundleId, toChannel);
  }
  await auditInTx(tx, {
    workspaceId: ws,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: "skill_registered",
    subject: bundleId,
    outcome: "ok",
    details: { name },
  });
  return { bundleId, name, ...(placement === undefined ? {} : { placement }) };
}

/** The `--to` placement inside a publish transaction — mode-gated, EXISTING channels only:
 * publish never mints a channel (channel creation is a deliberate curation act, done on the
 * web). The CLI verifies existence before sending; this in-transaction refusal closes the
 * race where the channel is deleted between that check and the write — the publish itself
 * (custody) still lands, and `channel_not_found` rides the receipt details. */
export async function placeIntoChannelInTx(
  tx: Tx,
  actor: PublishActor,
  bundleId: string,
  channelName: string,
): Promise<"placed" | "curated_role_required" | "channel_not_found"> {
  const ws = actor.workspaceId;
  // FOR UPDATE: pin the row for the rest of this transaction. A concurrent DELETE would
  // otherwise abort the final transaction on the FK (after custody already published) instead
  // of answering the typed refusal — and a concurrent mode flip (open → curated) or rename
  // must CONFLICT here too, or a member's placement could slip past the curation gate (or into
  // a channel `--to` no longer names).
  const rows = await tx
    .select({ id: channel.id, mode: channel.mode })
    .from(channel)
    .where(and(eq(channel.workspaceId, ws), eq(channel.name, channelName)))
    .limit(1)
    .for("update");
  const row = rows[0];
  if (row === undefined) {
    return "channel_not_found";
  }
  if (row.mode === "curated" && actor.role === "member") {
    return "curated_role_required";
  }
  await tx
    .insert(channelBundle)
    .values({ channelId: row.id, workspaceId: ws, bundleId, addedBy: actor.userId })
    .onConflictDoNothing();
  return "placed";
}

// ── Proposal rows + verdict notices ─────────────────────────────────────────────────────────

/**
 * Open a proposal row for an ingested candidate. Idempotent per (bundle, candidate): an open
 * row for the same candidate is reused (a lost-ack re-propose converges), a resolved one gets
 * a fresh row.
 */
export async function openProposalInTx(
  tx: Tx,
  actor: SessionActor,
  bundleId: string,
  candidateVersionId: string,
): Promise<{ proposalId: string }> {
  const openFilter = and(
    eq(proposal.workspaceId, actor.workspaceId),
    eq(proposal.bundleId, bundleId),
    eq(proposal.candidateVersionId, candidateVersionId),
    eq(proposal.status, "open"),
  );
  const existing = await tx.select({ id: proposal.id }).from(proposal).where(openFilter).limit(1);
  if (existing[0] !== undefined) {
    return { proposalId: existing[0].id };
  }
  // Insert-then-converge: the partial unique index (one open proposal per candidate) is the race
  // arbiter — a concurrent re-propose that lost the select gets ON CONFLICT DO NOTHING here and
  // re-reads the winner's row, so the inbox never carries two identical open proposals.
  const proposalId = mintProposalId();
  const inserted = await tx
    .insert(proposal)
    .values({
      id: proposalId,
      workspaceId: actor.workspaceId,
      bundleId,
      candidateVersionId,
      proposedBy: actor.userId,
    })
    .onConflictDoNothing()
    .returning({ id: proposal.id });
  if (inserted[0] === undefined) {
    const winner = await tx.select({ id: proposal.id }).from(proposal).where(openFilter).limit(1);
    if (winner[0] !== undefined) {
      return { proposalId: winner[0].id };
    }
    // No open row despite the conflict — the concurrent open resolved between the two reads; a
    // fresh propose is legitimate, so fall through to record this one under a new id.
    await tx.insert(proposal).values({
      id: proposalId,
      workspaceId: actor.workspaceId,
      bundleId,
      candidateVersionId,
      proposedBy: actor.userId,
    });
  }
  await auditInTx(tx, {
    workspaceId: actor.workspaceId,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: "proposal_opened",
    subject: bundleId,
    outcome: "ok",
    details: { versionId: candidateVersionId },
  });
  return { proposalId };
}

export interface OpenProposalRow {
  id: string;
  bundleId: string;
  candidateVersionId: string;
  proposedBy: string | null;
}

/** Lock ONE open proposal row by candidate — the review transaction's FOR UPDATE fence. */
export async function lockOpenProposalInTx(
  tx: Tx,
  ws: string,
  bundleId: string,
  candidateVersionId: string,
): Promise<OpenProposalRow | undefined> {
  const rows = await tx.execute(sql`
    SELECT id, bundle_id, candidate_version_id, proposed_by
    FROM ${proposal}
    WHERE workspace_id = ${ws} AND bundle_id = ${bundleId}
      AND candidate_version_id = ${candidateVersionId} AND status = 'open'
    FOR UPDATE
  `);
  const row = rows.rows[0] as
    | { id: string; bundle_id: string; candidate_version_id: string; proposed_by: string | null }
    | undefined;
  return row === undefined
    ? undefined
    : {
        id: row.id,
        bundleId: row.bundle_id,
        candidateVersionId: row.candidate_version_id,
        proposedBy: row.proposed_by,
      };
}

/** Resolve a locked proposal row + write the author's verdict notice. */
export async function resolveProposalInTx(
  tx: Tx,
  actor:
    | SessionActor
    | { userId: string; display: string; workspaceId: string; sessionId?: string },
  row: OpenProposalRow,
  verdict: "approved" | "rejected" | "withdrawn",
  reason: string | null,
): Promise<void> {
  await tx
    .update(proposal)
    .set({
      status: verdict,
      resolvedBy: actor.userId,
      resolvedReason: reason,
      resolvedAt: new Date(),
    })
    .where(eq(proposal.id, row.id));
  // Withdraw is the author's own act — no verdict notice for telling yourself.
  if (verdict !== "withdrawn" && row.proposedBy !== null && row.proposedBy !== actor.userId) {
    await tx.insert(notice).values({
      userId: row.proposedBy,
      workspaceId: actor.workspaceId,
      kind: "verdict",
      payload: {
        skill_id: row.bundleId,
        version_id: row.candidateVersionId,
        actor: actor.display,
        outcome: verdict === "approved" ? "approve" : "reject",
        ...(reason === null ? {} : { reason }),
      },
    });
  }
  await auditInTx(tx, {
    workspaceId: actor.workspaceId,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: `proposal_${verdict}`,
    subject: row.bundleId,
    outcome: "ok",
    details: { versionId: row.candidateVersionId, ...(reason === null ? {} : { reason }) },
  });
}

/** Run one final-transaction body (the routes' row-write + receipt step). */
export async function inFinalTx<T>(fn: (tx: Tx) => Promise<T>): Promise<T> {
  return await getDb().transaction(fn);
}

/** The escape a transaction body throws to roll back while carrying an answer out. */
class TxAbort<A> extends Error {
  constructor(readonly answer: A) {
    super("transaction aborted with an answer");
  }
}

/**
 * A final transaction that can END IN A REFUSAL — rolling everything back and still handing the
 * refusal to the caller.
 *
 * A genesis publish needs this. The bundle row must exist before its identity can be claimed
 * (the claim's foreign key points at it), but the claim can REFUSE — and a refused publish must
 * leave no catalog row behind. Returning the refusal normally would commit the registration it
 * is refusing, so the body throws instead and the rollback is what makes "refused" mean nothing
 * was written. The caller gets the answer either way and never sees the throw.
 */
export async function inFinalTxOrRefusal<T, A>(
  fn: (tx: Tx, refuse: (answer: A) => never) => Promise<T>,
): Promise<{ refused: null; value: T } | { refused: A }> {
  const refuse = (answer: A): never => {
    throw new TxAbort(answer);
  };
  try {
    return { refused: null, value: await getDb().transaction((tx) => fn(tx, refuse)) };
  } catch (error) {
    if (error instanceof TxAbort) {
      return { refused: error.answer as A };
    }
    throw error;
  }
}
