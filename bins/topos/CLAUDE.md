# `topos` — the client CLI

**lib:** the local domain operations, the **sidecar** (an embedded-git store per skill + crash-safe JSON
docs holding identity / per-skill history / mappings), and the bundle scanner — all over a single
fault-injectable fs/syscall seam. **bin:** a thin `clap` wiring; `--json` (no prompts) + a thin TTY
renderer over the SAME typed outcomes (one value, two presentations).

## Implemented (the local, accountless core)

- **The fs/syscall seam** (`fs_seam`) — every durable mutation goes through one `FsOps` trait. `RealFs`
  uses `rustix` (safe; no `unsafe`): `F_FULLFSYNC` on macOS, `flock` for the per-skill writer lock, a
  mode-preserving staged write, and a **namespace-atomic directory swap** (`RENAME_EXCHANGE` on Linux /
  `RENAME_SWAP` on macOS) — the primitive a byte-writing *update* uses to overwrite a harness dir. A
  test-only `FaultFs` fails the Nth op for the crash gate.
- **Crash-safe docs** (`atomic`, `doc`) — atomic write (temp → fsync → rename → fsync-dir; never in
  place) + a fail-closed `schema_version` migration dispatch (an unknown/newer doc is never handed to
  serde and never deleted).
- **The sidecar** (`sidecar`) — the `~/.topos/` layout, the `--footprint` walk, the per-skill lock, and an
  idempotent recovery sweep (torn-log repair, incomplete-staging removal, never delete on unknown schema).
- **The I/O scanner** (`scan`) — walks a real skill dir, rejects filesystem-level hazards
  (symlink/device/non-regular/non-UTF-8) before feeding bytes to the kernel digest.
- **The harness adapter wiring** (`config_io` + the `&dyn HarnessAdapter` seam on `Ctx`, selected through
  the `adapter_for(HarnessId)` dispatch — one match arm per harness) — `topos`
  drives `topos-harness::ClaudeCode` for discovery, adopt-in-place recognition, and the session-start
  auto-update hook. The adapter owns the strict-JSON `settings.json` merge; the durable write goes through a
  small `ConfigStore` port implemented here, which reuses the one `atomic_write` dance over `FsOps` (so
  the existing crash gate covers the config write too — never a second atomic-write to drift). The
  foreign-file writer adds the care a shared user file needs: ensure the parent dir, write through a
  symlink, a topos-namespaced temp, best-effort mode preservation. `RealFs` also implements the
  `CommandRunner` port (argv-only `std::process` spawn, output captured) — OpenClaw's cron trigger
  drives its own `openclaw` CLI through it. The **OpenClaw and Hermes arms** are
  wired too (`topos-harness::OpenClaw`: the two ports; `topos-harness::Hermes`: `$HERMES_HOME` + the
  `HERMES_ACCEPT_HOOKS` evidence resolved at construction), though v0's composition root still selects
  Claude Code only — harness *selection* lands later (the TTY receipt copy already branches on the
  report's `currency_kind`, so no surface overstates a sibling adapter's update moment). **The
  breadth arming sweep** (`ops/arm`, run at the composition root — the one layer holding the real
  ports + `$HOME`) additionally (un)installs the trigger of every OTHER detected agent at the same
  moments the active adapter is armed (the enrollment receipt; `add`'s adopt receipt) and scrubbed
  (`uninstall --yes`): the nine registry-slug trigger adapters (`topos-harness::triggers`) plus the
  two non-active sibling `HarnessAdapter`s, each row riding the payloads' additive `triggers` field
  honestly (evidence-gated Active; consent notes; degraded floors) and rendered on the TTY receipts.
- **The verbs** (`ops`) — `add <source> [--skill <name>] [--harness <slug>] [--global]` (**one
  source-polymorphic positional**, classified by shape in `crate::source`: a PATH (`./ ../ ~/ /`) adopts a
  directory in place; a bare NAME (optionally `<skill>@<harness>`) resolves against the same untracked
  inventory `list` discovers — `resolve_add_target`: `@<harness>` disambiguates a name found in more than
  one harness, a name under several dirs of one harness is a typed `AMBIGUOUS_SCOPE`; a REMOTE
  `owner/repo`/`owner/repo#<ref>`/github.com URL (incl. a `/tree/<ref>/<subdir>` URL) is **fetched +
  imported** by `add_remote` — a `.tar.gz` over the injectable `GitTarballSource` seam
  (`plane_http::UreqGitSource` = GitHub's public tarball endpoint), extracted + `..`/symlink-safe in
  `crate::git_source`, one skill selected (`--skill` picks from a multi-skill repo; a lone skill
  self-selects; several is typed `AMBIGUOUS_SKILL`), landed byte-exact into the destination harness dir
  (`registry::skills_root`; default the active harness, `--harness`/`--global` steer it) without clobbering
  a foreign dir (`PLACEMENT_OCCUPIED`), then adopted through the SAME core with a best-effort
  `origin.json` provenance adjunct (repo/commit/subdir/license — never injected into the bundle);
  fully non-interactive, no disclosure gate — the source's trust is the user/agent's to verify); then the
  one adoption path: mint id+name, scan + import, stage + publish with one rename — all-or-nothing;
  **recognize a Claude Code skill dir, tag it + arm the auto-update hook**; refuse re-adopting an
  already-tracked dir with `ALREADY_TRACKED`), `follow` (the device-flow enrollment + first-receive — see
  below), `invite` (the roster write — see below),
  `list [--footprint] [--tracked] [--remote]` (the tracked bucket + **untracked discovery** — skills sitting
  in any known harness's skill dir, across a baked registry ported from `vercel-labs/skills`, deduped against
  tracked placements by canonical path; `--tracked` suppresses discovery; `--remote` is the **catalog read** —
  a `GET /v1/workspaces/{ws}/skills` (under the device Bearer credential) per followed workspace, merged
  with local follow-state
  (Available / Following / FollowingBehind), a per-workspace transport fault degrading to a warning;
  `followed`/`published_by_you` still render empty; footprint = the `~/.topos/` walk plus any harness config
  topos holds an entry in), `diff`
  (draft↔current via the gitstore `unified_diff` renderer), `log` (local actions + git history), `pull
  [<skill>[@<hash>]] [--quiet]` (the session-start auto-update entry point — see the sync engine below),
  `uninstall [--yes]` (the two-phase MAINTENANCE teardown: bare describes; `--yes` **scrubs the auto-update
  hook** via the adapter's `remove_currency_trigger` then deletes the `~/.topos/` sidecar tree — the
  signed-in credential goes with it — leaving every SKILL FILE in the agent dirs untouched; the `topos`
  binary is NOT self-deleted, only its path disclosed with a remove-it-yourself note; needs no sign-in,
  mints no identity).
- **The resolution grammar** (`resolve`) — the ONE grammar every verb turns an argv token through:
  full addresses (`https://topos.sh/acme[/channels|skills/<name>]`), qualified paths
  (`acme/channels/eng` — three segments with the literal middle), bare names, the `<name>@<agent>`
  local domain, and the `owner/repo` `add`-lookalike (exactly TWO segments — refused toward `topos
  add`, never half-resolved). Resolution runs against the enrolled universe (address names from
  `/me`, channel names, catalog skills); a name with several meanings is a typed `AMBIGUOUS_NAME`
  carrying PASTE-READY qualified paths (machine-readable as the envelope's `data.candidates`); an
  out-of-scope kind refuses toward the right spelling; a batch resolves ALL-OR-NONE
  (`--channel`/`--skill` selectors force kinds); and the ONE uniform not-found mirrors the plane's
  non-answer ("not found, or is not visible to you…" — no enumeration oracle on either side).
  Unit-tested against a fixture universe (cross-workspace collisions, kind collisions, kind
  mismatches, the all-or-none batch).
- **The `follow` verb** (`ops/follow`, `enroll`, `plane_http::UreqDeviceClient`) — enrollment + the
  TWO-PHASE subscribe. Dispatched by shape (a pending WAL always wins — "re-invoking IS the resume";
  a KNOWN followed skill name wins over the address grammar; a retired `/i/` invite link refuses
  typed toward the workspace ADDRESS):
  - **the ADDRESS flow** (`follow <workspace>`, `<server>/<ws>`, a bare SERVER origin
    `<server>` / `https://topos.example.com` / the schemeless `topos.example.com` — "the workspace
    that origin addresses", the single-tenant install form, an empty `workspace` on the wire,
    `acme/channels/eng`, bare channel/skill names; a schemeless DOTTED first segment reads as an
    `https://` address, the dot disambiguating a host from a slug or an `owner/repo`): an unresolved
    workspace-shaped single target fetches the constant
    **protocol card** at the address — the bare ORIGIN when no slug was given (the card is constant
    on every path, so no existence signal), re-roots onto its declared `api_base_url` (same URL gate,
    https-never-downgrades), guards one-plane-per-install (the wrong-server refusal NAMES the
    `TOPOS_HOME` second-install hatch), starts the gh-style device flow
    (`POST /v1/device/authorize {requested_name: "topos CLI (<hostname>)", workspace:
    <address-name-or-empty>}` — an empty workspace names the origin's own workspace), and persists a
    ONE-phase `0600` WAL
    carrying the FOLLOW INTENT + the secret device code. A re-invoked `follow` polls
    `POST /v1/device/token` once: pending re-emits the server-built approval URL verbatim; denied /
    expired sweep the WAL typed; GRANTED carries the device's ONE bearer credential (the promoted
    device code), the registered device id, and the AUTHORITATIVE workspace context — the persist
    writes `instance.json` → `credentials.json` (whole) → the `user.json` membership → deletes the
    WAL → arms the auto-update trigger, and the flow CONTINUES into the intent's describe/apply in the
    same invocation. There is no post-grant fence phase: an approved flow re-answers the same
    granted poll, so a crash mid-persist recovers by re-polling;
  - **the classic skill path** (`follow <skill>[@<hash>]`) — the I-TOFU accept / the paused-entry
    resume, unchanged.
  The SUBSCRIBE is two-phase: bare = a DESCRIBE (`GET /me` + `/channels` + the catalog + `/delivery`
  → workspace/role/invited-by, the install list — SCOPED to the named targets: a WORKSPACE target
  lists the whole delivered set, a channel/skill target only what it entitles — with digests +
  `via` attribution, the all-devices +
  fleet-reporting disclosures, pre-placed channels, dirname collisions with the `--prefix-dirname`
  `<ws>.<name>` choice, freed-name new-identity notes, the direct-follow explanation) + `next_actions`
  carrying the paste-ready `--yes` argv (NOTHING mutates before `--yes` except the enrollment itself
  — identity, reversible, disclosed as `enrolled_now`); `--yes` = the row ops (`channel_join` /
  `follow_skill`) then the delivery-driven reconcile landing the DESCRIBED set THIS invocation
  (batch-accepting first-receive offers through the SAME engine — never a fork; `install_only`
  restricts INSTALLATION to exactly the ids the describe disclosed, so a waiting arrival outside
  the named targets is never swept in under an unrelated `--yes` — it stays an individually
  consentable offer, while already-followed skills still UPDATE under their standing follow mode,
  as on any sweep; collisions decline
  by default or install prefixed), then the fleet report. The transports are built per-base-URL
  behind injectable factories, so the whole flow is tested over fakes with no HTTP.
- **The `invite` verb** (`ops/invite`, `plane_http::UreqDeviceClient`) — the two-phase roster write
  (`POST /v1/workspaces/{ws}/invitations` under the ONE device Bearer credential; the server resolves
  credential → device → user → the invite-policy gate; the acting device is never a body field).
  **Nothing is signed** (git/GitHub-level trust). Emails are folded to the canonical ASCII-lowercase
  form ONCE before the wire body (the server re-folds at its parse boundary), so the roster rows
  carry one identity per human; there is no invite link and no role field — joining is
  `follow <address>` plus proof of the invited email. The POST rides through the `UreqDeviceClient`
  behind a `GovernanceSource` seam, mapping the all-outcome **200 envelope** (`ok` ⇒ `InvitationData`;
  a policy-DENIED `!ok` ⇒ a typed "not authorized").
- **The placement engine** (`placement`, `topos-harness::{coverage,registry}`) — WHERE a followed
  skill's bytes land, computed each sync from the machine + the skill's device-local agent scope.
  Policy is **shared-dir-first**: an UNSCOPED skill lands ONE copy in `~/.agents/skills` when at
  least one detected harness is covered by it (coverage carries provenance — live-probed vs
  vendor-docs vs unknown-treated-as-false), plus a native user-dir copy per detected-but-uncovered
  harness (the active adapter keeps its richer `placement_for`; the rest resolve through the
  registry's user skills root with the ONE shared naming discipline — sanitize → workspace-prefix on
  collision → the id; never a foreign dir). A SCOPED skill (`--agent` include-list and/or per-agent
  exclusions) places into exactly the scoped-and-not-excluded detected harnesses' native dirs —
  never the shared dir. No detection at all (or no `$HOME`) keeps the classic active-adapter single
  placement; an adopted agent-less dir (the author's working copy) is ALWAYS managed. Targets are
  reconciled each sync: new placements (a newly detected harness, newly true coverage) are appended
  and land on the next apply — from the LOCAL store when the team's current did not move (the
  converge pass) — while a placement leaves the record ONLY through an explicit verb (its dir
  cleaned snapshot-first); detection loss alone freezes the copy in place, never deletes it.
  `map.json` is now schema v2 (its OWN ceiling): per-placement `placement_state` rows (kind ·
  agent · materialized/pre-existing shas · swap capability), 1:1 with `placements`; a v1 document
  upgrades losslessly in memory on read and rewrites as v2.
- **The pull/apply sync engine** (`ops/sync_engine`, `ops/pull`, `materialize`, `plane`) — the
  `checkForUpdates → plan → apply` machine over the kernel's four-state transition: a conditional read of
  the **unsigned** `current` pointer through the `PlaneSource` seam, a workspace/skill **scope check** (a
  mis-scoped record is a wire-validation error, not the target), **the served record IS the sync target** —
  whenever its `(generation, version_id)` differs from the stored `observed`/`observed_version_id` in ANY
  direction (a server restore is a legitimate team rollback), the engine adopts it and drives toward it; a
  draft snapshot-on-touch before any decision, fetch + re-verify (**digest == tree == `commit_id`** — the
  content-addressed integrity story, a mismatch is a loud integrity ERROR) + an ancestor-backfilling durable
  record into the sidecar store, the post-fetch heal (a crash-after-swap advances `applied` with no second
  swap, never a false divergence), the consent decision (the kernel's one policy), and **crash-safe
  byte-writing materialization** into EVERY managed placement (per-dir staging sibling → fsync →
  atomic dir-swap → fsync parent, the map committed after each landed dir as the crash-progress
  marker; the final `map → lock → sync` commit advances `applied` only once every dir holds the new
  bytes; a dir whose bytes differ from ITS recorded per-placement sha is snapshotted into the store
  before any overwrite — never a lost byte). **Draft-anywhere**: every placement is scanned against
  its own recorded sha — exactly one edited copy IS the draft (diff/publish/merge read that dir via
  `placement::work_tree_dir`), several byte-identical copies collapse to one, and several DIVERGENT
  copies freeze typed (`PLACEMENTS_DIVERGED` — nothing overwritten, every path disclosed,
  `update --reset` the named way out; reset and go-back snapshot EVERY distinct edited copy first
  and converge all placements). `pull <skill>` accepts a pending update (the explicit
  command is the consent a confirm-each offer solicited); `pull <skill>@<hash>` goes back to a version
  locally (resolved against the local store's versions, sets `held`, leaves the served target untouched). In
  tests the plane response + follow-state are **fixture-fed**; in production they come from the real `ureq`
  transport + the on-disk follow-state that `follow` writes — so a bare `pull` with nothing followed stays
  an honest no-op. A **never-received** followed skill (the first-receive baseline `follow` lays: the
  all-zero `observed_version_id` sentinel) is a state-② offer the engine OFFERS on a bare sweep (never
  auto-lands — I-TOFU first-receive consent, even for an `auto` follower) and PLACES on an explicit accept /
  `follow <skill>`. There is **no pointer signing, no client-side verification, no anti-rollback floor, no
  key pinning** — the trust level is the same a team extends to its git host + CI. **The bare enrolled
  sweep is now the DELIVERY-DRIVEN RECONCILE** (`ops/pull::pull_reconcile` + the `DeliverySource` seam on
  `UreqPlane`, keyed by the per-workspace credentials): ONE `GET /v1/workspaces/{ws}/delivery` per
  enrolled workspace answers "what should this device have", and the engine converges — new arrivals lay
  a first-receive baseline under the skill's CATALOG name and still pass the kernel's I-TOFU offer;
  known skills sync against the delivery's already-resolved target (`sync_one_with` — no second pointer
  GET); the undelivered remainder splits by WHO ACTED (the served `detached` set = the person's
  unfollow/lapse → freeze in place, `PullAction::Detached`; otherwise upstream withdrew it → snapshot
  any draft, CLEAN the agent dirs, keep every sidecar byte, `PullAction::Withdrawn`); a whole-workspace
  404 (removed / revoked) freezes everything with a warning, never a clean. Each workspace then gets the
  device's post-reconcile applied snapshot (`PUT /v1/workspaces/{ws}/report`) — best-effort fleet
  visibility, never a sync blocker. Targeted pulls and the un-enrolled state keep the classic per-skill
  engine; the ancestor backfill SHALLOW-STOPS at a version the plane no longer serves (a purged
  ancestor's tombstoned history) via `commit_backfill`, so fresh installs of live descendants survive a
  purge.
- **The author-merge resolution** (`ops/merge_resolve`) — resolves a DIVERGED draft (not just detects it).
  Reachable only through a `DivergedWitness` capability token minted in the sync engine's diverged arm (the
  structural author-only gate; followers never reach merge code). The kernel `topos-core::merge` plans +
  decides; `topos-gitstore::merge` runs the per-file diff3; this assembles the **complete** resolved (or
  conflict-marked) tree, commits it as a **forward 1-parent** commit on `current`, and places it via the
  same crash-safe dir-swap. A **clean** merge lands a **draft-on-current** (state ③ with `base = current`,
  `applied = observed`) — publishable. A **conflict** writes the complete marker tree (binary / file-set
  conflicts keep both sides via a `.topos-mine` sidecar) AND a durable **`conflict.json`** that is both the
  publish-block fact (presence-based) and a pre-swap recovery journal (a crash mid-materialize is healed by
  re-rendering the recorded result, never by re-merging on-disk markers). The disclosed **escape**
  (`pull <skill> --onto-current`) commits the author's bytes on `current` (dropping the merge, disclosing
  what it drops) — always available, so no deadlock. Unrelated histories (no renderable base) fall back to
  a **2-way** manual choice, never a silent merge. Per the full-auto posture, an `auto` follower's
  bare sweep resolves unattended; a confirm-each follower is surfaced. Materialization never fires the
  auto-update/harness hook.
- **The real plane transport** (`plane_http`, `enroll`) — a blocking `ureq` (rustls+ring) `PlaneSource` that
  feeds the engine above (no engine change). `get_current` is the commit-sensitive conditional GET
  (`GET /v1/workspaces/{ws}/skills/{skill}/current` with `If-None-Match: "<generation>"` +
  `Topos-Known-Version-Id`); `fetch_version` is a version-metadata GET + per-blob content-addressed
  bundle GETs that **re-verify each `sha256 == object_id`** — all under the device's ONE **Bearer
  credential**. It is a dumb transport — the engine scope-checks the served (unsigned) pointer and
  re-verifies the fetched bytes against the version id. `FileFollow` + the crash-safe `instance.json`
  (the API base URL — no trust root), `follows.json` (per-skill workspace + mode — pure subscription
  state; its `skill_id → workspace_id` map is the URL-path scope each read splices), and
  `identity/credentials.json` (the ONE device credential + registered device id — the **secret**,
  redacted from `Debug`, never in an error message or URL) supply the transport cred + the consent
  state. `app.rs` (via `load_enrollment`) selects the real transport only when `instance.json` is
  present, else stays inert. The end-to-end pull-over-loopback-HTTP proof lives in the `tests/`
  member; adding `ureq` keeps the client arch-clean (no `plane-store`/`sqlx`/`tokio` edge).
- **The device credential** (`enroll`, `identity`) — the device holds **ONE bearer credential**
  (`identity/credentials.json`, a `0600` secret with the registered device id alongside), minted by
  the device-authorization flow: on approval the flow's device code is promoted server-side to the
  credential and the granted poll carries it back — one secret, one field, no keypair, no per-workspace
  mint. It authenticates EVERY request (reads AND writes AND governance) in every workspace the
  approving person's seats reach; the server resolves credential → device → user → seat per request.
  `host.json` keeps only the LOCAL commit-author id (`d_<hex>`) — a label, never an auth artifact.
- **The private-file FsOps primitives** (`fs_seam`, `atomic`, `doc`) — secrets need `0600`. The seam gains
  `write_private` (mode 0600 **from creation** — no world-readable window, no chmod-after-write race) +
  `private_perms_ok` (the refuse-on-permissive read gate), both threaded through the `FaultFs` crash gate;
  `atomic_write_private` is the crash-safe secret write (its temp is 0600 from creation, so a fault never
  leaves a world-readable partial), and `write_doc_private` / `read_doc_private` the typed secret-doc pair
  (`read_doc_private` fails closed on a group/other-accessible secret BEFORE parsing). The device seed,
  `identity/credentials.json`, `follows.json` (perm hygiene — pure subscription state now), **and** the
  enrollment WAL (`identity/enrollment.json`) all go through these `0600`
  primitives.

Identity is the kernel's: `version_id`/`bundle_digest` depend only on the bytes + device id + a fixed
message, so injectable id/time sources make `add` deterministic. Golden `--json` fixtures (add/list/diff/log)
are asserted byte-equal in tests.

- **The contribute write verbs** (`ops/{publish,review,revert}` + `ops/contribute` + `op_wal` + the plane
  half of `ops/diff`) — the client contribute writes (the op kind rides the ROUTE; the acting device rides
  the transport's workspace **Bearer credential** — never a body field — nothing is signed). A
  **`ContributeSource`** transport seam (mirroring
  `GovernanceSource` on `UreqDeviceClient`) POSTs the four write routes; `map_write_envelope` maps the
  **all-outcome 200 envelope** to a typed `WriteReceipt` (every protocol outcome — OK / NEEDS_REVIEW /
  CONFLICT / DENIED — is an `Ok(WriteReceipt)`; only a transport/non-200/malformed body
  is an `Err`; the served pointer (`wire_record`) is parsed leniently because an OK `review --reject`
  carries `data: {}`). **`publish [--propose] [--to <channel>] <target>[@<digest>]`** first runs the
  **auto-add pre-step** (`ensure_tracked`): an EXACT tracked name wins straight through, else the target is
  an untracked LOCAL source it adopts before publishing — a discovered `<name>` / `<name>@<harness>`
  (reusing `add`'s `resolve_add_target`) or a `<dir>` (adopted in place via `ops::add`); a remote
  `owner/repo`/URL is refused (add it first), a `@<harness>` disagreeing with an already-tracked skill is
  `HARNESS_MISMATCH`, and ANY un-enrolled publish is refused BEFORE any adoption ("not enrolled — run
  `topos follow <workspace-address>` first"). A folded-in add is disclosed on the receipt
  (`PublishData`/`ProposeData` `added`). Then it scans the draft
  (the same source `diff` uses), and when the target pins a `@<digest>` runs the **optional consent gate**
  (recompute the digest over the scanned bytes; refuse on mismatch — never a silent mode-flip; without a pin
  the computed digest just ships), computes the byte-identical `commit_id`/`bundle_digest`
  via the kernel (**I-COMMIT-PARITY** — author = `ctx.device_id`, message = a fixed `"topos: publish"`), pins
  the candidate in the store, persists an **op-WAL** (the extended `OpRecord`, `0600`) BEFORE the first send,
  POSTs, and maps the outcome (OK advances local state read-your-writes; a NEEDS_REVIEW with the `downgraded` detail is the
  protection gate REROUTING a member's direct publish into a proposal — surfaced as Proposed, never an
  error; CONFLICT surfaces rebase).
  `--to <channel>` rides the wire body + the op-WAL (a replay re-sends the identical placement; the
  channel's mode gates it server-side — `everyone` included, no string-match bypass — independently of
  the version gate; a brand-new skill with no `--to` lands in `everyone` when its mode admits the
  caller, while a CURATED `everyone` withholds a member's default placement — the publish still lands,
  catalog-only, the receipt disclosing it as `PublishData.placement_withheld` with the curator's
  `channel add` named on the TTY). **`review <skill>@<hash> --approve|--reject`** binds the proposal's re-derived
  identity at `expected` = the FRESH `current` (a reviewable proposal's base). **`revert --to <good>`** binds
  the forward commit `{parents:[FRESH current], tree: good.tree}` (a stale local parent would be a DENIED, so
  it reads the live current). An UNCERTAIN send keeps the WAL so the next attempt **replays the SAME `op_id`**
  (no double-advance); a settled op deletes it. **`diff <skill> <ref>`** gained the plane half
  (`current..<hash>` / `<hash>` / `<a>..<b>` — a plane endpoint fetches + re-verifies). The commit-id parity
  (I-COMMIT-PARITY) is proven by `topos-core`'s `commit_id` KAT; the op_id-replay test lives in
  `ops/contribute`; the full loop is proven e2e over loopback HTTP in `tests/`.

- **The `--agent` scope verbs** (`ops/agent_scope`) — DEVICE-LOCAL placement policy for a followed
  skill, two-phase and fully offline (the plane is NEVER told; the subscription never moves).
  `follow <skill> --agent <slug>` (repeatable; `'*'` clears back to unscoped) records the
  include-list on an already-followed skill and reconciles the placements (out-of-scope dirs cleaned
  snapshot-first, new native dirs landed from the local store); on a not-yet-followed skill the
  ordinary subscribe runs and the include-list is recorded at apply. `unfollow <skill> --agent
  <slug>` and `remove <skill> --agent <slug>` on a followed skill are ONE shared implementation
  (`exclude_agents` — the verbs alias it): record the per-agent exclusion + clean exactly that
  agent's placement. Unknown slugs refuse naming the registry's valid ones; a known-but-undetected
  slug is accepted with an honest note; the describes name the placement plan (shared vs native,
  with a vendor-docs-level parenthetical where coverage is docs-level). `remove`'s classic `-a`
  semantics for untracked/local copies are unchanged, and bare `remove`/`unfollow` keep their exact
  prior behavior.
- **The `unfollow` verb** (`ops/unfollow`) — the PERSON-scoped detach, two-phase and byte-inert.
  Resolves dual-kind through the one grammar: a WORKSPACE target is recognized and refused toward
  the web (leaving is a roster change); the structural `everyone` refuses with the alternatives
  spelled; a channel target describes what STOPS (delivered via this channel alone) vs what KEEPS
  arriving (another channel / a direct follow), then `--yes` DELETEs the membership; a skill target
  describes the everywhere-stop (the unfollow row subtracts the skill from the WHOLE entitlement,
  channels included), then `--yes` DELETEs `follows/{skill}` AND flips the local `follows.json`
  pause in the same identity-locked write (so `list`'s cause column reads the frozen copy offline).
  The describe names the three constants: every device of yours, bytes frozen in place, the final
  detach record. Un-enrolled (or a purely local skill) keeps the graceful local path — the pause
  flag flips, nothing dials. Idempotent; never a skill file, never a `held` pin, never the auto-update
  hook; an explicit local `update <skill>@<hash>` remains available on an unfollowed copy.
- **The `auth` group** (`ops/auth`) — `login` / `logout` / `status`. **`login [server]`** (default
  `https://topos.sh`, `TOPOS_PLANE_URL` override, or the enrolled plane) re-runs the SAME device flow
  `follow` runs, minus a follow intent: card → re-root → the wrong-server `TOPOS_HOME` refusal →
  `device/authorize` toward an enrolled membership's ADDRESS (a never-enrolled install is pointed at
  `follow <address>`) → the shared WAL/poll/resume idiom (a login-owned WAL; the BIN blocks
  interactively / under `--wait`) → on the granted poll the device's ONE credential REPLACES the
  stored one wholesale (the identity is whoever approved in the browser). **`logout`** is two-phase:
  describe, then best-effort self device-revoke per enrolled workspace (the governance
  `DELETE …/devices` naming the STORED device id) and delete `identity/credentials.json` — skills,
  follows, drafts, and the memberships stay (no credential IS signed-out). **`status`** is
  side-effect-free: whoami (the principal from the freshest `me` probe), per-workspace access health
  via a `GET /me` probe (healthy / "no access — revoked or removed" on the uniform 404 / unreachable
  / no credential), hook health (the adapter's config-entry probe), and the reporting posture from
  `state/sync_status.json`.
- **The hook posture + notices** (`ops/pull`, `sync_status`) — the delivery-driven reconcile writes
  `state/sync_status.json` (`{workspaces: {ws: {last_delivery_at, last_report_at,
  staleness_window_ms}}}` — a plain doc, no secret) on every successful delivery/report and mirrors
  it onto `PullData.sync`; the delivered NOTICES ride `PullData.notices` — an interactive or
  `--json` `update` ACKS exactly the ids it returns (`POST …/notices/ack`), the quiet hook fetches
  WITHOUT acking. **The bare quiet sweep self-throttles** (`ops/quiet_gate`): hooks may fire on
  every session-shaped event, so `update --quiet` passes a gate BEFORE any engine/network work —
  single-flight (`locks/currency.lock`, try-lock; a held lock = another sweep in flight → silent
  exit 0) + TTL (`state/quiet_sweep.json`, stamped AFTER a completed sweep; default 300 s, `--ttl`
  flag > `TOPOS_UPDATE_TTL` env > default, `0` disables). An explicit non-quiet bare `update`
  always sweeps (blocking lock, refreshes the stamp). `update --quiet` stays near-byte-silent: a
  no-change sweep emits nothing; a sweep that CHANGED skill bytes emits the ONE SessionStart
  hook-output JSON (`hookSpecificOutput.reloadSkills` — Claude Code re-scans its skill dirs
  same-session; other harnesses discard hook stdout) with the two facts a person must not miss —
  the removed-from-roster freeze, and unreachable-AND-stale ("last synced <age> ago — server
  unreachable", read against the recorded window) — riding its `additionalContext`; without
  changes those facts stay ONE plain line each. An auth/transport failure warns and exits 0 (the
  hook never fails a session start for a network blip; it still stamps, so a dead plane is not
  re-dialed every session event) while a genuinely local failure still exits nonzero. `follow
  --yes` reuses the same reconcile with explicit `ReconcileOpts` (batch-accepted first receives
  restricted to the describe's disclosed ids, declined/renamed collisions, one workspace, no ack)
  and no gate.

- **The BUILT-IN `topos` skill** (`ops/builtin`, `cli_ref`, the repo-top-level `skills/topos/`
  source) — the meta-skill that teaches an agent what topos is, how to drive it, and how to
  distill a session's own learnings into shared skills (origination: the capture bar,
  describe-first consent, deepen-before-new). Its SOURCE is the repo-top-level `skills/topos/`
  dir — an authored `SKILL.md` (self-contained: a `topos --version` routing step offers the
  install path when the CLI is absent; NO version stamp, so the committed file is byte-identical
  to what the binary places) + `INSTALL.md` (installer + join/start-fresh/self-host) + the
  committed generated `reference.md` — downloadable AS a skill straight from the public repo
  (`npx skills add`-style installers find it there; the frontmatter `name: topos` names the
  installed dir). The binary EMBEDS those same files (`include_str!`), and the bundle it places is
  the same three: `SKILL.md` + `INSTALL.md` + `reference.md` = the SAME bytes `docs/cli.md`
  carries (`cli_ref::cli_ref_md()` renders from the real clap tree; xtask's `gen-cli-ref`
  writes/checks BOTH committed copies with the same fn — one renderer, no drift). It
  lands through the ORDINARY placement engine (shared-dir-first; `--agent` scoping works) at the
  trigger-arming moments (`add`'s adopt receipt, the enrollment receipt) and re-syncs on every bare
  `update` sweep — FORCE-SYNCED to the binary (a hand edit is snapshotted into the store, then
  overwritten; never a draft; a binary change commits + re-places), with its byte changes riding the
  quiet hook's `reloadSkills`. A pre-existing `topos` dir is NEVER written by the sweep (the
  Foreign freeze — marker or not): one whose SKILL.md frontmatter carries the public copy's
  provenance marker (a `metadata:` entry `topos: builtin`, matched fail-closed — terminated
  frontmatter only, nested under `metadata:` only) is a stale DOWNLOADED copy that the CONSENTED
  `follow topos --yes` adopts — disclosed on the bare describe, snapshot-first into the sidecar
  store, then force-synced and managed; without the marker it stays the frozen Foreign
  reservation, never written, never deleted. NOT a subscription: no `follows.json`
  row, the plane never hears of
  it; its device-local state (`state/builtin.json`) carries the durable `remove topos` opt-out (no
  sweep resurrects; `follow topos` re-places, riding the agent-scope payload as `restore`) + the
  `--agent` scope. `list` shows it as `built-in`; `publish`/`unfollow`/targeted `update` refuse it
  typed toward the verbs that do act; `uninstall --yes` removes its placed copies (topos-authored
  artifacts — user skill files still stay). The NAME is reserved end-to-end: `add` refuses it, the
  one naming discipline never hands the `topos` dir to another skill
  (`topos_harness::RESERVED_SKILL_DIR`), and the app's catalog mint suffixes past it server-side.
- **The `self-update` maintenance command** (`ops/self_update`, `release`, `plane_http::UreqReleases`) —
  the native self-updater. It resolves the target release (the latest tag, or a `--version <tag>` pin —
  which allows a pinned downgrade), downloads that tag's `topos-<triple>.tar.gz` + its `SHA256SUMS`,
  runs the **release-signature gate** (`RELEASE_PUBKEY`, an Option-shaped compiled-in minisign key:
  `Some` makes the asset's `.minisig` MANDATORY and fail-closed — fetched + verified over the
  downloaded bytes BEFORE the checksum, a missing/invalid signature is a typed `INTEGRITY_ERROR`
  with no unsigned fallback, and the SIGNED trusted comment must name the exact tag + asset the
  update resolved, so an old release's valid signature cannot be re-served under a newer tag;
  `None` — the pre-key-ceremony state — keeps checksum-only behavior,
  disclosed as `signed: false` + an "unsigned build" note; the verify side is `minisign-verify`,
  pure Rust with zero deps, while SIGNING exists only in CI and a test-only dev-dependency —
  `scripts/mint-release-key.sh` is the ceremony that flips the constant, and `docs/RELEASE.md`
  documents the scheme), verifies the download's sha256 against the manifest (mandatory, never
  skippable — a mismatch is a typed `INTEGRITY_ERROR` refused BEFORE the binary is touched),
  extracts the `topos` entry in memory (never unpacking attacker paths to disk), and atomically
  replaces the running binary via a same-dir staged-temp → fsync → rename-over → fsync-dir (the
  existing crash gate covers it; a running process keeps its old inode). `--check` reports
  availability and stops. The upstream sits behind an injectable `ReleaseSource` seam, so the whole
  flow is unit-tested with a fake (no HTTP) — the signature tests mint a throwaway keypair per run;
  `build.rs` embeds the compiled target triple so the updater fetches THIS platform's asset. A
  default GitHub base is compiled in, overridable via `TOPOS_INSTALL_BASE_URL` for a local mirror /
  air-gap (a non-HTTPS base is warned, the checksum still enforced). NOT a behavior verb — it
  touches no skills, no plane, no account, and mints no device identity. (`topos upgrade` stays the
  hidden disambiguation refusal toward `update` / `self-update`.)
- **The passive version check** (`ops/version_check`, `release::ReleaseProbe`,
  `plane_http::UreqVersionProbe`) — after a SUCCESSFUL eligible command, at most once per day, the
  binary probes the public GitHub `releases/latest` 302 redirect (redirects DISABLED — the tag is
  parsed from the `Location` header; no API, no auth, no JSON) on a hard 2 s timeout and prints ONE
  newer-version line on STDERR (`stdout` stays byte-clean for `--json` consumers; nothing enters
  the envelope). Quiet by construction: every probe failure is silent, and the stamp
  (`state/version_check.json`) is written BEFORE the probe so an offline machine holds the daily
  cadence instead of re-dialing per command; the FIRST eligible command only lays the stamp and
  never probes (a fresh install is current; short-lived test homes never dial out); the quiet sweep
  (`update --quiet` — the session-start hook path), `self-update`/`upgrade`, `uninstall` (the stamp
  would recreate the state dir the teardown deleted), a `TOPOS_INSTALL_BASE_URL` mirror (it cannot
  answer `/latest`), and `TOPOS_NO_UPDATE_CHECK=1` all skip the check entirely — gated in the
  composition root (`app::run`), which runs the check ONCE after the dispatch, never per-verb.

- **The reshaped team verbs** (`ops/{remove,channel,protect,invite,review,log,list,pull,publish}`) — each
  runs the ONE resolution grammar + the two-phase describe/`--yes` gate over the built directory row ops:
  - **`remove`** (`ops/remove`) — take skills off THIS machine, two-phase. A FOLLOWED skill becomes a
    per-device **exclusion** (`PUT exclusions/{skill}`): delivery stops here, the person keeps following it
    (other devices still receive it), the agent dirs are cleaned (any draft snapshotted first) and every
    sidecar byte KEPT — via the shared `snapshot_and_clean` the upstream-withdrawal sweep also runs (factored
    out of `withdraw_upstream`, never forked). A tracked never-published local (or an untracked agent-dir
    copy `<name>@<agent>` / `-a`-scoped) is a **permanent** delete. Multi-skill, all-or-none; the local
    exclusion cause is marked on `follows.json` (`excluded_here`) for `list`.
  - **`channel add|remove`** (`ops/channel`) — channel-first placement, two-phase. Resolves every skill
    ALL-OR-NONE through the grammar, reads the channel's mode for the describe (create-on-first-place says
    "creates #<ch>"), then `PUT`/`DELETE channels/{ch}/skills/{skill}` per skill; a curated-channel role
    refusal (`CURATED_ROLE_REQUIRED`) is a typed refusal naming who can, and a later per-skill failure after
    an earlier landed is reported honestly, per skill.
  - **`protect`** (`ops/protect`) — dual-kind target, two-phase. Bare TIGHTENS to the kind's protected level
    (skill → `reviewed`, channel → `curated`); an explicit `open` LOOSENS (an owner act, per the describe);
    a level that does not apply to the kind is a typed usage error. The describe carries the audience — the
    reach (people) for a skill, the channel's member count — plus the pending-proposals-survive note on a
    skill loosening; `OWNER_ROLE_REQUIRED` / `REVIEWER_ROLE_REQUIRED` surface typed, naming the role.
  - **`invite`** (`ops/invite`) — the roster write is now two-phase, and a BARE `invite` (no emails) is a
    no-mutation `/me` read (the workspace address + invite policy + "nothing was sent or changed"). Emails
    without `--yes` describe (who gets seated, the channel pre-placements, the mail-or-paste note); `--yes`
    POSTs the folded-email invitation. (The `/i/`-link mint is retired — joining is `follow <address>`.)
  - **`review`** (`ops/review`) — a bare `review` is the review INBOX/OUTBOX across every enrolled workspace
    (`GET /proposals` per ws; author-message FIRST; the outbox is your own proposals, split by the
    server-computed `yours` — user-id equality server-side; the `user.json` principal match stays only as
    the compat fallback for an older server). A bare TARGET (`<skill>[@<hash>]`, a bare skill resolving to its one open
    proposal) DESCRIBES it — author, message, base, staleness, and the diff vs current (`current..<proposal>`
    through the same plane-diff machinery `diff` runs) — with the verdict next-actions (`--withdraw`, never
    `--approve`, on your own proposal); a target + a verdict
    flag applies directly (the verdict IS the consent).
  - **`log`** (`ops/log`) — the local action log + git history now MERGE the plane's version/proposal history
    for a followed skill (`GET /skills/{skill}/log`): versions with purge tombstones ("purged by <who> <when>
    — bytes gone"), proposal events, and the archived-successor hint when resolved by a freed base name. A
    channel target is refused toward the web; un-enrolled / local-only skills keep the local log.
  - **`list`** (`ops/list`) — each tracked row gains the SOURCE (workspace label | origin host | local),
    STATUS (`current` / `behind` / `draft` / `detached`), and CAUSE (`unfollowed` / `excluded-here` /
    `signed-out`) columns, derived offline from `follows.json` flags + credential presence + the origin doc.
    A purely-local never-followed skill carries no columns (the pinned shape stays byte-identical there).
  - **`update --reset`** (`ops/pull` + `sync_engine::reset_to_base`) — a loss-led two-phase discard. Refuses
    without a named skill (it never blanket-resets); the describe LEADS with the exact draft delta (the local
    diff); `--yes` snapshots the draft into the sidecar store, then re-materializes the followed `current`
    (an imported skill's adopted origin) over the placement.
  - **`publish`** (`ops/publish::publish_describe`) — a bare ENROLLED publish now DESCRIBES: the workspace,
    the gate outcome (`open` → lands directly / `reviewed` → a proposal), the placements (`--to`, or the default
    `everyone` on a GENESIS only — a bare republish alters no placement, so none is listed — annotated
    `curated: lands catalog-only; a curator places it afterwards` whenever the channel index the
    describe already reads resolves the placement target as curated against a member caller),
    the audience (reach), the share line, the undo path, and the origin-demotion
    note; a no-op (the draft equals the published current) is a typed `NO_CHANGES` — on BOTH the describe and
    the apply (`--yes`), keyed on a published `current` existing (not on follow-state), so even the genesis
    author's repeat publish is refused. The scan is local-first; the network is
    read only after it; the genesis / WAL apply paths stay byte-identical. The TTY success line leads with
    the skill NAME (`Published <name>@…` — `PublishData.name`; the opaque `skill_id` stays a `--json` key).
    `add -s/-a` accept MULTIPLE
    values (a remote import loops per skill × harness); `'*'` and the keep-as-yours re-adopt stay marked
    seams.
  - **The global `--workspace` selector** accepts the workspace's ADDRESS name as well as the opaque id —
    canonicalized ONCE at argv entry (`enroll::canonicalize_workspace_flag`, name → joined id, best-effort)
    so every consumer keeps id semantics; the selection/refusal guidance lists the joined ADDRESS names,
    never bare `w_…` ids.

- **The envelope's agent ergonomics** (`actions`, `ops/diff`'s budget, `ops/{list,log}`'s row page,
  `ops/merge_resolve::preview_merge`) — three additive `--json` affordances (schema_version stays 1;
  every new field omits when absent, so an uncapped envelope is byte-identical to before):
  **byte budgets** — `diff` and `review` take `--max-bytes <n>` (`0` = uncapped; explicit flags
  bind both surfaces; a `--json` run with no flag default-caps at 64 KiB) and truncate at FILE
  boundaries via the gitstore's `unified_diff_sections` (a clean prefix, never a cherry-pick),
  disclosing `truncated` + per-file `patch_omitted` rows and a `FETCH_FULL_DIFF` next action that
  re-runs the same diff uncapped (loss disclosures — `update --reset`'s drop diff, the merge
  escape's — stay deliberately uncapped); **pagination** — `list`/`log` take `--limit`/`--offset`
  (`--json` defaults 50/20 rows; `list` pages PER BUCKET) with typed truncation markers and a
  `NEXT_PAGE` next action carrying the complete re-spelled argv; **next-action safety metadata** —
  every emitted next action carries optional `mutates`/`needs_network`/`risk_note`, filled by the
  ONE rules module (`actions::next_action` — xtask's fixture generator calls the same fn, so no
  second table), keyed by action code with an argv-verb refinement where the code alone cannot
  answer (an unknowable fact stays absent, never guessed); and the **predicted-conflict preview** —
  `update`'s SURFACED diverged rows and `publish`'s describe on a behind copy carry
  `merge_preview` (`clean`/`conflicted` + conflicting paths), a pure in-memory dry run of the same
  kernel plan + diff3 executor the real resolution uses, computed ONLY from already-local bytes
  (describes gain no network call; when the needed version is not local the field is simply
  absent = unknown).

## Planned (lands later)

The **unified-identity credential model is now in place**: the device-authorization flow mints ONE
Bearer **device credential** that authenticates EVERY plane request — reads AND writes AND governance
— and authorization server-side is the credential's device → user → seat resolution. No key material
exists client-side (nothing signs). `follows.json` is pure subscription state;
`identity/credentials.json` (a `0600` secret) holds the one credential + the registered device id.
Still to come:
**Multi-reviewer
governance** (reviewer roles / N-approver / a rendered diff UI — single-approver, plain unified diff only) +
the **`review-required` policy toggle verb** (enforcement is built; the policy row is a plane/console
setting) + `log --team`'s plane half; harness *selection* in the composition root (v0 constructs Claude
Code only; both the OpenClaw and Hermes adapters are built + wired into `adapter_for` — OpenClaw's
concrete config bytes stay pilot-pending behind its readiness probe, Hermes's are probed against a
real local build with the pilot's exact build a named MUST-VERIFY; only Claude Code guarantees the
swap completes before skills resolve, so non-Claude adapters leave a named, bounded multi-file-read
residual). The identity step (sign-in) runs on the server's
approval page (the agent only polls), so the client needs no UI for it.

## Architectural layering (enforced at the dependency graph)

**No edge to `plane-store`, no `sqlx`, no `libsqlite3-sys`.** The client is a thin sync tool, never an
authority — a per-target `cargo tree -p topos` assertion (`cargo xtask check-arch`) holds the line.

The sidecar keys skills by id; harness skill directories stay byte-pristine, so uninstall is a no-op for
your skills.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `topos-harness`, `clap`, `serde`/`serde_json`,
`uuid`, `rustix` (safe fsync/flock + the atomic dir-swap; `system` for the host node name the
device-authorization start sends as the requested device display name), `hex` (decode sidecar id
fields), `base64` (the contribute candidate's `content_base64`; wire encoding only), `ureq` (the
blocking rustls+ring plane + enrollment transport — self-contained, so no `tokio`/`plane-store`/`sqlx`
edge), `tar`+`flate2` (the self-update asset codec, in-memory), `minisign-verify` (the self-update
release-signature VERIFIER — pure Rust, zero deps; signing exists only in CI and the test-only
`minisign` dev-dependency), `anyhow`, `thiserror`. No signing key material client-side: the device
authenticates with the ONE bearer credential the device flow mints, and the only key the binary
carries is the PUBLIC release key (`RELEASE_PUBKEY`, `None` until the key ceremony). None of these
crates cross `check-arch`'s line (it bans
only `plane-store`/`sqlx`/`libsqlite3-sys`/`tokio`/`reqwest`/`hyper`); `topos-core` is signature-free
`no_std`.
