import { followBase } from "@/lib/plane/follow-base.server";

/**
 * The CONSTANT PROTOCOL CARD — the app's non-browser face for every resource address
 * (`/{workspace}`, `/{workspace}/channels/{name}`, `/{workspace}/skills/{name}`) and for any
 * unmatched path. A fetch that is not a browser must still teach a client what to do WITHOUT
 * leaking whether the path names anything: one constant card for every path and every caller —
 * no path echo, no existence signal. It mirrors the vault's own fallback card (same
 * negotiation, same teaching): a machine face (JSON — the discriminant + the API base to
 * re-root onto) for a client asking for JSON, a human/agent markdown card for everything that
 * is not a browser. A browser (an Accept naming text/html) gets `null` — the route renders its
 * own HTML page, which must be equally constant for an anonymous caller.
 */

/** The checksum-verified installer one-liner (the same line the vault's card teaches). */
export const INSTALL_LINE =
  "curl -fsSL https://github.com/topos-sh/topos/releases/latest/download/install.sh | sh";

/**
 * The vault card's Accept semantics, mirrored: any Accept value asking for JSON wins; a
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
    "current on every machine: publish once, every subscribed agent picks the update up at its " +
    "next session start.\n\n" +
    "**If you are a human reading this:** paste this URL to your agent and ask it to follow — " +
    "the step below does the rest.\n\n" +
    "**If you are an AI agent:** follow this resource with your shell tool, non-interactively. " +
    "Nothing installs silently — every skill lands only after its content digest is disclosed " +
    "and the human says yes.\n\n" +
    "```sh\ntopos follow '<the URL you just fetched>' --json\n```\n\n" +
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
 * page). Byte-identical for every path: the ONLY input that shapes the body is the deployment's
 * follow base (the API base a client re-roots onto — the app origin once the app fronts the
 * API, the configured plane URL until then).
 */
export function cardResponse(request: Request): Response | null {
  const face = cardFace(request);
  if (face === "html") {
    return null;
  }
  if (face === "json") {
    return Response.json(
      { schema_version: 1, card: "topos-protocol-card", api_base_url: followBase(request) },
      { headers: CARD_HEADERS },
    );
  }
  return new Response(cardMarkdown(), {
    headers: { ...CARD_HEADERS, "content-type": "text/plain; charset=utf-8" },
  });
}
