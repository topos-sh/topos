import { Buffer } from "node:buffer";
import { useId, useState } from "react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, Link, redirect, useActionData, useLoaderData } from "react-router";
import { BusyFields, buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { requireMemberInScope } from "@/lib/auth/guards.server";
import { bundlePath } from "@/lib/bundle-base";
import { auditInTx, mintBundleId } from "@/lib/db/identity.server";
import { channelsOf } from "@/lib/db/queries.channels.server";
import { inFinalTx, registerGenesisBundleInTx } from "@/lib/db/queries.custody.server";
import { lockMcpNamesInTx } from "@/lib/db/queries.mcp.server";
import { mcpNameTaken } from "@/lib/mcp/catalog.server";
import {
  type CuratedMcpRow,
  curatedDocumentFor,
  curatedServerRows,
} from "@/lib/mcp/curated.server";
import {
  canonicalServerJson,
  fetchesUpstream,
  loadServerDocument,
  McpFetchError,
  type McpSourceKind,
  unwrapServerDocument,
} from "@/lib/mcp/fetch.server";
import {
  type McpGateRefusal,
  mcpCandidateRefusal,
  mcpNameTakenRefusal,
  SERVER_JSON,
} from "@/lib/mcp/publish-gate.server";
import { type McpSummary, suggestedNameFor, validateServerJson } from "@/lib/mcp/validate.server";
import { useSubmittingIntent } from "@/lib/pending";
import { publishVersion } from "@/lib/plane/custody.server";
import { allowUpstreamFetch } from "@/lib/rate-limit.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta() {
  return [{ title: "Add an MCP server · Topos" }];
}

/**
 * ADD AN MCP SERVER — the web way in for a `kind: 'mcp'` bundle. FOUR ways to name a server,
 * ONE publish:
 *
 *  · a PICK from the built-in list of popular servers (app/lib/mcp/curated.server.ts) — the
 *    page's resting state, because knowing an address by heart is the rare case;
 *  · a REGISTRY NAME — the server document as the official registry serves it today;
 *  · a DIRECT URL to a server.json — SSRF-guarded, https-only, redirect-refusing;
 *  · a PASTED document — no fetch at all, which is the safe path for a server that lives
 *    inside the network this process runs in.
 *
 * The three CUSTOM arms need this tier to read bytes nobody here has seen, so they keep their
 * preview round trip. The PICKER does not: every built-in row's document is committed data the
 * loader ships with the page, so choosing one opens a dialog on the click itself — what would
 * land, the exact bytes, the name it would publish under — with no request made and nothing on
 * the page put out of reach while it opens. Cancelling changes nothing.
 *
 * Whichever arm asked, the publish is one act running the same gate every publish passes
 * (app/lib/mcp/validate.server.ts) before any custody call. A picked row gets no shortcut past
 * it; it only saves the typing. A picked row's BYTES are re-derived from the list on this side
 * rather than read back off the form — a form is a client, and a client's word is not the
 * document.
 *
 * The published bundle is EXACTLY one file, `server.json`, holding those canonical bytes.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const channels = await channelsOf(actor);
  return {
    wsName: workspace.name,
    channels: channels.map((c) => ({ name: c.name, isDefault: c.isDefault, mode: c.mode })),
    // The whole built-in list, documents included — see `CuratedMcpRow` for why the bytes ride
    // along instead of being fetched on click.
    curated: curatedServerRows(),
  };
}

const SOURCE_KINDS: McpSourceKind[] = ["registry", "url", "paste"];
/** A pasted document is bounded the same way a fetched one is (the gate's own ceiling). */
const MAX_PASTE_CHARS = 256 * 1024;

interface PreviewData {
  form: "preview";
  /** Where the bytes came from, echoed so the publish arm's copy can say it. */
  origin: string;
  summary: McpSummary;
  suggestedName: string;
  /** The canonical bytes the bundle would store — the publish arm's payload, verbatim. */
  document: string;
}

interface Refusal {
  /**
   * WHERE THE ANSWER BELONGS. `preview` and `publish` are the custom arm's and render on the
   * page; `pick` is the dialog's and renders inside it, carrying the row it answers about so a
   * stale refusal can never attach itself to a different server.
   */
  form: "preview" | "publish" | "pick";
  error: string;
  /** The typed refusal code, when the gate produced one — shown as a quiet chip. */
  code?: string;
  /** The picked row a `pick` refusal answers about. */
  server?: string;
}

function refusal(form: Refusal["form"], error: string, code?: string, status = 400) {
  return data<Refusal>({ form, error, ...(code === undefined ? {} : { code }) }, { status });
}

/**
 * The bytes a bundle would store, from whatever arrived. A registry answer wraps the document
 * in `{ server, _meta }`; a URL or a paste is the document itself. Canonicalizing BOTH (and
 * again on the way back through the form) is what makes the preview and the published bundle
 * the same bytes, and what makes two people importing the same server converge on one version
 * id. Text that is not a JSON object passes through untouched, so the gate — not this — gets
 * to word the refusal.
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
    const validated = validateServerJson(document);
    if (!validated.ok) {
      return refusal("preview", validated.message, validated.code);
    }
    return data<PreviewData>({
      form: "preview",
      origin,
      summary: validated.summary,
      suggestedName: suggestedNameFor(validated.summary.name),
      document,
    });
  }

  if (intent === "publish") {
    const picked = String(formData.get("server") ?? "").trim();
    const name = String(formData.get("name") ?? "").trim();
    const channel = String(formData.get("channel") ?? "").trim();
    // A refusal from this arm goes back where the act was: into the dialog for a picked row,
    // onto the page for the custom arm's preview card.
    const refuseHere = (message: string, code?: string, status = 400) =>
      picked.length === 0
        ? refusal("publish", message, code, status)
        : data<Refusal>(
            {
              form: "pick",
              error: message,
              server: picked,
              ...(code === undefined ? {} : { code }),
            },
            { status },
          );

    let document: string;
    if (picked.length > 0) {
      // Looked up, never taken from the form: the browser posts an id, and an id the built-in
      // list does not hold is refused here rather than turned into a document.
      const bytes = curatedDocumentFor(picked);
      if (bytes === null) {
        return refuseHere("That is not one of the servers on this list.");
      }
      document = bytes;
    } else {
      const posted = String(formData.get("document") ?? "");
      if (posted.length === 0 || posted.length > MAX_PASTE_CHARS) {
        return refuseHere("Nothing to publish — run the preview again.");
      }
      // Canonicalized AGAIN rather than stored as posted: a form field round-trips through
      // multipart encoding, which normalizes line endings, so trusting the bytes back would make
      // the published version id depend on the browser rather than on the document.
      document = canonicalize(posted);
    }
    const files = [
      {
        path: SERVER_JSON,
        mode: "100644",
        content_base64: Buffer.from(document, "utf8").toString("base64"),
      },
    ];
    // The SAME gate the session lane runs, on the same bytes, before any custody call — a
    // refused document leaves nothing behind, and the web door gets no exemption from the
    // embedded-name uniqueness rule.
    const gate = await mcpCandidateRefusal(actor, files, null);
    if (gate.refusal !== null) {
      return refuseHere(gate.refusal.message, gate.refusal.code);
    }
    const validated = validateServerJson(document);
    if (!validated.ok) {
      return refuseHere(validated.message, validated.code);
    }
    const bundleId = mintBundleId();
    const published = await publishVersion(workspace.id, bundleId, {
      files,
      attribution: actor.display,
      message: `imported MCP server ${validated.summary.name}`,
    });
    if (published.kind !== "ok") {
      return refuseHere("The publish did not land — try again.", undefined, 500);
    }
    const landed = await inFinalTx<{ refused: McpGateRefusal } | { refused: null; name: string }>(
      async (tx) => {
        // The embedded name, looked at again under the lock every registering door takes: the
        // gate above answered before the vault call, so another publish could have claimed the
        // name in between. On a collision this transaction registers NOTHING and the page says
        // so — the published bytes stand in the vault with no catalog row, which is the same
        // sequencing the session lane has (custody first, catalog second).
        await lockMcpNamesInTx(tx, workspace.id);
        // On the HELD client — a pool checkout under the lock is the exhaustion shape.
        const taken = await mcpNameTaken(actor, validated.summary.name, bundleId, tx);
        if (taken.kind !== "free") {
          return { refused: mcpNameTakenRefusal(validated.summary.name, taken) };
        }
        // The birth name folds from the document's tail segment (or whatever the member typed
        // over it) through the catalog's own mint — same rules, same collision suffixes.
        const registration = await registerGenesisBundleInTx(
          tx,
          actor,
          bundleId,
          name.length > 0 ? name : suggestedNameFor(validated.summary.name),
          channel.length > 0 ? channel : null,
          "mcp",
        );
        await auditInTx(tx, {
          workspaceId: workspace.id,
          actor: { userId: actor.userId, display: actor.display },
          kind: "mcp_imported",
          subject: bundleId,
          outcome: "ok",
          details: {
            server: validated.summary.name,
            version: validated.summary.version,
            url: validated.summary.url,
          },
        });
        return { refused: null, name: registration.name };
      },
    );
    if (landed.refused !== null) {
      return refuseHere(landed.refused.message, landed.refused.code);
    }
    throw redirect(wsPathServer(workspace.name, bundlePath("mcp", landed.name)));
  }

  return refusal("publish", "Unknown action.");
}

export default function McpNew() {
  const { curated } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const wsPath = useWsPath();
  const flying = useSubmittingIntent();
  const busy = flying !== null;
  // The row whose dialog is open — plain local state, set by a click and cleared by Cancel or
  // Escape. It is the only thing choosing a server changes until the publish button is pressed.
  const [picked, setPicked] = useState<CuratedMcpRow | null>(null);
  const preview =
    actionData !== undefined && actionData.form === "preview" && !("error" in actionData)
      ? actionData
      : undefined;
  const error = actionData !== undefined && "error" in actionData ? actionData : undefined;
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
        An MCP server shared here is a <em className="text-ink not-italic">remote</em> address every
        agent on the team can reach — one <code className="font-mono text-[13px]">server.json</code>
        , the same bytes everywhere. Nothing that installs locally and nothing carrying a key: a
        credential belongs on the machine that uses it, never in something the whole team receives.
      </p>
      <ServerPicker servers={curated} onPick={setPicked} />
      {picked !== null && (
        <AddServerDialog
          server={picked}
          error={error !== undefined && error.form === "pick" ? error : undefined}
          onClose={() => setPicked(null)}
        />
      )}
      <CustomSource busy={busy} flying={flying} />
      {pageError !== undefined && <RefusalNote error={pageError.error} code={pageError.code} />}
      {preview !== undefined && <PreviewCard preview={preview} />}
    </div>
  );
}

/** Does this row match what someone typed? Title, blurb, host and registry name all count. */
function matches(server: CuratedMcpRow, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) {
    return true;
  }
  return `${server.title} ${server.description} ${server.host} ${server.name}`
    .toLowerCase()
    .includes(needle);
}

/** The auth chip — the one thing about a server worth knowing before choosing it. */
function AuthChip({ auth }: { auth: "oauth" | "none" }) {
  return auth === "oauth" ? (
    <Chip tone="accent">oauth</Chip>
  ) : (
    <Chip tone="neutral">no sign-in</Chip>
  );
}

/**
 * THE PICKER — the popular servers in a dense grid, narrowed by one text box. Each card is a
 * plain button that opens the dialog: no form, no submit, no navigation, so the grid neither
 * reloads nor goes inert while the answer appears. The density is the point — the list being
 * chosen from should be visible at once rather than scrolled through two at a time.
 *
 * The filter is the one piece of state here, and it is purely local — the list is small, ships
 * with the page, and never needs a round trip to narrow.
 */
function ServerPicker({
  servers,
  onPick,
}: {
  servers: CuratedMcpRow[];
  onPick: (server: CuratedMcpRow) => void;
}) {
  const [query, setQuery] = useState("");
  const visible = servers.filter((server) => matches(server, query));
  return (
    <section aria-labelledby="mcp-picker-heading" className="space-y-3" data-testid="mcp-picker">
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <SectionHeading>
          <span id="mcp-picker-heading">Popular servers</span>
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
        {visible.map((server) => (
          <button
            key={server.name}
            type="button"
            onClick={() => onPick(server)}
            data-testid="mcp-picker-option"
            className="flex flex-col items-stretch gap-0.5 rounded-lg border border-line-soft bg-panel px-3 py-2.5 text-left transition-colors hover:border-line hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
          >
            <span className="flex items-center gap-1.5">
              <span className="min-w-0 flex-1 truncate font-medium text-ink text-sm">
                {server.title}
              </span>
              <span className="shrink-0">
                <AuthChip auth={server.auth} />
              </span>
            </span>
            <span className="w-full truncate text-dim text-xs leading-snug">
              {server.description}
            </span>
            <span className="w-full truncate font-mono text-[11px] text-faint">{server.host}</span>
          </button>
        ))}
      </div>
      <p aria-live="polite" className="text-faint text-xs">
        {visible.length === servers.length
          ? `${servers.length} servers`
          : visible.length === 1
            ? "1 server matches"
            : `${visible.length} servers match`}
        {visible.length === 0 && " — add it below as a custom server."}
      </p>
    </section>
  );
}

/**
 * THE PICK DIALOG — the question a click asks, answered without leaving the list: is this the
 * server you meant, and here is exactly what would land if it is. Everything it shows came down
 * with the page, so it opens on the click itself; the ONE server call a picked row makes is the
 * publish, and until that button is pressed nothing anywhere has changed.
 *
 * The gate still decides. If it refuses these bytes — a name another bundle in this workspace
 * already claims is the live case — the refusal comes back INTO this dialog, beside the button
 * that asked for it, with the row still on screen.
 */
function AddServerDialog({
  server,
  error,
  onClose,
}: {
  server: CuratedMcpRow;
  error?: { error: string; code?: string; server?: string };
  onClose: () => void;
}) {
  const flying = useSubmittingIntent();
  const busy = flying !== null;
  // Only this row's own refusal: an answer about the server chosen before it is not about this
  // one, and showing it here would read as a verdict on a document the gate never saw.
  const mine = error !== undefined && error.server === server.name ? error : undefined;
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
          <DialogTitle>Add {server.title} to this workspace?</DialogTitle>
          <DialogDescription>
            It lands as one bundle holding one file — the document below — and every agent the
            channel reaches gets that address.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2 rounded-md border border-line-soft bg-panel2 px-3 py-2.5">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="font-mono text-[13px] text-ink">{server.name}</span>
            <span className="text-faint text-xs">{server.version}</span>
            <Chip tone="neutral">{server.transport}</Chip>
            <AuthChip auth={server.auth} />
          </div>
          <p className="text-dim text-sm">{server.description}</p>
          <p className="break-all font-mono text-[13px] text-dim" data-testid="mcp-dialog-url">
            {server.url}
          </p>
        </div>
        {server.auth === "oauth" && (
          <p className="text-faint text-xs leading-relaxed">
            The publisher says an agent signs in on first use — that sign-in happens on each
            person&apos;s own machine, never here, and no credential rides in these bytes.
          </p>
        )}
        <details>
          <summary className="cursor-pointer text-faint text-xs">
            The exact bytes this would store
          </summary>
          <pre
            data-testid="mcp-dialog-document"
            className="mt-2 max-h-56 overflow-auto rounded bg-panel2 p-3 font-mono text-[12px] text-dim leading-relaxed"
          >
            {server.document}
          </pre>
        </details>
        <Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="publish" />
          <input type="hidden" name="server" value={server.name} />
          <BusyFields busy={busy} className="space-y-3">
            <div className="flex flex-wrap items-end gap-2">
              <label className="block min-w-40 flex-1">
                <span className="mb-1 block font-medium text-dim text-sm">Publish as</span>
                <input
                  type="text"
                  name="name"
                  required
                  defaultValue={server.slug}
                  pattern="[a-z0-9][a-z0-9-]*"
                  className="block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink focus:border-accent focus:outline-none"
                />
              </label>
              <ChannelField />
            </div>
            {mine !== undefined && <RefusalNote error={mine.error} code={mine.code} />}
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="submit"
                data-testid="mcp-publish"
                className={`${buttonClasses("primary")} min-h-11`}
              >
                {flying === "publish" ? "Adding…" : "Add to the workspace"}
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

/** The destination channel, label and all — written once for both publishing surfaces. */
function ChannelField() {
  const { channels } = useLoaderData<typeof loader>();
  const id = useId();
  return (
    <div className="min-w-40 flex-1">
      <label htmlFor={id} className="mb-1 block font-medium text-dim text-sm">
        Into
      </label>
      <select
        id={id}
        name="channel"
        defaultValue=""
        className="block h-11 w-full rounded-md border border-line bg-panel px-3 text-ink text-sm focus:border-accent focus:outline-none"
      >
        <option value="">Everyone (the default channel)</option>
        {channels
          .filter((channel) => !channel.isDefault)
          .map((channel) => (
            <option key={channel.name} value={channel.name}>
              {channel.name}
            </option>
          ))}
      </select>
    </div>
  );
}

/**
 * THE CUSTOM ARM — the three typed sources, unchanged, behind a disclosure so the page rests on
 * the list. `<details>` because it is the browser's own disclosure: keyboard-operable, announced,
 * and open by nothing more than a click. These genuinely need this tier to read bytes it has
 * never seen, so they keep the preview round trip the picker no longer takes.
 */
function CustomSource({ busy, flying }: { busy: boolean; flying: string | null }) {
  return (
    <details className="max-w-2xl border-line-soft border-t pt-4" data-testid="mcp-custom">
      <summary className="cursor-pointer font-medium text-dim text-sm hover:text-ink">
        Custom server
      </summary>
      <p className="mt-2 max-w-2xl text-faint text-xs leading-relaxed">
        Anything not on the list: a server the official registry carries, a URL serving a{" "}
        <code className="font-mono">server.json</code>, or the document itself.
      </p>
      <SourceForm busy={busy} flying={flying} />
    </details>
  );
}

function SourceForm({ busy, flying }: { busy: boolean; flying: string | null }) {
  return (
    <Form method="post" className="mt-4 max-w-2xl space-y-4">
      <input type="hidden" name="intent" value="preview" />
      <BusyFields busy={busy} className="space-y-4">
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Where it comes from</span>
          <select
            name="source"
            defaultValue="registry"
            className="block h-11 w-full rounded-md border border-line bg-panel px-3 text-ink text-sm focus:border-accent focus:outline-none"
          >
            <option value="registry">The MCP registry, by name</option>
            <option value="url">A URL to a server.json</option>
            <option value="paste">Paste the server.json</option>
          </select>
        </label>
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Registry name</span>
          <input
            type="text"
            name="registry_name"
            placeholder="io.github.owner/server"
            className="block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink placeholder:text-faint focus:border-accent focus:outline-none"
          />
        </label>
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">URL</span>
          <input
            type="text"
            name="url"
            placeholder="https://example.com/.well-known/mcp/server.json"
            className="block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink placeholder:text-faint focus:border-accent focus:outline-none"
          />
        </label>
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
        <button type="submit" className={`${buttonClasses("primary")} min-h-11`}>
          {flying === "preview" ? "Reading…" : "Preview"}
        </button>
      </BusyFields>
    </Form>
  );
}

function RefusalNote({ error, code }: { error: string; code?: string }) {
  return (
    <p role="alert" data-testid="mcp-refusal" className="max-w-2xl text-red-700 text-sm">
      {error}
      {code !== undefined && (
        <>
          {" "}
          <code className="font-mono text-[12px] text-faint">{code}</code>
        </>
      )}
    </p>
  );
}

function PreviewCard({ preview }: { preview: PreviewData }) {
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
        <span id="mcp-preview-heading">What would land</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="font-mono text-[13px] text-ink">{summary.name}</span>
          <span className="text-faint text-xs">{summary.version}</span>
          <Chip tone="neutral">{summary.transport}</Chip>
          {summary.authHint === "oauth" && <Chip tone="accent">oauth</Chip>}
        </div>
        <p className="text-dim text-sm">{summary.description}</p>
        <p className="break-all font-mono text-[13px] text-dim" data-testid="mcp-preview-url">
          {summary.url}
        </p>
        <p className="text-faint text-xs">
          from {preview.origin === "" ? "the pasted document" : preview.origin}
          {summary.authHint === "oauth" && (
            <>
              {" · "}the publisher says an agent signs in on first use — that sign-in happens on the
              machine, never here
            </>
          )}
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
        <details>
          <summary className="cursor-pointer text-faint text-xs">
            The exact bytes this would store
          </summary>
          <pre className="mt-2 max-h-64 overflow-auto rounded bg-panel2 p-3 font-mono text-[12px] text-dim leading-relaxed">
            {preview.document}
          </pre>
        </details>
        <Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="publish" />
          <input type="hidden" name="document" value={preview.document} />
          <BusyFields busy={busy} className="space-y-3">
            <div className="flex flex-wrap items-end gap-2">
              <label className="block min-w-48 flex-1">
                <span className="mb-1 block font-medium text-dim text-sm">Publish as</span>
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
              data-testid="mcp-publish"
              className={`${buttonClasses("primary")} min-h-11`}
            >
              {flying === "publish" ? "Publishing…" : "Publish to the workspace"}
            </button>
          </BusyFields>
        </Form>
      </Card>
    </section>
  );
}
