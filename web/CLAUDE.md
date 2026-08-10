# `web/` — the product web app (TypeScript / React Router 8 on bun)

**THE ONE PUBLIC SURFACE.** This app is everything the world reaches: the signed-in pages, the
shareable resource addresses, the protocol card, `/docs`, AND the `/api/v1` session lane. It is
the **authority for identity and the whole directory** (its own Postgres schema `web`); the vault
(the Rust plane) is PURE BYTE CUSTODY behind it, internal-network-only, with this app as its one
caller.

## Architecture

- **The session lane terminates here.** Every `/api/v1/…` path is answered in this tier — Drizzle
  queries on schema `web` behind the session guard (Bearer → live `web.cli_session` row → person →
  seat; hashes computed IN Postgres — this tier holds zero crypto). A **session** is user ×
  workspace × installation, minted by the login flow, severable by both sides, DELETED never
  tombstoned. The guard requires an ACTIVE session; exactly TWO routes answer typed for a PENDING
  one (`GET …/me`, `GET …/delivery` — shape-complete and empty); every other miss folds into ONE
  uniform wire 404 (`api.v1.$.ts` catch-all — no path echo, no existence oracle); a rate belt
  wears the 429. Only the byte/pointer ops of a publish-family verb leave this tier: through the
  ONE custody transport (`app/lib/plane/client.server.ts` — `vaultFetch` + a route allowlist) to
  the vault's `/internal/v1` lane, authenticated by the shared internal bearer alone.
- **One identity.** A person is a `user.id` (Better Auth); email is a mutable login attribute —
  NOTHING authorizes by email equality (`check:email` enforces it). Human display is one rule
  written twice in lockstep: `app/lib/person-display.ts` (TS) +
  `app/lib/db/person-display.server.ts` (SQL).
- **The directory** (schema `web`): Better Auth tables, seats, `cli_session` + login-flow rows,
  invitations, the bundle catalog (a `kind` tag — a CLOSED vocabulary here, and clients branch on
  it to pick a bundle's delivery mechanics), channels (pure
  curated bundle sets, incl. the implicit default `everyone`), **assignment** + **decline** rows
  (the whole delivery predicate: assigned to you or to everyone, minus your declines — one
  positive row per provenance with a `self` flag, one negative per person per bundle; the
  baseline everyone-row is unassignable at the data layer), upstream provenance
  (`bundle_upstream`/`version_upstream`), notices, proposals + comments, op receipts,
  `audit_event`, `mail_event`.
- **The DAL** (`app/lib/db/queries*.server.ts`) is the one sanctioned door to `web` AND the
  read-only `plane` custody mirror. Every function REQUIRES a branded actor as its first
  argument; mutating ops emit their audit row in the SAME transaction. One named exception: the
  mail transport's metadata-only `mail_event` send log is a system write with no actor. Policy is
  TypeScript here — no guarded SQL functions, no plane row-writes.
- **Auth guards fail closed** (`app/lib/auth/guards.server.ts` — the only minters of branded
  actors: `requireSession → requireMember → requireWorkspaceOwner`/`requireReviewer`,
  `requireDeviceActor`); the brand symbol is module-private. **Misses render 404, never 403.**

## The ceremonies (`app/lib/db/identity.server.ts` — FOR-UPDATE-fenced, audit inside)

- **Boot:** the `web` schema migrates EAGERLY at process start (top-level await in
  `entry.server.tsx`) — a virgin database serves its first request 200. **First boot** mints the
  workspace + prints the claim link (single tenancy); `claim.tsx` seats the first owner.
- **The login flow** (`verify.tsx` + `api.v1.login-authorize|login-token|login-connect`):
  RFC-8628-shaped, WORKSPACE-LESS at the start (an optional `preselect` slug rides
  shape-checked, display-only); `/verify` is a POST code-lookup then the pick-or-create card —
  the signed-in approver CHOOSES the workspace (a seat, a pending invitation, or — multi, when
  creation is open — a workspace born in the same fence), and approval records consent + the
  choice only. THE SESSION MINTS AT THE CLI'S EXCHANGE — the first `login/token` poll that
  finds the flow approved (seat + knob re-read at collection; an approved flow never polled
  answers `expired` past its TTL) — so approval from any browser on any device completes.
  Two bindings: **device** (typed code — the typing binds approver to asker; the card never
  pre-arms) and **loopback** (RFC 8252-shaped; the 127.0.0.1 redirect is a pure accelerator —
  state + outcome only, no secret). `login/connect` is the lane-side second connect: any live
  ACTIVE session's bearer + a seat in the target workspace mints a further session, no
  browser. Born-status rule, written once: an owner's approval → `active`; else the
  workspace's `session_approval` knob decides.
- **Invitations** (`invite-redeem.tsx` + `identity.server.ts`): mailed single-use hash-stored
  token, 7-day lapse, re-invite supersedes; the accept ceremony is email-BOUND (one-click as the
  invited address · passwordless account mint born verified · switch page for the wrong account ·
  one constant dead-token page); optional ONE first-destination hint lands as an assignment.
  Enrolled devices accept lane-side (`POST /api/v1/invitations/accept`).
- **Ceremonies confirm, they don't re-authenticate** (`ceremony.server.ts` +
  `app/components/confirm.tsx`): destructive acts type the CURRENT name (`requireTypedName`);
  cross-person acts wear the in-place `<ConfirmButton>` two-step; settings saves are plain.
  Every attempt lands an `admin_event` row.
- **Registration is composition-owned** (`registration.server.ts`, wired as Better Auth's
  `user.create.before` hook). OSS default **`gated`**: claim ceremony, pending invitation on
  armed SMTP, or the single-tenancy-only `open` knob; everything else gets one constant refusal.
  A superset may pass `open` — sign-up alone still grants no seat.
- **Recovery:** reset mail when SMTP is armed; a mail-less solo owner runs the box-side
  `scripts/mint-recovery-code.mjs`.

## Surfaces

- **Two URL grammars, one route table** (`app/lib/ws-path.ts` + `ws-url.server.ts`): tenancy
  **single** (OSS default — origin-rooted, the install IS its workspace) or **multi** (a superset
  mounts the same modules under the `/:ws` name slug). No page hard-codes the grammar; the opaque
  `workspace.id` never appears in a URL. `/new` (self-serve workspace creation) mounts in multi
  only; reserved slugs (`app/topos-web/segments.ts` ∪ composition's list) refuse byte-identically
  to taken names.
- **Faces + the card:** workspace root / channel / skill / MCP server are each ONE route under
  `face-shell.tsx`. A non-browser document fetch gets the CONSTANT protocol card
  (`card.server.ts`, byte-identical on every path; `api_base_url` = this origin's `/api`). Bundle
  and channel faces are members-only — anonymous/non-member gets the house 404,
  existence-blind. Uniform miss surface: the root ErrorBoundary → `error-screen.tsx` (no
  `error.data`, path, or stack).
- **Two bases over ONE set of bundle pages** (`app/lib/bundle-base.ts`): the catalog `kind`
  decides where a bundle is addressed — `skills/:skill` or `mcp/:server` — and the face + its
  five subpages mount under BOTH (the MCP mount carries explicit route ids; the MOUNT is told
  apart by param NAME, never by route id). Each mount is kind-FENCED: a member on the wrong base
  is redirected to the canonical path (`bundle-base.server.ts`), everyone else already met the
  404. The sidebar, the dashboard index and the breadcrumb registry read the same mapping;
  channel curation deliberately carries both kinds in one set and labels them.
- **Machine discovery** (origin-rooted in both tenancies): `/llms.txt`,
  `/.well-known/agent-skills/index.json` (+ `/.well-known/skills/` alias) serving the built-in
  `topos` skill; its sha256 is computed in `agent-skills.server.ts` from the same bytes served —
  the ONE sanctioned digest in this tier.
- **`/docs`:** MDX source at the REPO root (`docs/`), compiled at generate time into a COMMITTED
  module (`app/lib/docs/content.generated.server.ts`) by `scripts/gen-docs.mjs` (own
  remark/rehype chain; closed component set; nav.json must match disk; the CLI reference page
  splices the generated `docs/cli.md`). Edit the MDX → `bun run gen:docs` → commit; `check:docs`
  fails on drift. `/docs/<page>.md` is the plain-markdown twin; `/docs/llms.txt` the index.
- **The MCP lane:** `kind: 'mcp'` bundles carry `server.json` (required; an optional `README.md`
  is the only allowed sibling — both files' bytes run the credential scan), gated by
  `app/lib/mcp/` — a remote `streamable-http` endpoint over https, no `{placeholder}`, no
  credential (the shapes live in the repo-root `tests/fixtures/mcp/`, compiled into
  `secret-patterns.generated.ts`), and an embedded registry name no other bundle here claims.
  Both publish doors run the same gate before any custody call: the session lane's
  publish/propose and the `mcp/new` page — the MCP section's own way in (built-in list ·
  registry name · SSRF-guarded URL — the guard DIALS the addresses it vetted (`https.request`
  with our own `lookup`, so nothing resolves a second time) · paste). The built-in list is
  committed data the loader ships whole, documents included, so picking one opens its confirm
  dialog with no round trip; the publish re-derives those bytes from the list. The three typed
  sources keep their server-side preview. The embedded name is re-claimed under one per-workspace
  lock by every door that MOVES a pointer onto an mcp version — publish, re-publish, unarchive,
  review approve, revert; the wire doors word that refusal once (`mcpNameTakenRefusal`), pointing
  at the bundle already holding the name. `mcp/new` mints a NEW bundle per import and its
  destination field RESTS ON NO CHANNEL — an import lands catalog-only and reaches nobody until a
  channel is chosen, here or later (`NO_CHANNEL`, the genesis destination distinct from the
  default-channel `null`; every channel INCLUDING the default is an ordinary named option, so no
  empty value stands in for one). When a channel IS chosen the page discloses what the publish did
  to the REACH: a curated channel withholds a member's placement, and the bundle face says so.
  `…/registry/v0.1/servers[/{name}/versions[/latest]]` serves the workspace's catalog in the
  official read-API shape, member-gated by cookie OR bearer, uniform-404 otherwise.
- **Signed-in:** dashboard (skills and MCP servers as separate sections) · bundle browser +
  lifecycle ceremonies (tabs: Current · Proposals · History · owner Settings; `skills/import`
  add-from-GitHub + the Upstream panel) · the rendered
  review UI (diff, approve/reject, comments, revert) · `/profile` (Mine grouped by provenance +
  Library) · `/visibility` · channel pages (tabs: Skills/curation · Members · History ·
  Settings) · roster · workspace Settings (General policy knobs · whole-catalog export ·
  Sessions incl. the pending-approval queue · Archive) · the account Your-sessions list ·
  claim. The left panel is one shadcn sidebar (`app/components/shell/`), loader-derived;
  breadcrumbs render from one central registry.
- **Mail — ONE transport** (`app/lib/mail/transport.server.ts`, the only module allowed an SMTP
  client; the five `TOPOS_MAIL_SMTP_*` vars arm it all-or-nothing; armed mail is the identity
  rung for multi-user installs). Send failures are coarse — no error ever echoes body, recipient,
  or relay response; every attempt lands one metadata-only `mail_event` row.

## Composition — four additive seams

`@topos/web` exports `./routes`, `./nav`, `./entitlements`, `./auth-config`. A superset composes
`[...ossRoutes({ dir, tenancy }), ...ownRoutes]` and appends nav/entitlements/auth rungs —
**additive-only**. The composition root also owns the registration policy and
`reservedWorkspaceNames`; OSS statics live in the dev-free `app/topos-web/segments.ts`.

## Stack + gates

React Router 8 framework mode (SSR, Vite, bun) · React 19 · Better Auth on Drizzle/Postgres ·
Tailwind 4 with the Klein token set (`DESIGN.md` is the source of truth) · self-hosted fonts ·
`@pierre/diffs` behind a sanitizing wrapper · zod · Biome · Vitest + Playwright. Blocking SSR;
every DB/vault read per-request fresh.

Gates (`bun run check`, all in CI): `check:tokens` (DESIGN.md ↔ `app.css`), `check:boundary` (no
crypto outside the named carve-out; vault URL/`fetch(`/`/internal/v1` confined to the one
transport; `.server` discipline; route-guard allowlist; DAL confinement; zero client env),
`check:email`, `check:contract` (OpenAPI-generated `schema.d.ts`), `check:docs`,
`check:mcp-patterns` (the credential shapes ↔ the generated module), `check:bundle`
(post-build client-bundle byte-scan). Repo-level `scripts/check-db-grants.sh` proves the
cross-lane grants by logging in as each role.

```sh
bun install
bun run dev          # needs DATABASE_URL + PLANE_INTERNAL_URL/PLANE_INTERNAL_TOKEN + BETTER_AUTH_SECRET/URL
bun run db:migrate
bun run test         # vitest unit — NOT `bun test` (bun's own runner writes snapshots vitest
                     # then reports as obsolete; CI fails on those)
bun run test:e2e     # playwright — E2E_{PLANE,APP,SMTP}_PORT override for side-by-side checkouts
bun run gen:docs     # recompile /docs from the repo's docs/*.mdx into the committed module
bun run gen:mcp-patterns  # recompile the credential shapes from tests/fixtures/mcp/
bun run check        # biome + the gates above + typecheck
```

`AGENTS.md` symlinks to this file.
