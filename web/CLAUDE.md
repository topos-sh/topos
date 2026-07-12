# `web/` — the product web app (TypeScript / React Router 8 on bun)

The signed-in surface for Topos: a workspace dashboard, the skill browser, the rendered review UI
(unified diff + Approve/Reject + comments), the verification page, the create/join flows, and the
ADMIN surfaces — the roster page in full (invite / role change / remove / self-serve leave), the
skill lifecycle ceremonies (archive / unarchive / delete / purge / rename-with-redirect), channel
existence-admin + history, the workspace policy page (review default · invite policy · staleness
window), the fleet page (staleness + the named blind spots: detached copies, removed-upstream
rows, stale devices), the "your devices" self-service list, and the first-run claim. It renders
state read from the vault over HTTP and its own Postgres schema. It holds **no signing key,
computes no digest, and initiates no device-signed write** — publishing stays on the enrolled
device; this app is surfaces.

**Step-up.** Every admin ceremony re-authenticates immediately before the act: the person
re-enters their password inside the ceremony form (`app/lib/auth/step-up.server.ts`, verified with
better-auth's own hasher against the SESSION's account — never a form-supplied identity; its own
rate belt, armed by `APP_ENV` like the sign-in limiter), and the destructive ceremonies (delete a
skill, purge a version, delete a channel) additionally require typing the resource's exact name,
compared against server-re-read state. Deliberately STATELESS — no sudo window. Every attempt
lands an `admin_event` audit row, refused step-ups included. **The grade of a ceremony and the
reach of its act stay matched IN THE DATABASE**, never by convention: the account page's
step-up-LESS device sign-out passes `topos_revoke_device`'s self-only flag, so it cannot reach the
owner arm that the fleet page's step-up ceremony earns. **Known limit (v1):** step-up IS the
password rung — a deployment configured with only magic-link or social sign-in has no password to
re-enter and every ceremony would refuse; a second factor for password-less deployments is later
work.

**Resource addresses + the protocol card.** `/{workspace}`, `/{workspace}/channels/{name}`, and
`/{workspace}/skills/{name}` are the shareable addresses, plus a root catch-all: a non-browser
fetcher gets the CONSTANT protocol card (`app/lib/card.server.ts` — the vault card's negotiation
mirrored; served whole from route middleware, byte-identical on every path, `api_base_url` = the
follow base); an anonymous browser gets one constant teaser page; a signed-in member is resolved
through their own confirmed seats into the workspace surface; everyone else gets the house 404.
No face is an existence oracle.

**Stack.** React Router 8 in framework mode (SSR, Vite, bun) · React 19 · Better Auth on Drizzle /
Postgres · Tailwind 4 with the Klein token set (`DESIGN.md` is the source of truth; the
`--color-*` table in `app/app.css @theme` is kept identical to it by `check:tokens`) · Martian Mono +
IBM Plex Sans/Mono self-hosted via `@fontsource` · `@pierre/diffs` behind a sanitizing wrapper · zod ·
Biome · Vitest + Playwright. Blocking SSR — every page ships one complete document, no visible loading
states on the signed-in path; every vault/DB read is per-request fresh.

**Composition — four additive seams.** The package (`@topos/web`) exports `./routes`, `./nav`,
`./entitlements`, and `./auth-config`. A deployment's `app/routes.ts` is one line — `ossRoutes()`; a
downstream superset build composes `[...ossRoutes({ dir }), ...ownRoutes]` and appends its own nav
entries, entitlements provider, and auth rungs. Composition is **additive-only**: a superset appends,
never patches or shadows an OSS entry. The route modules type their args with the generic
`LoaderFunctionArgs`/`ActionFunctionArgs` (never `./+types/*`) so the table works unchanged from another
app directory.

**Auth + authorization (fail-closed).** The OSS default rung is **email+password with zero delivery
dependency** — a self-hosted team signs in with no SMTP or OAuth. A session is evidence, never
authority: every admission resolves against the **directory roster** at request time.
`app/lib/auth/guards.server.ts` is the only place that mints **branded actors** (`requireSession` →
`requireMember` → `requireWorkspaceOwner`/`requireReviewer`); the brand symbol is module-private, so a
loader that skipped its guard cannot construct one. Every function in the DAL
(`app/lib/db/queries.server.ts`) requires an actor as its first argument, and workspace-scoped reads
take their scope from the actor — a wrong-scope actor fails loudly. **Misses render 404, never 403.**

**Data path split.** Row **reads** (roster, catalog, policy, memberships) are direct Drizzle SELECTs on
the read-only `plane` schema. Row **writes** go through the directory's guarded `topos_*` SQL functions
(e.g. `topos_invite`) — policy logic lives in the database, written once. **Byte/pointer** ops (current,
versions, bundles, proposals, review approve/reject, revert, workspace create, session approves) ride
the vault's **internal session lane** through the one transport, `app/lib/plane/client.server.ts`
(`vaultFetch` + a runtime route allowlist). The app keeps its **own** `web` schema (Better Auth tables +
the policy audit trail + proposal comments); migrations run at first request and via `bun run db:migrate`.

**Boundary gates** (`bun run check`, all in CI): `check:tokens` (DESIGN.md ↔ `app.css` color drift),
`check:boundary` (no crypto/digest/signature anywhere; the vault URL + `fetch(` confined to the one
transport; no device-signed write path spelled; server modules carry the `.server` suffix; every
data-reading route guards or is on the sessionless allowlist; the raw DB surface stays inside the DAL;
zero client env), `check:contract` (`app/lib/plane/contract/schema.d.ts` regenerated from the repo's
committed OpenAPI, drift-gated), and `check:bundle` (post-build byte-scan of `build/client` for server
secret names).

**Run it.**

```sh
bun install
bun run dev          # needs DATABASE_URL + PLANE_INTERNAL_URL/PLANE_INTERNAL_TOKEN + BETTER_AUTH_SECRET/URL
bun run db:migrate   # DATABASE_URL=… apply the web-schema migrations
bun test             # vitest unit
bun run test:e2e     # playwright
bun run check        # biome + the boundary/token/contract gates + typecheck
```

`AGENTS.md` symlinks to this file.
