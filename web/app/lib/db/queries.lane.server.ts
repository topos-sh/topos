import { and, asc, eq, sql } from "drizzle-orm";
import { alias } from "drizzle-orm/pg-core";
import { composition } from "@/composition.server";
import type { SessionActor } from "@/lib/auth/guards.server";
import { CATALOG_BUNDLE_KINDS } from "@/lib/bundle-base";
import {
  auditInTx,
  feedDemandSql,
  mintInvitationId,
  mintInviteToken,
  sessionUnexpiredSql,
  supersedeDeclinedInvitationTx,
} from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import {
  inviteCapRefusalInTx,
  inviteCooldownActiveInTx,
  lockInviteAddressesInTx,
  submissionCapRefusal,
} from "@/lib/db/invite-caps.server";
import { personDisplayLeftSql } from "@/lib/db/person-display.server";
import { MCP_RESOLVED_REVISION } from "@/lib/db/queries.mcp-catalog.server";
import { foldInviteEmail, INVITATION_TTL_MS } from "@/lib/db/queries.roster.server";
import {
  bundle,
  bundleUpstream,
  channel,
  invitation,
  notice,
  proposal,
  workspace,
} from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";
import { planeCurrentPointer, planeVersionDigest } from "@/lib/db/schema.custody";
import {
  gatewayDeliveryDocument,
  gatewayPublicBase,
  hasStreamableRemote,
  sanitizeReservedMeta,
} from "@/lib/gateway/delivery.server";

/**
 * The SESSION lane's data access — the row-op half of `/api/v1`, served entirely by this
 * tier. Every op takes the branded `SessionActor` (minted only by requireSessionActor) and
 * passes the actor's server-resolved person/session — never anything client-asserted beyond
 * the credential itself. Role gates read the actor's seat role; existence misses answer the
 * same status vocabulary as before so the wire mapping stays uniform.
 *
 * Delivery is DEMAND ∩ ENTITLEMENT: the person-side demand is their FEED (feedDemandSql —
 * everything assigned to them or to everyone, minus what they declined), the entitlement is
 * the seat itself (whole catalog). Project-side demand never reaches this module: the client
 * resolves `topos.toml` refs through ordinary catalog reads, each one seat-gated the same way.
 *
 * Multi-read answers (delivery, the channels index) run inside ONE REPEATABLE READ
 * transaction — one snapshot, so the served sets can never straddle a feed change.
 */

/** A REAL `text[]` value — an `ARRAY[...]::text[]` constructor with every element its own bind
 * parameter, so no hand-rolled literal ever has to escape a value. NULL elements ride through
 * (the report's per-row harness block is absent on a file bundle). */
function pgTextArray(values: (string | null)[]) {
  return values.length === 0
    ? sql`ARRAY[]::text[]`
    : sql`ARRAY[${sql.join(
        values.map((v) => sql`${v}`),
        sql`, `,
      )}]::text[]`;
}

// ── Delivery ─────────────────────────────────────────────────────────────────────────────────

export interface DeliverySkill {
  skill_id: string;
  name: string;
  kind: string;
  display_name?: string;
  protection: string;
  version_id: string;
  bundle_digest: string;
  generation: number;
  updated_at: number;
  /** Why the feed carries it: which assigned channels hold it, and/or a direct assignment.
   * The two OPTIONAL facts attribute DIRECT bundle assignments only (channel-carried delivery
   * keeps its `channels` attribution and gets neither): `assigned_by` is the display of the
   * assignment's creator when someone ELSE aimed it here (person-targeted preferred over the
   * everyone row), and `picked` marks the caller's own self-assignment. Both are OMITTED when
   * absent — never null, never false. */
  via: { channels: string[]; direct: boolean; assigned_by?: string; picked?: true };
}

/**
 * One connected MCP server in the feed — the DOCUMENT ITSELF, not a pointer to bytes.
 *
 * A `kind: 'mcp'` bundle names a row in the server catalog; there is nothing content-addressed to
 * fetch and no second round trip to make, so the document rides the delivery it belongs to and the
 * machine caches it beside its revision id. `revision_id` is what a device reports back as the
 * thing it holds — the catalog's version handle, where a file bundle reports a commit.
 */
export interface DeliveryMcpServer {
  skill_id: string;
  name: string;
  kind: string;
  display_name?: string;
  /** The revision this connection resolves to — a pin, else the server's `current`. */
  revision_id: string;
  /**
   * The `server.json`, in the official registry format — the resolved revision's document verbatim,
   * EXCEPT where routing resolves to the gateway: the server is then handed one remote at this
   * deployment's gateway, marked in `_meta` (app/lib/gateway/delivery.server.ts). Routing is
   * per row — the workspace switch, the connection's `gateway_policy`, the member's own choice
   * and the standing sign-in all weigh in (see `deliveryFor`); package-only documents and every
   * gateway-less deployment carry the stored bytes unchanged.
   */
  document: Record<string, unknown>;
  /** This connection follows ONE revision rather than the server's `current`. OMITTED when it
   *  does not — the wire spells absence by absence, never by `false`. */
  pinned?: true;
  /** The resolved revision was pulled back after publication. It is STILL SERVED — a pin is a
   *  promise — and the client discloses it. Omitted when it stands. */
  revoked?: true;
  /** When the resolved revision was published (epoch milliseconds). */
  updated_at: number;
  /** Why the feed carries it — the same attribution a skill row carries. */
  via: { channels: string[]; direct: boolean; assigned_by?: string; picked?: true };
}

/** One declined bundle of the caller's — identity + name, so a client can narrate the stance. */
export interface DeliveryDecline {
  skill_id: string;
  name: string;
}

export interface DeliveryNotice {
  id: string;
  kind: string;
  skill_id?: string;
  skill_name?: string;
  version_id?: string;
  actor?: string;
  outcome?: string;
  reason?: string;
  message?: string;
  created_at: string;
}

/** The complete `WireDelivery` body (the route serializes it verbatim). */
export interface DeliveryBody {
  schema_version: 1;
  workspace_id: string;
  /** The session's status; "pending" delivers NOTHING (the empty body below). */
  session_status: "active" | "pending";
  skills: DeliverySkill[];
  /** The connected MCP servers, documents inline — always present, possibly empty. */
  mcp_servers: DeliveryMcpServer[];
  /** The caller's standing declines (live bundles only), name-sorted — always present. */
  declined: DeliveryDecline[];
  notices: DeliveryNotice[];
  proposals_awaiting: number;
  staleness_window_ms: number;
}

/**
 * The PENDING session's delivery: shape-complete and EMPTY — no data flows over a pending
 * session (skills/notices empty, zero proposals), but the staleness clock still serves so the
 * client's freshness bookkeeping stays honest while it waits for approval.
 */
export async function emptyDeliveryFor(actor: SessionActor): Promise<DeliveryBody> {
  const wsRows = await getDb()
    .select({ stalenessWindowMs: workspace.stalenessWindowMs })
    .from(workspace)
    .where(eq(workspace.id, actor.workspaceId))
    .limit(1);
  return {
    schema_version: 1,
    workspace_id: actor.workspaceId,
    session_status: "pending",
    skills: [],
    mcp_servers: [],
    declined: [],
    notices: [],
    proposals_awaiting: 0,
    staleness_window_ms: wsRows[0]?.stalenessWindowMs ?? 604800000,
  };
}

/** RFC-3339 seconds + Z (the wire's timestamp spelling). */
function isoSeconds(date: Date): string {
  return date.toISOString().replace(/\.\d{3}Z$/, "Z");
}

/**
 * The person-layer answer for ONE session: their FEED (everything assigned to them or to
 * everyone, minus their declines), active + current-holding only, with `via` attribution, the
 * resolved protection, plus the caller's standing declines by name, the unacked notices, the
 * open-proposal count over the same set, and the ONE staleness clock. Every delivered skill is served at the workspace's `current` —
 * the server holds no version pins; a machine that wants an older version pins it locally.
 *
 * ONE READ, TWO LISTS, because the feed is one question and a bundle's KIND decides only what an
 * entitled row is made of. A file bundle carries a pointer into the vault; a connected MCP server
 * carries its document inline, resolved from the catalog (a pin, else the server's `current`) —
 * and a connection is the one thing that decides which list a row lands in, so the `via`
 * attribution, the ordering and the snapshot are shared rather than asked for twice.
 */
export async function deliveryFor(
  actor: FeedActor,
  /**
   * Whether the CALLING machine's renderer can attach its credential to a gateway address
   * (`clientPlacesGatewayEntries`). Defaults to false: a caller that cannot say — a page's own
   * feed read, a test — is never handed an address that needs a credential it may not attach.
   */
  placesGatewayEntries = false,
): Promise<DeliveryBody> {
  const ws = actor.workspaceId;
  // THE GATEWAY, resolved once per delivery rather than per row: an address can be handed out
  // only where a gateway is deployed AND the caller is a session (a page's own feed read has no
  // machine to address) AND that machine's renderer can attach its credential to a gateway
  // address. Per-ROW routing is decided below, from this plus the workspace switch, the
  // connection's own policy, the member's choice, and whether a sign-in stands.
  const gatewayBase =
    actor.sessionId === undefined || !placesGatewayEntries ? null : gatewayPublicBase();
  // WHETHER A SIGN-IN STANDS at the gateway for each connected server — the member's own or the
  // workspace's. Asked only where a gateway is deployed: on an install with none the `gateway`
  // schema does not exist, and the constant false keeps the query off it entirely.
  const credentialSql =
    gatewayBase === null
      ? sql`false`
      : sql`EXISTS (
               SELECT 1 FROM gateway.credential gc
               WHERE gc.workspace_id = ${ws} AND gc.server_id = bm.server_id
                 AND (gc.user_id = ${actor.userId} OR gc.user_id IS NULL)
             )`;
  return await getDb().transaction(
    async (tx) => {
      const rows = await tx.execute(sql`
        SELECT b.id AS skill_id, b.name, b.kind, b.display_name,
               COALESCE(b.protection, w.protection_default, 'open') AS protection,
               cp.version_id AS current_version_id, cp.generation,
               (extract(epoch from cp.moved_at) * 1000)::bigint AS updated_at,
               vd.bundle_digest AS current_digest,
               -- The catalog half: the revision this connection resolves to, and its document.
               r.id AS revision_id, r.document AS document, bm.server_id AS server_id,
               (bm.pinned_revision_id IS NOT NULL) AS pinned,
               (extract(epoch from r.published_at) * 1000)::bigint AS revision_at,
               -- The routing facts: the workspace switch, the connection's mandate, whether
               -- THIS member routed the server direct, and whether a sign-in stands.
               w.mcp_gateway AS ws_gateway,
               bm.gateway_policy AS gateway_policy,
               EXISTS (
                 SELECT 1 FROM web.mcp_gateway_optout go
                 WHERE go.workspace_id = ${ws} AND go.server_id = bm.server_id
                   AND go.user_id = ${actor.userId}
               ) AS gateway_optout,
               ${credentialSql} AS gateway_credential,
               ms.auth_mode AS auth_mode,
               COALESCE((
                 SELECT array_agg(DISTINCT ch.name ORDER BY ch.name)
                 FROM web.channel_bundle cb
                 JOIN web.channel ch ON ch.id = cb.channel_id
                 JOIN web.assignment ca
                   ON ca.channel_id = ch.id AND ca.workspace_id = ${ws}
                      AND (ca.user_id = ${actor.userId} OR ca.user_id IS NULL)
                 WHERE cb.workspace_id = ${ws} AND cb.bundle_id = b.id
               ), '{}') AS via_channels,
               EXISTS (
                 SELECT 1 FROM web.assignment da
                 WHERE da.workspace_id = ${ws} AND da.bundle_id = b.id
                   AND (da.user_id = ${actor.userId} OR da.user_id IS NULL)
               ) AS direct,
               -- Who aimed a direct bundle assignment here, when it was someone ELSE: curator
               -- provenance only (a self-pick coexisting with a curator's aim never masks it);
               -- the person-targeted row outranks the everyone one, newest as the tiebreak. The
               -- display COALESCEs to 'former member' inside the subquery so a gone creator
               -- account still reads as an attribution, not as "no assignment".
               (
                 SELECT COALESCE(NULLIF(btrim(u.name), ''), u.email, 'former member')
                 FROM web.assignment aa
                 LEFT JOIN web."user" u ON u.id = aa.created_by
                 WHERE aa.workspace_id = ${ws} AND aa.bundle_id = b.id
                   AND (aa.user_id = ${actor.userId} OR aa.user_id IS NULL)
                   AND NOT aa.self AND aa.created_by <> ${actor.userId}
                 ORDER BY (aa.user_id IS NOT NULL) DESC, aa.created_at DESC
                 LIMIT 1
               ) AS assigned_by,
               -- The caller's own pick: the provenance flag, not a created_by heuristic.
               EXISTS (
                 SELECT 1 FROM web.assignment pa
                 WHERE pa.workspace_id = ${ws} AND pa.bundle_id = b.id
                   AND pa.user_id = ${actor.userId} AND pa.self
               ) AS picked
        FROM (${feedDemandSql(actor.userId, ws)}) e
        JOIN web.bundle b ON b.id = e.bundle_id
        JOIN web.workspace w ON w.id = ${ws}
        LEFT JOIN plane.current_pointer cp ON cp.workspace_id = ${ws} AND cp.bundle_id = b.id
        LEFT JOIN plane.version_digest vd
          ON vd.workspace_id = ${ws} AND vd.bundle_id = b.id AND vd.version_id = cp.version_id
        -- THE CONNECTION AND WHAT IT RESOLVES TO: a pin is followed exactly, revoked or not (a
        -- pin is a promise); everything else follows the server's own current revision.
        LEFT JOIN web.bundle_mcp bm ON bm.workspace_id = ${ws} AND bm.bundle_id = b.id
        LEFT JOIN web.mcp_server ms ON ms.id = bm.server_id
        LEFT JOIN web.mcp_server_revision r ON r.id = ${MCP_RESOLVED_REVISION}
        -- A bundle delivers what its KIND delivers: bytes for a file bundle, a resolved document
        -- for a connected server. A connection whose server has nothing published resolves to
        -- nothing and is simply absent, as is a file bundle that never published.
        WHERE (r.id IS NOT NULL)
           OR (cp.version_id IS NOT NULL AND b.kind <> ALL(${pgTextArray([...CATALOG_BUNDLE_KINDS])}))
        ORDER BY b.name
      `);
      const via = (r: Record<string, unknown>): DeliverySkill["via"] => ({
        channels: r.via_channels as string[],
        direct: r.direct as boolean,
        // Optional attribution facts — OMITTED when absent, never null/false on the wire.
        ...(r.assigned_by === null ? {} : { assigned_by: r.assigned_by as string }),
        ...(r.picked === true ? { picked: true as const } : {}),
      });
      const skills: DeliverySkill[] = [];
      const mcpServers: DeliveryMcpServer[] = [];
      for (const r of rows.rows as Record<string, unknown>[]) {
        const common = {
          skill_id: r.skill_id as string,
          name: r.name as string,
          kind: r.kind as string,
          ...(r.display_name === null ? {} : { display_name: r.display_name as string }),
        };
        if (r.revision_id !== null) {
          // Sanitize FIRST — a stored document may carry a reserved `sh.topos/*` control key (an
          // older row, or a path that predates the gate); the trusted rewrite adds its own flag
          // only after. So a machine never receives a gateway flag the delivery side did not put
          // there, gateway deployed or not.
          const document = sanitizeReservedMeta(r.document as Record<string, unknown>);
          // HOW THIS ROW IS ROUTED, first answer wins: the workspace switch off = direct ·
          // the connection's 'direct' mandate = direct · its 'required' mandate = the gateway
          // for every machine, or NO ROW AT ALL for a machine that cannot be handed a gateway
          // address (too old, or a deployment running none) — a mandate that quietly fell back
          // to direct would not be one · no mandate = the gateway where one is deployed AND the
          // member has not chosen direct AND either the server needs no sign-in or one already
          // stands at the gateway (theirs or the workspace's). Until a sign-in stands the
          // machine keeps the server's own address, so turning the gateway on never breaks a
          // working server — the route follows the sign-in, in both directions.
          const wsGatewayOn = r.ws_gateway === "on";
          const policy = (r.gateway_policy as string | null) ?? null;
          const mandated = wsGatewayOn && policy === "required" && hasStreamableRemote(document);
          if (mandated && gatewayBase === null && actor.sessionId !== undefined) {
            continue;
          }
          const routed =
            gatewayBase !== null &&
            wsGatewayOn &&
            (mandated ||
              (policy === null &&
                r.gateway_optout !== true &&
                (r.auth_mode === "none" || r.gateway_credential === true)));
          mcpServers.push({
            ...common,
            revision_id: r.revision_id as string,
            document: !routed
              ? document
              : gatewayDeliveryDocument(document, {
                  base: gatewayBase as string,
                  // Non-null by construction: gatewayBase is null unless the actor is a session.
                  sessionId: actor.sessionId as string,
                  serverId: r.server_id as string,
                }),
            ...(r.pinned === true ? { pinned: true as const } : {}),
            updated_at: Number(r.revision_at),
            via: via(r),
          });
          continue;
        }
        skills.push({
          ...common,
          protection: r.protection as string,
          version_id: r.current_version_id as string,
          // A pointer without its digest row is a custody fault; serve the honest empty string
          // rather than fail the whole delivery (the client's re-hash will refuse the bundle).
          bundle_digest: (r.current_digest as string | null) ?? "",
          generation: Number(r.generation),
          updated_at: Number(r.updated_at),
          via: via(r),
        });
      }

      // The caller's standing declines — live bundles only, name-sorted for determinism. The
      // stance is served WITH delivery so a client can narrate "off by your choice" without a
      // second call.
      const declineRows = await tx.execute(sql`
        SELECT b.id AS skill_id, b.name
        FROM web.decline d
        JOIN web.bundle b ON b.id = d.bundle_id AND b.workspace_id = ${ws}
        WHERE d.workspace_id = ${ws} AND d.user_id = ${actor.userId}
          AND b.status <> 'deleted'
        ORDER BY b.name
      `);
      const declined: DeliveryDecline[] = (declineRows.rows as Record<string, unknown>[]).map(
        (r) => ({
          skill_id: r.skill_id as string,
          name: r.name as string,
        }),
      );

      const noticeRows = await tx.execute(sql`
        SELECT n.id, n.kind, n.payload, n.created_at, b.name AS live_name
        FROM web.notice n
        LEFT JOIN web.bundle b ON b.id = (n.payload ->> 'skill_id')
        WHERE n.workspace_id = ${ws} AND n.user_id = ${actor.userId} AND n.acked_at IS NULL
        ORDER BY n.created_at, n.id
      `);
      const notices: DeliveryNotice[] = (noticeRows.rows as Record<string, unknown>[]).map((r) => {
        const payload = (r.payload ?? {}) as Record<string, unknown>;
        const out: DeliveryNotice = {
          id: String(r.id),
          kind: r.kind as string,
          created_at: isoSeconds(new Date(r.created_at as string)),
        };
        for (const key of [
          "skill_id",
          "version_id",
          "actor",
          "outcome",
          "reason",
          "message",
        ] as const) {
          const value = payload[key];
          if (typeof value === "string" && value.length > 0) {
            out[key] = value;
          }
        }
        // The live catalog name outranks the payload snapshot (joined for narration).
        const liveName = r.live_name as string | null;
        const snapName = payload.skill_name;
        if (liveName !== null) {
          out.skill_name = liveName;
        } else if (typeof snapName === "string" && snapName.length > 0) {
          out.skill_name = snapName;
        }
        return out;
      });

      const proposalRows = await tx.execute(sql`
        SELECT COUNT(*) AS n FROM web.proposal p
        WHERE p.workspace_id = ${ws} AND p.status = 'open'
          AND p.bundle_id IN (${feedDemandSql(actor.userId, ws)})
      `);
      const proposalsAwaiting = Number((proposalRows.rows[0] as { n: string | number }).n);

      const wsRows = await tx
        .select({ stalenessWindowMs: workspace.stalenessWindowMs })
        .from(workspace)
        .where(eq(workspace.id, ws))
        .limit(1);

      const body: DeliveryBody = {
        schema_version: 1,
        workspace_id: ws,
        session_status: "active",
        skills,
        mcp_servers: mcpServers,
        declined,
        notices,
        proposals_awaiting: proposalsAwaiting,
        staleness_window_ms: wsRows[0]?.stalenessWindowMs ?? 604800000,
      };
      return body;
    },
    { isolationLevel: "repeatable read", accessMode: "read only" },
  );
}

// ── The applied-state report ─────────────────────────────────────────────────────────────────

/** One harness's applied state for a config-placed ('mcp') bundle, as the session reports it:
 * the registry slug, the state word (an OPEN vocabulary — stored verbatim, never branched on),
 * and an optional short qualifier. */
export interface ReportedHarnessState {
  slug: string;
  state: string;
  note?: string;
}

/**
 * The sessions page's applied-state report: UPSERT this session's (bundle, applied version,
 * per-harness states) rows and DELETE the rows it no longer reports — the session's report is
 * a complete snapshot of what the installation holds for this workspace, so absence is
 * meaningful (a removed project or an edited manifest stops reporting a bundle and the row
 * goes). A report is CLIENT-ASSERTED data, so every named bundle is re-checked to exist in the
 * workspace. The write FENCES on the live ACTIVE session row (FOR UPDATE): an in-flight report
 * that lost a race with a revocation must not resurrect state the ending just cascaded away.
 */
export async function reportApplied(
  actor: SessionActor,
  applied: { skillId: string; versionId: string; harnesses?: ReportedHarnessState[] | null }[],
): Promise<"ok" | "session_ended"> {
  const ws = actor.workspaceId;
  const skillIds = pgTextArray(applied.map((a) => a.skillId));
  const versionIds = pgTextArray(applied.map((a) => a.versionId));
  // The per-harness block rides as JSON text and lands cast — NULL where the session reported
  // none. Absence and emptiness are the SAME fact (a file bundle has no harness story), so an
  // empty list never reaches the column: one shape to read back, whatever the caller passed.
  const harnessStates = pgTextArray(
    applied.map((a) =>
      a.harnesses == null || a.harnesses.length === 0 ? null : JSON.stringify(a.harnesses),
    ),
  );
  return await getDb().transaction(async (tx) => {
    // The same liveness the guard decides — active AND unexpired — re-checked inside the
    // fence: a session may pass the route guard a breath before expiry and must not write
    // reporting state after it.
    const live = await tx.execute(
      sql`SELECT cs.id FROM web.cli_session cs
          JOIN web.workspace w ON w.id = cs.workspace_id
          WHERE cs.id = ${actor.sessionId} AND cs.workspace_id = ${ws} AND cs.status = 'active'
            AND ${sessionUnexpiredSql("cs", "w")}
          FOR UPDATE OF cs`,
    );
    if (live.rows.length === 0) {
      return "session_ended";
    }
    await tx.execute(sql`
      INSERT INTO web.session_bundle_state
        (session_id, bundle_id, applied_version_id, harness_state, reported_at)
      SELECT ${actor.sessionId}, r.skill_id, r.version_id, r.harness_state::jsonb, now()
      FROM UNNEST(${skillIds}, ${versionIds}, ${harnessStates})
        AS r(skill_id, version_id, harness_state)
      JOIN web.bundle b ON b.id = r.skill_id AND b.workspace_id = ${ws}
      ON CONFLICT (session_id, bundle_id) DO UPDATE
        SET applied_version_id = excluded.applied_version_id,
            harness_state = excluded.harness_state,
            reported_at = excluded.reported_at
    `);
    await tx.execute(sql`
      DELETE FROM web.session_bundle_state st
      WHERE st.session_id = ${actor.sessionId}
        AND NOT (st.bundle_id = ANY(${skillIds}))
    `);
    return "ok";
  });
}

// ── The describe reads (me / channels) ──────────────────────────────────────────────────

export interface LaneMe {
  name: string;
  displayName: string;
  role: string;
  /** The inviter's login address, when the seat records one (display attribution only). */
  invitedBy: string | null;
}

/** The caller's own membership facts (`GET /me`). */
export async function laneMe(actor: SessionActor): Promise<LaneMe | null> {
  const rows = await getDb().execute(sql`
    SELECT w.name, w.display_name, s.role, iu.email AS invited_by
    FROM web.workspace w
    JOIN web.seat s ON s.workspace_id = w.id AND s.user_id = ${actor.userId}
    LEFT JOIN web."user" iu ON iu.id = s.invited_by
    WHERE w.id = ${actor.workspaceId}
  `);
  const row = rows.rows[0] as
    | {
        name: string;
        display_name: string;
        role: string;
        invited_by: string | null;
      }
    | undefined;
  if (row === undefined) {
    return null;
  }
  return {
    name: row.name,
    displayName: row.display_name,
    role: row.role,
    invitedBy: row.invited_by,
  };
}

export interface LaneChannel {
  /** The immutable channel id (the web editor's toggle key; the wire route omits it). */
  channelId: string;
  name: string;
  mode: string;
  builtin: boolean;
  /** Whether this channel is ASSIGNED to the caller — to them by name, or to everyone (which
   * is how the workspace baseline reaches them). The wire keeps the field name `included`. */
  included: boolean;
  skills: { skillId: string; name: string }[];
}

/** The workspace channels index (`GET /channels`) — name-sorted, the default included. */
export async function laneChannels(actor: FeedActor): Promise<LaneChannel[]> {
  const ws = actor.workspaceId;
  return await getDb().transaction(
    async (tx) => {
      const skillRows = await tx.execute(sql`
        SELECT cb.channel_id, cb.bundle_id, b.name
        FROM web.channel_bundle cb
        JOIN web.bundle b ON b.id = cb.bundle_id
        WHERE cb.workspace_id = ${ws}
        ORDER BY b.name
      `);
      const byChannel = new Map<string, { skillId: string; name: string }[]>();
      for (const raw of skillRows.rows as {
        channel_id: string;
        bundle_id: string;
        name: string;
      }[]) {
        const list = byChannel.get(raw.channel_id) ?? [];
        list.push({ skillId: raw.bundle_id, name: raw.name });
        byChannel.set(raw.channel_id, list);
      }
      const channelRows = await tx.execute(sql`
        SELECT ch.id, ch.name, ch.mode, ch.is_default,
          EXISTS (SELECT 1 FROM web.assignment a
                  WHERE a.channel_id = ch.id AND a.workspace_id = ${ws}
                    AND (a.user_id = ${actor.userId} OR a.user_id IS NULL)) AS included
        FROM web.channel ch
        WHERE ch.workspace_id = ${ws}
        ORDER BY ch.name
      `);
      return (channelRows.rows as Record<string, unknown>[]).map((r) => ({
        channelId: r.id as string,
        name: r.name as string,
        mode: r.mode as string,
        builtin: r.is_default as boolean,
        included: r.included as boolean,
        skills: byChannel.get(r.id as string) ?? [],
      }));
    },
    { isolationLevel: "repeatable read", accessMode: "read only" },
  );
}

// ── The feed actor (the shape every feed-shaped read takes) ────────────────────────────────

/**
 * The actor shape BOTH feed doors satisfy: the session lane's SessionActor and the web page's
 * MemberActor (these reads take only the person + workspace — a feed is personal, so no role
 * gates apply). Structural, so both branded actors pass without a cast.
 */
export interface FeedActor {
  readonly userId: string;
  readonly workspaceId: string;
  /**
   * The CALLING session, where one made the call. Present on the lane's SessionActor and absent on
   * a page's MemberActor — the delivery rewrite needs it (a gateway address names the session that
   * will dial it), and every other feed read ignores it.
   */
  readonly sessionId?: string;
}

// ── Protection setters ───────────────────────────────────────────────────────────────────────

/** Tightening takes reviewer+; loosening back to open widens what members can do — owner. */
function protectionRoleGate(
  role: SessionActor["role"],
  tightens: boolean,
): "owner_role_required" | "reviewer_role_required" | null {
  if (tightens) {
    return role === "member" ? "reviewer_role_required" : null;
  }
  return role === "owner" ? null : "owner_role_required";
}

/**
 * Whether ENABLING review protection is available to this workspace (`allows("reviews")`,
 * OSS default: open). The gate is on enabling only — loosening back to `open` is never gated,
 * and bundles already protected keep working (no retroactive strip).
 */
export async function reviewsEnableRefused(workspaceId: string): Promise<boolean> {
  const entitlements = await composition.entitlements.forWorkspace(workspaceId);
  return !entitlements.allows("reviews");
}

/** Pin a bundle's protection level (`open` | `reviewed`; the route validated the value). */
export async function laneProtectBundle(
  actor: SessionActor,
  bundleId: string,
  level: "open" | "reviewed",
): Promise<
  "set" | "unknown_skill" | "owner_role_required" | "reviewer_role_required" | "reviews_unavailable"
> {
  const gate = protectionRoleGate(actor.role, level === "reviewed");
  if (gate !== null) {
    return gate;
  }
  if (level === "reviewed" && (await reviewsEnableRefused(actor.workspaceId))) {
    return "reviews_unavailable";
  }
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const updated = await tx
      .update(bundle)
      .set({ protection: level })
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId), eq(bundle.status, "active")))
      .returning({ id: bundle.id });
    if (updated.length === 0) {
      return "unknown_skill";
    }
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
      kind: "protect_skill",
      subject: bundleId,
      outcome: "ok",
      details: { level },
    });
    return "set";
  });
}

/** Set a channel's mode (`open` | `curated`; the route validated the value). */
export async function laneProtectChannel(
  actor: SessionActor,
  channelName: string,
  mode: "open" | "curated",
): Promise<
  | "set"
  | "unknown_channel"
  | "owner_role_required"
  | "reviewer_role_required"
  | "reviews_unavailable"
> {
  const gate = protectionRoleGate(actor.role, mode === "curated");
  if (gate !== null) {
    return gate;
  }
  if (mode === "curated" && (await reviewsEnableRefused(actor.workspaceId))) {
    return "reviews_unavailable";
  }
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const updated = await tx
      .update(channel)
      .set({ mode })
      .where(and(eq(channel.workspaceId, ws), eq(channel.name, channelName)))
      .returning({ id: channel.id });
    if (updated.length === 0) {
      return "unknown_channel";
    }
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
      kind: `mode_${mode}`,
      subject: updated[0]?.id ?? channelName,
      outcome: "ok",
    });
    return "set";
  });
}

// ── Notices ack ──────────────────────────────────────────────────────────────────────────────

/** Mark the caller's own notices read by id — idempotent; unknown ids are ignored. */
export async function laneAckNotices(actor: SessionActor, ids: string[]): Promise<"acked"> {
  const numeric = ids.map((id) => Number(id)).filter((n) => Number.isSafeInteger(n));
  if (numeric.length === 0) {
    return "acked";
  }
  await getDb()
    .update(notice)
    .set({ ackedAt: new Date() })
    .where(
      and(
        eq(notice.workspaceId, actor.workspaceId),
        eq(notice.userId, actor.userId),
        sql`${notice.ackedAt} IS NULL`,
        sql`${notice.id} = ANY(${`{${numeric.join(",")}}`}::bigint[])`,
      ),
    );
  return "acked";
}

// ── Invitations (a claim on a FUTURE user; requires armed mail — the route gates that) ─────

/** The session lane's minted invitations (folded address + fresh link token per address). */
export type LaneInviteOutcome =
  | {
      outcome: "invited";
      minted: { email: string; token: string }[];
      /** Addresses on a cooldown (invited repeatedly, server-wide) — no row written, no mail;
       * the receipt reports each as `already invited recently`. */
      skipped: string[];
      /**
       * The RESOLVED first destination — the bundle's own catalog kind ('skill' or 'mcp') or
       * 'channel', read from the row the hint resolved to rather than assumed from which field
       * the caller filled in. The audit row and the invitation mail both name the destination,
       * and both must call it what it is.
       */
      hint?: { kind: string; name: string };
    }
  | { outcome: "owner_role_required" }
  | { outcome: "unknown_skill" }
  | { outcome: "unknown_channel" }
  | { outcome: "bad_email" }
  /** Over the per-submission address cap — the whole submission refuses. */
  | { outcome: "too_many_addresses" }
  /** The workspace's member limit (seats + pending invitations) is reached. */
  | { outcome: "member_limit" }
  /** The inviter's rolling-day cap is reached. */
  | { outcome: "invite_limit" };

/**
 * The session lane's invitation write. Inviting is OWNER-ONLY — the gate runs against the
 * actor's seat role. The optional FIRST-DESTINATION hint (at most one — a skill or a channel
 * of this workspace, named by the caller) must resolve (all-or-none), lands on the invitation
 * row, and is delivered by the accept ceremony as a PROFILE PREFILL: the seat first, then the
 * include line in the same transaction. Each invitation mints a fresh single-use link token
 * (hash-stored); re-inviting an address supersedes its old link and any declined record.
 */
export async function laneInvite(
  actor: SessionActor,
  emails: string[],
  hint: { skill?: string; channel?: string },
): Promise<LaneInviteOutcome> {
  const ws = actor.workspaceId;
  const folded: string[] = [];
  for (const email of emails) {
    const canonical = foldInviteEmail(email);
    if (canonical === null) {
      return { outcome: "bad_email" };
    }
    folded.push(canonical);
  }
  if (actor.role !== "owner") {
    return { outcome: "owner_role_required" };
  }
  if (submissionCapRefusal(folded.length) !== null) {
    return { outcome: "too_many_addresses" };
  }
  return await getDb().transaction(async (tx) => {
    // The stateful caps (member limit + the rolling-day cap), counted INSIDE this transaction —
    // the same hard-cap discipline as the members-page door (invite-caps.server.ts).
    const refusal = await inviteCapRefusalInTx(tx, {
      workspaceId: ws,
      actorUserId: actor.userId,
      emails: folded,
    });
    if (refusal !== null) {
      return { outcome: refusal };
    }
    let hintBundleId: string | null = null;
    let hintChannelId: string | null = null;
    // The destination as RESOLVED. The lane's `skill` field names any bundle in the catalog,
    // MCP servers included, so the kind is read off the row — never inferred from the field.
    let resolvedHint: { kind: string; name: string } | undefined;
    if (hint.skill !== undefined) {
      const rows = await tx
        .select({ id: bundle.id, kind: bundle.kind })
        .from(bundle)
        .where(
          and(eq(bundle.workspaceId, ws), eq(bundle.name, hint.skill), eq(bundle.status, "active")),
        )
        .limit(1);
      const row = rows[0];
      if (row === undefined) {
        return { outcome: "unknown_skill" };
      }
      hintBundleId = row.id;
      resolvedHint = { kind: row.kind, name: hint.skill };
    } else if (hint.channel !== undefined) {
      const rows = await tx
        .select({ id: channel.id })
        .from(channel)
        .where(and(eq(channel.workspaceId, ws), eq(channel.name, hint.channel)))
        .limit(1);
      const row = rows[0];
      if (row === undefined) {
        return { outcome: "unknown_channel" };
      }
      hintChannelId = row.id;
      resolvedHint = { kind: "channel", name: hint.channel };
    }
    const expiresAt = new Date(Date.now() + INVITATION_TTL_MS);
    const minted: { email: string; token: string }[] = [];
    const skipped: string[] = [];
    await lockInviteAddressesInTx(tx, folded);
    for (const email of folded) {
      if (await inviteCooldownActiveInTx(tx, email)) {
        skipped.push(email);
        continue;
      }
      const token = mintInviteToken();
      await supersedeDeclinedInvitationTx(tx, ws, email);
      await tx.execute(sql`
        insert into ${invitation}
          (id, workspace_id, email, role, status, invited_by, expires_at,
           token_sha256, hint_bundle_id, hint_channel_id)
        values (${mintInvitationId()}, ${ws}, ${email}, 'member', 'pending', ${actor.userId},
                ${expiresAt}, sha256(convert_to(${token}, 'UTF8')),
                ${hintBundleId}, ${hintChannelId})
        on conflict (email, workspace_id) where status = 'pending'
        do update set invited_by = excluded.invited_by, expires_at = excluded.expires_at,
                      token_sha256 = excluded.token_sha256,
                      hint_bundle_id = excluded.hint_bundle_id,
                      hint_channel_id = excluded.hint_channel_id,
                      created_at = now()
      `);
      await auditInTx(tx, {
        workspaceId: ws,
        actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
        kind: "invitation_created",
        subject: email,
        outcome: "ok",
        details: { ...(resolvedHint === undefined ? {} : { hint: resolvedHint }) },
      });
      minted.push({ email, token });
    }
    return {
      outcome: "invited",
      minted,
      skipped,
      ...(resolvedHint === undefined ? {} : { hint: resolvedHint }),
    };
  });
}

// ── The session-lane catalog read (`GET /v1/workspaces/{ws}/skills`) ────────────────────────

export interface LaneSkillIndexEntry {
  skill_id: string;
  name: string;
  kind: string;
  status: string;
  version_id: string;
  bundle_digest: string;
  generation: number;
  display_name?: string;
  updated_at: number;
  open_proposals: number;
  /** The recorded upstream origin, present when the bundle was imported from an external
   * source — lets a client suggest the governed copy when the same source is added again. */
  upstream_host?: string;
  upstream_repo?: string;
  upstream_path?: string;
}

/** One connected MCP server in the workspace catalog — the document, resolved as delivery
 *  resolves it, so a project reference to a server this caller does not follow still renders. */
export interface LaneMcpIndexEntry {
  skill_id: string;
  name: string;
  kind: string;
  status: string;
  display_name?: string;
  revision_id: string;
  document: Record<string, unknown>;
  pinned?: true;
  revoked?: true;
  updated_at: number;
}

/** The workspace catalog — every FILE bundle holding a `current`, ordered by id. A connected
 *  server holds no pointer and is served by `laneMcpServersIndex` below. */
export async function laneSkillsIndex(
  // Only the workspace scope is read — session and token actors both pass.
  actor: { readonly workspaceId: string },
): Promise<LaneSkillIndexEntry[]> {
  const ws = actor.workspaceId;
  const rows = await getDb()
    .select({
      skillId: bundle.id,
      name: bundle.name,
      kind: bundle.kind,
      status: bundle.status,
      displayName: bundle.displayName,
      versionId: planeCurrentPointer.versionId,
      generation: planeCurrentPointer.generation,
      updatedAtMs: sql<string>`(extract(epoch from ${planeCurrentPointer.movedAt}) * 1000)::bigint`,
      bundleDigest: planeVersionDigest.bundleDigest,
      openProposals: sql<string>`(
        SELECT COUNT(*) FROM web.proposal p
        WHERE p.workspace_id = ${ws} AND p.bundle_id = ${bundle.id} AND p.status = 'open'
      )`,
      upstreamHost: bundleUpstream.host,
      upstreamRepo: bundleUpstream.repo,
      upstreamPath: bundleUpstream.path,
    })
    .from(bundle)
    .innerJoin(
      planeCurrentPointer,
      and(
        eq(planeCurrentPointer.workspaceId, bundle.workspaceId),
        eq(planeCurrentPointer.bundleId, bundle.id),
      ),
    )
    .leftJoin(
      bundleUpstream,
      and(
        eq(bundleUpstream.workspaceId, bundle.workspaceId),
        eq(bundleUpstream.bundleId, bundle.id),
      ),
    )
    .leftJoin(
      planeVersionDigest,
      and(
        eq(planeVersionDigest.workspaceId, bundle.workspaceId),
        eq(planeVersionDigest.bundleId, bundle.id),
        eq(planeVersionDigest.versionId, planeCurrentPointer.versionId),
      ),
    )
    .where(
      and(
        eq(bundle.workspaceId, ws),
        sql`${bundle.status} <> 'deleted'`,
        sql`${bundle.kind} <> ALL(${pgTextArray([...CATALOG_BUNDLE_KINDS])})`,
      ),
    )
    .orderBy(asc(bundle.id));
  return rows.map((r) => ({
    skill_id: r.skillId,
    name: r.name,
    kind: r.kind,
    status: r.status,
    version_id: r.versionId,
    bundle_digest: r.bundleDigest ?? "",
    generation: Number(r.generation),
    ...(r.displayName === null ? {} : { display_name: r.displayName }),
    updated_at: Number(r.updatedAtMs),
    open_proposals: Number(r.openProposals),
    ...(r.upstreamHost === null || r.upstreamRepo === null
      ? {}
      : {
          upstream_host: r.upstreamHost,
          upstream_repo: r.upstreamRepo,
          upstream_path: r.upstreamPath ?? "",
        }),
  }));
}

/**
 * The catalog's OTHER half: every connected MCP server, each resolved to the document a machine
 * would receive. It carries the document for the same reason delivery does — a project that
 * references a server nobody in the room follows still has to render its config, and there is no
 * second lane to fetch bytes from.
 */
export async function laneMcpServersIndex(
  actor: { readonly workspaceId: string },
): Promise<LaneMcpIndexEntry[]> {
  const ws = actor.workspaceId;
  const rows = await getDb().execute(sql`
    SELECT b.id AS skill_id, b.name, b.kind, b.status, b.display_name,
           r.id AS revision_id, r.document,
           (bm.pinned_revision_id IS NOT NULL) AS pinned,
           (extract(epoch from r.published_at) * 1000)::bigint AS revision_at
    FROM web.bundle_mcp bm
    JOIN web.bundle b ON b.id = bm.bundle_id AND b.workspace_id = bm.workspace_id
    JOIN web.mcp_server ms ON ms.id = bm.server_id
    JOIN web.mcp_server_revision r ON r.id = ${MCP_RESOLVED_REVISION}
    WHERE bm.workspace_id = ${ws} AND b.status <> 'deleted'
    ORDER BY b.id
  `);
  return (rows.rows as Record<string, unknown>[]).map((r) => ({
    skill_id: r.skill_id as string,
    name: r.name as string,
    kind: r.kind as string,
    status: r.status as string,
    ...(r.display_name === null ? {} : { display_name: r.display_name as string }),
    revision_id: r.revision_id as string,
    document: r.document as Record<string, unknown>,
    ...(r.pinned === true ? { pinned: true as const } : {}),
    updated_at: Number(r.revision_at),
  }));
}

// ── The shared helpers other DAL modules use ────────────────────────────────────────────────

/** The open-proposal rows of one bundle (the session lane's list read). */
export async function openProposalsOf(
  actor: SessionActor,
  bundleId: string,
): Promise<{ versionId: string; createdAt: Date }[]> {
  const rows = await getDb()
    .select({ versionId: proposal.candidateVersionId, createdAt: proposal.createdAt })
    .from(proposal)
    .where(
      and(
        eq(proposal.workspaceId, actor.workspaceId),
        eq(proposal.bundleId, bundleId),
        eq(proposal.status, "open"),
      ),
    )
    .orderBy(asc(proposal.createdAt), asc(proposal.candidateVersionId));
  return rows;
}

/** The log decoration read: the bundle's catalog identity + this app's proposal events. */
export interface LaneLogIdentity {
  bundleId: string;
  name: string;
  kind: string;
  status: string;
  baseName: string | null;
}

export interface LaneLogProposal {
  versionId: string;
  status: string;
  proposer: string;
  resolvedBy: string | null;
  resolvedReason: string | null;
  resolvedAt: Date | null;
  createdAt: Date;
}

export async function laneLogOf(
  actor: SessionActor,
  bundleId: string,
): Promise<{ identity: LaneLogIdentity; proposals: LaneLogProposal[] } | null> {
  const rows = await getDb()
    .select({
      bundleId: bundle.id,
      name: bundle.name,
      kind: bundle.kind,
      status: bundle.status,
      baseName: bundle.baseName,
    })
    .from(bundle)
    .where(and(eq(bundle.workspaceId, actor.workspaceId), eq(bundle.id, bundleId)))
    .limit(1);
  const identity = rows[0];
  if (identity === undefined) {
    return null;
  }
  // A second aliased `user` join resolves the RESOLVER's display (the proposer join already
  // resolves the proposer) so the wire serves a person display, never a raw user id.
  const resolver = alias(user, "resolver");
  const proposalRows = await getDb()
    .select({
      versionId: proposal.candidateVersionId,
      status: proposal.status,
      proposerDisplay: personDisplayLeftSql(user),
      resolvedBy: proposal.resolvedBy,
      resolverDisplay: personDisplayLeftSql(resolver),
      resolvedReason: proposal.resolvedReason,
      resolvedAt: proposal.resolvedAt,
      createdAt: proposal.createdAt,
    })
    .from(proposal)
    .leftJoin(user, eq(user.id, proposal.proposedBy))
    .leftJoin(resolver, eq(resolver.id, proposal.resolvedBy))
    .where(and(eq(proposal.workspaceId, actor.workspaceId), eq(proposal.bundleId, bundleId)))
    .orderBy(sql`${proposal.createdAt} DESC`);
  return {
    identity,
    proposals: proposalRows.map((p) => ({
      versionId: p.versionId,
      status: p.status,
      proposer: p.proposerDisplay ?? "former member",
      // Unresolved (open) carries no resolver — stays null; a resolved row serves the display,
      // falling back to "former member" when the resolver's user row is gone (mirrors proposer).
      resolvedBy: p.resolvedBy === null ? null : (p.resolverDisplay ?? "former member"),
      resolvedReason: p.resolvedReason,
      resolvedAt: p.resolvedAt,
      createdAt: p.createdAt,
    })),
  };
}

/** Every open proposal in the workspace (the review inbox), bundle name joined. */
export async function openProposalsIndex(actor: SessionActor): Promise<
  {
    id: string;
    bundleId: string;
    bundleName: string;
    versionId: string;
    proposedBy: string | null;
    proposerDisplay: string;
    proposerEmail: string | null;
    createdAt: Date;
  }[]
> {
  const rows = await getDb()
    .select({
      id: proposal.id,
      bundleId: proposal.bundleId,
      bundleName: bundle.name,
      versionId: proposal.candidateVersionId,
      proposedBy: proposal.proposedBy,
      proposerName: personDisplayLeftSql(user),
      proposerEmail: user.email,
      createdAt: proposal.createdAt,
    })
    .from(proposal)
    .innerJoin(
      bundle,
      and(eq(bundle.workspaceId, proposal.workspaceId), eq(bundle.id, proposal.bundleId)),
    )
    .leftJoin(user, eq(user.id, proposal.proposedBy))
    .where(and(eq(proposal.workspaceId, actor.workspaceId), eq(proposal.status, "open")))
    .orderBy(asc(proposal.createdAt), asc(proposal.id));
  return rows.map((r) => ({
    id: r.id,
    bundleId: r.bundleId,
    bundleName: r.bundleName,
    versionId: r.versionId,
    proposedBy: r.proposedBy,
    proposerDisplay: r.proposerName ?? "former member",
    proposerEmail: r.proposerEmail,
    createdAt: r.createdAt,
  }));
}
