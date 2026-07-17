# `web/` — the product web app (TypeScript / React Router 8 on bun)

**THE ONE PUBLIC SURFACE.** This app is everything the world reaches: the signed-in pages below, the
shareable resource addresses, AND the device API — `/api/v1/…` is served here. Since the identity
model landed, this tier is the **authority for identity and the whole directory**, in its own Postgres
schema `web`; the vault (the Rust plane) is PURE BYTE CUSTODY behind it, internal-network-only, and the
app is its one caller.

**The device lane terminates here.** Every `/api/v1/…` path is answered in this tier — there is no splat
forwarder to the vault anymore. The row ops (delivery · the fleet report · me/channels/reach ·
subscriptions/follows · curation · exclusions · protection · notices ack · invitations) are Drizzle
queries against this app's OWN `web` schema, behind the device-credential guard (`requireDeviceActor` —
the presented `Authorization: Bearer` resolved credential → device → person → seat, the hash computed IN
Postgres, so this tier still holds zero crypto). The **byte/pointer** ops of a publish-family verb
(ingest, the `current` CAS, revert, purge, the verified object/version/log reads) are the only things
that leave this tier: they go through the ONE custody transport, `app/lib/plane/client.server.ts`
(`vaultFetch` + a runtime route allowlist), to the vault's internal `/internal/v1` custody lane —
authenticated by the shared internal bearer alone (the vault is identity-free; authorization already
happened here). Every `/api/v1` miss answers the ONE uniform wire 404 (`api.v1.$.ts` catch-all — no path
echo, no existence oracle); a rate belt wears the frozen 429.

**One identity, app-owned directory.** There is ONE identity: a person's `user.id` (Better Auth). Email
is a login name and a mutable attribute — NOTHING authorizes by email equality. A person's
human-facing DISPLAY is one rule, written twice in lockstep: the profile name, else the email (a
magic-link sign-up is born with `name = ''`, and a blank never surfaces as a label) —
`app/lib/person-display.ts` for TS compositions, `app/lib/db/person-display.server.ts` for the SQL
selects (member lists, attribution, the device-lane actor). Every seat, device,
subscription, notice, and audit row references a `user.id`. The whole directory lives in schema `web`:
the Better Auth tables (`user`/`session`/`account`/`verification`), **seats** (workspace membership +
role), devices + the device-auth flow rows, invitations, the bundle catalog (each row carrying a `kind`
tag — `'skill'` today — displayed, never branched on), channels (incl. the implicit default `everyone`
channel with per-person `channel_optout`), subscriptions (ONE `bundle_subscription` stance row per
person per bundle), detachment records, notices with read-state, proposals + comments, op receipts, and
the `audit_event` trail. The DATA ACCESS LAYER (`app/lib/db/queries*.server.ts`) is the one sanctioned
door to `web` AND the read-only `plane` custody mirror; every function REQUIRES a branded actor as its
first argument, and mutating ops emit their audit row in the SAME transaction. There are NO guarded
`topos_*` SQL functions and no plane row-writes — policy logic is written here, once, in TypeScript with
the role gate carried by the actor's type.

**The identity ceremonies** (`app/lib/db/identity.server.ts` — the concurrency-critical writes, each one
transaction, FOR UPDATE-fenced or single-statement-atomic, audit row inside):
- **Boot** — the `web` schema migrates EAGERLY at process start (a top-level await in
  `entry.server.tsx`; loaders run BEFORE `handleRequest`, so an in-request gate can never protect
  the first request): a virgin database serves its FIRST request 200, and a broken one crashes the
  boot loudly instead of serving unmigrated.
- **First boot** (`ensureSetup`) mints the workspace + its default `everyone` channel on a virgin
  database (first-request-once — it needs the request origin for the printed link), and while
  unclaimed (re)mints the claim code and prints ONE line to the logs
  (`→ Finish setup: <origin>/claim?code=…`; `TOPOS_SETUP_CODE` presets it, `TOPOS_SETUP_LINK_FILE`
  mirrors it to a file). Only the code's SHA-256 is stored.
- **The claim** (`claim.tsx` → `consumeClaim`): one atomic UPDATE consumes the code and seats the first
  **owner** (email + password). Single-use by construction.
- **The gh-style device flow** (`verify.tsx` + `api.v1.device-authorize`/`api.v1.device-token`;
  `startDeviceAuth`/`pollDeviceAuth`/`approveDeviceAuth`): the CLI prints "open `<origin>/verify` and
  enter AB12-CD34" and polls; the signed-in person approves with a **plain accept** — a live session
  plus the explicit approve click IS the whole ceremony (no step-up) — minting the device (owned by
  that person) + its ONE bearer credential (the device code is promoted to the credential — same
  plaintext, same stored hash). The flow row records the workspace ADDRESS SLUG the authorize call
  named; multi tenancy shape-checks that slug only (an unauthenticated start is never a
  workspace-existence oracle — the workspace may be created mid-flow), and approval resolves it under
  the tenancy grammar and requires the approver's SEAT in the resolved workspace, inside the same
  FOR-UPDATE fence — a missing workspace or a seatless approver gets the same uniform refusal an
  expired code does. On a multi-tenant deployment a signed-in approver with zero seats anywhere is
  first woven through workspace creation (`/verify` redirects to `/new` carrying itself as `next` +
  the flow's slug as a `name` prefill). The signed-out loader bounce carries the code as `next`, so a
  password OR a magic-link sign-in both return to finish the approval. Revocation is self-service,
  immediate, and FINAL (a DB trigger refuses any un-revoke).
- **Recovery** (`app/lib/auth/recovery.server.ts` + `scripts/mint-recovery-code.mjs`): reset mail when
  SMTP is armed; a mail-less solo owner runs the one-shot box-side script to print a single-use recovery
  code (machine control is the proof).

Secrets are HASH-STORED, and the hashing happens IN Postgres (`sha256(convert_to(…))`) or inside Better
Auth's own password hasher — this tier generates randomness (the two mints in `identity.server.ts` +
`recovery.server.ts`) but computes no digest.

**Registration is composition-owned** (`app/lib/auth/registration.server.ts`, wired as Better Auth's
`user.create.before` hook so no rung can bypass it; the policy is `composition.registration`). The OSS
default is **`gated`**: a sign-up succeeds only inside the claim ceremony, OR with a pending invitation
on a deployment whose SMTP is armed (the invited seat binds only after the mailbox round-trip, via
`bindInvitedSeats` on `afterEmailVerification` — and only in the invitation's own workspace), OR — in
SINGLE tenancy only — under the one workspace's off-by-default `registration = 'open'` knob (a
workspace-scoped knob never opens a multi-tenant server, so the knob's settings panel and its intent
exist only in single tenancy). Everything else gets ONE constant, non-enumerating refusal. A downstream
composition may return **`open`** instead: every rung then admits sign-up (the magic-link lead becomes
"continue with email", creating an account for a new address) — sign-up alone still grants no seat.

**Self-serve workspace creation (multi tenancy only).** `/new` (`workspace-new.tsx`, mounted only when
`tenancy: "multi"`) is both onboarding and the panel dropdown's "New workspace" form: display name →
an editable address slug with live availability (`?check=` on the same route), then ONE transaction in
`app/lib/db/workspace-create.server.ts` — the workspace row born CLAIMED + its implicit `everyone`
channel + the creator's owner seat + the audit row. A reserved slug (the route table's multi-mode
statics in `app/topos-web/segments.ts` ∪ the future-reserve list ∪ `composition.reservedWorkspaceNames`)
refuses byte-identically to a taken name and is never enumerable through the form; a vitest red-test
locks the segment list to the real route table. The name `topos` is reserved across ALL three name
spaces for the CLI's built-in skill and the product itself: the future-reserve list carries it for
workspace slugs, `CHANNEL_RESERVED` refuses it as a channel name (`bad_name`), and the bundle
catalog mint (`RESERVED_BUNDLE_NAMES` in `queries.custody.server.ts`) treats it as always-taken —
the genesis suffix walks past it (`topos-2`), byte-identical to a collision, no oracle. A seatless signed-in visitor is routed here (`/app` →
`/new`; the `/verify` weave preserves the device code); the dashboard's empty state is the first-skill
card — the same publish-from-your-agent instructions the panel's `+ new` skill dialog shows. In single
tenancy `/new` does not mount (the house 404) and the seatless answer stays the 404.

**Step-up** (`app/lib/auth/step-up.server.ts`). Every admin ceremony (roster mutations, skill lifecycle,
purge, channel existence-admin, policy setters) re-authenticates immediately before the act. The RUNG is
the person's step-up METHOD (`stepUpMethod`, resolved once per ceremony page and carried to the shared
`<StepUpFields>` via a context provider): an account with a password re-enters it (verified with Better
Auth's own hasher against the SESSION's account — never a form-supplied identity), the OSS default since
every account is born with one. A password-LESS account (a magic-link/social deployment) confirms through
the MAIL round-trip instead: `beginStepUpConfirmation` mints a single-use token, stores ONLY its
Postgres-computed hash in Better Auth's `verification` table under a `step-up:<userId>` identifier with a
~10-minute TTL, and mails a link back to the SAME ceremony page carrying `?stepup=<token>`; the submit
then consumes that token in ONE atomic `DELETE … RETURNING`. No password AND no armed mail ⇒ a typed
refusal (set a password or arm SMTP), never a silent dead end. Deliberately STATELESS whichever rung —
no sudo window; the token authorizes exactly the one submission that carries it, once. Its own rate belt,
armed by `APP_ENV`. The destructive ceremonies additionally require typing the resource's exact name.
Every attempt lands an `admin_event` audit row, refused step-ups included. The grade of a ceremony and the
reach of its act stay matched IN THE DATABASE: the account page's step-up-LESS device sign-out is
SELF-ONLY (a device is a possession; no owner arm reaches into someone else's pocket), fenced in
`revokeOwnDevice`. The `/verify` device-approve is also step-up-LESS — a live session plus the explicit
approve click is the whole ceremony there.

**Mail — ONE transport, whole product.** `app/lib/mail/transport.server.ts` is the only module allowed to
hold an SMTP client; every product mail rides it — the invite notice (`invite-mail.server.ts`), the
verification + reset mails (`auth-mail.server.ts`), and a composition's magic links
(`magic-link-mail.server.ts`). BRING YOUR OWN SMTP: the five `TOPOS_MAIL_SMTP_*` env vars arm it
all-or-nothing; unarmed, `mailDelivery().canSend` is false and every flow keeps its honest no-send
posture (and armed mail is the identity rung for a MULTI-USER install — inviting requires it). A send
failure is COARSE — a body can carry a live credential, so no error ever echoes the message, the
recipient, or the relay response.

**Two URL grammars, one route table (`app/lib/ws-path.ts` + `app/lib/ws-url.server.ts`).** The
signed-in surface addresses workspaces by a TENANCY mode the composition passes to
`ossRoutes({ tenancy })`: **single** (the OSS default) — the install IS its one workspace, so the whole
surface is ORIGIN-ROOTED (`/`, `/members`, `/skills/:skill`) and a shareable address is the bare origin;
**multi** (a downstream superset) — the same page modules mount under `/:ws`, where `:ws` is the
workspace NAME slug (`workspace.name`), and an address is `<origin>/<name>`. No page hard-codes the
grammar: `wsHref`/`useWsPath` build in-app links, `wsPathServer`/`workspaceAddress` build server-side
redirects + the shareable address, and every workspace-scoped loader resolves through `workspaceInScope`
(single → `theWorkspace()`, multi → look up by name) before the id-keyed guards run. The opaque
`workspace.id` stays the wire/DB key but never appears in a browser URL.

**Resource addresses + the protocol card.** The three shareable FACES — the workspace ROOT, a channel,
and a skill — are each ONE route (resource address AND canonical page) under `face-shell.tsx`, plus a
catch-all. A non-browser DOCUMENT fetch gets the CONSTANT protocol card (`app/lib/card.server.ts` —
served whole from the server entry's `handleRequest`, byte-identical on every path incl. `/`,
`api_base_url` = this origin's own `/api` mount where the device lane is served). For an ANONYMOUS
browser the faces split by kind: the workspace ROOT gets the constant teaser (the landing page at the
single-tenant origin root, the constant resource teaser in multi), but a SKILL or CHANNEL face is
members-only and gets the house 404 — indistinguishable from a mistyped path, so a signed-out visitor
gets NO signal that the address shape names a resource (existence-blind: a real name and an invented
one throw the same `notFound()` before any read). A signed-in member gets the canonical page with the
app chrome; everyone else (a signed-in non-member, an unknown slug) gets the house 404. No face is an
existence oracle. A browser on an ALIAS origin is 301'd to the canonical one (`TOPOS_PUBLIC_URL`). The
uniform miss/fault surface is the root ErrorBoundary → `app/components/error-screen.tsx` (a Klein-voiced
404/500 page carrying no `error.data`, path, or stack — so every 404 is byte-constant).

**The signed-in surface:** a workspace dashboard, the skill browser, the rendered review UI (unified diff +
Approve/Reject + comments + one-click revert), the verification page, the create/join flows, and the ADMIN
surfaces — the roster page in full (invite / role change / remove / self-serve leave, sole-owner-fenced),
the skill lifecycle ceremonies (archive / unarchive / delete / purge / rename-with-redirect), the
channel pages — TABBED into **Skills** (the face, hosting in-app curation: whoever may curate — any
member of an open channel, reviewer+ of a curated one — adds/removes the channel's skill references
through the same core the device lane runs) · **Members** · **History** · **Settings** (the owner
rename/delete ceremonies) under one shared tab header (`app/components/channel/channel-tabs.tsx`) —
the **Settings** page — TABBED into **General** (the workspace policy: review
default · invite policy · staleness window · the `registration` knob) and **Devices** (the workspace fleet
view: staleness + the named blind spots — detached copies, removed-upstream rows, stale devices), both under
one shared tab header (`app/components/settings-tabs.tsx`) at `settings` / `settings/devices`), the "your
devices" self-service list, and the first-run claim. It renders state read from its own `web` schema and,
read-only, from the vault's `plane` schema; it holds no signing key, computes no digest, and initiates no
device-signed write — publishing stays on the enrolled device.

**The left panel** (`app/components/shell/{shell-chrome,app-sidebar}.tsx`, data from
`app/lib/shell/chrome.server.ts`) is one shadcn collapsible sidebar shared by both signed-in layouts —
whose content column's header bar carries the global BREADCRUMB trail
(`app/components/shell/breadcrumbs.tsx`: workspace → section → resource → tab, driven by the route
match against one central registry; signed-in only, since the anonymous teaser renders no chrome): a
header strip carrying the `topos_` wordmark beside the ONE collapse toggle (reachable in the icon-collapsed
state), then the workspace identity (STATIC name in single tenancy, a seat DROPDOWN in multi), the
workspace's **Skills** and **Channels** lists (each row a name linking to its face, each section header a
`+ new` — Skills opens a **publish-from-your-agent** dialog of copyable lines composed for this workspace's
real address, since the app never authors a bundle; Channels links to the create form), the workspace nav
(Members · Settings, from the registry's `workspace` section) as plain bottom items, and an account menu
footer (the registry's non-`workspace` sections + Sign out). The Skills/Channels/nav sections render only
when a workspace is in scope; every list is loader-derived, so the panel — living in the layout — never
reads a child route's `:ws` param (it builds links from the loader-supplied address through
`app/lib/ws-path.ts`). The chrome loader derives the active seat from the request's DESTINATION
path — React Router's client navigations fetch loaders from `<path>.data`, and that suffix is
normalized before the seat match (`destinationPathname`), so a client-side arrival at a workspace
dashboard keeps the full panel.

**Stack.** React Router 8 in framework mode (SSR, Vite, bun) · React 19 · Better Auth on Drizzle /
Postgres · Tailwind 4 with the Klein token set (`DESIGN.md` is the source of truth; the `--color-*` table
in `app/app.css @theme` is kept identical to it by `check:tokens`) · Martian Mono + IBM Plex Sans/Mono
self-hosted via `@fontsource` · `@pierre/diffs` behind a sanitizing wrapper · zod · Biome · Vitest +
Playwright. Blocking SSR — every page ships one complete document; every DB/vault read is per-request
fresh.

**Composition — four additive seams.** The package (`@topos/web`) exports `./routes`, `./nav`,
`./entitlements`, and `./auth-config`. A deployment's `app/routes.ts` is one line — `ossRoutes()` (single
by default); a downstream superset build composes `[...ossRoutes({ dir, tenancy }), ...ownRoutes]` and
appends its own nav entries, entitlements provider, and auth rungs. Composition is **additive-only**. The
OSS build is **single-tenant** — one workspace per install, origin-rooted (`theWorkspace()`); a superset
passes `tenancy: "multi"` to mount the same modules under the `/:ws` name slug (no boot workspace is
minted, and the first-run claim ceremony does not exist). The composition root also owns the
**registration policy** (`gated` — the OSS default — or `open`) and **`reservedWorkspaceNames`** (extra
top-level segments a superset reserves so no workspace slug can occlude its own routes); the OSS statics
themselves live in the dev-free `app/topos-web/segments.ts`, importable without dragging
`@react-router/dev` into a server bundle.

**Auth + authorization (fail-closed).** The OSS default rung is **email+password with zero delivery
dependency** — a self-hosted team signs in with no SMTP or OAuth. A session is evidence, never authority:
`app/lib/auth/guards.server.ts` is the only place that mints **branded actors**
(`requireSession → requireMember → requireWorkspaceOwner`/`requireReviewer`, and `requireDeviceActor` for
the device lane); the brand symbol is module-private, so a loader that skipped its guard cannot construct
one. Every DAL function requires an actor, and workspace-scoped reads take (or assert) their scope from the
actor. **Misses render 404, never 403.**

**Gates** (`bun run check`, all in CI): `check:tokens` (DESIGN.md ↔ `app.css` color drift),
`check:boundary` (no crypto/digest/signature anywhere; the vault URL + `fetch(` + `/internal/v1` confined
to the one transport; the retired `x-topos-acting-email` header banned; server modules carry `.server`;
every data-reading route guards or is on the sessionless allowlist; the raw DB surface stays inside the
DAL; zero client env), `check:email` (nothing authorizes by email equality — the one-identity rule),
`check:contract` (`app/lib/plane/contract/schema.d.ts` regenerated from the committed OpenAPI,
drift-gated), and `check:bundle` (post-build byte-scan of `build/client` for server secret names + that
the emitted CSS carries app-only utilities). The repo-level `scripts/check-db-grants.sh` (run in CI)
proves the cross-lane grant boundary by logging in as each role.

**Run it.**

```sh
bun install
bun run dev          # needs DATABASE_URL + PLANE_INTERNAL_URL/PLANE_INTERNAL_TOKEN + BETTER_AUTH_SECRET/URL
bun run db:migrate   # DATABASE_URL=… apply the web-schema migrations
bun run test         # vitest unit — NOT `bun test`, which runs BUN's own runner and writes
                     # snapshot entries vitest then reports as obsolete (CI fails on those)
bun run test:e2e     # playwright
bun run check        # biome + the boundary/email/token/contract gates + typecheck
```

`AGENTS.md` symlinks to this file.
