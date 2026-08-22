import { Form, useActionData } from "react-router";
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

export interface McpGatewayUsageRow {
  id: number;
  person: string;
  machine: string;
  toolName: string | null;
  method: string;
  outcome: string;
  createdAtMs: number;
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
  /** The most recent calls, newest first — a bounded window, not the whole ledger. */
  usage: McpGatewayUsageRow[];
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

/** USAGE. Who called, from which machine, which tool, how it ended, when. Never an argument and
 *  never a result — the ledger records that a call happened, not what was in it. */
function UsageSection({ view }: { view: McpGatewayView }) {
  return (
    <section aria-labelledby="mcp-usage-heading" className="space-y-3">
      <SectionHeading>
        <span id="mcp-usage-heading">Usage</span>
      </SectionHeading>
      <Card className="px-4 py-3">
        {view.usage.length === 0 ? (
          <p className="text-faint text-sm" data-testid="mcp-usage-empty">
            No calls yet.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm" data-testid="mcp-usage-table">
              <thead>
                <tr className="text-dim text-xs">
                  <th className="py-1 pr-3 text-left font-medium">Person</th>
                  <th className="py-1 pr-3 text-left font-medium">Machine</th>
                  <th className="py-1 pr-3 text-left font-medium">Tool</th>
                  <th className="py-1 pr-3 text-left font-medium">Outcome</th>
                  <th className="py-1 text-left font-medium">When</th>
                </tr>
              </thead>
              <tbody>
                {view.usage.map((row) => (
                  <tr key={row.id} className="border-line-soft border-t">
                    <td className="py-1.5 pr-3 text-ink">{row.person}</td>
                    <td className="py-1.5 pr-3 text-dim">{row.machine}</td>
                    <td className="py-1.5 pr-3 font-mono text-[13px] text-dim">
                      {row.toolName ?? "—"}
                    </td>
                    <td className="py-1.5 pr-3 font-mono text-[13px] text-dim">{row.outcome}</td>
                    <td className="py-1.5 text-faint text-xs">
                      {relativeTime(new Date(row.createdAtMs))}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
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
