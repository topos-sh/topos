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
  (`crypto.ts`), the master key backends (`master-key.ts` picks one; `kms.ts` + `kms-gcp.ts` +
  `kms-aws.ts` + `gcp-auth.ts` are the KMS clients), the master-key migration (`rewrap.ts`), the
  SSRF-guarded fetch (`guarded-fetch.ts`), the OAuth authorize walk (`oauth.ts`), the
  web→gateway internal lane (`internal.ts`), the public listener (`server.ts`), boot migration
  (`migrate.ts`), entry point (`main.ts`).
- `scripts/` — one-off operator tools run against a stopped deployment
  (`rewrap-workspace-keys.ts`).
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
a per-workspace data key wrapped by the master key, and AAD binding every ciphertext to its
(credential id, workspace id) so copied bytes decrypt nowhere else.

The leading byte of every envelope says which key opens it, and a backend refuses the other's
bytes by name rather than failing to decrypt:

| Byte | Meaning |
|---|---|
| `0x01` | AES-256-GCM under a key this process holds. Every credential ciphertext, always; and a data key wrapped by the `file` backend. |
| `0x02` | A data key wrapped by an external KMS. Second byte is the provider (`0x01` gcp-kms, `0x02` aws-kms); the rest is that service's own ciphertext. |

An unwrapped data key is cached in memory for 15 minutes, keyed by workspace **and** by the
wrapped bytes it came from — so one KMS round trip serves a workspace's traffic instead of one per
proxied call, and a row re-wrapped to another backend can never be served from a stale entry.
Credential revocation is unaffected: it is a row delete, and the row is gone before any key is
consulted.

## Master key backends

`GATEWAY_KEY_BACKEND` picks one. Missing configuration REFUSES TO BOOT — there is no fallback to a
file key, ever. Whichever backend is configured, boot wraps and unwraps a probe value before the
first listener binds, so a deployment that cannot open its own credentials fails in the deploy
rather than on some agent's first tool call.

- **`file`** (default, today's deployment) — 32 raw bytes at `GATEWAY_MASTER_KEY_FILE`, minted on
  first boot by the image's entrypoint. Lose the volume and every stored sign-in becomes
  unreadable; back it up with the database.
- **`gcp-kms`** — wrap/unwrap are `cryptoKeys.encrypt`/`cryptoKeys.decrypt` calls to Cloud KMS
  over its REST API with the workspace AAD in `additionalAuthenticatedData`. The master key never
  enters this process. Credentials come from a mounted service-account JSON key
  (`GOOGLE_APPLICATION_CREDENTIALS`, RFC 7523 JWT-bearer, scope
  `https://www.googleapis.com/auth/cloudkms`) or, with none set, the instance metadata server.
- **`aws-kms`** — the same shape over `Encrypt`/`Decrypt`, with the AAD as the `EncryptionContext`.
  Static environment credentials only (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optionally
  `AWS_SESSION_TOKEN`); the ECS/EKS container credential providers are not implemented and their
  absence is a refusal, not an ambient guess.

**Why plain `fetch` and not a vendor SDK.** Each provider is two calls against one host with a
bearer or a signature. `@google-cloud/kms` brings gRPC and protobuf loading; `@aws-sdk/client-kms`
brings a large dependency tree — into an image whose entire runtime dependency list is `pg` and
`zod`, and into a service whose other transport is a hand-written SSRF-guarded fetch precisely so
nothing dials on its own. Each client here is ~100 lines and every byte on the wire is visible in
its file. The cost is the AWS request signature (SigV4, ~70 lines): worth it for one POST to one
path with no query string, and pinned by the published test vectors.

The KMS clients use the PLAIN fetch, not the guarded one: the guard exists to keep
upstream-influenced addresses out of private ranges, and the metadata server is a link-local
address it is built to refuse. These hosts are operator configuration, not upstream input.

### What each backend does and does not protect against

The file backend does NOT survive host compromise. Anyone who can read the volume — a container
escape, a stolen backup of the volume, a root shell on the box — holds the master key, and with a
copy of the database can decrypt every stored credential offline, forever, with no trace.

A KMS keeps the master key out of this process and out of any backup: an attacker with the
database and the disk holds only ciphertext, and unwrapping requires calls the key service
authorizes, rate-limits, and logs. That is the real gain — key custody moves to a system with its
own audit trail and its own revocation, and disabling the key ends all decryption everywhere.

What a KMS does NOT protect against: an attacker who is executing code IN this process. The
unwrapped data key transits this process's memory at call time and is cached there for up to 15
minutes, and the credentials it opens are handed to the upstream on every proxied call — so a live
compromise of the running container reads secrets as they flow, whichever backend is configured.
Nor does it protect against a stolen service-account key file or a leaked instance credential:
those are the authority to call unwrap, and anyone holding them plus the database is back to
decrypting rows. A KMS narrows the window from "forever, offline, silently" to "while this
credential is valid, online, and logged" — which is the point, and is not the same as immunity.

## Environment

| Var | Meaning |
|---|---|
| `DATABASE_URL` | as role `topos_gateway` |
| `GATEWAY_BIND` | public listener, default `127.0.0.1:8788` |
| `GATEWAY_INTERNAL_BIND` | internal lane listener, default `127.0.0.1:8789` — never published |
| `GATEWAY_PUBLIC_URL` | the gateway's public base; the OAuth redirect_uri is `{here}/oauth/callback` |
| `TOPOS_PUBLIC_URL` | the web app's origin — authorize-page links, callback return fence |
| `GATEWAY_INTERNAL_TOKEN` | the lane's shared bearer; unset = lane unarmed (404) |
| `GATEWAY_KEY_BACKEND` | `file` (default) · `gcp-kms` · `aws-kms` |
| `GATEWAY_MASTER_KEY_FILE` | path to 32 raw bytes; any other size refuses boot. Required for `file`; kept mounted alongside a KMS only while a re-wrap is pending |
| `GATEWAY_KMS_KEY` | required by both KMS backends — the provider's full resource name: `projects/{p}/locations/{l}/keyRings/{r}/cryptoKeys/{k}` or `arn:aws:kms:{region}:{account}:key/{id}` (the AWS region is read out of the ARN) |
| `GOOGLE_APPLICATION_CREDENTIALS` | `gcp-kms`: path to a service-account JSON key. Unset ⇒ the instance metadata server |
| `GATEWAY_GCP_ACCESS_TOKEN` | `gcp-kms`: a ready-made token for a hand smoke test. REFUSED in production — nothing renews it |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` | `aws-kms`: the credentials, the first two required |
| `GATEWAY_ALLOW_PRIVATE_UPSTREAMS` | `1` lets upstreams resolve to private ranges (self-host) |

## Moving the master key to a KMS

Only `gateway.workspace_key` changes: the data keys themselves are untouched, so every stored
credential ciphertext stays exactly as it is. The sweep is idempotent (a row the destination can
already open is left alone), atomic per row (locked, converted, committed one at a time), and
verified before every write (the new envelope is unwrapped again through the destination and
compared before the UPDATE). A row neither backend can open is reported and left intact.

1. Create the key, grant the gateway's identity encrypt/decrypt on it, and put the credential in
   place (for GCP: the service-account key file on the volume).
2. **Stop the gateway.** It speaks exactly one backend, so a converted row is unreadable to a
   still-running old process — and a workspace signing in mid-run would mint a key under the old
   backend after the sweep had passed it.
3. Rehearse, with the NEW environment (`GATEWAY_KEY_BACKEND=gcp-kms`, `GATEWAY_KMS_KEY=…`,
   `GOOGLE_APPLICATION_CREDENTIALS=…`) and the OLD `GATEWAY_MASTER_KEY_FILE` still mounted. The dry
   run does every step for real — unwrap, re-wrap, read back — and rolls the write away:

   ```sh
   docker compose run --rm \
     -e GATEWAY_KEY_BACKEND=gcp-kms \
     -e GATEWAY_KMS_KEY=projects/…/cryptoKeys/… \
     -e GOOGLE_APPLICATION_CREDENTIALS=/run/topos/gcp-kms.json \
     gateway bun scripts/rewrap-workspace-keys.ts --from file --dry-run
   ```

4. Run it for real (same command without `--dry-run`). Exit code is 0 only when every row is
   readable under the destination; re-running after any interruption converts exactly what is
   left.
5. Deploy the gateway with the new environment. Once it is serving, unmount the key file volume —
   until it is gone the box still holds a master key, and boot says so in a warning.

The bundled `docker-compose.yml` runs the `file` backend and passes no KMS variables: it is the
self-host path, where a volume the operator already backs up is the right custody. A KMS
deployment sets these on the container itself (an orchestrator's environment panel), which is why
the one-off above passes them with `-e`.

The same command moves back (`--from gcp-kms` with `GATEWAY_KEY_BACKEND=file`). Re-keying WITHIN
one backend — a new key file, a rotated KMS key — is refused: nothing here can express which key
is the old one.

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
