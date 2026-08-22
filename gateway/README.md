# `gateway/` — the MCP gateway (TypeScript on Bun)

The service every agent's MCP traffic rides: a workspace signs into an upstream MCP server ONCE
(on the web), and every enrolled machine gets working tools through
`{gateway}/{sessionId}/{serverId}` with nothing but its standing session credential. The gateway
impersonates the upstream server toward the client, attaches the workspace's credential toward
the upstream, filters `tools/*` to the workspace's tool policy, and records one usage row per
call. Revocation is a row change, effective next call.

## Layout

- `core/` — the portable session engine: pure TS, ZERO runtime dependencies, no fs/net/process.
  All I/O arrives through the injected ports (`core/ports.ts` — the ONE contract between the
  engine and whatever hosts it). The same engine runs in this container service and in an edge
  isolate.
- `service/` — the container host: Postgres adapters (`store.ts`), envelope crypto
  (`crypto.ts`), the SSRF-guarded fetch (`guarded-fetch.ts`), the OAuth authorize walk
  (`oauth.ts`), the web→gateway internal lane (`internal.ts`), the public listener
  (`server.ts`), boot migration (`migrate.ts`), entry point (`main.ts`).
- `migrations/` — the plain-SQL lineage for schema `gateway` (owned by role `topos_gateway`),
  applied at boot under an advisory lock, ledger `gateway.__migrations` (sha256-pinned; an
  applied file that changes on disk refuses to boot). Hand-rolled on purpose: the lineage is
  raw SQL and the store's queries are cross-schema joins, so no ORM migrator earns its place.
- `tests/` — vitest suites (`service-*.test.ts` for this host; `core-*.test.ts` for the
  engine). DB suites provision a scratch database per run on `TEST_DATABASE_URL` (default
  `postgres://postgres:postgres@127.0.0.1:5461/postgres`), create both app roles, run the WEB
  drizzle lineage as `topos_web` and this lineage as `topos_gateway`, and probe the grant
  boundary by LOGGING IN as each role.

## Two listeners, one secret

- **Public** (`GATEWAY_BIND`, default `127.0.0.1:8788`): `/{sessionId}/{serverId}` (every
  method → the engine), `/oauth/callback`, `/healthz`. Everything else is a uniform 404.
- **Internal** (`GATEWAY_INTERNAL_BIND`, default `127.0.0.1:8789`): the web app's lane —
  `POST /internal/v1/authorize/begin`, `POST /internal/v1/credentials/manual`,
  `DELETE /internal/v1/credentials/{id}`. A SEPARATE socket, so the lane is unreachable from
  the public bind by construction; compose publishes only the public one. Gate: with no
  `GATEWAY_INTERNAL_TOKEN` the whole lane answers 404; a wrong bearer answers 401; only the
  token's sha256 survives boot and the compare is constant-time.

## Custody

Credentials split across two tables: `gateway.credential` (metadata — `topos_web` holds SELECT,
so the web renders sign-in state with no lane call) and `gateway.credential_secret` (ciphertext —
NO grant to web, ever; the grants suite proves the refusal). Envelope: AES-256-GCM via WebCrypto,
a per-workspace data key wrapped by the master key (`GATEWAY_MASTER_KEY_FILE`, exactly 32 raw
bytes, warn-on-loose-mode), and AAD binding every ciphertext to its (credential id, workspace id)
so copied bytes decrypt nowhere else.

## Environment

| Var | Meaning |
|---|---|
| `DATABASE_URL` | as role `topos_gateway` |
| `GATEWAY_BIND` | public listener, default `127.0.0.1:8788` |
| `GATEWAY_INTERNAL_BIND` | internal lane listener, default `127.0.0.1:8789` — never published |
| `GATEWAY_PUBLIC_URL` | the gateway's public base; the OAuth redirect_uri is `{here}/oauth/callback` |
| `TOPOS_PUBLIC_URL` | the web app's origin — authorize-page links, callback return fence |
| `GATEWAY_INTERNAL_TOKEN` | the lane's shared bearer; unset = lane unarmed (404) |
| `GATEWAY_MASTER_KEY_FILE` | path to 32 raw bytes; any other size refuses boot |
| `GATEWAY_ALLOW_PRIVATE_UPSTREAMS` | `1` lets upstreams resolve to private ranges (self-host) |

## Upstream reach (SSRF posture)

Https-only; loopback/RFC 1918/CGNAT/link-local/metadata ranges (and their IPv6 equivalents,
IPv4-mapped spellings included) refuse unless `GATEWAY_ALLOW_PRIVATE_UPSTREAMS=1`; every
resolved address of a hostname is classified before the dial; redirects are handled manually and
re-guarded per hop, with Authorization dropped on any cross-origin hop. Known residual: the
runtime re-resolves the name at connect time (no connect-by-IP seam in Bun's fetch), so a DNS
answer flipping between check and dial is a window this cannot close yet — documented in
`service/guarded-fetch.ts`.

## Build / test

```sh
bun install
bun run check        # tsc --noEmit
bun run test         # vitest — NOT `bun test`; DB suites need a reachable Postgres at
                     # TEST_DATABASE_URL (they fail, never skip)
bun run start        # env above + a migrated-at-boot database
```
