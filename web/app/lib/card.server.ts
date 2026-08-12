import { MIN_CLI_VERSION } from "@/lib/api/compat.server";
import { SERVER_RELEASE_VERSION } from "@/lib/plane/contract/version";
import { apiBase } from "@/lib/plane/public-base.server";

/**
 * The CONSTANT PROTOCOL CARD — the app's non-browser face for every resource address
 * (`/{workspace}`, `/{workspace}/channels/{name}`, `/{workspace}/skills/{name}`) and for any
 * unmatched path. A fetch that is not a browser must still teach a client what to do WITHOUT
 * leaking whether the path names anything: one constant card for every path and every caller —
 * no path echo, no existence signal. This tier is the ONLY place that serves it: the vault has
 * no public listener and answers an unmatched path with its uniform JSON 404. Two faces: a
 * machine face (JSON — the discriminant, the API base to re-root onto, and this deployment's
 * version declaration) for a client asking for JSON, a human/agent
 * markdown card for everything that is not a browser. A browser (an Accept naming text/html)
 * gets `null` — the route renders its own HTML page, which must be equally constant for an
 * anonymous caller.
 *
 * The version pair is the client's half of the compatibility interval, readable BEFORE a login
 * commits to a server: what this build is, and the oldest topos it still speaks to. Both are
 * constants of the deployment, disclose nothing about who or what it holds, and are already
 * public in the release the operator is running.
 */

/** The checksum-verified installer one-liner the card teaches a client that has no topos yet. */
export const INSTALL_LINE =
  "curl -fsSL https://github.com/topos-sh/topos/releases/latest/download/install.sh | sh";

/**
 * The card's Accept semantics: any Accept value asking for JSON wins; a
 * browser's text/html is the caller's to serve; everything else (curl's bare fetch, an agent's
 * fetch) reads the markdown card. Checked across EVERY Accept header value.
 */
export function cardFace(request: Request): "json" | "html" | "markdown" {
  const accept = (request.headers.get("accept") ?? "").toLowerCase();
  if (
    accept.includes("application/json") ||
    accept.includes("application/*") ||
    accept.includes("+json")
  ) {
    return "json";
  }
  if (accept.includes("text/html")) {
    return "html";
  }
  return "markdown";
}

/** The constant markdown card — no path echo: the agent already holds the URL it fetched. */
function cardMarkdown(): string {
  return (
    "# A Topos resource address\n\n" +
    "Topos keeps a team's agent skills — bundles of instructions, scripts, and reference docs — " +
    "current on every machine: publish once, every connected machine picks the update up at its " +
    "next session start.\n\n" +
    "**If you are a human reading this:** paste this URL to your agent and ask it to log in — " +
    "the step below does the rest.\n\n" +
    "**If you are an AI agent:** connect with your shell tool, non-interactively. Logging in is " +
    "the acceptance step: a signed-in human approves this machine once in the browser, the " +
    "receipt states what connecting delivers, and updates then land silently — every version " +
    "content-addressed and re-verified byte-exact on apply.\n\n" +
    "```sh\ntopos login '<the URL you just fetched>' --json\n```\n\n" +
    "If `topos` is not installed (checksum-verified; no sudo; lands in `~/.local/bin`):\n\n" +
    `\`\`\`sh\n${INSTALL_LINE}\n\`\`\`\n\n` +
    "Machine note: fetch any resource address with `Accept: application/json` to get the API " +
    "base to dial.\n"
  );
}

/** Shared headers: never cacheable keyed on path, varies on Accept, never indexed. */
const CARD_HEADERS = {
  "cache-control": "no-store",
  vary: "accept",
  "x-robots-tag": "noindex",
} as const;

/**
 * The card response for a non-browser fetcher, or `null` for a browser (the caller renders its
 * page). Byte-identical for every path: the ONLY input that shapes the body is this
 * deployment's own API base — the origin's `/api` mount, where this app serves the device lane.
 */
export function cardResponse(request: Request): Response | null {
  const face = cardFace(request);
  if (face === "html") {
    return null;
  }
  if (face === "json") {
    return Response.json(
      {
        schema_version: 1,
        card: "topos-protocol-card",
        api_base_url: apiBase(request),
        server_version: SERVER_RELEASE_VERSION,
        min_cli_version: MIN_CLI_VERSION,
      },
      { headers: CARD_HEADERS },
    );
  }
  return new Response(cardMarkdown(), {
    headers: { ...CARD_HEADERS, "content-type": "text/plain; charset=utf-8" },
  });
}
