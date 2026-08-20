import { useId, useState } from "react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, Link, redirect, useActionData, useLoaderData } from "react-router";
import { McpMark } from "@/components/mcp-mark";
import { BusyFields, buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { webNewDestination } from "@/lib/api/genesis.server";
import { requireMemberInScope, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { bundlePath } from "@/lib/bundle-base";
import { channelsOf } from "@/lib/db/queries.channels.server";
import {
  connectableMcpServers,
  connectMcpServer,
  createPrivateMcpServer,
  mcpRevisionFacts,
} from "@/lib/db/queries.mcp-catalog.server";
import {
  canonicalServerJson,
  fetchesUpstream,
  loadServerDocument,
  McpFetchError,
  type McpSourceKind,
  unwrapServerDocument,
} from "@/lib/mcp/fetch.server";
import { scheduleRevisionProbe } from "@/lib/mcp/probe.server";
import type { McpGateRefusal } from "@/lib/mcp/publish-gate.server";
import { type McpSummary, suggestedNameFor, validateServerJson } from "@/lib/mcp/validate.server";
import { useSubmittingIntent } from "@/lib/pending";
import { allowUpstreamFetch } from "@/lib/rate-limit.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta() {
  return [{ title: "Add an MCP server · Topos" }];
}

/**
 * ADD AN MCP SERVER — the web way in for a `kind: 'mcp'` bundle. TWO acts, and they are genuinely
 * different:
 *
 *  · CONNECT one from the catalog — the page's resting state. The server already exists as a row
 *    somebody verified; connecting is this workspace saying "we use that one", and from then on it
 *    receives what the catalog publishes. Any member may.
 *  · WRITE ONE DOWN that the catalog does not carry — a server inside your own network, or one
 *    nobody has curated yet. It becomes the workspace's OWN row, exported nowhere, and an owner is
 *    the one who may create it, because it is the workspace speaking for a server nobody checked.
 *
 * A custom server still needs a document nobody here has seen, so that arm keeps its preview round
 * trip — a registry name, a URL, or the document pasted in. The picker takes none: what a catalog
 * row is, is already on the page, so choosing one opens its confirm dialog on the click itself.
 *
 * Whichever act it was, the document passes the same gate (app/lib/mcp/validate.server.ts) before
 * anything is written.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const [channels, servers] = await Promise.all([channelsOf(actor), connectableMcpServers(actor)]);
  return {
    wsName: workspace.name,
    channels: channels.map((c) => ({ name: c.name, isDefault: c.isDefault, mode: c.mode })),
    // The viewer's own role: a CURATED channel takes a member's placement away and the picker must
    // say so BEFORE the act rather than after it (see `channelOptionLabel`), and writing a server
    // down is an owner's.
    role: actor.role,
    servers: servers.map((server) => ({
      serverId: server.serverId,
      name: server.name,
      displayName: server.displayName,
      description: server.description,
      icon: server.icon,
      authMode: server.authMode as "oauth" | "none" | "manual" | null,
      authNote: server.authNote,
      url: server.url,
      transport: server.transport,
      host: hostOf(server.url),
      suggestedName: suggestedNameFor(server.name ?? server.displayName),
      connectedAs: server.connectedAs,
      inArchive: server.inArchive,
    })),
  };
}

/** The address a row shows under its name — the host alone, which is what a reader scans. */
function hostOf(url: string | null): string {
  if (url === null) {
    return "";
  }
  try {
    return new URL(url).host;
  } catch {
    return "";
  }
}

/** One catalog row as this page renders it. */
type ServerRow = Awaited<ReturnType<typeof loader>>["servers"][number];

const SOURCE_KINDS: McpSourceKind[] = ["registry", "url", "paste"];
/** A pasted document is bounded the same way a fetched one is (the gate's own ceiling). */
const MAX_PASTE_CHARS = 256 * 1024;

interface PreviewData {
  form: "preview";
  /** Where the bytes came from, echoed so the next step's copy can say it. */
  origin: string;
  summary: McpSummary;
  suggestedName: string;
  /** The canonical document the server would be created with — the next step's payload. */
  document: string;
}

interface Refusal {
  /**
   * WHERE THE ANSWER BELONGS. `preview` and `create` are the custom arm's and render on the page;
   * `pick` is the dialog's, and carries the row it answers about so a stale refusal can never
   * attach itself to a different server.
   */
  form: "preview" | "create" | "pick";
  error: string;
  /** The typed refusal code, when a gate produced one — shown as a quiet chip. */
  code?: string;
  /** The in-workspace path the refusal points at, when there is one. */
  at?: string;
  /** The picked row a `pick` refusal answers about. */
  server?: string;
  /**
   * THE STAGED DOCUMENT, HANDED BACK. A refused create must not cost the person the document they
   * staged: the preview card renders from this echo, so the retry is one click and not a re-paste.
   */
  preview?: PreviewData;
}

function refusal(form: Refusal["form"], error: string, code?: string, status = 400) {
  return data<Refusal>({ form, error, ...(code === undefined ? {} : { code }) }, { status });
}

/**
 * The document a server would be created with, from whatever arrived. A registry answer wraps it
 * in `{ server, _meta }`; a URL or a paste is the document itself. Canonicalizing BOTH (and again
 * on the way back through the form) is what makes the preview and the stored document the same
 * bytes. Text that is not a JSON object passes through untouched, so the gate — not this — gets to
 * word the refusal.
 */
function canonicalize(text: string): string {
  const unwrapped = unwrapServerDocument(text);
  return unwrapped === null ? text : canonicalServerJson(unwrapped);
}

/** The form field each custom arm reads its one value out of. */
const SOURCE_FIELD: Record<McpSourceKind, string> = {
  registry: "registry_name",
  url: "url",
  paste: "document",
};

/** Read the one source field the chosen arm uses. */
function sourceFrom(formData: FormData): { kind: McpSourceKind; value: string } | null {
  const kind = String(formData.get("source") ?? "");
  if (!SOURCE_KINDS.includes(kind as McpSourceKind)) {
    return null;
  }
  const value = String(formData.get(SOURCE_FIELD[kind as McpSourceKind]) ?? "").trim();
  if (value.length === 0 || value.length > MAX_PASTE_CHARS) {
    return null;
  }
  return { kind: kind as McpSourceKind, value };
}

export async function action({ request, params }: ActionFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");

  if (intent === "preview") {
    const source = sourceFrom(formData);
    if (source === null) {
      return refusal("preview", "Pick a source and fill in the matching field.");
    }
    // The two fetching arms reach the network from this process — belted per acting user, the
    // same belt the GitHub import wears (route actions bypass the /api/v1 belt entirely). A
    // paste never leaves the process, so it does not spend the belt.
    if (fetchesUpstream(source.kind) && !allowUpstreamFetch(actor.userId)) {
      throw data(null, { status: 429 });
    }
    let text: string;
    let origin: string;
    try {
      const fetched = await loadServerDocument(source);
      text = fetched.text;
      origin = source.kind === "paste" ? "pasted" : fetched.url;
    } catch (error) {
      return refusal(
        "preview",
        error instanceof McpFetchError ? error.message : "That fetch did not complete.",
      );
    }
    const document = canonicalize(text);
    // FIRST the document gate: it reads the raw bytes and answers MCP_INVALID for anything that is
    // not a well-formed server.json (malformed JSON included — a paste is often that), so nothing
    // below ever parses bytes the gate has not vouched for. A registry read must carry a version;
    // a URL or a paste is the author's own and may omit it.
    const validated = validateServerJson(document, {
      requireVersion: source.kind === "registry",
    });
    if (!validated.ok) {
      return refusal("preview", validated.message, validated.code);
    }
    // THEN the catalog's fact gate — the one `create` lands through — so the preview never offers an
    // add that cannot land: a document declaring an official schema that itself requires a version,
    // or a schema this build cannot read, is refused here in the same words the write would use.
    // The bytes parsed cleanly above, so this parse cannot throw.
    const landing = mcpRevisionFacts(JSON.parse(document) as Record<string, unknown>, {
      requireVersion: source.kind === "registry",
    });
    if (landing.refusal !== null) {
      return refusal("preview", landing.refusal.message, landing.refusal.code);
    }
    return data<PreviewData>({
      form: "preview",
      origin,
      summary: validated.summary,
      suggestedName: suggestedNameFor(validated.summary.name),
      document,
    });
  }

  // CONNECT — the picker's act. The server is a row that already exists; this workspace gets the
  // bundle that names it, and every machine the bundle reaches receives the document the catalog
  // publishes for it.
  if (intent === "connect") {
    const serverId = String(formData.get("server_id") ?? "").trim();
    const picked = String(formData.get("server") ?? "").trim();
    const name = String(formData.get("name") ?? "").trim();
    const channel = String(formData.get("channel") ?? "").trim();
    const connected = await connectMcpServer(actor, {
      serverId,
      displayName: name.length > 0 ? name : null,
      to: webNewDestination("mcp", channel),
    });
    if (connected.refusal !== null) {
      return data<Refusal>(
        {
          form: "pick",
          error: connected.refusal.message,
          server: picked,
          code: connected.refusal.code,
          ...(connected.refusal.at === undefined ? {} : { at: connected.refusal.at }),
        },
        { status: 400 },
      );
    }
    throw redirect(landingFor(workspace.name, connected.registration, channel));
  }

  // CREATE — the custom arm's act, and an OWNER's: a private server is this workspace speaking for
  // an endpoint nobody outside it has checked, so the roster's own gate decides who may (its
  // refusal is the uniform 404 every owner-only act answers with).
  if (intent === "create") {
    const owner = await requireWorkspaceOwner(request, workspace.id);
    const name = String(formData.get("name") ?? "").trim();
    const channel = String(formData.get("channel") ?? "").trim();
    const posted = String(formData.get("document") ?? "");
    if (posted.length === 0 || posted.length > MAX_PASTE_CHARS) {
      return refusal("create", "Nothing to add — run the preview again.");
    }
    // Canonicalized AGAIN rather than stored as posted: a form field round-trips through multipart
    // encoding, which normalizes line endings, so trusting the bytes back would make what is
    // stored depend on the browser rather than on the document.
    const document = canonicalize(posted);
    // The custom arm writes this workspace's OWN server: a missing version is a truthful state, not
    // a refusal, and the create write below stores it as such.
    const validated = validateServerJson(document, { requireVersion: false });
    if (!validated.ok) {
      return refusal("create", validated.message, validated.code);
    }
    const staged: PreviewData = {
      form: "preview",
      // The provenance line the card already showed, carried back with the document; a client may
      // say anything here, so it is bounded and only ever rendered to its own author.
      origin: String(formData.get("origin") ?? "").slice(0, 300),
      summary: validated.summary,
      suggestedName: name.length > 0 ? name : suggestedNameFor(validated.summary.name),
      document,
    };
    const refuseCreate = (gate: McpGateRefusal) =>
      data<Refusal>(
        {
          form: "create",
          error: gate.message,
          code: gate.code,
          ...(gate.at === undefined ? {} : { at: gate.at }),
          preview: staged,
        },
        { status: 400 },
      );
    const created = await createPrivateMcpServer(
      owner,
      {
        displayName: name.length > 0 ? name : validated.summary.name,
        description: validated.summary.description,
        // NOTHING IS CLAIMED ABOUT THE SIGN-IN. A tier is something somebody establishes by
        // checking; a document's own word for it is a claim, and this row makes none.
        authMode: null,
      },
      JSON.parse(document) as Record<string, unknown>,
    );
    if (created.refusal !== null) {
      return refuseCreate(created.refusal);
    }
    const connected = await connectMcpServer(owner, {
      serverId: created.serverId,
      displayName: name.length > 0 ? name : null,
      to: webNewDestination("mcp", channel),
    });
    if (connected.refusal !== null) {
      // The server row stands — it is this workspace's own and now on the list above, so the
      // connection is one click away rather than a lost document.
      return refuseCreate(connected.refusal);
    }
    // THE ADVISORY PROBE, and only now: the revision is durable and this call cannot change it.
    // Not waited on — a report about somebody else's uptime must not lengthen the act.
    scheduleRevisionProbe({ revisionId: created.revisionId, endpoint: validated.summary.url });
    throw redirect(landingFor(workspace.name, connected.registration, channel));
  }

  return refusal("create", "Unknown action.");
}

/**
 * WHERE THE ACT LANDS, and WHAT IT DID TO THE REACH. The bundle exists; the PLACEMENT is a
 * separate outcome and may have been withheld (a curated channel takes a member's placement) or
 * found nothing to place into. The dialog promised that a chosen channel's agents get this
 * server, so a withheld placement is said out loud on the page the redirect lands on rather than
 * read as a silent success. Choosing NO channel promises nothing and says nothing.
 */
function landingFor(
  wsName: string,
  registration: { name: string; placement?: string },
  channel: string,
): string {
  const path = wsPathServer(wsName, bundlePath("mcp", registration.name));
  if (registration.placement === undefined || registration.placement === "placed") {
    return path;
  }
  return `${path}?${new URLSearchParams({ placement: registration.placement, channel })}`;
}

export default function McpNew() {
  const { servers, role } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const wsPath = useWsPath();
  const flying = useSubmittingIntent();
  const busy = flying !== null;
  // The row whose dialog is open — plain local state, set by a click and cleared by Cancel or
  // Escape. It is the only thing choosing a server changes until the button is pressed.
  const [picked, setPicked] = useState<ServerRow | null>(null);
  const error = actionData !== undefined && "error" in actionData ? actionData : undefined;
  // The card renders from a fresh preview OR from the one a refused create handed back — the
  // staged document survives the refusal, so a retry is a click rather than a re-paste.
  const preview =
    actionData !== undefined && actionData.form === "preview" && !("error" in actionData)
      ? actionData
      : error?.preview;
  // A `pick` refusal is the dialog's to show; everything else belongs to the page.
  const pageError = error !== undefined && error.form !== "pick" ? error : undefined;
  return (
    <div className="space-y-6">
      <PageHeader
        title="Add an MCP server"
        actions={
          <Link to={wsPath("")} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <p className="max-w-3xl text-dim text-sm leading-relaxed">
        A server here is one address every agent on the team calls, delivered into each machine's
        own MCP config. Connecting one shares the address, never a credential: signing in happens on
        the machine that uses it.
      </p>
      <ServerPicker servers={servers} onPick={setPicked} />
      {picked !== null && (
        <AddServerDialog
          server={picked}
          error={error !== undefined && error.form === "pick" ? error : undefined}
          onClose={() => setPicked(null)}
        />
      )}
      {role === "owner" && <CustomSource busy={busy} flying={flying} />}
      {pageError !== undefined && (
        <RefusalNote error={pageError.error} code={pageError.code} at={pageError.at} />
      )}
      {preview !== undefined && <PreviewCard preview={preview} />}
    </div>
  );
}

/** Does this row match what someone typed? Name, blurb, host and registry name all count. */
function matches(server: ServerRow, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) {
    return true;
  }
  return `${server.displayName} ${server.description ?? ""} ${server.host} ${server.name ?? ""}`
    .toLowerCase()
    .includes(needle);
}

/**
 * The auth chip — the one thing about a server worth knowing before choosing it. A row where
 * nobody established a tier gets no chip: silence is the honest answer there, not "no sign-in".
 *
 * THREE WORDS, TWO WORLDS. `oauth` and `no sign-in` are both "nothing to prepare"; `manual setup`
 * is the one that costs somebody an errand, so it takes the amber tone the rest of the app spends
 * on a state waiting on a person rather than the accent that reads as a feature.
 */
export function AuthChip({ auth }: { auth: "oauth" | "none" | "manual" | null }) {
  if (auth === null) {
    return null;
  }
  if (auth === "manual") {
    return <Chip tone="pending">manual setup</Chip>;
  }
  return auth === "oauth" ? (
    <Chip tone="accent">oauth</Chip>
  ) : (
    <Chip tone="neutral">no sign-in</Chip>
  );
}

/**
 * THE PICKER — the catalog in a dense grid, narrowed by one text box. Each card is a plain button
 * that opens the dialog: no form, no submit, no navigation, so the grid neither reloads nor goes
 * inert while the answer appears. The density is the point — the list being chosen from should be
 * visible at once rather than scrolled through two at a time.
 *
 * A server this workspace ALREADY runs is not offered again: one connection per server is the
 * database's rule, so the row links to the bundle instead of asking for a second one.
 */
function ServerPicker({
  servers,
  onPick,
}: {
  servers: ServerRow[];
  onPick: (server: ServerRow) => void;
}) {
  const wsPath = useWsPath();
  const [query, setQuery] = useState("");
  const visible = servers.filter((server) => matches(server, query));
  return (
    <section aria-labelledby="mcp-picker-heading" className="space-y-3" data-testid="mcp-picker">
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <SectionHeading>
          <span id="mcp-picker-heading">Servers you can add</span>
        </SectionHeading>
        <label className="block">
          <span className="sr-only">Search these servers</span>
          <input
            type="search"
            value={query}
            data-testid="mcp-picker-search"
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search — linear, docs, deploys…"
            className="block h-9 w-64 rounded-md border border-line bg-panel px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none"
          />
        </label>
      </div>
      <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
        {visible.map((server) =>
          server.inArchive ? (
            // The one connection this workspace may hold to this server is spoken for by a bundle
            // in the archive — offering Add here would be offering an act that refuses. Restoring
            // the archived one is what brings it back, so the row points at the archive.
            <Link
              key={server.serverId}
              to={wsPath("settings/archive")}
              data-testid="mcp-picker-archived"
              className="flex items-start gap-2 rounded-lg border border-line-soft bg-panel2 px-3 py-2.5 text-left transition-colors hover:border-line focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
            >
              <ServerCardBody server={server} chip="in your archive" />
            </Link>
          ) : server.connectedAs === null ? (
            <button
              key={server.serverId}
              type="button"
              onClick={() => onPick(server)}
              data-testid="mcp-picker-option"
              className="flex items-start gap-2 rounded-lg border border-line-soft bg-panel px-3 py-2.5 text-left transition-colors hover:border-line hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
            >
              <ServerCardBody server={server} />
            </button>
          ) : (
            <Link
              key={server.serverId}
              to={wsPath(bundlePath("mcp", server.connectedAs))}
              data-testid="mcp-picker-added"
              className="flex items-start gap-2 rounded-lg border border-line-soft bg-panel2 px-3 py-2.5 text-left transition-colors hover:border-line focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
            >
              <ServerCardBody server={server} chip="added" />
            </Link>
          ),
        )}
      </div>
      <p aria-live="polite" className="text-faint text-xs">
        {visible.length === servers.length
          ? `${servers.length} servers`
          : visible.length === 1
            ? "1 server matches"
            : `${visible.length} servers match`}
      </p>
    </section>
  );
}

/** The three lines every row carries, whichever affordance wraps them. A `chip` replaces the
 *  sign-in tier where the row is not on offer — what the reader needs there is why. */
function ServerCardBody({ server, chip }: { server: ServerRow; chip?: string }) {
  return (
    <>
      {/* Leading, never stacked: the mark rides beside the three lines rather than above them,
          so a row costs the same height it did before anyone had a logo. */}
      <McpMark logo={server.icon ?? undefined} className="mt-0.5" />
      <span className="flex min-w-0 flex-1 flex-col items-stretch gap-0.5">
        <span className="flex items-center gap-1.5">
          <span className="min-w-0 flex-1 truncate font-medium text-ink text-sm">
            {server.displayName}
          </span>
          <span className="shrink-0">
            {chip === undefined ? (
              <AuthChip auth={server.authMode} />
            ) : (
              <Chip tone="neutral">{chip}</Chip>
            )}
          </span>
        </span>
        <span className="w-full truncate text-dim text-xs leading-snug">
          {server.description ?? ""}
        </span>
        <span className="w-full truncate font-mono text-[11px] text-faint">{server.host}</span>
      </span>
    </>
  );
}

/**
 * THE PICK DIALOG — the question a click asks, answered without leaving the list: is this the
 * server you meant, and here is exactly what this workspace would be running. Everything it shows
 * came down with the page, so it opens on the click itself; the ONE server call a picked row makes
 * is the connect, and until that button is pressed nothing anywhere has changed.
 */
function AddServerDialog({
  server,
  error,
  onClose,
}: {
  server: ServerRow;
  error?: { error: string; code?: string; server?: string; at?: string };
  onClose: () => void;
}) {
  const flying = useSubmittingIntent();
  const busy = flying !== null;
  // Only this row's own refusal: an answer about the server chosen before it is not about this
  // one, and showing it here would read as a verdict on a server nothing was asked about.
  const mine = error !== undefined && error.server === server.serverId ? error : undefined;
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
    >
      <DialogContent className="max-w-xl" data-testid="mcp-pick-dialog">
        <DialogHeader>
          <DialogTitle>Add {server.displayName} to this workspace?</DialogTitle>
          <DialogDescription>
            This workspace follows the catalog's version of it, and corrections arrive as they are
            published. Share it into a channel and every agent that channel reaches gets the
            address; leave that empty and it waits here until someone shares it.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2 rounded-md border border-line-soft bg-panel2 px-3 py-2.5">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            {/* The same mark the row carried, so the answer to "is this the one you clicked?" is
                the first thing on the block rather than a name to re-read. */}
            <McpMark logo={server.icon ?? undefined} className="size-4" />
            <span className="font-mono text-[13px] text-ink">
              {server.name ?? server.displayName}
            </span>
            {server.transport !== null && <Chip tone="neutral">{server.transport}</Chip>}
            <AuthChip auth={server.authMode} />
          </div>
          {server.description !== null && <p className="text-dim text-sm">{server.description}</p>}
          {server.url !== null && (
            <p className="break-all font-mono text-[13px] text-dim" data-testid="mcp-dialog-url">
              {server.url}
            </p>
          )}
        </div>
        {server.authMode === "oauth" && (
          <p className="text-faint text-xs leading-relaxed">
            An agent signs in on first use — that sign-in happens on each person&apos;s own machine,
            never here, and no credential rides in what the team receives.
          </p>
        )}
        {/* THE ERRAND, SAID WHERE THE DECISION IS MADE. The card carries only the chip; the one
            sentence about a token someone has to mint or an app an admin registers lives here,
            because this dialog is the moment of adding — a caveat on a grid card competes with
            twenty-four neighbours, and a caveat here is read by exactly the person acting. */}
        {server.authMode === "manual" && server.authNote !== null && (
          <p className="text-faint text-xs leading-relaxed" data-testid="mcp-dialog-auth-note">
            {server.authNote}
          </p>
        )}
        <Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="connect" />
          <input type="hidden" name="server_id" value={server.serverId} />
          <input type="hidden" name="server" value={server.serverId} />
          <BusyFields busy={busy} className="space-y-3">
            <div className="flex flex-wrap items-start gap-2">
              <label className="block min-w-40 flex-1">
                <span className="mb-1 block font-medium text-dim text-sm">Add as</span>
                <input
                  type="text"
                  name="name"
                  required
                  defaultValue={server.suggestedName}
                  pattern="[a-z0-9][a-z0-9-]*"
                  className="block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink focus:border-accent focus:outline-none"
                />
              </label>
              <ChannelField />
            </div>
            {mine !== undefined && <RefusalNote error={mine.error} code={mine.code} at={mine.at} />}
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="submit"
                data-testid="mcp-connect"
                className={`${buttonClasses("primary")} min-h-11`}
              >
                {flying === "connect" ? "Adding…" : "Add to the workspace"}
              </button>
              <button
                type="button"
                onClick={onClose}
                className={`${buttonClasses("quiet")} min-h-11`}
              >
                Cancel
              </button>
            </div>
          </BusyFields>
        </Form>
      </DialogContent>
    </Dialog>
  );
}

/**
 * What one destination reads as, for THIS viewer. A curated channel is curation-gated: a member's
 * placement into it is withheld and the bundle lands catalog-only. That is worth knowing before
 * the button is pressed, not after — so the option says it, in the same words the page says
 * afterwards. Reviewers and owners place into a curated channel freely, so they see nothing extra.
 *
 * Exported for the unit suite: which viewer sees which label is the whole disclosure.
 */
export function channelOptionLabel(
  channel: { name: string; mode: string },
  label: string,
  role: "owner" | "reviewer" | "member",
): string {
  return channel.mode === "curated" && role === "member"
    ? `${label} — curated; placement needs a reviewer`
    : label;
}

/**
 * THE DESTINATION — written once for both acts, and RESTING ON NOTHING. Adding a server to the
 * workspace and handing it to people are two different things, so the field opens on "no channel":
 * the server lands in the workspace, reaches nobody, and stays there until someone chooses to
 * share it. A channel is the opt-in, taken here or on the channel's own page later.
 *
 * Every real channel is an ordinary option carrying its own NAME, the default `everyone`
 * included — there is no empty value standing in for a channel, so the one thing an untouched
 * form can mean is the one thing it says.
 */
function ChannelField() {
  const { channels, role } = useLoaderData<typeof loader>();
  const id = useId();
  const everyone = channels.find((channel) => channel.isDefault);
  return (
    <div className="min-w-40 flex-1">
      <label htmlFor={id} className="mb-1 block font-medium text-dim text-sm">
        Share into
      </label>
      <select
        id={id}
        name="channel"
        defaultValue=""
        className="block h-11 w-full rounded-md border border-line bg-panel px-3 text-ink text-sm focus:border-accent focus:outline-none"
      >
        {/* Short on purpose: a closed select is narrow, and a label clipped to "No channel —
            just add it to the works…" says less than two plain words do. The sentence that
            explains it lives under the field, where there is room for it. */}
        <option value="">No channel</option>
        {everyone !== undefined && (
          <option value={everyone.name}>
            {channelOptionLabel(everyone, `${everyone.name} (everyone here)`, role)}
          </option>
        )}
        {channels
          .filter((channel) => !channel.isDefault)
          .map((channel) => (
            <option key={channel.name} value={channel.name}>
              {channelOptionLabel(channel, channel.name, role)}
            </option>
          ))}
      </select>
      <p className="mt-1 text-faint text-xs">Optional — a channel is how it reaches people.</p>
    </div>
  );
}

/**
 * THE CUSTOM ARM — the three typed sources behind a disclosure, so the page rests on the list.
 * `<details>` because it is the browser's own disclosure: keyboard-operable, announced, and open
 * by nothing more than a click. These genuinely need this tier to read a document it has never
 * seen, so they keep the preview round trip the picker does not take.
 */
function CustomSource({ busy, flying }: { busy: boolean; flying: string | null }) {
  return (
    <details className="max-w-2xl border-line-soft border-t pt-4" data-testid="mcp-custom">
      <summary className="cursor-pointer font-medium text-dim text-sm hover:text-ink">
        A server that is not on this list
      </summary>
      <p className="mt-2 max-w-2xl text-faint text-xs leading-relaxed">
        It becomes this workspace&apos;s own server: a server the official registry carries, a URL
        serving a <code className="font-mono">server.json</code>, or the document itself.
      </p>
      <SourceForm busy={busy} flying={flying} />
    </details>
  );
}

/**
 * ONE SOURCE, ONE FIELD. The select decides which field the action reads, so showing all three
 * at once invites filling the wrong one and getting "Pick a source and fill in the matching
 * field." back for a form that looked filled in. The chosen source is the only field on screen,
 * which makes the select's job visible instead of implied.
 */
function SourceForm({ busy, flying }: { busy: boolean; flying: string | null }) {
  const [source, setSource] = useState<McpSourceKind>("registry");
  const fieldClasses =
    "block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink placeholder:text-faint focus:border-accent focus:outline-none";
  return (
    <Form method="post" className="mt-4 max-w-2xl space-y-4">
      <input type="hidden" name="intent" value="preview" />
      <BusyFields busy={busy} className="space-y-4">
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Where it comes from</span>
          <select
            name="source"
            value={source}
            onChange={(event) => setSource(event.target.value as McpSourceKind)}
            className="block h-11 w-full rounded-md border border-line bg-panel px-3 text-ink text-sm focus:border-accent focus:outline-none"
          >
            <option value="registry">The MCP registry, by name</option>
            <option value="url">A URL to a server.json</option>
            <option value="paste">Paste the server.json</option>
          </select>
        </label>
        {source === "registry" && (
          <label className="block">
            <span className="mb-1 block font-medium text-dim text-sm">Registry name</span>
            <input
              type="text"
              name="registry_name"
              placeholder="io.github.owner/server"
              className={fieldClasses}
            />
          </label>
        )}
        {source === "url" && (
          <label className="block">
            <span className="mb-1 block font-medium text-dim text-sm">URL</span>
            <input
              type="text"
              name="url"
              placeholder="https://example.com/.well-known/mcp/server.json"
              className={fieldClasses}
            />
          </label>
        )}
        {source === "paste" && (
          <label className="block">
            <span className="mb-1 block font-medium text-dim text-sm">server.json</span>
            <textarea
              name="document"
              rows={8}
              data-testid="mcp-paste"
              placeholder={'{\n  "name": "io.github.owner/server",\n  …\n}'}
              className="block w-full rounded-md border border-line px-3 py-2 font-mono text-[12px] text-ink placeholder:text-faint focus:border-accent focus:outline-none"
            />
            <span className="mt-1 block text-faint text-xs">
              The one path that makes no outbound request — use it for a server inside your own
              network.
            </span>
          </label>
        )}
        <button type="submit" className={`${buttonClasses("primary")} min-h-11`}>
          {flying === "preview" ? "Reading…" : "Preview"}
        </button>
      </BusyFields>
    </Form>
  );
}

/**
 * One refusal, said where the act was. When the answer POINTS somewhere, the path it names is
 * rendered as a real link, rooted for this deployment's grammar: the message's own spelling is
 * workspace-relative (it also travels the wire, where no tenancy is known), so the tail is
 * replaced rather than repeated.
 */
function RefusalNote({ error, code, at }: { error: string; code?: string; at?: string }) {
  const wsPath = useWsPath();
  const tail = at === undefined ? undefined : `/${at}`;
  const points = tail !== undefined && error.endsWith(tail);
  return (
    <p role="alert" data-testid="mcp-refusal" className="max-w-2xl text-red-700 text-sm">
      {points && tail !== undefined ? error.slice(0, -tail.length) : error}
      {points && at !== undefined && (
        <Link to={wsPath(at)} className="underline underline-offset-2" data-testid="mcp-refusal-at">
          {wsPath(at)}
        </Link>
      )}
      {code !== undefined && (
        <>
          {" "}
          <code className="font-mono text-[12px] text-faint">{code}</code>
        </>
      )}
    </p>
  );
}

export function PreviewCard({ preview }: { preview: PreviewData }) {
  const flying = useSubmittingIntent();
  const busy = flying !== null;
  const { summary } = preview;
  return (
    <section
      aria-labelledby="mcp-preview-heading"
      // The hook the e2e reads. It sits on the SECTION, not the Card: Card takes only
      // `children` and `className`, so an attribute handed to it is silently dropped.
      data-testid="mcp-preview"
      className="max-w-2xl space-y-3"
    >
      <SectionHeading>
        <span id="mcp-preview-heading">What would be added</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="font-mono text-[13px] text-ink">{summary.name}</span>
          <span className="text-faint text-xs">{summary.version ?? "unversioned"}</span>
          {summary.transport !== null && <Chip tone="neutral">{summary.transport}</Chip>}
        </div>
        <p className="text-dim text-sm">{summary.description}</p>
        {/* AN ADDRESS OR A PACKAGE LIST — never a blank line where an address would be. A document
            may offer a remote endpoint, a set of packages each machine installs, or both; the
            preview shows what this one actually holds. */}
        {summary.url !== null && (
          <p className="break-all font-mono text-[13px] text-dim" data-testid="mcp-preview-url">
            {summary.url}
          </p>
        )}
        {summary.packages.length > 0 && (
          <ul className="space-y-0.5 text-dim text-xs" data-testid="mcp-preview-packages">
            {summary.packages.map((pkg) => (
              <li key={`${pkg.registryType}:${pkg.identifier}`} className="break-all font-mono">
                {pkg.registryType} {pkg.identifier}
                {pkg.version === null ? "" : ` ${pkg.version}`} · {pkg.transport}
              </li>
            ))}
          </ul>
        )}
        <p className="text-faint text-xs">
          from {preview.origin === "" ? "the pasted document" : preview.origin}
        </p>
        {summary.headers.length > 0 && (
          <ul className="text-faint text-xs">
            {summary.headers.map((header) => (
              <li key={header.name} className="font-mono">
                {header.name}: {header.value}
              </li>
            ))}
          </ul>
        )}
        <details className="min-w-0">
          <summary className="cursor-pointer text-faint text-xs">The exact document</summary>
          <pre className="mt-2 max-h-64 overflow-auto rounded bg-panel2 p-3 font-mono text-[12px] text-dim leading-relaxed">
            {preview.document}
          </pre>
        </details>
        <Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="create" />
          <input type="hidden" name="document" value={preview.document} />
          {/* Carried so a refused create can hand this card back whole, provenance line and all,
              instead of costing the person the document they staged. */}
          <input type="hidden" name="origin" value={preview.origin} />
          <BusyFields busy={busy} className="space-y-3">
            <div className="flex flex-wrap items-start gap-2">
              <label className="block min-w-48 flex-1">
                <span className="mb-1 block font-medium text-dim text-sm">Add as</span>
                <input
                  type="text"
                  name="name"
                  required
                  defaultValue={preview.suggestedName}
                  pattern="[a-z0-9][a-z0-9-]*"
                  className="block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink focus:border-accent focus:outline-none"
                />
              </label>
              <ChannelField />
            </div>
            <button
              type="submit"
              data-testid="mcp-create"
              className={`${buttonClasses("primary")} min-h-11`}
            >
              {flying === "create" ? "Adding…" : "Add to the workspace"}
            </button>
          </BusyFields>
        </Form>
      </Card>
    </section>
  );
}
