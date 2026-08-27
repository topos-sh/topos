import { type SQL, sql } from "drizzle-orm";
import { isTokenActor, type ReadActor } from "@/lib/auth/guards.server";
import { gatewayCredentialsReadable } from "@/lib/db/queries.gateway.server";
import {
  gatewayDeliveryDocument,
  gatewayPublicBase,
  hasStreamableRemote,
  sanitizeReservedMeta,
} from "@/lib/gateway/delivery.server";

/**
 * THE ROUTING RULING — written ONCE, for every lane that hands a machine an MCP document.
 *
 * A connected server reaches a machine one of two ways: DIRECT (the config carries the vendor's
 * own address and the agent signs in for itself) or THROUGH THE GATEWAY (the config carries
 * `topos relay` at an address naming the calling session, and the gateway attaches the
 * workspace's sign-in on the far side). Which one is a per-server, per-person, per-deployment
 * decision, and it used to live inside the delivery query — so the delivery feed routed and the
 * other two lanes did not. A project manifest can only ever use explicit `[mcp]` rows and
 * channels (the feed walk is the machine scope's alone), which meant every project MCP row was
 * delivered direct and a server set "Required by workspace" was not, in fact, required.
 *
 * So the decision lives here and the lanes are callers: `deliveryFor` (the machine's own feed),
 * `laneMcpServersIndex` (the workspace catalog — every explicit row and every channel member, in
 * both scopes) and `laneMcpRevision` (the `topos.lock` path). A FOURTH lane added later has one
 * obvious thing to call, and the shared case table in the suite fails loudly if it does not.
 *
 * Two halves, because a routing decision needs facts from Postgres and judgement in TypeScript:
 * `mcpRoutingFactsSql` is the select-list fragment every lane splices into its own read, and
 * `routeMcpDocument` is the pure ruling over one such row. The ORDER inside the ruling is load
 * bearing: SANITIZE the stored document first, DECIDE second, REWRITE last — see below.
 *
 * This module composes SQL and holds no pool, no schema import and no query of its own. The one
 * thing it asks the database is through the DAL: whether the gateway's own schema is readable
 * here (see below). It is the ruling, not a door into the database.
 */

/**
 * WHO is asking, in the only two terms routing cares about.
 *
 * A machine token (CI) is a workspace credential with no person behind it, which is exactly what
 * `userId: null` means here: no personal opt-out row, no personal sign-in at the gateway — only
 * the workspace's own. It is still a MACHINE, so it has a session segment for its address (its
 * service session) and a `required` connection reaches it like anyone else's.
 */
export interface RoutingCaller {
  /** The person this delivery is for, or null where the caller is not one. */
  readonly userId: string | null;
  /**
   * The session a gateway address would name, or null where no machine is asking (a web page
   * reading its own feed). A person-less, machine-less read is never rewritten and never
   * withheld: there is nothing to hand an address to.
   */
  readonly sessionId: string | null;
}

/**
 * The caller behind a read lane's branded actor — a person's session, or a machine token.
 *
 * THE ID'S PREFIX IS A CONTRACT, not a label. The session segment of a gateway address is how
 * both far ends decide which credential may satisfy it: the machine's relay hands an `sn_…`
 * address to a stored person session and an `ss_…` one to `TOPOS_TOKEN`, refusing the crossed
 * case outright, and the gateway matches the segment against the credential it resolved. Handing
 * a machine token a person's session id (or the reverse) would forward one caller's work under
 * the other's name — the wrong entry in the usage ledger, the wrong vendor sign-in — so the two
 * ids below are never interchangeable, however alike their shapes look.
 */
export function routingCallerOf(actor: ReadActor): RoutingCaller {
  return isTokenActor(actor)
    ? { userId: null, sessionId: actor.serviceSessionId ?? null }
    : // `?? null` on BOTH, and not for tidiness: null is the value that falls to direct, while
      // `undefined` is truthy enough to reach the rewrite and would mint a live-looking address
      // with the literal "undefined" as its session segment. Every actor carries a session today;
      // the day one does not, this must degrade rather than emit that.
      { userId: actor.userId, sessionId: actor.sessionId ?? null };
}

/**
 * The gateway's public base for THIS caller, or null — resolved once per read rather than per
 * row. Null covers three things: nobody is asking for a machine, this deployment runs no gateway,
 * and — the one that is not a setting — a deployment that MEANS to run one whose schema is not
 * (yet) there. Null is also what keeps the query off the `gateway` schema entirely (see below).
 *
 * The env var alone was not enough. It says what a deployment intends; the schema says what
 * exists. A self-hoster who sets `GATEWAY_PUBLIC_URL` without starting a gateway, or any rolling
 * deploy where web is up before the gateway has run its lineage, would otherwise take a parse
 * error on every routed read — and routing is now three lanes, so that is every project's MCP
 * install rather than one machine's feed. Answering "no gateway" instead means those reads serve
 * the direct shape, which is exactly what such an install can honor.
 */
export async function mcpGatewayBaseFor(caller: RoutingCaller): Promise<string | null> {
  if (caller.sessionId === null) {
    return null;
  }
  const base = gatewayPublicBase();
  if (base === null) {
    return null;
  }
  return (await gatewayCredentialsReadable()) ? base : null;
}

/**
 * THE ROUTING FACTS, as a select-list fragment: the workspace switch, the connection's mandate,
 * whether THIS person routed the server direct, whether a sign-in stands at the gateway, the
 * server's auth tier, and the server id an address is built from.
 *
 * Splice it into a select list — it emits its own columns, comma-separated, with no trailing
 * comma. The query it lands in must join the three tables it reads under these aliases:
 * `w` = web.workspace · `bm` = web.bundle_mcp · `ms` = web.mcp_server.
 *
 * `gatewayDeployed` false keeps the `gateway` schema out of the SQL text altogether (a constant
 * false, not an EXISTS that resolves to none): on an install running no gateway that schema does
 * not exist, and a query naming it would fail rather than answer "no sign-in stands".
 */
export function mcpRoutingFactsSql(args: {
  workspaceId: string;
  userId: string | null;
  gatewayDeployed: boolean;
}): SQL {
  const { workspaceId: ws, userId } = args;
  // A person-less caller has no opt-out to honor — the row is keyed by user, and a machine token
  // is not one. Written as a constant rather than a lookup on a null id, which would read as an
  // EXISTS that happens to be empty.
  const optoutSql =
    userId === null
      ? sql`false`
      : sql`EXISTS (
               SELECT 1 FROM web.mcp_gateway_optout go
               WHERE go.workspace_id = ${ws} AND go.server_id = bm.server_id
                 AND go.user_id = ${userId}
             )`;
  // WHETHER A SIGN-IN STANDS at the gateway for this server: the caller's own or the
  // workspace's. A person-less caller can ride only the workspace one — a machine must never
  // route on some member's personal sign-in.
  const credentialSql = !args.gatewayDeployed
    ? sql`false`
    : userId === null
      ? sql`EXISTS (
               SELECT 1 FROM gateway.credential gc
               WHERE gc.workspace_id = ${ws} AND gc.server_id = bm.server_id
                 AND gc.user_id IS NULL
             )`
      : sql`EXISTS (
               SELECT 1 FROM gateway.credential gc
               WHERE gc.workspace_id = ${ws} AND gc.server_id = bm.server_id
                 AND (gc.user_id = ${userId} OR gc.user_id IS NULL)
             )`;
  return sql`w.mcp_gateway AS ws_gateway,
             bm.gateway_policy AS gateway_policy,
             ${optoutSql} AS gateway_optout,
             ${credentialSql} AS gateway_credential,
             ms.auth_mode AS auth_mode,
             bm.server_id AS server_id`;
}

/**
 * WHY a row was withheld — an OPEN vocabulary on the wire, with exactly one value today. A client
 * shows an unknown reason as itself; the field's PRESENCE is the ruling, the string only shapes
 * the sentence a person reads.
 */
export const WITHHELD_GATEWAY_REQUIRED = "gateway_required";

/**
 * The reasons this side may MINT — closed here even though the wire's vocabulary is open, because
 * an empty one is a silent bypass rather than a smaller answer: a client treats `""` as NO
 * withhold and falls through to an ordinary document read, so a blanked reason would land as an
 * unreadable row instead of a ruling.
 *
 * That fall-through is the CLIENT'S deliberate choice, not an oversight to route around, and the
 * asymmetry runs toward safety: a withhold REMOVES entries, so an emitter that blanked this field
 * across every row — an ORM writing `""` where it meant null — would strip every MCP entry on
 * every machine that asked. Leaving one server's entry standing beats stripping a fleet's on a
 * serializer's slip. Which is exactly why the guarantee has to live HERE, at the one construction
 * site: a literal union makes a blank unspellable rather than merely untested, so this side never
 * needs the client to rescue it. Adding a value is a two-sided change — the client needs an arm
 * for its sentence, or it narrates the generic one.
 */
export type WithheldReason = typeof WITHHELD_GATEWAY_REQUIRED;

/**
 * The ruling's answer for ONE row: the document to serve, or a withhold that must be SAID.
 *
 * `withheld` is not an error and not an empty document. It happens in exactly one case: a
 * connection MANDATED through the gateway that this caller cannot be handed an address for. A
 * mandate that quietly fell back to the server's own address would not be a mandate.
 *
 * The catalog-shaped lanes carry the reason rather than dropping the row, and that is the whole
 * point of naming it. Silence there is AMBIGUOUS: a client cannot tell a withdrawn entry from a
 * fetch that did not land, and it must not guess — it resolves that ambiguity toward KEEPING what
 * it has, deliberately, so that a flaky network never uninstalls a team's servers. A dropped row
 * would therefore leave every machine that already holds a direct entry holding it forever, which
 * is precisely the bypass the mandate exists to close. The machine's own FEED is the exception
 * and needs no marker: the feed is the demand list itself, so absence from it already means
 * "not yours", and the client reads it that way.
 */
export type RoutedMcpDocument =
  | { readonly kind: "withheld"; readonly reason: WithheldReason }
  | { readonly kind: "direct" | "gateway"; readonly document: Record<string, unknown> };

/**
 * HOW ONE ROW IS ROUTED, first answer wins: the workspace switch off = direct · the connection's
 * 'direct' mandate = direct · its 'required' mandate = the gateway for every machine, or NO ROW
 * AT ALL where no address can be produced (a deployment running none) · no mandate = the gateway
 * where one is deployed AND this person has not chosen direct AND either the server needs no
 * sign-in or one already stands at the gateway (theirs or the workspace's). Until a sign-in
 * stands the machine keeps the server's own address, so turning the gateway on never breaks a
 * working server — the route follows the sign-in, in both directions.
 *
 * THE ORDER IS THE POINT — sanitize, decide, rewrite:
 *  1. SANITIZE first. A stored document may carry a reserved `sh.topos/*` control key (a row
 *     written before the gate learned to refuse them, or by some future path). Delivery is the
 *     last chokepoint every document passes, so a machine can never receive a gateway flag this
 *     side did not put there — gateway deployed or not.
 *  2. DECIDE on the sanitized document (its remotes are what `required` can act on).
 *  3. REWRITE last, adding the trusted flag to a document already cleaned.
 * Anything that must VALIDATE a document validates the STORED one, before this runs: a rewritten
 * document carries `sh.topos/gateway`, which the document gate refuses by design.
 */
export function routeMcpDocument(args: {
  /** The document exactly as stored on the revision. */
  stored: Record<string, unknown>;
  /** One row of `mcpRoutingFactsSql` — this module's own columns. */
  facts: Record<string, unknown>;
  caller: RoutingCaller;
  /** `mcpGatewayBaseFor(caller)`, resolved once by the lane. */
  gatewayBase: string | null;
}): RoutedMcpDocument {
  const document = sanitizeReservedMeta(args.stored);
  const { facts, caller } = args;
  const wsGatewayOn = facts.ws_gateway === "on";
  const policy = (facts.gateway_policy as string | null) ?? null;
  const mandated = wsGatewayOn && policy === "required" && hasStreamableRemote(document);
  // No address to hand out AND a machine asking: withhold rather than deliver a mandated server
  // direct. A read with no machine behind it (a page's own feed view) is not a delivery, so it
  // sees the stored shape instead of a hole.
  const addressable = args.gatewayBase !== null && caller.sessionId !== null;
  if (mandated && !addressable) {
    return caller.sessionId === null
      ? { kind: "direct", document }
      : { kind: "withheld", reason: WITHHELD_GATEWAY_REQUIRED };
  }
  const routed =
    addressable &&
    wsGatewayOn &&
    (mandated ||
      (policy === null &&
        facts.gateway_optout !== true &&
        (facts.auth_mode === "none" || facts.gateway_credential === true)));
  if (!routed) {
    return { kind: "direct", document };
  }
  return {
    kind: "gateway",
    document: gatewayDeliveryDocument(document, {
      // Non-null by construction: `addressable` is what proves both.
      base: args.gatewayBase as string,
      sessionId: caller.sessionId as string,
      serverId: facts.server_id as string,
    }),
  };
}
