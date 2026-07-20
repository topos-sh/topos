/**
 * GET /llms.txt — the machine-readable site guide (the llms.txt convention): where an LLM that
 * landed on this origin looks next. Deliberately a STATIC module-level constant (the agent.ts
 * posture): no request input, byte-identical on every fetch, and a resource route — its loader
 * Response returns directly, so the protocol-card interception (which lives in handleRequest,
 * reached only by document renders) never touches it. Served as text/plain so a browser renders
 * it readable instead of downloading.
 */

const LLMS_TXT = `# Topos

> Topos keeps a team's agent skills — bundles of instructions, scripts, and reference docs —
> current on every enrolled machine: publish once, every subscribed agent picks the update up
> at its next session start. Agents drive the \`topos\` CLI non-interactively; nothing lands
> silently — every skill installs only after its contents are disclosed and consented to.

## Setup

- [Agent setup guide](/agent): step-by-step instructions for an AI agent told "set up Topos"
- [Install script](/install): the checksum-verified installer — \`curl -fsSL https://topos.sh/install | sh\`

## Skills

- [Agent-skills discovery index](/.well-known/agent-skills/index.json): the downloadable
  \`topos\` skill, digest-pinned

## Source

- [GitHub repository](https://github.com/topos-sh/topos): the CLI, the self-hostable server,
  and this web app — Apache-2.0
- [Security policy](https://github.com/topos-sh/topos/blob/main/SECURITY.md): the trust model
  and how to report a vulnerability
- [Architecture](https://github.com/topos-sh/topos/blob/main/ARCHITECTURE.md): trust
  boundaries and the consent + sync design
`;

export async function loader(): Promise<Response> {
  return new Response(LLMS_TXT, {
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
