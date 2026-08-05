# `topos` — the client CLI

**lib** (the domain operations, the per-scope sidecar stores, the manifest engine, the sync engine —
all over one fault-injectable fs/syscall seam) + **bin** (thin `clap` wiring; `--json` with no
prompts + a TTY renderer over the SAME typed outcomes). The whole verb surface is documented by the
generated `docs/cli.md` (`cargo xtask gen-cli-ref`).

## The model

- **Manifests, two UNBLENDED scopes.** PERSON = the machine's own `~/.topos/topos.toml` when it
  exists, else the implicit FEED (one row per connected workspace — assignments minus declines,
  computed server-side and served whole). PROJECT = the NEAREST `topos.toml` covering the cwd,
  taken whole (walk up only to find it; no ancestor merging, no cross-scope shadowing). A
  manifest's `[bundles]` keys ARE references, classified by shape: local paths ·
  `github.com/<owner>/<repo>[/<skill>]` · `<host>/<ws>` (the feed) · `<host>/<ws>/<bundle>` ·
  `<host>/<ws>/channels/<name>`; values are `"*"`, a 64-hex version, a 7–40-hex commit pin,
  `"off"` (global-file-only), or an inline table. `topos fmt` writes the normal form.
- **Sessions.** A session = user × workspace × installation. `topos login` (RFC 8252 loopback when
  a local browser exists, else the RFC 8628 typed code) mints ONE workspace-scoped bearer
  credential in `identity/sessions.json` (0600). Login IS the standing acceptance — delivery is
  silent from then on, npm-style; `logout` is the server-side self-end.
- **The client writes NO server state.** Declines and assignments are web acts; the one
  machine-local negative is the `"off"` row. A manifest row IS the demand: the reconcile converges
  a git row automatically, like every other kind, and the describe-then-`--yes` shape belongs to
  the `add` verb — where a person is present to read what a repo holds.
- **`update` is the manifest reconcile:** one delivery call per live session, per-scope resolution
  (explicit rows beat sets beat feed; pins never move), per-scope placement over per-scope stores
  (person = `~/.topos/`; project = the checkout's own `.topos/state/<user>/`, every project path
  proven inside the checkout — refused, never redirected), then a snapshot-first clean of what each
  scope no longer demands and an applied report per session. Forge rows ride their OWN hardcoded
  clock (`forge_check`, machine-scoped, no config surface): a floating row is PROBED (the git ref
  advertisement — outside the REST allowance) and downloaded only on a real change, with a
  per-round circuit breaker and a clock that advances on failure too. `update --quiet` is the
  harness-hook sweep: self-throttled, schema-conservative stdout (`--hook claude-code` opts that
  harness into `reloadSkills`).
- **`add`/`remove` are exact file inverses** (property-tested). `add` is source-polymorphic
  (workspace refs, a path adopted in place, a forge import, `add topos` for the built-in); a BARE
  NAME resolves against both the untracked local inventory and the connected workspaces' catalogs
  — one local dir adopts in place, a name only a workspace publishes subscribes to its canonical
  reference, and either side ambiguous refuses naming every way out;
  `remove` drops the row / writes `"off"` / rewrites a set line minus its members. Two-phase
  (describe → `--yes`) for loss, a set split, and every git source (the listing is the point of
  the command); everything else applies immediately with an undo-led receipt.
- **Contribute:** `publish` (a landed publish of a path-ref item transfers governance — catalog
  entry + the manifest line rewritten to the workspace reference), `review`, `revert`, `protect`,
  `invite` — op-WAL idempotent retry over each skill's session lane.

## Module map (`src/`)

- `fs_seam`, `atomic`, `doc` — the fault-injectable syscall seam (O_NOFOLLOW everywhere,
  dir-handle-anchored writes, private-file primitives) + crash-safe JSON docs with fail-closed
  schema migration. Every durable mutation goes through here; crash-safety contracts live in the
  module docs.
- `sidecar`, `forge_check`, `visited_stores`, `sync_status` — the per-scope store layout +
  recovery sweep, and the machine-local state documents (the forge auto-update clock + per-source
  check log, visited project stores, the offline delivery cache that keeps `status`/`list`
  honest).
- `manifest/` (`keys`, `document`, `normal`, `scopes`) — the reference grammar, the
  format-preserving `toml_edit` editor (property-tested exact inverse), the normal form, scope
  discovery. `ops/manifest_edit` picks the file a verb edits and owns file birth + the
  writer-lock + compare-and-swap discipline every manifest mutation rides.
- `ops/` — the verbs: `add`, `remove`, `reconcile` (update), `sync_engine`, `publish`, `review`,
  `revert`, `protect`, `invite`, `login`/`loopback`, `status`, `auth`, `list`, `diff`, `log`,
  `init`, `fmt`, `uninstall`, `builtin` (the embedded meta-skill from `skills/topos/`),
  `self_update` (minisign-gated via `release`), `version_check`, `arm` (the breadth arming
  sweep), `quiet_gate`, `merge_resolve` (diverged-draft diff3 behind the `DivergedWitness`
  token).
- `materialize` — crash-safe dir-swap placement writes. The destructive-path rail is
  PARK-THEN-VERIFY (park aside, re-read, drop only what is accounted for; unaccounted bytes are
  preserved as `.topos-kept-*` siblings) plus the park journal + recovery; the contracts live in
  the module docs. The rule everywhere: **no byte differing from its recorded baseline is
  destroyed unless a snapshot taken after the last revalidation holds it.**
- `placement` — where a bundle's bytes land per scope (shared-dir-first over the home;
  project-rooted with containment proven at the write boundary), composing
  `topos-harness::{coverage,registry}`.
- `mcp_validate` + `ops/add_mcp` — the `kind = "mcp"` bundle's AUTHORING half. `mcp_validate` is
  the server-document gate, mirroring the web tier rule for rule over the shared vectors at
  `tests/fixtures/mcp/` (six typed refusal codes; the credential scan runs FIRST, over the whole
  raw text; the named shapes are compiled-in matcher fns — no regex engine ships — and a
  dev-dependency `regex` referees them against the JSON's own sources in test). `ops/add_mcp` is
  `add --mcp`: a local folder is gated then adopted in place with `kind = "mcp"` on its row; a
  registry-shaped name resolves WORKSPACE-FIRST against the connected catalogs' embedded server
  names (the listing's additive `mcp_server_name` field) — exactly one hit subscribes to that
  bundle by catalog name through the ordinary reference arm, source-disclosed; several refuse
  toward `--workspace`; a miss falls through to the official registry, whose 404 then names both
  consulted sources; a registry name or https URL is fetched, gated, and applied immediately with
  an undo-led receipt
  (the canonical document written, the row recorded, the scope's config converged; the per-scope
  `state/mcp_imports.json` record is what lets `remove` delete the written folder only when its
  bytes still match). `publish` reads the kind
  (ledger → delivery cache → the manifest row, the last only when the draft carries a root
  `server.json`), re-runs the gate BEFORE the op WAL, and threads it onto the wire.
- `mcp_engine` + `mcp_ledger` — the `kind = "mcp"` bundle's delivery half: a store-only sync
  (lock custody, no dir placement; a durable per-skill `kind.json` marker written at first sync
  keeps kind classification alive if the ledger is lost — marker → delivery cache → manifest row
  → ledger, failing CLOSED over an empty placement map) feeds a per-scope config CONVERGE over
  `topos-harness::mcp`'s pure drivers — server.json parsed fail-closed, per-harness surfaces
  joined onto detection (project surfaces containment-proven) and narrowed by ONE shared
  resolution (the row's own `harness = [...]`, else `[defaults.mcp]`) so an add never places
  where the next sweep would claw it back, removal via prior-matched keys with drift left in
  place. ONE converge path serves every surface, the wholly-topos-owned Claude plugin dir
  included (its `.mcp.json` is an ordinary driver surface; content topos did not write backs the
  whole surface off), and every entry point — the sweep, add's inline converge, a targeted
  accept/go-back, `remove` — serializes on the per-scope `locks/mcp.lock` (unavailable = a
  refusal, never a fallback). `mcp_ledger` (`state/mcp_ledger.json`, per scope) is the ownership
  record: minted immutable config keys (retired, never reused), `key → fingerprint` entries
  scoped to the FILE they were written into (a surface path that moves leaves a disclosed stale
  class rather than re-pointed custody), and the intent journal every config write rides (crash
  recovery promotes or drops by OBSERVING the file; an unreadable file keeps the standing entry).
- `plane_http` — the blocking `ureq` transports; one `SessionTransports` set per session;
  `resolve_session_lane` picks a write verb's lane. `scan` — the bundle scanner (rejects
  fs-level hazards before the kernel digest). `config_io` + `ctx` — the injected
  `HarnessAdapter`/`ConfigStore` seams. `actions` — the one `NextAction` construction fn.
- `progress` — the ACTIVITY seam: a stderr-only transient line naming what is in flight while a
  fetch runs. One phase at a time, opened through an RAII guard; a verb's label (`updating docs
  (2 of 3)`) wins over a transport's (`contacting topos.sh`), which opens only when idle; byte
  progress attaches to whichever phase is live, so the transports stream bytes into a label the
  op chose. Three sinks — animated (`indicatif`, terminal), one plain line per phase (piped), and
  silent (`update --quiet`, the offline verbs, every test). **Never stdout**: the `--json`
  envelope and the golden fixtures are untouched by it.

Golden `--json` fixtures are asserted byte-equal in tests; the composed-e2e fixture rig is
`test_support::SessionInstall` (feature `test-fixtures`).

## Constraints

**No edge to `plane-store` / `sqlx` / an async runtime** — enforced by `cargo xtask check-arch`.
No signing key material client-side: a session authenticates with its bearer credential, and the
only key in the binary is the PUBLIC release key (`RELEASE_PUBKEY`, which makes self-update
signatures mandatory). Kernel identity (`version_id`/`bundle_digest`) depends only on bytes +
device id, so injectable id/time sources keep `add` deterministic. Notable deps: `rustix` (safe
syscalls: fsync/flock/atomic dir-swap), `ureq` (rustls), `toml_edit`, `tar`+`flate2` (self-update
+ forge import, in-memory), `minisign-verify`, `indicatif` (the stderr activity line).
