import type { ActionFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { deniedCodeEnvelope, okDataEnvelope } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { NO_CHANNEL } from "@/lib/db/queries.custody.server";
import {
  connectableMcpServerByName,
  connectedMcpBundle,
  connectMcpServer,
  createPrivateMcpServer,
} from "@/lib/db/queries.mcp-catalog.server";
import {
  canonicalServerJson,
  loadServerDocument,
  McpFetchError,
  unwrapServerDocument,
} from "@/lib/mcp/fetch.server";
import { scheduleRevisionProbe } from "@/lib/mcp/probe.server";
import { suggestedNameFor, validateServerJson } from "@/lib/mcp/validate.server";
import { allowUpstreamFetch } from "@/lib/rate-limit.server";

/**
 * `POST /api/v1/workspaces/{ws}/mcp-servers` — SHARE a server with this workspace, from a machine.
 *
 * The caller sends a spelling and nothing else: a `registry_name`, or a `document_url` pointing at
 * a `server.json`. Reading the document, deciding whether it may be shared, and naming the bundle
 * all happen HERE, so the answer is the same one the web page gives and there is one gate rather
 * than two vocabularies to drift apart.
 *
 * TWO ACTS, one door, told apart by what the catalog already holds. A registry name the catalog
 * offers is a CONNECTION — any member may make one, and asking twice answers with the bundle that
 * already stands rather than refusing. Anything else means this workspace writes a server of its
 * OWN down, which is an owner's act: the document is fetched over the SSRF-guarded transport,
 * validated, published as the private server's first revision, and connected in the same call.
 *
 * The connection REACHES NOBODY until a channel is chosen — the same ruling the web form rests on.
 * A machine that asked for the server gets it through its own manifest row; everyone else waits
 * for a curator, which is what sharing a server with a team is.
 */
const BODY_CAP = 8 * 1024;

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const raw = await readCappedBody(request, BODY_CAP, "mcp server body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  if (typeof parsed !== "object" || parsed === null) {
    return badRequest("malformed mcp server body");
  }
  const body = parsed as { registry_name?: unknown; document_url?: unknown };
  const registryName = body.registry_name;
  const documentUrl = body.document_url;
  if (registryName !== undefined && typeof registryName !== "string") {
    return badRequest("malformed mcp server body: registry_name");
  }
  if (documentUrl !== undefined && typeof documentUrl !== "string") {
    return badRequest("malformed mcp server body: document_url");
  }
  if ((registryName === undefined) === (documentUrl === undefined)) {
    return badRequest("a server is named by a registry name OR a document url, never both");
  }

  // THE CONNECT ARM: a name the catalog already offers. Any member.
  if (typeof registryName === "string") {
    const standing = await connectedMcpBundle(actor, registryName);
    if (standing !== null) {
      return okDataEnvelope("mcp", {
        skill_id: standing.bundleId,
        name: standing.name,
        created: false,
      });
    }
    const offered = await connectableMcpServerByName(actor, registryName);
    if (offered !== null) {
      const connected = await connectMcpServer(actor, {
        serverId: offered.serverId,
        // The name a machine will `topos add` it by — the same one the web form suggests, so a
        // server shared from a terminal and one shared from a browser answer to one word.
        displayName: suggestedNameFor(registryName),
        to: NO_CHANNEL,
      });
      if (connected.refusal !== null) {
        return deniedCodeEnvelope("mcp", connected.refusal.code, connected.refusal.message);
      }
      return okDataEnvelope("mcp", {
        skill_id: connected.registration.bundleId,
        name: connected.registration.name,
        created: false,
      });
    }
  }

  // THE WRITE-IT-DOWN ARM: a name this catalog does not offer, or a link to a document. The
  // workspace is about to hold a server of its own, which is an owner's ruling to make.
  if (actor.role !== "owner") {
    return deniedCodeEnvelope(
      "mcp",
      "OWNER_ROLE_REQUIRED",
      typeof registryName === "string"
        ? `this catalog does not offer ${registryName}, so adding it means writing a server down for your workspace — a workspace owner does that`
        : "adding a server from a link means writing it down for your workspace — a workspace owner does that",
    );
  }
  const owner = { ...actor, role: "owner" as const };
  // The upstream fetch is somebody else's server being dialled on this caller's word: the same
  // belt the web form wears, keyed per person.
  if (!allowUpstreamFetch(actor.userId)) {
    return deniedCodeEnvelope(
      "mcp",
      "TOO_MANY_LOOKUPS",
      "too many server lookups just now — wait a minute and try again",
    );
  }
  let fetched: { text: string };
  try {
    fetched = await loadServerDocument(
      typeof registryName === "string"
        ? { kind: "registry", value: registryName }
        : { kind: "url", value: documentUrl as string },
    );
  } catch (error) {
    if (error instanceof McpFetchError) {
      return deniedCodeEnvelope("mcp", "MCP_UNREACHABLE", error.message);
    }
    throw error;
  }
  const unwrapped = unwrapServerDocument(fetched.text);
  if (unwrapped === null) {
    return deniedCodeEnvelope(
      "mcp",
      "MCP_INVALID",
      "that is not a server.json — a server document is a JSON object",
    );
  }
  const validated = validateServerJson(canonicalServerJson(unwrapped));
  if (!validated.ok) {
    return deniedCodeEnvelope("mcp", validated.code, validated.message);
  }
  const created = await createPrivateMcpServer(
    owner,
    {
      displayName: validated.summary.name,
      description: validated.summary.description,
      // The row makes NO sign-in claim: what a server actually requires is verified, never read
      // off the document's own word.
      authMode: null,
    },
    unwrapped,
  );
  if (created.refusal !== null) {
    return deniedCodeEnvelope("mcp", created.refusal.code, created.refusal.message);
  }
  const connected = await connectMcpServer(owner, {
    serverId: created.serverId,
    displayName: suggestedNameFor(validated.summary.name),
    to: NO_CHANNEL,
  });
  if (connected.refusal !== null) {
    // The server row STANDS — it is written down and an owner can connect it from the web. Saying
    // so is the honest answer; unwinding a durable write to make one call look atomic is not.
    return deniedCodeEnvelope("mcp", connected.refusal.code, connected.refusal.message);
  }
  // Strictly after the revision is durable, and it can never block or slow what it follows.
  scheduleRevisionProbe({
    revisionId: created.revisionId,
    endpoint: validated.summary.url,
  });
  return okDataEnvelope("mcp", {
    skill_id: connected.registration.bundleId,
    name: connected.registration.name,
    created: true,
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
