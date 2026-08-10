import { Buffer } from "node:buffer";
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
import { publishGenesisBundle, webNewDestination } from "@/lib/api/genesis.server";
import { requireMemberInScope } from "@/lib/auth/guards.server";
import { bundlePath } from "@/lib/bundle-base";
import { auditInTx } from "@/lib/db/identity.server";
import { channelsOf } from "@/lib/db/queries.channels.server";
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
import { type McpGateRefusal, SERVER_JSON } from "@/lib/mcp/publish-gate.server";
import { type McpSummary, suggestedNameFor, validateServerJson } from "@/lib/mcp/validate.server";
import { useSubmittingIntent } from "@/lib/pending";
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
    // The viewer's own role, because a CURATED channel takes a member's placement away and the
    // picker must say so BEFORE the publish rather than after it (see `channelOptionLabel`).
    role: actor.role,
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
  /**
   * The in-workspace path the refusal points at — the server already holding the name. Carried
   * so the note can render it as a real link instead of a path to retype.
   */
  at?: string;
  /** The picked row a `pick` refusal answers about. */
  server?: string;
  /**
   * THE STAGED DOCUMENT, HANDED BACK. A refused publish on the custom arm must not cost the
   * person the bytes they staged: the preview card renders from this echo, so the retry is one
   * click and not a re-paste. Absent when the posted bytes no longer preview at all (a doctored
   * form), where there is nothing honest to render.
   */
  preview?: PreviewData;
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
    // The custom arm's staged bytes, echoed back with whatever this arm refuses so the card
    // stays on the page and the retry costs nothing. It is filled the moment the document is in
    // hand — a refusal before that has no bytes to hand back, which is honest.
    let staged: PreviewData | undefined;
    // A refusal from this arm goes back where the act was: into the dialog for a picked row,
    // onto the page for the custom arm's preview card.
    const refuseHere = (message: string, code?: string, status = 400, at?: string) =>
      picked.length === 0
        ? data<Refusal>(
            {
              form: "publish",
              error: message,
              ...(code === undefined ? {} : { code }),
              ...(at === undefined ? {} : { at }),
              ...(staged === undefined ? {} : { preview: staged }),
            },
            { status },
          )
        : data<Refusal>(
            {
              form: "pick",
              error: message,
              server: picked,
              ...(code === undefined ? {} : { code }),
              ...(at === undefined ? {} : { at }),
            },
            { status },
          );
    /** A gate answer, whole — its pointer home included, wherever the refusal renders. */
    const refuseGate = (r: McpGateRefusal) => refuseHere(r.message, r.code, 400, r.at);

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
      const restaged = validateServerJson(document);
      if (restaged.ok) {
        staged = {
          form: "preview",
          // The provenance line the card already showed, carried back with the bytes; a client
          // may say anything here, so it is bounded and only ever rendered to its own author.
          origin: String(formData.get("origin") ?? "").slice(0, 300),
          summary: restaged.summary,
          suggestedName: name.length > 0 ? name : suggestedNameFor(restaged.summary.name),
          document,
        };
      }
    }
    const files = [
      {
        path: SERVER_JSON,
        mode: "100644",
        content_base64: Buffer.from(document, "utf8").toString("base64"),
      },
    ];
    const validated = validateServerJson(document);
    if (!validated.ok) {
      return refuseHere(validated.message, validated.code);
    }
    // THE ORDINARY GENESIS PUBLISH — the same sequence the session lane and add-from-GitHub run
    // (kind gate → vault → registration + identity claim in one transaction). This door's only
    // additions are its own audit line and the birth name: the document's tail segment, or
    // whatever the member typed over it, folded through the catalog's own mint.
    //
    // AN EMPTY DESTINATION IS A DESTINATION. Importing a server is not the same act as handing
    // it to people, and nothing arriving without a channel may be read as consent to reach the
    // whole workspace — so the empty value means NO channel, which is also what this kind's
    // record says a genesis publish defaults to. It never means the default channel.
    const landed = await publishGenesisBundle({
      actor,
      kind: "mcp",
      candidate: {
        files,
        attribution: actor.display,
        message: `imported MCP server ${validated.summary.name}`,
      },
      displayName: name.length > 0 ? name : suggestedNameFor(validated.summary.name),
      destination: webNewDestination("mcp", channel),
      alsoInTx: async (tx, registered) => {
        await auditInTx(tx, {
          workspaceId: workspace.id,
          actor: { userId: actor.userId, display: actor.display },
          kind: "mcp_imported",
          subject: registered.bundleId,
          outcome: "ok",
          details: {
            server: validated.summary.name,
            version: validated.summary.version,
            url: validated.summary.url,
          },
        });
      },
    });
    if (landed.kind === "refused") {
      return refuseGate(landed.refusal);
    }
    if (landed.kind !== "ok") {
      return refuseHere("The publish did not land — try again.", undefined, 500);
    }
    const registered = landed;
    // WHAT ACTUALLY HAPPENED TO THE REACH. The publish landed; the PLACEMENT is a separate
    // outcome and may have been withheld (a curated channel takes a member's placement) or found
    // nothing to place into. The dialog promised that a chosen channel's agents get that address,
    // so a withheld placement must be said out loud on the page the redirect lands on rather than
    // read as a silent success. Choosing NO channel produces no outcome at all — nothing was
    // promised, and the server's own page already says it is in no channel.
    const path = wsPathServer(workspace.name, bundlePath("mcp", registered.name));
    if (registered.placement === undefined || registered.placement === "placed") {
      throw redirect(path);
    }
    // Only a NAMED channel can be withheld now, so the note's subject is the one on the form.
    const query = new URLSearchParams({ placement: registered.placement, channel });
    throw redirect(`${path}?${query.toString()}`);
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
  const error = actionData !== undefined && "error" in actionData ? actionData : undefined;
  // The card renders from a fresh preview OR from the one a refused publish handed back — the
  // staged bytes survive the refusal, so a retry is a click rather than a re-paste.
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
      {pageError !== undefined && (
        <RefusalNote error={pageError.error} code={pageError.code} at={pageError.at} />
      )}
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

/**
 * The auth chip — the one thing about a server worth knowing before choosing it. ONE component
 * for both doors: a picked row and a previewed document say how a person gets in with the same
 * two words, because they are answering the same question about the same field
 * (`_meta["sh.topos/auth"]`). A document that DECLARES nothing gets no chip: silence is the
 * honest answer there, not "no sign-in".
 */
export function AuthChip({ auth }: { auth: "oauth" | "none" | null }) {
  if (auth === null) {
    return null;
  }
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
            className="flex items-start gap-2 rounded-lg border border-line-soft bg-panel px-3 py-2.5 text-left transition-colors hover:border-line hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
          >
            {/* Leading, never stacked: the mark rides beside the three lines rather than above
                them, so a row costs the same height it did before anyone had a logo. */}
            <McpMark logo={server.logo} className="mt-0.5" />
            <span className="flex min-w-0 flex-1 flex-col items-stretch gap-0.5">
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
              <span className="w-full truncate font-mono text-[11px] text-faint">
                {server.host}
              </span>
            </span>
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
  error?: { error: string; code?: string; server?: string; at?: string };
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
            It lands as one bundle holding one file — the document below — in this workspace and
            nowhere else. Share it into a channel and every agent that channel reaches gets the
            address; leave that empty and it waits here until someone does.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2 rounded-md border border-line-soft bg-panel2 px-3 py-2.5">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            {/* The same mark the row carried, so the answer to "is this the one you clicked?" is
                the first thing on the block rather than a name to re-read. */}
            <McpMark logo={server.logo} className="size-4" />
            <span className="font-mono text-[13px] text-ink">{server.name}</span>
            {/* LABELLED, because the number is this document's and not the vendor's release: the
                built-in list holds a minimal document naming one endpoint, and a bare "1.0.0"
                beside a product name reads as a claim about the product. */}
            <span className="text-faint text-xs">document version {server.version}</span>
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
            <div className="flex flex-wrap items-start gap-2">
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
            {mine !== undefined && <RefusalNote error={mine.error} code={mine.code} at={mine.at} />}
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

/**
 * What one destination reads as, for THIS viewer. A curated channel is curation-gated: a member's
 * placement into it is withheld and the publish lands catalog-only. That is worth knowing before
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
 * THE DESTINATION — written once for both publishing surfaces, and RESTING ON NOTHING. Adding a
 * server to the workspace and handing it to people are two different acts, so the field opens on
 * "no channel": the import lands in the catalog, reaches nobody, and stays there until someone
 * chooses to share it. A channel is the opt-in, taken here or on the channel's own page later.
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
 * One refusal, said where the act was. When the answer POINTS somewhere — the server already
 * holding a name is the live case — the path it names is rendered as a real link, rooted for
 * this deployment's grammar: the message's own spelling is workspace-relative (it also travels
 * the wire, where no tenancy is known), so the tail is replaced rather than repeated.
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
        <span id="mcp-preview-heading">What would land</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="font-mono text-[13px] text-ink">{summary.name}</span>
          <span className="text-faint text-xs">{summary.version}</span>
          <Chip tone="neutral">{summary.transport}</Chip>
          <AuthChip auth={summary.authHint} />
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
          {/* Carried so a refused publish can hand this card back whole, provenance line and
              all, instead of costing the person the bytes they staged. */}
          <input type="hidden" name="origin" value={preview.origin} />
          <BusyFields busy={busy} className="space-y-3">
            <div className="flex flex-wrap items-start gap-2">
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
