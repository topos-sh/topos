# `topos` — the client CLI

**lib** (the domain operations, the per-scope sidecar stores, the manifest engine, the sync engine —
all over one fault-injectable fs/syscall seam) + **bin** (thin `clap` wiring; `--json` with no
prompts + a TTY renderer over the SAME typed outcomes). The whole verb surface is documented by the
generated `docs/cli.md` (`cargo xtask gen-cli-ref`).

## The model

- **Manifests, two UNBLENDED scopes.** PERSON = the machine's own `~/.topos/topos.toml`, the WHOLE
  machine-wide recipe: no file (or no row) means nothing is demanded machine-wide. `topos login`
  writes the workspace's FEED row (`"<host>/<ws>" = "*"` — assignments minus declines, computed
  server-side and served whole) on this machine's FIRST connection to it and never again, so a row
  someone deleted stays deleted. PROJECT = the NEAREST `topos.toml` covering the cwd,
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
  reference, and either side ambiguous refuses naming every way out. `-a <agent>`/`--dest
  <folder>` on `add` freeze the row's `dest` (the `ops/dest_select` resolution: `-a` is registry/
  descriptor sugar for the scope-correct folder or config file; unknown slugs refuse with the
  registry list, closing `nothing changed`);
  `remove` drops the row / writes `"off"` / rewrites a set line minus its members — a row edit
  uninstalls its copies EAGERLY (one scope reconcile in the same invocation; edited copies kept
  in place) — and `-a`/`--dest` on `remove` SUBTRACT destinations from the row's `dest`
  (materialized first from the current resolved set on a no-dest row; the last subtraction drops
  the row; a shared-folder-only copy refuses with both ways out). Two-phase
  (describe → `--yes`) for an indeterminate edit scan, a set split, and every git source (the
  listing is the point of the command); everything else applies immediately with an undo-led
  receipt (a dest row's undo reconstructs `-a`/`--dest`).
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
- `manifest/` (`keys`, `document`, `dest`, `normal`, `scopes`) — the reference grammar, the
  format-preserving `toml_edit` editor (property-tested exact inverse), the `dest` vocabulary
  (default destination spellings, the retired-`path`/`harness`/`[defaults]` rewrites, the
  known-MCP-file table), the normal form, scope discovery. `ops/manifest_edit` picks the file a
  verb edits and owns file birth + the writer-lock + compare-and-swap discipline every manifest
  mutation rides.
- `ops/` — the verbs: `add`, `remove`, `reconcile` (update), `sync_engine`, `publish`, `review`,
  `revert`, `protect`, `invite`, `login`/`loopback`, `status`, `auth`, `list`, `diff`, `log`,
  `init`, `fmt`, `uninstall`, `builtin` (the embedded meta-skill from `skills/topos/`),
  `self_update` (minisign-gated via `release`), `version_check`, `arm` (the arming sweep — every
  detected agent's auto-update trigger, iterating the ONE harness table and asking
  `topos-harness::triggers` for each row's adapter, so no caller knows which machinery serves
  which agent), `quiet_gate`, `merge_resolve` (diverged-draft diff3 behind the `DivergedWitness`
  token).
- `materialize` — crash-safe dir-swap placement writes. The destructive-path rail is
  PARK-THEN-VERIFY (park aside, re-read, drop only what is accounted for; unaccounted bytes are
  preserved as `.topos-kept-*` siblings) plus the park journal + recovery; the contracts live in
  the module docs. The rule everywhere: **no byte differing from its recorded baseline is
  destroyed unless a snapshot taken after the last revalidation holds it.**
- `placement` — where a bundle's bytes land per scope, in the TWO target shapes one plan carries:
  a DIRECTORY the bundle owns (shared-dir-first over the home; project-rooted with containment
  proven at the write boundary), or ENTRIES it owns inside a config file shared with the whole
  machine (`entries_plan` — the harness table's MCP surfaces joined onto detection, narrowed by
  the reach the caller resolved, with the surfaces it withheld and their reasons). A kind picks
  which arm plans and which mechanic applies; a plan says what SHOULD stand and never what does
  (that is the bundle record's custody). Composes `topos-harness::{coverage,registry,mcp}`. `Drift`
  is the ONE payload-free vocabulary both shapes project onto (Absent/Clean/Modified/Foreign/
  Unscannable) for the words receipts and the wire choose; `ScanStatus` keeps its scanned bytes.
- `mcp_validate` + `ops/add_mcp` — the `kind = "mcp"` bundle's AUTHORING half. `mcp_validate` is
  the server-document gate, mirroring the web tier rule for rule over the shared vectors at
  `tests/fixtures/mcp/` (six typed refusal codes; the credential scan runs FIRST, over the whole
  raw text; the named shapes are compiled-in matcher fns — no regex engine ships — and a
  dev-dependency `regex` referees them against the JSON's own sources in test). `ops/add_mcp` is
  `add --kind mcp`'s ONE door: a local folder holding a root `server.json` is gated whole then
  adopted in place with `kind = "mcp"` on its row, and the scope's configs converge in the same
  invocation. The client FETCHES NOTHING — a registry-shaped name and an https link to a document
  are classified only so their refusal can teach the covered path (the server is added to a
  workspace on the web, then every machine adds it by catalog name, needing no flag at all). The
  kind word is `--kind`'s value enum, which IS `BundleKind`, so the CLI vocabulary and the
  deliverable vocabulary cannot drift; the server-bundle guard on the plain skill doors arms only
  on SILENCE, so `--kind skill` adopts a `server.json`-rooted folder as a skill. `publish` reads
  the kind through the one classifier (`bundle_kind`), re-runs the gate BEFORE the op WAL, and
  threads it onto the wire.
- `mcp_engine` + `config_custody` — the `kind = "mcp"` bundle's delivery half: a store-only sync
  (lock custody, no dir placement) feeds a per-scope config CONVERGE over
  `topos-harness::mcp`'s pure drivers — server.json parsed fail-closed, and the converge itself
  is DEMAND (each bundle's entries plan) plus CUSTODY (its own recorded rows). Reach resolves at
  PLANNING and nowhere else: a row's `dest` config-file entries map to the harnesses that claim
  them (no dest = every MCP-capable agent), a targeted verb plans only the harnesses whose
  recorded rows prove the bundle already stands there (`recorded_reach`), and the plan carries the
  surfaces it withheld so a receipt still says what reach cost. Every demand the sweep and `add`
  converge is built by `DemandedBundle::planned`, so a caller cannot hand the converge a reach the
  planner did not compute; the targeted converge builds its own demand from a reach it re-derives
  from the record immediately before the lock, because a verb plans at its top and converges after
  a materialize — a stale reach would claw back an entry a sweep placed in that window. An add
  never places where the next sweep would claw it back. Removal and
  key retirement resolve their surfaces from the descriptor table and their reach from the
  recorded rows — prior-matched keys, with drift left in place. ONE converge path serves every
  surface, the wholly-topos-owned Claude plugin dir included (its `.mcp.json` is an ordinary
  driver surface; content topos did not write backs the whole surface off), and every entry
  point — the sweep, add's inline converge, a targeted accept/go-back, `remove` — serializes on
  the per-scope `locks/mcp.lock` (unavailable = a refusal, never a fallback), over the ONE
  scope-store resolution (`manifest_edit::mcp_scope_target`).
- `config_custody` — WHO OWNS WHICH CONFIG ENTRY. Placement ownership is the BUNDLE's, recorded
  in `skills/<id>/entries.json` — the SIBLING of its `map.json`, so the record directory is still
  the one place a bundle's custody lives and deleting the record deletes both halves. Two files,
  because they have two writers: `map.json` is written only under the per-skill flock and
  `entries.json` only under `locks/mcp.lock`, which makes single-writer-per-file structural rather
  than a rule the two lock domains have to honour. A row is
  `{agent, file, key, fingerprint, owns_file, version_id}`, and its `fingerprint` is to an entry
  what a dir's `materialized_sha` is to a folder. Readers that need both halves (list, the orphan
  resolution, remove's describe) read both files lock-free. Only two things outlive or span a
  bundle, and they live in
  the per-scope `state/config_custody.json`: the minted immutable config keys with their
  retirement reservations (a key names an OAuth trust surface — retired, never re-minted for
  another bundle), and the intent journal every config write rides (one write covers many
  bundles' entries; crash recovery promotes or drops per bundle record by OBSERVING the file, and
  an unreadable file keeps the standing row). `ScopeEntries` is the read-modify-write view that
  joins the scope document onto every bundle's rows as the one index the converge asks its
  ownership questions of; a record write that fails puts its intents BACK in the journal, so the
  durable journal always describes exactly the work that has not landed. A bundle with no record
  of its own — the fetch door's `local:` identities, and a bundle whose drifted entries outlived
  the record a `remove` deleted — rides the scope document's `unrecorded` map.
- `bundle_kind` — WHAT A BUNDLE IS: the closed kind vocabulary (`skill` · `mcp`) and the ONE parse
  of a kind word. A word this build does not own is refused at every door it can enter — the sweep
  (the row is skipped whole, warned, never cached), `add`, and a hand-written manifest at LOAD
  (`manifest/document`, naming the known kinds) — never guessed into the nearest kind. The durable
  per-record `kind.json` marker is written at every bundle's first sync/adopt, and is the first
  rung of the ONE classification chain every delivery path asks: marker → the manifest local-path
  row that demands the placements → indeterminate, which over an EMPTY placement map is a REFUSAL,
  not a default.
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
