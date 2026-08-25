import { Form, Link, useActionData, useSearchParams } from "react-router";
import { ConfirmButton } from "@/components/confirm";
import { relativeTime } from "@/components/format";
import { BusyFields, buttonClasses, Card, Chip, SectionHeading } from "@/components/ui";
import { useSubmittingIntent } from "@/lib/pending";

/**
 * THE GATEWAY'S THREE SECTIONS on a connected server's face — the whole of what a person does
 * about a server once a gateway stands in front of it: connect a sign-in ONCE for everyone's
 * agents, say which of the server's tools those agents may use, and see every call that was made.
 *
 * They render only where a gateway is deployed. On an install with none, the loader hands over
 * `null` and this file contributes nothing — no empty panels promising a capability that is not
 * there, and no reads against a schema that does not exist.
 *
 * Presentational only: every gate (owner-only, connected-or-not) is decided server-side and
 * arrives as a fact. Nothing here holds or renders a secret — a stored sign-in is a state line and
 * a Disconnect button, never a value.
 */

export interface McpGatewayToolRow {
  name: string;
  description: string | null;
  /** The server still lists it. A tool that stopped being offered stays visible and marked. */
  currentlyOffered: boolean;
  selected: boolean;
}

/**
 * ONE MACHINE'S WHOLE HISTORY with this server — one row per session, never one per call. A
 * working agent makes hundreds of calls, and a hundred identical lines is not a record anybody can
 * read; a session is the unit a person can act on (find that machine, end that session).
 */
export interface McpGatewayUsageSession {
  sessionId: string;
  person: string;
  machine: string;
  calls: number;
  /** How the calls ended. The two add up to `calls`. */
  ok: number;
  failed: number;
  /** WHY the failed ones failed — the ledger's own outcome names with their counts, biggest
   *  first, summing to `failed`. Empty when nothing failed. */
  failures: { kind: string; count: number }[];
  /** The distinct tools this session called, alphabetical — empty where the ledger holds only
   *  non-tool methods (initialize, a listing), which the row says once instead of a dash column. */
  tools: string[];
  firstCallMs: number;
  lastCallMs: number;
}

export interface McpGatewayUsage {
  sessions: McpGatewayUsageSession[];
  /** 1-based, already clamped into range by the read. */
  page: number;
  /** At least 1, so a reader is never on "page 1 of 0". */
  pageCount: number;
  /** Every session that has called this server, not just the ones on this page. */
  total: number;
}

export interface McpGatewayView {
  /** The server's display name — what the sign-in lines and the Connect page name. */
  displayName: string;
  /** The established sign-in tier; `none` shows no Sign-in section at all. */
  authMode: "oauth" | "none" | "manual" | null;
  /** Which sign-in answers for THIS viewer: their own, the workspace's, or neither. */
  signedIn: "mine" | "workspace" | null;
  /** This viewer has no sign-in of their own for this server. */
  canConnectPersonal: boolean;
  /** An owner, and the workspace account is not connected yet. */
  canConnectWorkspace: boolean;
  mode: "all" | "selected";
  tools: McpGatewayToolRow[];
  /** The whole ledger, aggregated per session and paged — newest activity first. */
  usage: McpGatewayUsage;
}

/**
 * WHY AN ARM DID NOT LAND. Connecting leaves for an upstream and disconnecting is one lane call, so
 * a form that came back saying nothing would read as an act that quietly did nothing. The refusal
 * copy is the shape the whole app uses: one honest sentence, scoped to the arm that was clicked.
 */
function useArmRefusal(intent: string): string | null {
  const answer = useActionData<{ intent?: string; status?: string; message?: string }>();
  if (answer?.intent !== intent || answer.status !== "error") {
    return null;
  }
  return answer.message ?? "That didn't go through. Try again.";
}

/** The member's route control, as the loader resolves it: a toggle where the choice is theirs
 *  (no mandate), the one mandate line where it is not. */
export type McpGatewayRoute = { kind: "toggle"; usingGateway: boolean } | { kind: "required" };

/**
 * THE VIEWER'S OWN ROUTE for this server — gateway (the default) or direct, for their machines
 * alone. Rendered only where the choice or the mandate can matter (an addressable server, gateway
 * delivery on, the workspace switch on — the loader's ruling). Changing it changes which document
 * their machines are delivered on the next update, and nothing anyone else receives.
 */
export function McpGatewayRouteCard({ route }: { route: McpGatewayRoute }) {
  const pending = useSubmittingIntent() === "gateway-route";
  const refusal = useArmRefusal("gateway-route");
  return (
    <section aria-labelledby="gateway-route-heading" className="space-y-3">
      <SectionHeading>
        <span id="gateway-route-heading">Use gateway</span>
      </SectionHeading>
      <Card className="space-y-2 px-4 py-3">
        {route.kind === "required" ? (
          <p className="text-ink text-sm">Required by workspace.</p>
        ) : (
          <Form method="post" className="flex flex-wrap items-center gap-3">
            <BusyFields busy={pending}>
              <input type="hidden" name="intent" value="gateway-route" />
              <input
                type="hidden"
                name="use_gateway"
                value={route.usingGateway ? "false" : "true"}
              />
              <p className="text-ink text-sm">
                <span className="font-medium">{route.usingGateway ? "On" : "Off"}</span>
              </p>
              <button type="submit" className={buttonClasses("quiet")} disabled={pending}>
                {route.usingGateway ? "Turn off" : "Turn on"}
              </button>
            </BusyFields>
          </Form>
        )}
        {refusal !== null && (
          <p role="alert" data-testid="mcp-route-refusal" className="text-red-700 text-sm">
            {refusal}
          </p>
        )}
      </Card>
    </section>
  );
}

/** The state line: which sign-in a call from this person's agents would carry. */
function signInLine(view: McpGatewayView): string {
  if (view.signedIn === "mine") {
    return `Using your ${view.displayName} account.`;
  }
  if (view.signedIn === "workspace") {
    return "Using the workspace account.";
  }
  return "No sign-in connected. Agents can't reach this server yet.";
}

/**
 * SIGN-IN. One connect per person (or one for the whole workspace, which is an owner's call), and
 * a disconnect that acts on the sign-in the state line just named — so what the button removes is
 * never in doubt.
 */
function SignInSection({ view }: { view: McpGatewayView }) {
  const flying = useSubmittingIntent();
  const busy = flying === "gateway-connect" || flying === "gateway-disconnect";
  // Both hooks run every render (a `??` would short-circuit the second one out of the call order).
  const connectRefusal = useArmRefusal("gateway-connect");
  const disconnectRefusal = useArmRefusal("gateway-disconnect");
  const refusal = connectRefusal ?? disconnectRefusal;
  return (
    <section aria-labelledby="mcp-signin-heading" className="space-y-3">
      <SectionHeading>
        <span id="mcp-signin-heading">Sign-in</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-dim text-sm" data-testid="mcp-signin-state">
          {signInLine(view)}
        </p>
        {(view.canConnectPersonal || view.canConnectWorkspace) && (
          <div className="flex flex-wrap items-center gap-2">
            {view.canConnectPersonal && (
              <Form method="post">
                <input type="hidden" name="intent" value="gateway-connect" />
                <input type="hidden" name="scope" value="mine" />
                <BusyFields busy={busy}>
                  <button type="submit" className={`${buttonClasses("quiet")} min-h-11`}>
                    Connect your account
                  </button>
                </BusyFields>
              </Form>
            )}
            {view.canConnectWorkspace && (
              <Form method="post">
                <input type="hidden" name="intent" value="gateway-connect" />
                <input type="hidden" name="scope" value="workspace" />
                <BusyFields busy={busy}>
                  <button type="submit" className={`${buttonClasses("quiet")} min-h-11`}>
                    Connect a workspace account
                  </button>
                </BusyFields>
              </Form>
            )}
          </div>
        )}
        {view.signedIn !== null && (
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-line-soft border-t pt-3">
            <span className="text-faint text-xs">
              Agents using this sign-in lose access on their next call.
            </span>
            <Form method="post" className="ml-auto">
              <input type="hidden" name="intent" value="gateway-disconnect" />
              <input type="hidden" name="scope" value={view.signedIn} />
              <ConfirmButton label="Disconnect" tone="danger" pending={busy} />
            </Form>
          </div>
        )}
        {refusal !== null && (
          <p role="alert" data-testid="mcp-signin-refusal" className="text-red-700 text-sm">
            {refusal}
          </p>
        )}
      </Card>
    </section>
  );
}

/**
 * WHICH TOOLS A FRESHLY RENDERED CHECKLIST STARTS CHECKED.
 *
 * THE CHECKLIST STARTS FULL. Narrowing is switching tools OFF, never hunting for what to switch on:
 * a workspace that has never narrowed this server sees every tool checked, so the radio and the
 * checklist agree before anything is touched, and the act of narrowing is unchecking.
 *
 * A workspace with a STANDING SELECTION sees that selection instead — its answer is never
 * overwritten by opening the page. A `selected` policy carrying no observed tool is the state this
 * rule exists to heal: it has nothing to preserve, so it starts full like any other, and the way
 * out of it is one click on All tools or one Save.
 */
export function startingToolChecks(tools: readonly McpGatewayToolRow[]): ReadonlySet<string> {
  const standing = tools.filter((tool) => tool.selected);
  return new Set((standing.length > 0 ? standing : tools).map((tool) => tool.name));
}

/**
 * TOOLS. The radio is the whole policy: every tool, or only the checked ones. The second line says
 * outright what happens to a tool the server adds later, because that is the question a reader has
 * the moment they narrow anything. Saving an empty checklist under `selected` is refused
 * server-side, with the sentence that says what it would have done.
 */
function ToolsSection({ view }: { view: McpGatewayView }) {
  const flying = useSubmittingIntent();
  const busy = flying === "gateway-tools";
  const refreshing = flying === "gateway-tools-refresh";
  // Both hooks run every render (a `??` would short-circuit the second one out of the call order).
  const saveRefusal = useArmRefusal("gateway-tools");
  const refreshRefusal = useArmRefusal("gateway-tools-refresh");
  const refusal = saveRefusal ?? refreshRefusal;
  const startsChecked = startingToolChecks(view.tools);
  return (
    <section aria-labelledby="mcp-tools-heading" className="space-y-3">
      <div className="flex items-center justify-between gap-3">
        <SectionHeading>
          <span id="mcp-tools-heading">Tools</span>
        </SectionHeading>
        {/* Its own form, outside the policy form — a form cannot nest inside another. */}
        <Form method="post">
          <input type="hidden" name="intent" value="gateway-tools-refresh" />
          <BusyFields busy={refreshing}>
            <button type="submit" className={buttonClasses("quiet")}>
              Refresh tools
            </button>
          </BusyFields>
        </Form>
      </div>
      <Card className="px-4 py-3">
        <Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="gateway-tools" />
          <BusyFields busy={busy} className="space-y-3">
            <fieldset className="space-y-2">
              <legend className="sr-only">Which tools agents may use</legend>
              <label className="flex items-start gap-2">
                <input
                  type="radio"
                  name="mode"
                  value="all"
                  defaultChecked={view.mode === "all"}
                  className="mt-1"
                />
                <span className="text-ink text-sm">
                  All tools — agents can use every tool this server offers.
                </span>
              </label>
              <label className="flex items-start gap-2">
                <input
                  type="radio"
                  name="mode"
                  value="selected"
                  defaultChecked={view.mode === "selected"}
                  className="mt-1"
                />
                <span className="text-ink text-sm">
                  Selected tools — agents can use only the tools checked below. New tools the server
                  adds start unchecked.
                </span>
              </label>
            </fieldset>
            {view.tools.length === 0 ? (
              <p className="text-faint text-sm" data-testid="mcp-tools-empty">
                No tools observed yet. The list fills in when a sign-in is connected, or when you
                refresh it.
              </p>
            ) : (
              <ul className="space-y-2" data-testid="mcp-tools-list">
                {view.tools.map((tool) => (
                  <li key={tool.name} className="flex items-start gap-2">
                    <input
                      type="checkbox"
                      name="tool"
                      value={tool.name}
                      defaultChecked={startsChecked.has(tool.name)}
                      id={`mcp-tool-${tool.name}`}
                      className="mt-1"
                    />
                    <label htmlFor={`mcp-tool-${tool.name}`} className="min-w-0">
                      <span className="font-mono text-[13px] text-ink">{tool.name}</span>
                      {!tool.currentlyOffered && <Chip tone="neutral">not offered</Chip>}
                      {tool.description !== null && (
                        <span className="block text-dim text-sm">{tool.description}</span>
                      )}
                    </label>
                  </li>
                ))}
              </ul>
            )}
            {refusal !== null && (
              <p role="alert" data-testid="mcp-tools-refusal" className="text-red-700 text-sm">
                {refusal}
              </p>
            )}
            <button type="submit" className={`${buttonClasses("quiet")} min-h-11`}>
              Save
            </button>
          </BusyFields>
        </Form>
      </Card>
    </section>
  );
}

/**
 * THE LEDGER'S OUTCOME NAMES, said the way a person would say them. The gateway owns the set (its
 * own CHECK constraint), so an unknown one is shown as itself with its underscores opened out
 * rather than dropped — a failure kind this tier has not learned about yet is still a fact.
 */
const FAILURE_WORDS: Record<string, string> = {
  denied_tool: "tool not allowed",
  no_credential: "no sign-in",
  unauthorized: "sign-in refused",
  upstream_error: "server error",
};

function failureWord(kind: string): string {
  return FAILURE_WORDS[kind] ?? kind.replaceAll("_", " ");
}

/**
 * ONE ROW'S OUTCOME CELL: `12 ok · 3 failed (2 no sign-in, 1 server error)`.
 *
 * The parenthetical is the whole reason this column is worth reading. "3 failed" is a number
 * nobody can act on — a tool the workspace switched off, a sign-in nobody connected and a server
 * that is down are three different problems with three different fixes, and the ledger already
 * knows which is which.
 */
function outcomeLine(row: McpGatewayUsageSession): string {
  const counts = `${row.ok} ok · ${row.failed} failed`;
  if (row.failures.length === 0) {
    return counts;
  }
  const why = row.failures.map((f) => `${f.count} ${failureWord(f.kind)}`).join(", ");
  return `${counts} (${why})`;
}

/**
 * THE ONE LINE ABOVE THE TABLE — how much there is, and where in it you are standing. The count is
 * the WHOLE ledger, not this page: a reader who sees five rows must not read that as five machines.
 */
function usageSummary(usage: McpGatewayUsage): string {
  const sessions = usage.total === 1 ? "1 session" : `${usage.total} sessions`;
  if (usage.pageCount > 1) {
    return `${sessions}, newest activity first — page ${usage.page} of ${usage.pageCount}.`;
  }
  return `${sessions}, newest activity first.`;
}

/**
 * Prev/next, and nothing else — the position is on the summary line, said once. Rendered only
 * where there is a second page to go to.
 *
 * A page link REWRITES `page` and carries everything else on the address through. A bare
 * `?page=2` would have replaced the whole query, so paging the Usage table would silently drop
 * whatever else the URL was saying — the post-publish placement note this page reads, and any
 * parameter added later. Turning a page is a move within the page, not a fresh arrival at it.
 */
function UsagePager({ usage }: { usage: McpGatewayUsage }) {
  const [params] = useSearchParams();
  if (usage.pageCount <= 1) {
    return null;
  }
  const pageHref = (page: number): string => {
    const next = new URLSearchParams(params);
    next.set("page", String(page));
    return `?${next.toString()}`;
  };
  return (
    <nav
      aria-label="Usage pages"
      data-testid="mcp-usage-pager"
      className="flex flex-wrap items-center gap-2 border-line-soft border-t pt-3"
    >
      {usage.page > 1 && (
        <Link to={pageHref(usage.page - 1)} className={buttonClasses("quiet")}>
          Previous
        </Link>
      )}
      {usage.page < usage.pageCount && (
        <Link to={pageHref(usage.page + 1)} className={buttonClasses("quiet")}>
          Next
        </Link>
      )}
    </nav>
  );
}

/**
 * USAGE. Who called, from which machine, how many times, how it ended, which tools, over what
 * stretch. Never an argument and never a result — the ledger records that a call happened, not
 * what was in it.
 *
 * ONE ROW PER SESSION. Per-call rows were the honest shape of the table and the wrong one to read:
 * a single agent session filled the page with visually identical lines, all naming the same person
 * and the same machine, and a `—` under Tool on every one of them because most calls are not tool
 * calls. Aggregated, the row answers what a person came here to ask — which machines are using
 * this, how hard, and is anything failing — and the page control makes the older ones reachable
 * rather than silently dropped.
 */
function UsageSection({ view }: { view: McpGatewayView }) {
  const usage = view.usage;
  return (
    <section aria-labelledby="mcp-usage-heading" className="space-y-3">
      <SectionHeading>
        <span id="mcp-usage-heading">Usage</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        {usage.sessions.length === 0 ? (
          <p className="text-faint text-sm" data-testid="mcp-usage-empty">
            No calls yet.
          </p>
        ) : (
          <>
            <p className="text-faint text-xs" data-testid="mcp-usage-summary">
              {usageSummary(usage)}
            </p>
            <div className="overflow-x-auto">
              <table className="w-full text-sm" data-testid="mcp-usage-table">
                <thead>
                  <tr className="text-dim text-xs">
                    <th className="py-1 pr-3 text-left font-medium">Person</th>
                    <th className="py-1 pr-3 text-left font-medium">Machine</th>
                    <th className="py-1 pr-3 text-left font-medium">Calls</th>
                    <th className="py-1 pr-3 text-left font-medium">Outcome</th>
                    <th className="py-1 pr-3 text-left font-medium">Tools</th>
                    <th className="py-1 pr-3 text-left font-medium">First call</th>
                    <th className="py-1 text-left font-medium">Last call</th>
                  </tr>
                </thead>
                <tbody>
                  {usage.sessions.map((row) => (
                    <tr
                      key={row.sessionId}
                      data-testid={`mcp-usage-${row.sessionId}`}
                      className="border-line-soft border-t"
                    >
                      <td className="py-1.5 pr-3 text-ink">{row.person}</td>
                      <td className="py-1.5 pr-3 text-dim">{row.machine}</td>
                      <td className="py-1.5 pr-3 text-dim tabular-nums">{row.calls}</td>
                      <td className="py-1.5 pr-3 text-dim">{outcomeLine(row)}</td>
                      <td className="py-1.5 pr-3 font-mono text-[13px] text-dim">
                        {row.tools.length === 0 ? "—" : row.tools.join(", ")}
                      </td>
                      <td className="py-1.5 pr-3 text-faint text-xs">
                        {relativeTime(new Date(row.firstCallMs))}
                      </td>
                      <td className="py-1.5 text-faint text-xs">
                        {relativeTime(new Date(row.lastCallMs))}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <UsagePager usage={usage} />
          </>
        )}
      </Card>
    </section>
  );
}

/** The three sections, in the order a person meets them: get in, decide what may be used, see what
 *  was. A server that asks for no sign-in skips the first — there is nothing to connect. */
export function McpGatewayPanel({ view }: { view: McpGatewayView }) {
  return (
    <>
      {view.authMode !== "none" && <SignInSection view={view} />}
      <ToolsSection view={view} />
      <UsageSection view={view} />
    </>
  );
}

/**
 * The MANUAL tier's own page — a server whose sign-in is a secret somebody creates by hand, so
 * there is no walk to send a browser on. The helper line is the catalog row's own note, verbatim:
 * it is the one line saying what a person must do first, which is the whole reason such a row may
 * stand in a catalog people receive from.
 */
export function McpConnectForm({
  authNote,
  scope,
  busy,
  error,
}: {
  authNote: string | null;
  /** Whose sign-in this stores. Rides the FORM, so the action re-guards it rather than the query. */
  scope: "mine" | "workspace";
  busy: boolean;
  error: string | null;
}) {
  return (
    <Card className="space-y-3 px-4 py-3">
      <Form method="post">
        <input type="hidden" name="intent" value="gateway-manual" />
        <input type="hidden" name="scope" value={scope} />
        <BusyFields busy={busy} className="space-y-3">
          <label className="block">
            <span className="mb-1 block font-medium text-sm text-dim">Secret</span>
            <input
              type="password"
              name="secret"
              required
              autoComplete="off"
              spellCheck={false}
              className="block h-11 w-full min-w-56 rounded-md border border-line px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
            />
          </label>
          {authNote !== null && <p className="text-faint text-xs leading-relaxed">{authNote}</p>}
          <button type="submit" className={`${buttonClasses("quiet")} min-h-11`}>
            Save
          </button>
        </BusyFields>
      </Form>
      {error !== null && (
        <p className="text-red-600 text-sm" role="alert">
          {error}
        </p>
      )}
    </Card>
  );
}
