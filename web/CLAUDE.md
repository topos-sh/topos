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
is a login name and a mutable attribute — NOTHING authorizes by email equality. Every seat, device,
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
- **First boot** (`ensureSetup`) mints the workspace + its default `everyone` channel on a virgin
  database, and while unclaimed (re)mints the claim code and prints ONE line to the logs
  (`→ Finish setup: <origin>/claim?code=…`; `TOPOS_SETUP_CODE` presets it, `TOPOS_SETUP_LINK_FILE`
  mirrors it to a file). Only the code's SHA-256 is stored.
- **The claim** (`claim.tsx` → `consumeClaim`): one atomic UPDATE consumes the code and seats the first
  **owner** (email + password). Single-use by construction.
- **The gh-style device flow** (`verify.tsx` + `api.v1.device-authorize`/`api.v1.device-token`;
  `startDeviceAuth`/`pollDeviceAuth`/`approveDeviceAuth`): the CLI prints "open `<origin>/verify` and
  enter AB12-CD34" and polls; the signed-in person approves **behind step-up**, which mints the device
  (owned by that person) + its ONE bearer credential (the device code is promoted to the credential —
  same plaintext, same stored hash). Revocation is self-service, immediate, and FINAL (a DB trigger
  refuses any un-revoke).
- **Recovery** (`app/lib/auth/recovery.server.ts` + `scripts/mint-recovery-code.mjs`): reset mail when
  SMTP is armed; a mail-less solo owner runs the one-shot box-side script to print a single-use recovery
  code (machine control is the proof).

Secrets are HASH-STORED, and the hashing happens IN Postgres (`sha256(convert_to(…))`) or inside Better
Auth's own password hasher — this tier generates randomness (the two mints in `identity.server.ts` +
`recovery.server.ts`) but computes no digest.

**Registration is never open** (`app/lib/auth/registration.server.ts`, wired as Better Auth's
`user.create.before` hook so no rung can bypass it): a sign-up succeeds only inside the claim ceremony,
OR with a pending invitation on a deployment whose SMTP is armed (the invited seat binds only after the
mailbox round-trip, via `bindInvitedSeats` on `afterEmailVerification`), OR under the off-by-default
`registration = 'open'` knob. Everything else gets ONE constant, non-enumerating refusal.

**Step-up** (`app/lib/auth/step-up.server.ts`). Every admin ceremony re-authenticates immediately before
the act: the person re-enters their password inside the ceremony form (verified with Better Auth's own
hasher against the SESSION's account — never a form-supplied identity; its own rate belt, armed by
`APP_ENV`), and the destructive ceremonies (delete a skill, purge a version, delete a channel)
additionally require typing the resource's exact name. Deliberately STATELESS — no sudo window. Every
attempt lands an `admin_event` audit row, refused step-ups included. The grade of a ceremony and the
reach of its act stay matched IN THE DATABASE: the account page's step-up-LESS device sign-out is
SELF-ONLY (a device is a possession; no owner arm reaches into someone else's pocket), fenced in
`revokeOwnDevice`. **Known limit (v1):** step-up IS the password rung — a deployment configured with only
magic-link or social sign-in has no password to re-enter; a second factor for password-less deployments
is later work.

**Mail — ONE transport, whole product.** `app/lib/mail/transport.server.ts` is the only module allowed to
hold an SMTP client; every product mail rides it — the invite notice (`invite-mail.server.ts`), the
verification + reset mails (`auth-mail.server.ts`), and a composition's magic links
(`magic-link-mail.server.ts`). BRING YOUR OWN SMTP: the five `TOPOS_MAIL_SMTP_*` env vars arm it
all-or-nothing; unarmed, `mailDelivery().canSend` is false and every flow keeps its honest no-send
posture (and armed mail is the identity rung for a MULTI-USER install — inviting requires it). A send
failure is COARSE — a body can carry a live credential, so no error ever echoes the message, the
recipient, or the relay response.

**Resource addresses + the protocol card.** `/{workspace}`, `/{workspace}/channels/{name}`, and
`/{workspace}/skills/{name}` are the shareable addresses, plus the ORIGIN ROOT and a catch-all. A
non-browser DOCUMENT fetch gets the CONSTANT protocol card (`app/lib/card.server.ts` — served whole from
the server entry's `handleRequest`, byte-identical on every path incl. `/`, `api_base_url` = this
origin's own `/api` mount where the device lane is served); an anonymous browser gets the constant
landing page at `/`; a signed-in member resolves through their own confirmed seats into the workspace
surface; everyone else gets the house 404. No face is an existence oracle. A browser on an ALIAS origin is
301'd to the canonical one (`TOPOS_PUBLIC_URL`).

**The signed-in surface:** a workspace dashboard, the skill browser, the rendered review UI (unified diff +
Approve/Reject + comments + one-click revert), the verification page, the create/join flows, and the ADMIN
surfaces — the roster page in full (invite / role change / remove / self-serve leave, sole-owner-fenced),
the skill lifecycle ceremonies (archive / unarchive / delete / purge / rename-with-redirect), channel
existence-admin + history, the workspace policy page (review default · invite policy · staleness window ·
the `registration` knob), the fleet page (staleness + the named blind spots: detached copies,
removed-upstream rows, stale devices), the "your devices" self-service list, and the first-run claim. It
renders state read from its own `web` schema and, read-only, from the vault's `plane` schema; it holds no
signing key, computes no digest, and initiates no device-signed write — publishing stays on the enrolled
device.

**Stack.** React Router 8 in framework mode (SSR, Vite, bun) · React 19 · Better Auth on Drizzle /
Postgres · Tailwind 4 with the Klein token set (`DESIGN.md` is the source of truth; the `--color-*` table
in `app/app.css @theme` is kept identical to it by `check:tokens`) · Martian Mono + IBM Plex Sans/Mono
self-hosted via `@fontsource` · `@pierre/diffs` behind a sanitizing wrapper · zod · Biome · Vitest +
Playwright. Blocking SSR — every page ships one complete document; every DB/vault read is per-request
fresh.

**Composition — four additive seams.** The package (`@topos/web`) exports `./routes`, `./nav`,
`./entitlements`, and `./auth-config`. A deployment's `app/routes.ts` is one line — `ossRoutes()`; a
downstream superset build composes `[...ossRoutes({ dir }), ...ownRoutes]` and appends its own nav entries,
entitlements provider, and auth rungs. Composition is **additive-only**. The OSS build is **single-tenant**
— one workspace per install (`theWorkspace()`).

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
