import { Buffer } from "node:buffer";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, Link, redirect, useActionData, useLoaderData } from "react-router";
import { BusyFields, buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import { requireMemberInScope } from "@/lib/auth/guards.server";
import { auditInTx, mintBundleId } from "@/lib/db/identity.server";
import { channelsOf } from "@/lib/db/queries.channels.server";
import { inFinalTx, registerGenesisBundleInTx } from "@/lib/db/queries.custody.server";
import { lockMcpNamesInTx } from "@/lib/db/queries.mcp.server";
import { mcpNameTaken } from "@/lib/mcp/catalog.server";
import {
  canonicalServerJson,
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
  return [{ title: "Add an MCP server" }];
}

/**
 * ADD AN MCP SERVER — the web import flow for a `kind: 'mcp'` bundle. Three ways in, ONE
 * preview and ONE publish:
 *
 *  · a REGISTRY NAME — the server document as the official registry serves it today;
 *  · a DIRECT URL to a server.json — SSRF-guarded, https-only, redirect-refusing;
 *  · a PASTED document — no fetch at all, which is the safe path for a server that lives
 *    inside the network this process runs in.
 *
 * All three land in the same place: the document is canonicalized, run through the same gate
 * every publish passes (app/lib/mcp/validate.server.ts), and shown as what it actually
 * promises — endpoint, transport, literal headers, and whether the publisher declares an auth
 * dance. Nothing is written until the second click.
 *
 * The published bundle is EXACTLY one file, `server.json`, holding the canonical bytes the
 * preview displayed. The publish arm re-validates them rather than trusting the round-trip:
 * the form is a client, and this is the same gate the session lane runs.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const channels = await channelsOf(actor);
  return {
    wsName: workspace.name,
    channels: channels.map((c) => ({ name: c.name, isDefault: c.isDefault, mode: c.mode })),
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
  form: "preview" | "publish";
  error: string;
  /** The typed refusal code, when the gate produced one — shown as a quiet chip. */
  code?: string;
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

/** Read the one source field the chosen arm uses. */
function sourceFrom(formData: FormData): { kind: McpSourceKind; value: string } | null {
  const kind = String(formData.get("source") ?? "");
  if (!SOURCE_KINDS.includes(kind as McpSourceKind)) {
    return null;
  }
  const field = kind === "registry" ? "registry_name" : kind === "url" ? "url" : "document";
  const value = String(formData.get(field) ?? "").trim();
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
    // same belt the GitHub import wears (route actions bypass the /api/v1 belt entirely).
    if (source.kind !== "paste" && !allowUpstreamFetch(actor.userId)) {
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
    const posted = String(formData.get("document") ?? "");
    const name = String(formData.get("name") ?? "").trim();
    const channel = String(formData.get("channel") ?? "").trim();
    if (posted.length === 0 || posted.length > MAX_PASTE_CHARS) {
      return refusal("publish", "Nothing to publish — run the preview again.");
    }
    // Canonicalized AGAIN rather than stored as posted: a form field round-trips through
    // multipart encoding, which normalizes line endings, so trusting the bytes back would make
    // the published version id depend on the browser rather than on the document.
    const document = canonicalize(posted);
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
      return refusal("publish", gate.refusal.message, gate.refusal.code);
    }
    const validated = validateServerJson(document);
    if (!validated.ok) {
      return refusal("publish", validated.message, validated.code);
    }
    const bundleId = mintBundleId();
    const published = await publishVersion(workspace.id, bundleId, {
      files,
      attribution: actor.display,
      message: `imported MCP server ${validated.summary.name}`,
    });
    if (published.kind !== "ok") {
      return refusal("publish", "The publish did not land — try again.", undefined, 500);
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
      return refusal("publish", landed.refused.message, landed.refused.code);
    }
    throw redirect(wsPathServer(workspace.name, `skills/${landed.name}`));
  }

  return refusal("publish", "Unknown action.");
}

export default function McpImport() {
  const actionData = useActionData<typeof action>();
  const wsPath = useWsPath();
  const flying = useSubmittingIntent();
  const busy = flying !== null;
  const preview =
    actionData !== undefined && actionData.form === "preview" && !("error" in actionData)
      ? actionData
      : undefined;
  const error = actionData !== undefined && "error" in actionData ? actionData : undefined;
  return (
    <div className="space-y-8">
      <PageHeader
        title="Add an MCP server"
        actions={
          <Link to={wsPath("")} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <p className="max-w-2xl text-dim text-sm leading-relaxed">
        An MCP server shared here is a <em className="text-ink not-italic">remote</em> address every
        agent on the team can reach — one <code className="font-mono text-[13px]">server.json</code>
        , the same bytes everywhere. Servers that install locally, endpoints with a placeholder to
        fill in, and documents carrying a key are refused: a credential belongs on the machine that
        uses it, never in something the whole team receives.
      </p>
      <SourceForm busy={busy} flying={flying} />
      {error !== undefined && <RefusalNote error={error.error} code={error.code} />}
      {preview !== undefined && <PreviewCard preview={preview} />}
    </div>
  );
}

function SourceForm({ busy, flying }: { busy: boolean; flying: string | null }) {
  return (
    <Form method="post" className="max-w-2xl space-y-4">
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
  const { channels } = useLoaderData<typeof loader>();
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
              <label className="block min-w-48 flex-1">
                <span className="mb-1 block font-medium text-dim text-sm">Into</span>
                <select
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
              </label>
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
