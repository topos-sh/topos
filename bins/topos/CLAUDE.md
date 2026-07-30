# `topos` — the client CLI

**lib:** the local domain operations, the **sidecar** (an embedded-git store per skill + crash-safe JSON
docs holding identity / per-skill history / mappings), and the bundle scanner — all over a single
fault-injectable fs/syscall seam. **bin:** a thin `clap` wiring; `--json` (no prompts) + a thin TTY
renderer over the SAME typed outcomes (one value, two presentations).

The client runs the **manifest architecture** with UNBLENDED scopes. Two recipes, never mixed:

- **PERSON** — the machine's own `~/.topos/topos.toml` when it exists (COMPLETE: only its rows
  deliver), else the implicit recipe of one FEED row per connected workspace. The feed is a SERVER
  fact — assignments minus declines, computed app-side and served whole by the delivery route — so
  connecting adopts it and there is no client-side arithmetic over it.
- **PROJECT** — the NEAREST `topos.toml` covering the working directory, taken WHOLE (walking up
  only to FIND it; no merging with ancestors, no cross-scope shadowing). A repo fact, identical for
  every contributor.

A **session** (user × workspace × installation, minted by `topos login`) is the standing acceptance —
delivery is silent from login on, npm-style. **The client writes NO server state:** the only
negatives are the web decline (server-side, per person per bundle, the everywhere stance) and the
`"off"` row (machine-local, global file only). There is no subscribe verb, no per-skill consent step,
no profile row op, and no device-link lane; `follow`/`unfollow`/`channel` are gone.

## Implemented (the local core)

- **The fs/syscall seam** (`fs_seam`) — every durable mutation goes through one `FsOps` trait. `RealFs`
  uses `rustix` (safe; no `unsafe`): `F_FULLFSYNC` on macOS, `flock` for the per-skill writer lock, a
  mode-preserving staged write, and a **namespace-atomic directory swap** (`RENAME_EXCHANGE` on Linux /
  `RENAME_SWAP` on macOS) — the primitive a byte-writing *update* uses to overwrite a harness dir. A
  test-only `FaultFs` fails the Nth op for the crash gate. The **private-file primitives** (`write_private`
  0600-from-creation, `private_perms_ok`, `atomic_write_private`, the `read_doc_private`/`write_doc_private`
  pair) carry every secret: `identity/sessions.json` and the login WAL.
- **Crash-safe docs** (`atomic`, `doc`) — atomic write (temp → fsync → rename → fsync-dir; never in
  place) + a fail-closed `schema_version` migration dispatch (an unknown/newer doc is never handed to
  serde and never deleted). The machine-local STATE documents carry that discipline through to their
  own answers: an undecipherable `state/forge_trust.json` grants NOTHING and REFUSES every write
  (typed `UPGRADE_REQUIRED`), and an undecipherable `state/visited_stores.json` contributes no
  recorded store and is never written over — an older binary must not clobber a newer document, and
  "unreadable" must never read as "empty, so re-derive".
- **The sidecar** (`sidecar`) — the `~/.topos/` layout, the `--footprint` walk, the per-skill lock, and an
  idempotent recovery sweep (torn-log repair, incomplete-staging removal, never delete on unknown schema).
  Recovery also deletes the RETIRED identity documents (`device.key`, `instance.json`,
  `credentials.json`, `user.json`, `follows.json`) — the session model's clean break.
- **The I/O scanner** (`scan`) — walks a real skill dir, rejects filesystem-level hazards
  (symlink/device/non-regular/non-UTF-8) before feeding bytes to the kernel digest.
- **The harness adapter wiring** (`config_io` + the `&dyn HarnessAdapter` seam on `Ctx`, selected through
  the `adapter_for(HarnessId)` dispatch) — the Claude Code reference adapter (discovery, adopt-in-place
  recognition, the idempotent `settings.json` SessionStart hook), with the OpenClaw and Hermes arms wired
  (v0's composition root still selects Claude Code). The **breadth arming sweep** (`ops/arm`) additionally
  (un)installs every OTHER detected agent's trigger at the same moments the active adapter is armed (the
  login receipt; `add`'s adopt receipt) and scrubbed (`uninstall --yes`), each row riding the payloads'
  additive `triggers` field honestly.

- **The MANIFEST format** (`manifest/{keys,document,normal,scopes}`) — one `[bundles]` namespace plus
  `[defaults.<kind>]` sections. **THE JOIN RULE:** an entry's reference is its TOML key path under
  `[bundles]`, segments joined with `/`, so a flat quoted key and a grouped section are the SAME entry
  (two spellings of one reference in a file is a parse error; a `[bundles.…]` header is always
  GROUPING, never an entry). Keys classify BY SHAPE alone (`manifest/keys`, no resolution order, no
  fallbacks): `./x`/`../x`/`/abs`/`~/x` a local folder · `github.com|bitbucket.org/<owner>/<repo>` every
  skill the repo holds · `…/<repo>/<skill>` one of them by leaf dir name · `<host>/<ws>` the workspace
  FEED · `<host>/<ws>/<bundle>` one bundle · `<host>/<ws>/channels/<name>` a channel (`channels` is
  RESERVED in position three; a 5+-segment forge key refuses toward the `subdir` field; `gitlab.com`
  refuses plainly). Values type by shape too: `"*"`, a 64-hex version (workspace bundle), a 7–40-hex
  commit (forge), `"off"` (the per-bundle switch — workspace bundles, GLOBAL file only), or an inline
  table (`version`/`path`/`harness`/`name`/`subdir`/`kind`, legality per shape; `[defaults.<kind>]`
  carries `harness`/`path` only). Feed rows are global-only as well — a project manifest is a repo
  fact, so both refuse there toward channels. `parse_input` additionally accepts the CLI sugar (`@ws`,
  `@ws/bundle`, `@ws/channels/x`, bare `owner/repo[/skill]`, pasted `https://` URLs incl.
  `/tree/<ref>/<path>`, a trailing `@<pin>`) and resolves to the same shapes; manifests store only the
  canonical joined form. A subtree URL carrying a PATH names ONE skill, so `add` records the
  4-segment skill row (the leaf name selects) whose fields carry the literal `subdir` and the pin —
  both legal there, neither legal on the repo-set row the grammar canonicalizes the URL to; a path
  holding several skills names none of them and refuses with their names. The `#` sigil is a typed refusal. **The editor** (`document::ManifestEditor`)
  is format-preserving over `toml_edit` with a property-tested INVERSE: set-then-remove restores the
  bytes exactly (it never deletes an emptied section header), which is what makes `add`/`remove` exact
  file inverses. **`fmt_normal`** (`manifest/normal`, the `topos fmt [-g]` verb over `ops/fmt`) renders
  the deterministic, idempotent normal form — feed/local/forge rows flat and sorted, one
  `[bundles."<host>/<ws>"]` section per workspace, defaults last, comments carried — with ONE TOML-forced
  carve-out: a value and a table cannot share a key path, so a workspace carrying its feed row keeps its
  explicit rows FLAT. **Scope discovery** (`manifest/scopes`, pure, no network) loads the two recipes and
  partitions them into the plans the reconcile drives; each scope's delivered set is the union of its own
  rows' resolutions minus its `"off"` switches, deduped by bundle identity (an explicit row's
  version/fields beat a SET's delivery of the same identity; a row is meaningful or inert, never
  harmful — `status` discloses the inert ones). `ops/manifest_edit` picks the file a verb edits (the
  nearest covering the cwd, else a fresh one at the enclosing git root — the npm-init precedent; `-g` the
  global file) and owns FILE BIRTH: any path topos creates is written MATERIALIZED first (the global file
  gets the header plus one feed row per connected workspace, a project file the commented template), then
  the requested edit runs; a hand-written file is never materialized, and a born file STAYS even when a
  gate refuses the act that birthed it (the receipt says so). `init` creates a file without an edit.
  Every manifest mutation is a read-modify-write, so every one of them — `add`, `remove`, `fmt`,
  `init`, login's feed-row append, publish's governance rewrite — takes the file's own WRITER LOCK
  (an `flock` in the home store's `locks/`, keyed by a stable hash of the manifest path) across
  read→edit→write; without it a concurrent process's row is built into a document the other one has
  already replaced, and vanishes with no error anywhere. `remove` holds that lock across the RESOLVE
  as well as the write — its arms (which row, at which value, which split survivors already have
  their own row) are decisions read FROM the file — and re-proves every arm against the file
  immediately before writing: a document that moved under a prepared edit (a person's editor, a
  `sed` — topos's own writers serialize on the lock) earns the typed `MANIFEST_CHANGED` refusal
  with nothing written, never a set line rebuilt from a member list that predates someone else's row.
- **SESSIONS** (`sessions`, `ops/login`, `enroll`) — `topos login <address>` runs against
  `/v1/login/authorize|token` (the constant protocol card re-roots onto the declared API base,
  same-security only; the `0600` WAL holds the flow; re-invoking IS the resume). TWO SHAPES,
  chosen BEFORE the flow starts because the choice IS what the flow is started as: with a browser
  here and a 127.0.0.1 listener bound, the login is **RFC-8252 loopback** — the approval page
  auto-opens, and its redirect hands this listener a one-time authorization code that the
  exchange must present alongside the device code, so a mailed approve-link is worthless to
  whoever sent it and NO code is typed; otherwise it is the **RFC-8628 device grant** — the human
  types the short code, which is what binds the approver to the asker when no local hand-off
  exists. A failed bind says so rather than degrading silently. The granted poll mints ONE
  **workspace-scoped bearer credential** persisted as a session row in `identity/sessions.json` (`0600`;
  statuses `active`/`pending`/`ended`) — login IS the acceptance event (the receipt names what the
  workspace GIVES this person and, when a global manifest exists, appends that workspace's feed row to
  it; no offer step follows). Further workspaces are further logins; `logout
  [<ws>|--all]` runs the server-side self-end (`DELETE /v1/session` under that session's OWN credential;
  the uniform 404 = already ended) then deletes the local row — skills, drafts, and manifests stay. A
  session the server no longer answers (owner-ended, seat removed, workspace gone — indistinguishable)
  is marked `ended` locally by the sweep, prints its ONE typed `SESSION_ENDED` line, and freezes that
  workspace's items in place; `login` reconnects.
- **The RECONCILE** (`ops/reconcile` — what `update` runs) — dial each live session's
  `GET /v1/workspaces/{ws}/delivery` ONCE (a pending session skips quietly; NotFound = the ended-session
  line + local flip; ANY no-fresh-delivery fault (unreached · unsuccessful exchange · unreadable answer)
  degrades that workspace to the OFFLINE CACHE so the local converge keeps working), load the TWO scope
  plans, then converge each scope on its own recipe. Within a scope the resolution order is explicit
  THINGS, then SETS (channel, repo), then FEED rows — an explicit row's version/fields beat a set's
  delivery of the same identity, and a set delivers its curator's current truth. Per row kind: a FEED
  row expands to the delivery's pre-resolved targets (installed SILENTLY under their catalog names —
  login was the acceptance, and a bundle the person DECLINED on the web is simply absent from it); a
  workspace bundle or channel row resolves through that workspace's session (catalog index / channel
  index; a row pin overrides the served current, falling back honestly when the pinned version is gone,
  and a row naming a declined bundle still delivers with the decline disclosed — a file is a machine
  fact, the decline is a server fact about the feed); a forge row resolves against ITS SCOPE'S OWN
  store (a project row's tracked import lives in the checkout's `.topos/` store — two checkouts of one
  repo row never share state) and NEVER first-installs an ungranted origin: the gate is the MACHINE's
  trust registry (`state/forge_trust.json`, home sidecar — `crate::forge_trust`; a one-time legacy seed
  from the home store's own imports), consulted in EVERY scope, and an origin it has not granted
  refuses with one typed line naming the `topos add … --yes` gate — neither a manifest row NOR
  per-checkout store contents vouch (both are repo facts anyone could have committed: demand, never
  consent); a GRANTED origin's rows advance ONLY on an explicit `update` (never on the
  quiet sweep — no session start dials a forge), receipted with the commit motion and the member delta,
  members the new archive dropped cleaned snapshot-first in the same run. A row's `harness` list (a
  `-a` selector import's recorded placement decision) is CONSUMED here: one converge slot per named
  agent, each paired with the copy already placed under that agent's skills root, so a refresh keeps
  the bytes where they were asked for instead of re-landing them in the default dir (no field = one
  default slot = the old behavior). A PINNED repo set is
  settled only when the tracked members cover the member set RECORDED at landing (`origin.json`) —
  "every tracked member is at the pin" is true and useless after a partial landing, and would leave
  the missing members unreachable forever; an import that recorded NO member set is not evidence of
  completeness either ("nothing missing" ≠ "nothing known"), so it is unsettled: the next explicit
  update refetches ONCE, writes the archive's member list onto those imports, converges, and rests
  from then on (the quiet sweep still never dials — it carries no git source at all). A local-path row is an
  adopt-in-place presence check (and the idempotent converge of a landed publish's PENDING governance
  rewrite). A manifest the grammar refuses FREEZES its whole scope — no delivery, no cleaning: a typo
  must keep bytes, never drop them. Placement is PER-SCOPE (see below); after the fan-out
  `clean_undemanded` retires what its scope no longer demands (snapshot-first, resetting to
  never-received; each scope consults ITS OWN mention set — a project mention never shields a
  person-scope clean — and a dropped repo row's members retire like any undemanded item) and each
  session gets the applied report (`PUT …/report` — complete-state per session, across the home store
  AND every visited project store: this run's cwd chain unioned with the machine-local
  **visited-stores index** (`state/visited_stores.json` — every project store a reconcile visits is
  recorded, so an update run from one checkout still reports another checkout's holdings; a store
  that is gone, or whose placements are, drops out naturally on read; the whole read→union→write
  runs under the index's own `locks/visited-stores.lock`, because two sweeps from two checkouts are
  the ordinary case and an unserialized union simply drops the other one's checkout), manifest-row
  deliveries included — declined-but-locally-added
  bundles report too, which is what makes the web's declined-but-applied disclosure real). The wire
  carries exactly ONE row per (session, bundle), so which store answers is DETERMINISTIC and stated:
  the person store whenever it holds the bundle, else the project stores in ascending path order —
  never "whichever checkout this run happened to start from" — and a bundle held at DIFFERENT
  versions in more than one store keeps that row and earns one `VERSION_SPLIT` line naming the other
  holder (the same fact `status`'s cross-scope note makes, said where the pick is made). The **delivery
  cache** (`state/sync_status.json`) records host/workspace_name per workspace + name/review_required/
  served_version per skill PLUS each row's attribution and the caller's declines, so `status`/`list`
  answer offline and `CacheFollow` (the FollowSource over the cache) + `SessionRoutedPlane` (the
  PlaneSource routing each skill to its session's lane) wire the composition root. Notices ACK by id
  after an interactive `update`; the quiet hook fetches without acking.
- **The verbs** (`ops`) — `add <source> [-s <name>] [-a <slug>] [-g]`: ONE source-polymorphic positional —
  a workspace-shaped reference (`@ws/name`, `@ws/channels/x`, canonical `host/ws/name`, a bare catalog
  name when sessions exist) records ONE row in the nearest manifest — or, `-g`, in the global file —
  and DELIVERS in the same invocation; `add -g @ws` adopts a whole FEED (global only: a feed row is
  personal by nature, so a project file refuses it toward channels); `add topos` is the built-in's
  restore; a PATH adopts a directory in place (mint id+name, scan + import, stage + publish with one
  rename — all-or-nothing; the manifest records the dir-relative `./path` row; recognize a Claude Code
  skill dir, tag it + arm the hook); a bare NAME with no sessions resolves against `list`'s untracked
  discovery; a FORGE reference is fetched read-only + imported (`add_remote` over the injectable
  `GitTarballSource`, `..`/symlink-safe, `origin.json` provenance adjunct). Three `-g` arms are pure
  disclosure rather than a write: an add the FEED already delivers writes nothing and says so; an add
  over a standing `"off"` row DELETES the row (never a stacked positive); an explicit pin or field set
  converts a feed-delivered bundle to machine-local control. FIRST TRUST is the one gate here, and it
  is a MACHINE fact: a forge origin this machine's trust registry (`state/forge_trust.json`, home
  sidecar) has not granted is DESCRIBED (source, discovered members, the exact row and file) and
  applied only under `--yes` — neither a manifest ROW nor per-checkout store contents vouch (both are
  repo facts anyone could have committed; a describe over an already-recorded row says so), and a
  granted origin applies immediately with an undo-led receipt. A forge apply records the CONSENT first
  (the origin lands in the registry at the consent moment), then the ROW (the
  demand), then installs member-by-member into the scope's store — a member failure never unwinds the
  batch: the receipt names the partial landing and the recorded row is what the next update converges;
  each import also records the archive's MEMBER SET at that commit in `origin.json`, which is what
  later lets a pinned repo-set row tell a partial landing from a complete one. The `-s`/`-a`
  SELECTORS (single, several, or the `*` fan-outs) ride EXACTLY that gate and that store
  (`add_forge_selected`): a selector narrows which members land and into which agent dirs, never
  whose bytes these are — so an ungranted origin describes (members + where each lands) and applies
  only under `--yes`, ONE fetch serves every (skill × harness) combination, and a project-destined
  import writes the checkout's own `.topos/` store, the one the project reconcile converges. The
  `-a` selection is RECORDED on the row (`harness = [slugs]`) — a placement decision only the
  invocation remembers is no decision at all, and the reconcile's forge arms plan against it. The
  destination is PARKED, then judged, immediately before the rename that lands it: the emptiness
  check ran before the staging window, so the dir is moved aside (a rename has no window) and read
  there — empty or byte-identical drops, anything else is put back whole with `PLACEMENT_OCCUPIED`,
  never deleted on a stale observation. Re-adding an EXISTING row keeps the exact-inverse discipline: the same value is the redundancy
  disclosure (nothing written, no undo); a different value applies with the prior value named and the
  undo offered ONLY where it verifiably restores it (`add <ref>` for a prior `"*"`, `add <ref>@<pin>`
  for a prior pin; a prior fields table gets NO undo, said why). `remove <targets…> [-g]` is the EXACT
  FILE INVERSE (property-tested): drop
  the row; or, `-g`, write `"off"` when the feed still provides the bundle, or drop a FEED row whole; a
  set member's removal is the set-minus-one rewrite (that one line replaced by its current members —
  explicit beats set: a survivor that already has its OWN row keeps it untouched, and new rows are
  written only for the rest, each carrying the set line's pin/fields where the member's shape legally
  takes them; the describe names which survivors get new rows, which keep their own, and anything not
  carryable), offered as a describe because later curation stops arriving through
  this file. A set line is split exactly ONCE per invocation: several targets inside one set resolve
  against it together (members minus ALL of them), because two sequential splits would each rebuild
  the line from its full member list and the second would write the first's removal straight back.
  A bare NAME is resolved by GATHERING every candidate first — the file's feed row, its explicit
  rows, the bundles the feed delivers, and every SET line's current members — and then counting: one
  answers, several earn the `AMBIGUOUS_NAME` refusal listing the qualified references (a set member
  is qualified by its own reference AND the line it comes from, because two channels carrying one
  bundle are two different edits and splitting whichever expanded first leaves the other delivering
  it). An EXACT spelling is always an answer, never an ambiguity, which is what keeps the refusal
  actionable; one identity reached two ways (its own row and the feed that also delivers it) is ONE
  candidate. Its two-phase arms are LOSS (an affected bundle with unshared edits, or an unclassifiable
  scan — the guard fails TOWARD the gate, and it covers EVERY placement-retiring arm: the row drop, the
  feed-row drop, and the `"off"` switch alike) and the set split; everything else applies immediately,
  and the receipt offers the literal inverse ONLY when it restores the whole prior state. A tracked never-published local (or an untracked
  agent-dir copy) keeps the two-phase permanent delete; `remove topos --yes` is the built-in's durable
  opt-out. `fmt [-g]` writes the normal form (validating the whole document first — it never launders a
  malformed file). `update [<target>…] [--quiet]` is the reconcile above (targeted forms narrow it;
  `--reset` keeps the loss-led two-phase discard; `--rebuild` absorbs drafts then re-projects every
  managed folder; `--onto-current` keeps the per-skill engine's escape). `list [--footprint] [--tracked]
  [--remote]` (tracked bucket + untracked discovery across the baked ~73-harness registry; `--remote`
  reads each session's catalog, session-routed). `diff`, `log` (the plane half rides the skill's session
  lane), `init`, `status` (below), `uninstall [--yes]` (two-phase teardown: hook scrub + sidecar delete;
  the sessions go with it; skill files stay).
- **The placement engine** (`placement`, `topos-harness::{coverage,registry}`) — WHERE a managed skill's
  bytes land, computed each sync FOR ITS SCOPE. **Person scope** (`plan_for_skill`/`plan_targets`) is
  shared-dir-first over the home: one `~/.agents/skills` copy where a detected harness is covered, plus a
  native user-dir copy per detected-but-uncovered harness (the active adapter keeps its richer
  `placement_for`); prior-(kind, agent) stability never reuses a dir recorded INSIDE a project checkout
  (`under_project_manifest` — the mirror of the project plan's project-local rule). **Project scope**
  (`project_plan`) mirrors the policy ROOTED AT THE CHECKOUT: `<proj>/.agents/skills` for covered
  agents, the registry's project dirs for the rest, the Claude-Code-shaped `.claude/skills` default when
  nothing is detected; a manifest `path` field pins ONE project-relative dir instead (the row's own
  field beats `[defaults.<kind>].path` beats the registry mapping, and within each level a
  harness-named key beats that level's `default`); **containment is a property of EVERY project-scope
  path, not just the override** (`within_project`): each candidate root — `.agents/skills`, each
  registry project dir, the `.claude/skills` default, and the `path` override alike — must have no
  symlink component below the checkout AND canonicalize inside it, or that root is REFUSED with a
  `PLACEMENT_ESCAPES_PROJECT` line and the placement is skipped, never redirected (a repo can commit
  `.claude/skills` as a symlink exactly as easily as it can write `path = "../.."`) — and a REUSED
  prior path passes the same proof, because a record is a memory, not a permission: the checkout can
  turn a recorded ancestor into a symlink long after the record was written, and a lexical
  `starts_with` would still call it inside; every landed
  project dir SELF-IGNORES (the staged tree carries the exact sentinel `.gitignore` unless the bundle
  ships its own — the node_modules model; the scanner treats the byte-exact sentinel at the bundle
  root as topos metadata, so it never reads as an edit; no repository file is ever touched).
  Placements are reconciled each sync (new targets appended and converged from the LOCAL store; a
  record leaves only through an explicit act; detection loss freezes, never deletes). `map.json` is
  schema v2 (per-placement `placement_state` rows).
- **The PER-SCOPE stores** (`sidecar`) — ONE `Layout` shape serves both scopes: the person scope's
  `~/.topos/`, and each project's OWN store at `<project>/.topos/state/<user>/` (per-user because
  checkouts are shared; `.topos/` ignores itself whole, venv-style, via an exclusive-create
  `.gitignore` of `*`). The project store passes the SAME containment rail its placements do: a
  `<project>/.topos` that does not resolve inside the checkout is refused by both the write
  (`ensure_project_store`) and the read probe (`existing_project_store`), so a committed symlink can
  never route this machine's engine state — or the bytes that flow through it — out of the tree.
  The reconcile routes each item's engine run through its scope's layout —
  the same bundle followed at both scopes has two state trees, two drafts, two baselines, with no
  cross-scope anything — and recovery sweeps a project store lazily when a run visits its project.
  Pre-1.0 handover: home-map rows pointing into a visited project are dropped (bytes in place,
  agent-less rows and external imports excepted; a project-only home entry retires whole) and the
  project pass adopts what it finds — byte-identical copies in place, a divergent occupant
  namespaced beside, disclosed, never clobbered. The write verbs resolve names across the stores
  (`resolve_skill_stored`: home first, then the cwd chain's project stores), so a project-delivered
  draft still publishes from inside its checkout.
- **The pull/apply sync engine** (`ops/sync_engine`, `materialize`, `plane`) — the `checkForUpdates →
  plan → apply` machine over the kernel's four-state transition, now PLAN-THREADED: `sync_one_planned`
  takes an optional `PlanFn` so the reconcile drives per-scope placement through the ONE engine (no
  fork). The served record IS the sync target (adopted in ANY direction); draft snapshot-on-touch;
  fetch + re-verify (digest == tree == `commit_id`); crash-safe dir-swap materialization into every
  managed placement; the ancestor backfill shallow-stops at purged history. **No byte differing from
  its recorded baseline is destroyed unless a snapshot taken AFTER the last revalidation holds it:**
  the pre-swap scan decides, but the capability probe and the staging build take real time, so the
  materializer re-fingerprints the dir (stat-only — `(mtime_ns, ctime_ns, size)` per entry, O(stat),
  never a second hash) immediately before the swap; a tree that MOVED in that window is re-snapshotted
  where a snapshotter exists and otherwise SKIPPED (its recorded state stays behind the bytes, so the
  next sweep reconciles from a fresh scan). That is the DECISION rail, and a decision must precede
  the mutation — so the residual window it cannot close (between the stat and the syscall) is closed
  from the other side: **PARK-THEN-VERIFY** is the one primitive every destructive path uses now
  (`materialize::park_aside` / `restore_parked`). Checking harder never closes a window; a rename
  has none. The swap PARKS the old tree (the exchange leaves it at the staging path, the
  rename-dance at the graveyard) and the park is READ before it is dropped — bytes that are the
  target, this placement's recorded baseline, or an already-taken snapshot go; anything else is
  snapshotted first (and, with no snapshotter at all, moved to a `.topos-kept-*` sibling no litter
  sweep touches). The retiring clean parks each placement aside and reads THAT tree — an
  unreadable park goes back and the clean refuses, never removes blind. A remote import parks the
  destination before the landing rename: empty or byte-identical drops, anything else is restored
  whole with the typed `PLACEMENT_OCCUPIED`. The forge refresh's stash IS its park (and the whole
  replacement now runs under the skill's writer lock): each stashed dir is re-read, and a local edit
  that arrived after the classifying scan restores every stash and refuses, exactly like an edit
  found up front. The TWO DETECTORS are
  split: DRAFT reads each copy against the pristine version (one distinct edited content per
  bundle+scope is THE draft), CONFLICT reads the edited copies against each other — competitors
  ONLY when neither's bytes equal the other's RECORDED baseline (a copy at a sibling's baseline is
  stale behind its draft, a sync target); only true competitors freeze typed with `update --reset`
  the way out. The SETTLED-DRAFT fan-out: a draft unchanged across two runs (`sync.json`'s
  `draft_observed` observation) is copied onto the bundle's other placements in its scope — an
  ordinary atomic swap per stale/clean sibling, each landing recorded as that placement's NEW
  baseline (a later edit there is a fresh draft, never a false competitor), the swap re-stat
  SKIPPING a dir whose bytes moved in the window; disclosed as one `draft_synced` receipt line, and
  it counts as a changed sweep for the hook.
- **The author-merge resolution** (`ops/merge_resolve`) — resolves a DIVERGED draft behind the
  `DivergedWitness` capability token (followers never reach merge code). Kernel-planned diff3; a clean
  merge lands a publishable draft-on-current; a conflict writes the complete marker tree + the durable
  `conflict.json` (publish-block + recovery journal); the disclosed escape (`--onto-current`) always
  works offline. An `auto` item's bare sweep resolves unattended.
- **The transport** (`plane_http`) — blocking `ureq` (rustls+ring). `UreqPlane` (conditional `current`
  GET, verified per-blob bundle fetch, delivery + report) and `UreqDeviceClient` (the directory reads,
  the contribute writes, invitations, notices, `DELETE /v1/session`, and the creds-free login flow —
  there is NO row-op call left: the retired `…/profile*` paths answer the uniform wire 404, and every
  positive/negative stance is a web act) — every credentialed call under the SESSION's workspace-scoped Bearer. The
  composition root builds ONE transport set per session (`SessionTransports`
  {plane, directory, contribute, governance}); `SessionUniverse` assembles the resolver universe from
  each session's own reads; `resolve_session_lane` picks a verb's write lane (the delivered skill's
  workspace wins via the cache, else the one resolver).
- **The contribute write verbs** (`ops/{publish,review,revert}` + `ops/contribute` + `op_wal`) — op-WAL
  idempotent retry (the same `op_id` replays byte-identically; `OpRecord` carries the GitHub
  `upstream` provenance so a crash replay keeps the identity); the all-outcome 200 envelope mapping;
  I-COMMIT-PARITY via the kernel. **`publish [--propose] [--to <channel-ref>] <target>[@<digest>]`**
  auto-adopts an untracked local source, scans the draft (draft-anywhere across placements), publishes
  through the session lane — and a LANDED publish of a path-ref item runs the **governance transfer by
  default**: catalog entry (or proposal, per the target's protection), the local copy becomes a managed
  placement, and `rewrite_to_governed` flips the manifest's path line to the canonical workspace
  reference (`PublishData.manifest`/`reference`/`converted_from` disclose it; the PROPOSAL arm
  transfers too — delivery follows approval — and an imported bundle's receipt discloses that the
  GitHub origin-pin line is NOT rewritten). `--to` accepts channel references (workspace-checked)
  and must name an EXISTING channel — verified on the describe and the apply alike, never a silent
  server-side mint (a nonexistent channel refuses toward web curation; the workspace slug gets the
  pointed near-miss); a curated `everyone` withholds a member's default placement, disclosed. A
  logged-out publish refuses typed toward `topos login` — every session-required refusal also
  carries the concrete `topos login <address>` as an executable next action. **`review`** (inbox across
  sessions; describe; approve/reject-with-reason/withdraw — the outbox split by the server-computed
  `yours`) and **`revert --to <good>`** (the forward commit) ride the same lanes.
- **`protect`, `invite`** — `protect <target> [<level>]` two-phase over the session universe (skill →
  `reviewed`, channel → `curated`; explicit `open` loosens, owner-gated server-side; the skill describe
  carries reach). `invite <emails…>` (owner-only server-side; the mailed single-use link is the token's
  only channel; at most one `--skill`/`--channel` first-destination hint; a bare `invite` is the
  no-mutation `/me` read).
- **`auth status`** (`ops/auth`) — the one remaining `auth` subcommand (sessions are managed by the
  top-level `login`/`logout`): per-SESSION access health via a `me` probe under that session's own
  credential ("pending — awaiting owner approval" via the served `session_status`; the uniform 404 reads
  "no access — ended, removed, or gone"), hook health, and the reporting posture.
- **The `status` verb + the bare `topos` orientation** (`ops/status`) — the offline trust rail,
  SECTIONED per scope because scopes are unblended: the nearest project manifest, then the person
  scope (the global manifest when it exists, else the implicit feed rows materialized from the offline
  delivery cache), plus each connected workspace's REGIME. Per line: the winning reference, ONE source
  label (which row — or which workspace's feed — asked), the delivery attribution (`assigned by
  <name>` / `picked by you`), and an HONEST state — `applied as of <last sync>` with the version (the
  cache's stamp, never a live claim), local-edits, behind, off, not-available/pending from the local
  session file. What a row cannot carry rides `notes`: a global manifest that does not adopt a
  workspace's assignments, rows that add nothing, `"off"` switches nobody assigns any more, set
  collisions, declined-but-delivered bundles, cross-scope version splits. `status <bundle>` is the
  deep single-bundle answer (the exact row and file, or the feed; where its files are). Also per-agent
  trigger state probed read-only, and the binary version. No network, no writes; it dispatches ahead of
  the recovery sweep and leaves a pending-recovery sidecar byte-identical. A bare `topos` on a TTY
  renders the same snapshot (a fresh machine gets the welcome); piped bare invocations keep the usage
  error.
- **The hook posture + notices** (`ops/reconcile`, `sync_status`, `ops/quiet_gate`) — the reconcile
  stamps `state/sync_status.json` on every delivery/report; `update --quiet` (the session-start hook
  path) self-throttles (single-flight lock + TTL, default 300 s) and stays near-byte-silent. ONE
  renderer (`hook_output_json`) decides its stdout in every case: at most ONE SessionStart hook
  JSON, in the DIALECT the calling trigger declares with the hidden `--hook <harness>` arg, and
  NOTHING when there is nothing to say. Two INDEPENDENT axes drive it — `changed` (bytes actually
  moved) is the only thing that may set `reloadSkills`, and the person-facing facts (the
  ended-session freeze, no-fresh-delivery-AND-stale) ride `additionalContext` whenever they exist,
  changed or not. The staleness half is a CONDITIONAL line: a workspace that got no fresh delivery
  warns only once its own recorded window is blown, so the sweep hands the hook both halves of that
  workspace's identity — the id the freshness cache is keyed by, and the name the line says — plus
  WHICH fault it was. All three faults (the plane could not be dialed; the exchange did not land —
  a failure status, or an answer that never fully arrived; a COMPLETE answer whose structure is
  wrong) degrade the sweep identically and are equally stale, but the line names the real one, and
  each clause must hold for its whole variant: blaming the network for garbled bytes — or the
  server for a truncated read — sends a person the wrong way.
  Raw text is never used: `additionalContext` is what INJECTS into the session,
  and a harness validating hook stdout against a strict schema may discard anything else — a fact
  a person must not miss cannot depend on that. So `--hook claude-code` (the marker that harness's
  own registered hook command carries) speaks when either axis is live, carrying `reloadSkills`
  only on a real byte change; unmarked and any unrecognized name (it fails closed, never errors)
  is the schema-conservative dialect — those two keys only, ever, so it speaks only when there are
  lines. An auth/transport failure warns and exits 0; a genuinely local failure exits nonzero.
- **The BUILT-IN `topos` skill** (`ops/builtin`, `cli_ref`, the repo-top-level `skills/topos/` source) —
  the meta-skill teaching an agent the manifest surface, the contribute loop, distillation, and the
  team-genesis runbook. The binary embeds the three files (`SKILL.md` + `INSTALL.md` + the generated
  `reference.md` = the same bytes as `docs/cli.md`, one renderer `cli_ref::cli_ref_md()`, both copies
  drift-gated) and places them through the ORDINARY engine at the trigger-arming moments, force-synced
  on every bare sweep (a change counts as a changed sweep for the hook document above). A
  pre-existing `topos` dir is never written by the
  sweep; a marker-carrying downloaded copy is adopted snapshot-first by the explicit **`add topos`**
  restore (which also clears the durable **`remove topos --yes`** opt-out). NOT a subscription — no
  manifest row, the plane never hears of it; the name is reserved end-to-end.
- **The `self-update` maintenance command** (`ops/self_update`, `release`) — resolve the target release
  (latest, or a `--version <tag>` pin), download + the **release-signature gate** (`RELEASE_PUBKEY`
  compiled in: the `.minisig` is mandatory + fail-closed, the SIGNED trusted comment must name the
  exact tag + asset), verify the sha256 manifest, extract in memory, atomically replace the running
  binary. Injectable `ReleaseSource` seam; `--check` reports and stops.
- **The passive version check** (`ops/version_check`) — after a successful eligible command, at most
  once per day, probe the public releases redirect on a 2 s timeout and print ONE newer-version line on
  stderr. Quiet by construction; the first eligible command only lays the stamp; the quiet sweep,
  self-update, uninstall, mirrors, and `TOPOS_NO_UPDATE_CHECK=1` all skip it.
- **The envelope's agent ergonomics** (`actions`, the budgets/pagination) — byte budgets (`diff`/
  `review --max-bytes`, file-boundary truncation + `FETCH_FULL_DIFF`), `list`/`log` pagination with
  typed truncation + `NEXT_PAGE`, next-action safety metadata (`mutates`/`needs_network`/`risk_note`
  from the ONE rules module), and the in-memory `merge_preview` on diverged rows and behind-copy
  publish describes.

Identity is the kernel's: `version_id`/`bundle_digest` depend only on the bytes + device id + a fixed
message, so injectable id/time sources make `add` deterministic. Golden `--json` fixtures are asserted
byte-equal in tests; the composed-e2e fixture rig is `test_support::SessionInstall` (feature
`test-fixtures` — the session-model client over the genuine `ureq` transports).

## Planned (lands later)

**Multi-reviewer governance** (reviewer roles / N-approver / a rendered diff UI — single-approver,
plain unified diff only) + the **`review-required` policy toggle verb** (enforcement is built; the
policy row is a web setting) + `log --team`'s plane half; harness *selection* in the composition root
(v0 constructs Claude Code only; the OpenClaw and Hermes adapters are built + wired into
`adapter_for`); per-workspace short-lived credentials and the further session hardening stay journaled
design work. The identity step (sign-in) runs on the server's approval page (the agent only polls), so
the client needs no UI for it.

## Architectural layering (enforced at the dependency graph)

**No edge to `plane-store`, no `sqlx`, no `libsqlite3-sys`.** The client is a thin sync tool, never an
authority — a per-target `cargo tree -p topos` assertion (`cargo xtask check-arch`) holds the line.

The sidecar keys skills by id; harness skill directories stay byte-pristine, so uninstall is a no-op for
your skills.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `topos-harness`, `clap`, `serde`/`serde_json`,
`uuid`, `rustix` (safe fsync/flock + the atomic dir-swap; `system` for the host node name the login
start sends as the requested machine display name), `hex`, `base64` (wire encoding only), `ureq` (the
blocking rustls+ring transport — self-contained, so no `tokio`/`plane-store`/`sqlx` edge),
`toml_edit` (the manifest editor — comment-preserving), `tar`+`flate2` (the self-update asset codec +
the GitHub import, in-memory), `minisign-verify` (the self-update release-signature VERIFIER; signing
exists only in CI and the test-only `minisign` dev-dependency), `anyhow`, `thiserror`. No signing key
material client-side: a session authenticates with the workspace-scoped bearer credential the login
flow mints, and the only key the binary carries is the PUBLIC release key (`RELEASE_PUBKEY`). None of
these crates cross `check-arch`'s line; `topos-core` is signature-free `no_std`.
