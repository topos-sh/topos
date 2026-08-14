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
  scope no longer demands and an applied report per session. A clean that took surfaces a row
  STOPPED demanding while the bundle keeps delivering is a NARROWING, and the receipt leads with
  it (`PullSkill::narrowed` — what still stands, then one line per entry or folder that left),
  instead of a bare `removed` row over a bundle the machine still holds. Forge rows ride their OWN hardcoded
  clock (`forge_check`, machine-scoped, no config surface): a floating row is PROBED (the git ref
  advertisement — outside the REST allowance) and downloaded only on a real change, with a
  per-round circuit breaker and a clock that advances on failure too. Every reconcile CLOSES by
  REGISTERING a newly detected agent's auto-update trigger (`ops::register_new_detected` over
  `ctx.triggers`, the ACTIVE harness a candidate like any other): detected + trigger-capable + no
  record → install and record; a trigger already standing is recorded without an attempt; a harness
  WITH a record is never touched, which is what makes a hand-removal permanent. The non-quiet
  receipt lists what it registered. `update --quiet` is the
  harness-hook sweep: self-throttled, schema-conservative stdout (`--hook claude-code` opts that
  harness into `reloadSkills`) — a registration moves no bundle bytes, so it adds nothing there.
- **`add`/`remove` are exact file inverses** (property-tested). `add` is source-polymorphic
  (workspace refs, a path adopted in place, a forge import, `add topos` for the built-in); a BARE
  NAME resolves STANDING first — a name the invoked scope already records answers `ALREADY_TRACKED`
  with the record's own source, and one the OTHER scope records is added here from that same source
  — and only then discovery: the untracked local inventory ∪ the connected workspaces' catalogs,
  where exactly one candidate acts and two or more become THE CHOOSER (`AMBIGUOUS_NAME`, one
  runnable `topos add <full-reference>` per candidate on both surfaces). Several FOLDERS of one
  name are the chooser's own shape: the answer lists every folder with what its bytes are against
  the bundle (`matches the published current` / `edited` / `edited differently`, proved by digest
  or withheld) and closes with the one `topos add <folder> [--as <bundle>]` — a runnable claim
  still rides the agent surface for every folder a version EXPLAINS, and never for one it does
  not. **AN `add` NEVER NARROWS:** `-a <agent>`/`--dest <folder>` EXTEND the row's `dest` (the
  `ops/dest_select` resolution: `-a` is registry/descriptor sugar for the scope-correct folder or
  config file; unknown slugs refuse with the registry list, closing `nothing changed`), and on a
  row that named none they join the reserved **`"*"` token** (`manifest/dest`'s `DEFAULT_REACH`) —
  the row's DEFAULT REACH, recomputed at plan time on every run (`placement::dest_reach_plan` over
  `default_reach_plan`; for an MCP row the token is simply the unnarrowed reach), with named
  entries placed regardless of detection as ever. On a row standing for that reach, an asked entry
  the reach ALREADY holds (`manifest_edit::reach_already_held`, over the same `default_reach_roots`
  the collapse consults) records nothing — recording it would make the printed undo a narrowing —
  and an ask that drops whole is the ordinary `nothing changed`. The normal form sorts `"*"`
  first, collapses duplicates, and spells a row whose only field is `dest = ["*"]` as the plain
  `"*"` value; the token is ROW GRAMMAR alone — `--dest '*'` refuses at the argv and a channel's
  `mcp_dest` refuses it at load.
  A `-a`/`--dest` on a bundle the invoked scope has NO row for and a channel or feed line
  provably delivers (`manifest_edit::set_delivering`, the proof the collapse asks) writes NO row
  at all — one could only narrow what the set reaches — and converges that bundle's placements in
  the same invocation instead, answering with the surfaces the converge wrote and no undo. That
  converge's outcome IS the answer: an error is the add's own, a bundle it failed says so on the
  asked agent's line (never `not placed — it is not set up here`, which is the DETECTION answer)
  and never closes on `nothing changed`, and its warnings ride the add's envelope. Asked agents
  match the converge's surfaces by FOLDER, so an agent reading a shared skills root is reported
  against that root instead of missing.
  `remove` drops the row / writes `"off"` / rewrites a set line minus its members — a row edit
  uninstalls its copies EAGERLY (one scope reconcile in the same invocation; edited copies kept
  in place) — and `-a`/`--dest` on `remove` SUBTRACT destinations from the row's `dest`
  (materialized first from the current resolved set on a row naming none of its own, `"*"`
  included; a remainder still holding the WHOLE default reach collapses back onto the token, and
  goes entirely where a channel or feed line in the same file provably delivers the bundle — which
  is what makes an `-a` add and its `remove -a` undo exact inverses; the last subtraction drops
  the row; a shared-folder-only copy refuses with both ways out). Two-phase
  (describe → `--yes`) for an indeterminate edit scan, a set split, and every git source (the
  listing is the point of the command); everything else applies immediately with an undo-led
  receipt (a dest row's undo reconstructs `-a`/`--dest`).
- **Contribute:** `publish` (a landed publish of a path-ref item transfers governance — catalog
  entry + the manifest line rewritten to the workspace reference), `review`, `revert`, `protect`,
  `invite` — op-WAL idempotent retry over each skill's session lane. `publish` resolves its copy
  STANDING-FIRST across the per-scope stores (`ops::resolve_skill_stored`): the scope you stand in
  when it holds a DRAFT — bytes ahead of the lock, settled or not (`ops::store_draft_dir`, the one
  draft definition) — else the other reachable scope's drafted copy, else where you stand; `-g`
  narrows to the machine store and misses honestly. A ship from outside the standing scope names
  its folder on both surfaces, and the other scope's draft is disclosed with the command that
  shares it. A copy already at `current` is a SUCCESS document (`result: no_changes`), never a
  refusal.

## Module map (`src/`)

- `fs_seam`, `atomic`, `doc` — the fault-injectable syscall seam (O_NOFOLLOW everywhere,
  dir-handle-anchored writes, private-file primitives) + crash-safe JSON docs with fail-closed
  schema migration. Every durable mutation goes through here; crash-safety contracts live in the
  module docs.
- `sidecar`, `forge_check`, `harness_registry`, `visited_stores`, `sync_status`, `trigger_record`
  — the per-scope
  store layout + recovery sweep, and the machine-local state documents (the forge auto-update
  clock + per-source check log, visited project stores, the offline delivery cache that keeps
  `status`/`list` honest, and which agents this machine has been OFFERED a trigger).
  `trigger_record` is the sweep's one-time-offer gate: a row per harness slug, written by every
  trigger installation there is (`login`/`add`'s unconditional breadth arming and the sweep's own),
  so a hook the person deleted stays deleted — a FAILED attempt is recorded too and retried on the
  forge clock's rhythm, never per sweep; a newer build's document is neither read nor written, and
  an unparseable one reads as absent (garbage is not evidence that anybody removed anything).
  `harness_registry` is the HARNESS TABLE's own lane, on the forge
  clock's rhythm and its fail-open discipline: the bare sweep downloads
  `TOPOS_HARNESS_REGISTRY_URL` (default `https://topos.sh/harness-registry.toml`), runs
  `topos_harness::registry`'s fences, and writes `~/.topos/harness-registry/registry.toml`
  atomically — a strictly-newer version only, an equal version with different bytes an error, and
  a cached copy the fences now refuse DELETED in the round that finds it (the loader would warn
  about it on every command otherwise). A landing table's skipped rows are said HERE, once, and
  nowhere else. Nothing here can fail a sweep, and there is NO hot reload: the next process reads
  the table. `TOPOS_NO_HARNESS_REGISTRY_UPDATE` switches the lane off.
- `manifest/` (`keys`, `document`, `dest`, `normal`, `scopes`) — the reference grammar, the
  format-preserving `toml_edit` editor (property-tested exact inverse), the `dest` vocabulary
  (default destination spellings, the known-MCP-file table, and the one entry that is not a path —
  the `"*"` DEFAULT-REACH token), the normal form, scope discovery. `ops/manifest_edit` picks the file a
  verb edits and owns file birth + the writer-lock + compare-and-swap discipline every manifest
  mutation rides.
- `ops/claim` — `add <path> --as <bundle>`: a folder that ALREADY holds a copy of a bundle the
  invoked scope manages becomes one of its places. No version is minted and nothing in the folder
  is written; the whole mechanic is the BASELINE the new `map.json` row carries (a row with none
  scans Foreign and is frozen out of every plan forever) — the current digest, the matching
  historical digest for a stale copy, or the lock's digest AFTER snapshotting bytes no version
  explains, so the copy scans Modified and rides the draft engine. The row also carries the
  `PlacementClaim` marker, which is what two invariants key on: a claimed folder is a target of
  EVERY dir plan (`placement::keep_claimed`, at the end of the three planners that choose targets;
  the classic plan keeps every prior placement already — the claim's promise is currency, and no
  planner reaches a folder the PERSON named by right), and its detach
  subtracts from the manifest row exactly the `dest` entry the claim recorded ADDING, never merely
  the root the folder sits under. Resolution is scope-strict over that one store (never a catalog);
  the folder's other claimants refuse across the machine store AND every ancestor project store
  (two engines would converge one directory) — ONE probe, `manifest_edit::other_scope_stores` +
  `add::refuse_other_scope`, which the plain path adopt asks too, and which only a server bundle
  (no directory to converge, either side) passes; the record half runs under the per-skill flock with
  the ownership questions asked inside it; a dest-frozen row gains the folder through the same
  additive `dest` write an `-a` add makes; and a clean collision-suffixed twin beside the claimed
  folder retires with it. The two writes (record, then row) are ordered so a RE-RUN repairs a crash
  between them — the already-a-place path verifies and repairs rather than returning early, and
  reads its receipt's state off the record. The inverse is `remove <bundle> --dest <folder>`
  (`manifest_edit`'s `Arm::ClaimDetach`, reachable from a row, an off-switch, and a set line alike):
  the record forgets the folder, the bytes stay, and a detach with nothing to edit writes no
  manifest at all.
- `ops/` — the verbs: `add`, `remove`, `reconcile` (update), `sync_engine`, `publish`, `review`,
  `revert`, `protect`, `invite`, `login`/`loopback`, `status`, `auth`, `list`, `diff`, `log`,
  `init`, `fmt`, `uninstall`, `builtin` (the embedded meta-skill from `skills/topos/`),
  `self_update` (minisign-gated via `release`), `version_check`, `arm` (the TRIGGER half of the
  harness ports: `Triggers` — what `ctx.triggers` carries, the active agent's trigger plus the
  machine root the whole-machine set resolves under, so `artifacts` (the uninstall describe, and its
  path rows `list --footprint`) and `scrub_others` (`uninstall --yes`) walk THE SAME set — plus the
  detection-scoped `arm_detected`/`probe_detected`/`register_new_detected` sweeps. All of it iterates harness-table rows
  and asks `topos-harness::triggers` for each row's adapter, so no caller knows which machinery
  serves which agent — ARMING over the table this machine resolved, TEARDOWN over
  `registry::teardown_harnesses()` (that table plus the bundled floor), because a row a newer
  downloaded table dropped is one topos stops arming and never one it stops scrubbing),
  `quiet_gate`, `merge_resolve` (diverged-draft diff3 behind the `DivergedWitness` token).
- `materialize` — crash-safe dir-swap placement writes. The destructive-path rail is
  PARK-THEN-VERIFY (park aside, re-read, drop only what is accounted for; unaccounted bytes are
  preserved as `.topos-kept-*` siblings) plus the park journal + recovery; the contracts live in
  the module docs. The rule everywhere: **no byte differing from its recorded baseline is
  destroyed unless a snapshot taken after the last revalidation holds it.**
- `placement` — where a bundle's bytes land per scope. **Every target dir comes from the harness's
  registry ROW through the one resolver — the ACTIVE harness's exactly like every other detected
  one**, so a machine-local table that moved an agent's skills dir moves the bytes with it; the
  `HarnessAdapter` answers for the dir only where no row can (no machine roots at all). Two target
  shapes ride one plan:
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
- `mcp_render` — WHAT ONE `server.json` BECOMES on this machine, for one agent. The document is
  read once (`ServerDoc`: the address it offers, the packages it pins, the auth word — the header
  re-check of the publish gate lives here, fail-closed), then `select` crosses it with the
  harness's capability columns and this machine's runtimes, in ONE fixed order: the address where
  the agent dials one · the address through the table-pinned bridge (`npx -y mcp-remote@<v> <url>`,
  headers as `Name:${VAR}` with the value in the environment — the spelling that survives Windows
  and Cursor) where it does not · else the first package this build can set up, npm before pypi
  (`npx -y <id>@<v>` / `uvx <id>@<v>`, always the pinned version). A missing runtime NEVER falls
  through to another form — a shared bundle must not become different bytes per laptop — it
  becomes a plain-words `Gap` the next sweep re-decides. Env slots the document leaves empty
  render as references in the harness's own syntax, so a name travels and a value never does. The
  runtime probe is a seam (`RuntimeProbe` on `ScopeIo`), because "needs node" has to be a tested
  outcome and not a property of the machine running the suite.
- `mcp_engine` + `config_custody` — the `kind = "mcp"` bundle's delivery half: a store-only sync
  (lock custody, no dir placement) feeds a per-scope config CONVERGE over
  `topos-harness::mcp`'s pure drivers — server.json parsed fail-closed, and the converge itself
  is DEMAND (each bundle's entries plan) plus CUSTODY (its own recorded rows). Before a surface is
  written a COLLISION PRE-FLIGHT asks what already stands where each entry would go — by name or by
  canonical server address, in the dest file AND in the read-only files the harness also reads
  servers from (the table's `conflict_paths`) — and an entry topos cannot prove is its own blocks
  that one placement: `conflicting`, never touched, reported with the way out (a `topos-`-looking
  name earns the possible-leftover wording, because a prefix is a spelling and not a provenance).
  The verdict is re-decided every run from the files, so it clears itself; a key topos already
  holds on that surface is never blocked, since dropping it from the desired set would uninstall
  the placement that stands. WHAT each engaged agent is given is `mcp_render::select`'s answer,
  taken per agent inside the same loop; a capability it cannot answer for reads `not placed` with
  the reason, and a bundle no agent could be given anything for joins the failed count, because a
  summary saying "up to date" over a machine holding nothing is the one thing a receipt may never
  say. Reach resolves at
  PLANNING and nowhere else: a row's `dest` config-file entries map to the harnesses that claim
  them (no dest — or a dest carrying `"*"` — = every MCP-capable agent), a targeted verb plans only the harnesses whose
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
  what a dir's `materialized_sha` is to a folder. Its `file` is the RESOLVED path, and every
  comparison resolves too (`canonical_file`, over the seam's `canonicalize`): one config file
  reached through two spellings — a symlinked agent home — is one surface, or topos disowns the
  entry it just wrote. Readers that need both halves (list, the orphan
  resolution, remove's describe) read both files lock-free. Only two things outlive or span a
  bundle, and they live in
  the per-scope `state/config_custody.json`: the minted immutable config keys with their
  retirement reservations (a key names an OAuth trust surface — retired, and given back ONLY AT A
  LATER MINT FOR THE SAME SERVER: the address the key pointed at is recorded at every mint and kept
  through retirement, and a reservation goes back only when the mint's canonical address matches it
  AND no entry stands under the key in any file this scope's harnesses read servers from — the
  writable surfaces and the table's read-only `conflict_paths` alike, with an unreadable or
  unparseable one keeping the lot, because absence is what has to be proven. A different address
  keeps the reservation and takes a `-2`, because harness auth state outlives config entries and no
  config file can rule a sign-in out), and the intent journal every config write rides (one write covers many
  bundles' entries; crash recovery promotes or drops per bundle record by OBSERVING the file, and
  an unreadable file keeps the standing row). `ScopeEntries` is the read-modify-write view that
  joins the scope document onto every bundle's rows as the one index the converge asks its
  ownership questions of; a record write that fails puts its intents BACK in the journal, so the
  durable journal always describes exactly the work that has not landed. A bundle with no record
  of its own — a row no reader could resolve to one (`local:<name>`), and a bundle whose drifted
  entries outlived the record a `remove` deleted — rides the scope document's `unrecorded` map.
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
  fs-level hazards before the kernel digest). `config_io` + `ctx` — the injected `ConfigStore` seam
  and the two harness ports a verb holds apart (`ctx.harness` = placement, `ctx.triggers` =
  auto-update triggers). `actions` — the one `NextAction` construction fn.
- `out` — the process's two STREAMS, and the only place either is written (`outln!`/`errln!`). A
  closed reader (`topos list | head`) is not a failure: the write is dropped and that stream
  remembers it — `println!` would panic there, and the `SIGPIPE` cure needs `unsafe`, which is
  forbidden here. The two latches are SEPARATE, and only stdout's ends the run at exit 0 (its
  reader has the answer); a gone stderr reader silences diagnostics and touches neither the exit
  code nor stdout. Every other write error still panics, as `std`'s macros do.
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
