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
- **The MCP gateway is the SECOND internal lane, and it is OPTIONAL.** Where a deployment runs the
  gateway service, MCP sign-ins live there (never here): `app/lib/gateway/client.server.ts` is its
  one transport — `gatewayFetch` + a route allowlist, the custody transport's twin, same shared
  bearer, same boundary-gate treatment — with three typed wrappers in `credentials.server.ts`
  (begin an authorize walk · store a pasted secret · revoke) and one in `tools.server.ts` (ask the
  server for its tool list again; the gateway probes it automatically the moment a sign-in lands,
  so this is for a list that has since changed). This tier reads the gateway's
  non-secret rows through a second read-only mirror (`app/lib/db/schema.gateway.ts`: credential
  METADATA, observed tools, usage events — the ciphertext table has no grant and no mirror), and
  owns the tool policy itself (`web.mcp_tool_policy` + `mcp_tool_selection`, hanging off the
  connection). ONE policy state is refused rather than written: `selected` with nothing selected
  means every tool off, which is switching a server off and never what an empty checklist meant —
  so the checklist renders STARTING FULL (narrowing is unchecking; a standing selection is
  preserved) and the write answers `empty_selection`. Three env vars, all optional and none
  defaulted: `GATEWAY_INTERNAL_URL` +
  `GATEWAY_INTERNAL_TOKEN` (paired — half a lane refuses at parse time) arm the lane and the
  server-page sections; `GATEWAY_PUBLIC_URL` arms GATEWAY DELIVERY: where set, each MCP row is
  ROUTED per delivery (`deliveryFor` in `queries.lane.server.ts`; the rewrite itself is
  `app/lib/gateway/delivery.server.ts` — one remote at the gateway plus
  `_meta["sh.topos/gateway"]`). Routing is three layers, first answer wins: the workspace switch
  (`workspace.mcp_gateway`, off = direct for everything) · the connection's owner mandate
  (`bundle_mcp.gateway_policy`: 'direct' = direct for everyone, 'required' = gateway for
  everyone — a machine that cannot place gateway entries gets NO row, never a quiet direct
  fallback; both mandates are ALSO enforced in the gateway's own resolver) · NULL = Auto, where
  the member's own opt-out row (`web.mcp_gateway_optout`) and the standing sign-in decide — a
  sign-in server keeps its own address until a credential stands at the gateway (read via the
  metadata mirror; a DELIBERATE reversal of the old "delivery never reads sign-in state" stance),
  and an `auth_mode = 'none'` server routes at once. Unset, every one of those surfaces is
  simply absent and delivery carries the stored document — the whole rollback is clearing a
  variable.
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
  (`bundle_upstream`/`version_upstream`), the MCP SERVER CATALOG (`mcp_server` +
  `mcp_server_revision` + `bundle_mcp` — see the MCP lane below), notices, proposals + comments,
  op receipts, `audit_event`, `mail_event`.
- **The DAL** (`app/lib/db/queries*.server.ts`) is the one sanctioned door to `web` AND the
  read-only `plane` custody mirror. Every function REQUIRES a branded actor as its first
  argument; mutating ops emit their audit row in the SAME transaction. One named exception: the
  mail transport's metadata-only `mail_event` send log is a system write with no actor. Policy is
  TypeScript here — no guarded SQL functions, no plane row-writes.
- **Publishing a BRAND-NEW bundle happens once** (`app/lib/api/genesis.server.ts`): the session
  lane's publish and add-from-GitHub run one sequence — the vault call → ONE transaction holding
  the registration and whatever that door adds (upstream rows, an import audit line, the op
  receipt). A kind whose bundles are NOT files refuses there, ahead of every cap and every custody
  call. The DESTINATION is a required argument each door states outright, because where a new
  bundle reaches is the door's ruling, not the kind's.
- **Auth guards fail closed** (`app/lib/auth/guards.server.ts` — the only minters of branded
  actors: `requireSession → requireMember → requireWorkspaceOwner`/`requireReviewer`,
  `requireSessionActor`); the brand symbol is module-private. **Misses render 404, never 403.**

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
- **ONE RECORD PER KIND** (`app/lib/bundle-base.ts` — `BUNDLE_KINDS`): everything the product
  knows about a kind of bundle, written once. Base · noun · section label · route param · its own
  way in · where its WEB CREATION PAGE rests when no channel is picked · whether its bytes face a
  content gate ·
  its rail and list marks. The route table BUILDS its per-kind mounts from it (`skills/:skill`,
  `mcp/:server` — the face + its six subpages once per kind; every mount past the first carries
  an explicit route id from the same record, and the MOUNT is told apart by param NAME, never by
  route id). The rail's one section component, the dashboard index, the breadcrumb registrations
  and the genesis publish all read the same records, so a kind cannot be half-present. Each mount
  is kind-FENCED: a member on the wrong base is redirected to the canonical path
  (`bundle-base.server.ts`), everyone else already met the 404. Channel curation deliberately
  carries both kinds in one set and labels them.
- **Machine discovery** (origin-rooted in both tenancies): `/llms.txt`,
  `/.well-known/agent-skills/index.json` (+ `/.well-known/skills/` alias) serving the built-in
  `topos` skill; its sha256 is computed in `agent-skills.server.ts` from the same bytes served —
  the ONE sanctioned digest in this tier.
- **`/docs`:** MDX source at the REPO root (`docs/`), compiled at generate time into a COMMITTED
  module (`app/lib/docs/content.generated.server.ts`) by `scripts/gen-docs.mjs` (own
  remark/rehype chain; closed component set; nav.json must match disk; the CLI reference page
  splices the generated `docs/cli.md`). Edit the MDX → `bun run gen:docs` → commit; `check:docs`
  fails on drift. `/docs/<page>.md` is the plain-markdown twin; `/docs/llms.txt` the index.
- **The MCP lane — a CATALOG, not bytes:** a `kind: 'mcp'` bundle holds no files. It NAMES a
  server (`web.bundle_mcp`, one connection per server per workspace, unique-enforced), the server
  holds its version history (`web.mcp_server` + `mcp_server_revision`, append-only), and delivery
  resolves the document from there — a pin to a promoted revision, else the server's `current`.
  Maturity is the POINTER, not a column: `current_revision_id` names the one people receive, every
  other revision is a proposal or history (told apart by `seq` against the current), and
  `dismissed_at` is the one terminal state staff can put a proposal in. ONE storage model for
  every server, told apart by `workspace_id`: public rows (`NULL`) are the GENERIC catalog, a
  workspace's own rows are private and exported nowhere. `auth_mode` is VERIFIED truth (`null` =
  nobody established it, never rendered as `none`; a `manual` row is promoted only with the one
  line saying what a person must do first). The whole data layer is
  `app/lib/db/queries.mcp-catalog.server.ts`, where `KNOWN_MCP_SCHEMA_VERSIONS` is the fail-closed
  `$schema` vocabulary and `promoteMcpRevisionInTx` is the ONE function every pointer move goes
  through (an owner's save, the file sync, a staff promote) — the seam a later gateway-compat gate
  sits behind.
  **The generic catalog is FILE-SOURCED** (`app/lib/mcp/catalog.json`, a top-level array of
  registry-format `server.json` objects; editorial rides `_meta["sh.topos/catalog"]`). A boot
  sync (`app/lib/db/mcp-catalog-sync.server.ts`, run before the first request) reconciles the file
  into public rows: it upserts by `name`, computes each delivered document, dedupes by content, and
  — while a server is untouched by staff (`manually_curated = false`) — advances its `current` to
  each new file version; once staff edit or promote it, a file version lands as a non-current
  PROPOSAL instead. It never deletes, never clobbers a live `current`, and never re-proposes a
  document staff dismissed; a settled catalog no-ops.
  **The document gate** (`app/lib/mcp/validate.server.ts`) still decides what may enter the
  catalog at every door: SOMETHING TO RUN (a remote `streamable-http` endpoint over https, or
  `packages[]`, or both; neither refuses), every package PINNED to one immutable thing (exact
  version · OCI tag-or-digest · `fileSha256`; `latest` and version ranges refuse), no
  `{placeholder}` in the endpoint, no credential (the shapes live in this app's own
  `tests/fixtures/mcp/`, compiled into `secret-patterns.generated.ts`; a package env/flag may
  NAME a credential slot — the machine fills it — but never arrive with the value in it).
  `registryType` is open-world: what a given machine can set up is the client's answer at render
  time, not a refusal for everyone.
  **Bytes are refused for the kind**: a publish or a proposal naming it answers `KIND_HAS_NO_FILES`
  before any custody call, and the file pages (history · proposals · a version's tree) mount for
  file kinds only.
  **The advisory probe** (`app/lib/mcp/probe.server.ts`) runs strictly AFTER the revision is
  durable, over the SSRF-guarded transport (POST `initialize`, bounded JSON/SSE read, no redirect
  followed): 401/403 = sign-in-required and healthy (outranks the body) · silence/5xx/429 = not
  responding, never a protocol verdict · a private or unresolvable address = "not verifiable from
  cloud" (NEUTRAL — internal servers are first-class). It writes onto the revision and can never
  block or slow the act it follows; a revision with no answer reads "not checked yet".
  **`mcp/new`** offers the catalog and CONNECTS (any member), or — for an owner — takes a registry
  name · an SSRF-guarded URL (the guard DIALS the addresses it vetted, so nothing resolves a
  second time) · a pasted document and writes the workspace's OWN server down. Its destination
  field RESTS ON NO CHANNEL (`NO_CHANNEL`, distinct from the default-channel `null`, read from the
  kind's record): adding a server reaches nobody until a channel is chosen, here or later, and
  every channel INCLUDING the default is an ordinary named option. That is the FORM's ruling and
  scoped to it. When a channel IS chosen the page discloses what happened to the REACH: a curated
  channel withholds a member's placement, and the server's face says so.
  **One read lane, its serializer** (`app/lib/mcp/registry-api.server.ts`, the official read-API
  shape; curation rides `_meta["sh.topos/catalog"]` and never the official registry's namespace):
  `…/registry/v0.1/servers[/{name}/versions[/{version}]]` serves what THIS WORKSPACE runs,
  member-gated by cookie OR bearer, uniform-404 otherwise. There is NO public catalog feed — the
  generic catalog is the committed file, and the boot sync is what makes it authoritative.
  A boot backfill (`app/lib/db/mcp-backfill.server.ts`) connects every MCP bundle written before
  the catalog existed — the name lives in the vault's bytes, so it runs where bytes are readable,
  before the first request; what it cannot read is NAMED in the log, never skipped silently.
  **The lane a MACHINE shares one through** (`api.v1.mcp-servers.ts`): a registry name or an https
  link goes over the wire and every ruling is made here, so the terminal and the form answer alike.
  A name the catalog carries is a CONNECTION (any member; asking twice answers with the bundle
  that already stands); anything else fetches the document over the guarded transport and writes
  the workspace's OWN server down, which is an owner's act. Either way the connection rests on NO
  CHANNEL, the same ruling the form keeps.
- **Signed-in:** dashboard (skills and MCP servers as separate sections) · bundle browser +
  lifecycle ceremonies (a skill's tabs: Current · Proposals · History · owner Settings; a server's
  face carries the server, what this workspace receives, and its revisions instead, and mounts
  Settings alone beside it — plus, where a gateway is deployed, the three gateway sections
  (Sign-in · Tools · Usage) and the manual tier's own `mcp/:server/connect` page;
  `skills/import` add-from-GitHub + the Upstream panel) · the rendered
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
crypto outside the named carve-out; each internal lane's URL/token/`fetch(`/`gatewayFetch(` or
`vaultFetch(` confined to its one transport, and `/internal/v1` to those two directories;
`.server` discipline; route-guard allowlist; DAL confinement; zero client env),
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
