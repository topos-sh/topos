/**
 * GET /agent — the stable agent-onboarding document: plain-text markdown written FOR an AI agent
 * that was told "set up Topos for us". Deliberately a STATIC module-level constant (no request
 * input, no `.server` import reachable from root.tsx — the install.ts posture): the doc teaches
 * the hosted addresses and the self-host alternative, and stays byte-identical on every fetch.
 * Served as text/plain so a browser renders it readable instead of downloading.
 */

const AGENT_DOC = `# Topos setup — instructions for an AI agent

You were asked to set up Topos. Topos distributes a team's agent skills — bundles of
instructions, scripts, and reference docs — and keeps them current wherever agents work.
An agent gets what the MANIFESTS covering its working directory ask for, filtered by what its
person's workspace seats allow: a project's committed \`topos.toml\` supplies the project's
skills; each person's per-workspace profile (server-stored, web-editable) supplies their
personal set. Logging in is the acceptance: from then on delivery is silent and automatic,
npm-style. Work non-interactively; every command below prints what to do next.

## First: do you have a workspace address?

A workspace address looks like \`https://topos.sh/<workspace>\` — or a self-hosted origin such
as \`https://topos.example.com\`. Invite emails carry it, and members can read it off the
workspace dashboard. If you were not given one, ask your human for it.

**No workspace exists yet?** Your human creates one in a browser at https://topos.sh/new —
or self-hosts the server (see the Self-hosting section of the README at
https://github.com/topos-sh/topos). Come back with the address, then continue below.

## 1. Install the CLI

Check whether it is already installed: \`topos --version\`. If missing, install it (no sudo —
the binary lands in \`~/.local/bin\`; the installer verifies the release checksum and refuses
on a mismatch):

    curl -fsSL https://topos.sh/install | sh

macOS (Apple Silicon and Intel) and Linux (x86_64 and arm64); on Windows, run inside WSL2.
Make sure \`~/.local/bin\` is on PATH before the next step.

## 2. Log in

    topos login <workspace-address>

This starts a browser approval and waits: it prints an approval URL and a short code. Show
that URL to your human — they open it, sign in, check the code matches, and approve the
login. You cannot complete the approval yourself. The code expires after 15 minutes;
re-running the command resumes or restarts the ceremony, and \`--wait\` blocks in one command
until the approval settles.

Approval mints a SESSION — this machine logged into that ONE workspace (another workspace is
another \`topos login\`). The login states what connecting delivers; your profile's skills
then land and stay current automatically — a session-start trigger runs
\`topos update --quiet\` wherever you work.

## 3. Verify

    topos status        # sessions, the resolved skill set and which manifest asked for each
    topos update        # reconcile this directory's agents against the manifests now

## Driving topos afterwards

- \`topos add <skill>\` in a project writes the project's \`topos.toml\` (created on first
  add; commit it — teammates' agents pick the skills up from the file). \`topos add -g\`
  writes your per-workspace profile instead — your personal set, on every machine you log
  in to. \`topos remove\` is the inverse; removing something a broader layer provides records
  an exclude line, and the receipt says so.
- References are shape-determined: \`code-review\` (a skill in your workspaces),
  \`@acme/code-review\` (workspace-qualified), \`@acme/channels/backend\` (a curated set),
  \`vercel-labs/skills\` (GitHub, pinned by default), \`./my-skill\` (a local folder).
- \`topos publish ./my-skill\` shares a skill to the workspace and hands it governance: the
  catalog entry is created (or a proposal opens, where review is required) and your manifest
  entry is rewritten to the workspace reference.
- Add \`--json\` to any verb for exactly one machine-readable envelope — never a prompt.
- On exit 1, the envelope's \`error\` carries \`next_actions\` naming the fix — run it, then
  retry once.
- The \`topos\` skill that lands with your first session teaches the full verb surface,
  including how to propose improvements back to the team.
`;

export async function loader(): Promise<Response> {
  return new Response(AGENT_DOC, {
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
