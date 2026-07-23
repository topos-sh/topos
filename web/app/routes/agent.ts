/**
 * GET /agent — the stable agent-onboarding document: plain-text markdown written FOR an AI agent
 * that was told "set up Topos for us". Deliberately a STATIC module-level constant (no request
 * input, no `.server` import reachable from root.tsx — the install.ts posture): the doc teaches
 * the hosted addresses and the self-host alternative, and stays byte-identical on every fetch.
 * Served as text/plain so a browser renders it readable instead of downloading.
 */

const AGENT_DOC = `# Topos setup — instructions for an AI agent

You were asked to set up Topos. Topos distributes a team's agent skills — bundles of
instructions, scripts, and reference docs — and keeps them current on every enrolled machine.
Work non-interactively; every command below prints what to do next. Nothing lands silently:
skills install only after their contents are disclosed and consented to with an explicit
\`--yes\`.

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

## 2. Enroll this machine

    topos follow <workspace-address>

This starts a browser approval and waits: it prints an approval URL and a short code. Show
that URL to your human — they open it, sign in, check the code matches, and approve this
device. You cannot complete the approval yourself. The code expires after 15 minutes;
re-running the command resumes or restarts the ceremony, and \`--wait\` blocks in one command
until the approval settles.

Approval enrolls the device. The command then DESCRIBES what the workspace offers and prints
a paste-ready next action ending in \`--yes\`:

    topos follow <workspace-address> --yes

Run it to land the offered skills. From then on they stay current automatically — a
session-start trigger runs \`topos update --quiet\` on this machine.

## 3. Verify

    topos status        # enrollment, workspaces, followed skills, trigger arm state
    topos list --json   # every managed skill, with source and status

## Driving topos afterwards

- Add \`--json\` to any verb for exactly one machine-readable envelope — never a prompt.
- topos asks first when an act reaches the team, loses local work, or trusts something new —
  those verbs are two-phase: bare DESCRIBES (nothing written); \`--yes\` applies. Everything
  else applies immediately and prints its undo.
- On exit 1, the envelope's \`error\` carries \`next_actions\` naming the fix — run it, then
  retry once.
- The \`topos\` skill that lands with enrollment teaches the full verb surface, including how
  to publish improvements back to the team.
`;

export async function loader(): Promise<Response> {
  return new Response(AGENT_DOC, {
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
