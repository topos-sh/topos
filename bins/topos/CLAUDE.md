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
  currency hook. The adapter owns the strict-JSON `settings.json` merge; the durable write goes through a
  small `ConfigStore` port implemented here, which reuses the one `atomic_write` dance over `FsOps` (so
  the existing crash gate covers the config write too — never a second atomic-write to drift). The
  foreign-file writer adds the care a shared user file needs: ensure the parent dir, write through a
  symlink, a topos-namespaced temp, best-effort mode preservation. The **OpenClaw and Hermes arms** are
  wired too (`topos-harness::OpenClaw`; `topos-harness::Hermes`: `$HERMES_HOME` + the
  `HERMES_ACCEPT_HOOKS` evidence resolved at construction), though v0's composition root still selects
  Claude Code only — harness *selection* lands later (the TTY receipt copy already branches on the
  report's `currency_kind`, so no surface overstates a sibling adapter's update moment).
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
  **recognize a Claude Code skill dir, tag it + arm the currency hook**; refuse re-adopting an
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
  [<skill>[@<hash>]] [--quiet]` (the session-start currency entry point — see the sync engine below),
  `uninstall` (**scrub the currency hook**, then remove the binary + `~/.topos/`, touch no skill bytes).
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
    WAL → arms the currency trigger, and the flow CONTINUES into the intent's describe/apply in the
    same invocation. There is no post-grant fence phase: an approved flow re-answers the same
    granted poll, so a crash mid-persist recovers by re-polling;
  - **the classic skill path** (`follow <skill>[@<hash>]`) — the I-TOFU accept / the paused-entry
    resume, unchanged.
  The SUBSCRIBE is two-phase: bare = a DESCRIBE (`GET /me` + `/channels` + the catalog + `/delivery`
  → workspace/role/invited-by, the install list with digests + `via` attribution, the all-devices +
  fleet-reporting disclosures, pre-placed channels, dirname collisions with the `--prefix-dirname`
  `<ws>.<name>` choice, freed-name new-identity notes, the direct-follow explanation) + `next_actions`
  carrying the paste-ready `--yes` argv (NOTHING mutates before `--yes` except the enrollment itself
  — identity, reversible, disclosed as `enrolled_now`); `--yes` = the row ops (`channel_join` /
  `follow_skill`) then the delivery-driven reconcile landing the set THIS invocation
  (batch-accepting first-receive offers through the SAME engine — never a fork; collisions decline
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
  byte-writing materialization** (staging sibling → fsync → atomic dir-swap → fsync parent → `map → lock →
  sync` commit; `applied` advances only post-swap). `pull <skill>` accepts a pending update (the explicit
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
  currency/harness hook.
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
  channel's mode gates it server-side, independently of the version gate; a brand-new skill with no
  `--to` lands in `everyone`). **`review <skill>@<hash> --approve|--reject`** binds the proposal's re-derived
  identity at `expected` = the FRESH `current` (a reviewable proposal's base). **`revert --to <good>`** binds
  the forward commit `{parents:[FRESH current], tree: good.tree}` (a stale local parent would be a DENIED, so
  it reads the live current). An UNCERTAIN send keeps the WAL so the next attempt **replays the SAME `op_id`**
  (no double-advance); a settled op deletes it. **`diff <skill> <ref>`** gained the plane half
  (`current..<hash>` / `<hash>` / `<a>..<b>` — a plane endpoint fetches + re-verifies). The commit-id parity
  (I-COMMIT-PARITY) is proven by `topos-core`'s `commit_id` KAT; the op_id-replay test lives in
  `ops/contribute`; the full loop is proven e2e over loopback HTTP in `tests/`.

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
  flag flips, nothing dials. Idempotent; never a skill file, never a `held` pin, never the currency
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
  WITHOUT acking. `update --quiet` stays byte-silent EXCEPT two one-liners a person must not miss:
  the removed-from-roster freeze, and unreachable-AND-stale ("last synced <age> ago — server
  unreachable", read against the recorded window); an auth/transport failure warns and exits 0 (the
  hook never fails a session start for a network blip) while a genuinely local failure still exits
  nonzero. `follow --yes` reuses the same reconcile with explicit `ReconcileOpts`
  (batch-accepted first receives, declined/renamed collisions, one workspace, no ack).

- **The `upgrade` maintenance command** (`ops/upgrade`, `release`, `plane_http::UreqReleases`) — the native
  self-updater. It resolves the target release (the latest tag, or a `--version <tag>` pin — which allows a
  pinned downgrade), downloads that tag's `topos-<triple>.tar.gz` + its `SHA256SUMS`, verifies the
  download's sha256 against the manifest (mandatory, never skippable — a mismatch is a typed
  `INTEGRITY_ERROR` refused BEFORE the binary is touched), extracts the `topos` entry in memory (never
  unpacking attacker paths to disk), and atomically replaces the running binary via a same-dir
  staged-temp → fsync → rename-over → fsync-dir (the existing crash gate covers it; a running process
  keeps its old inode). `--check` reports availability and stops. The upstream sits behind an injectable
  `ReleaseSource` seam, so the whole flow is unit-tested with a fake (no HTTP); `build.rs` embeds the
  compiled target triple so the updater fetches THIS platform's asset. A default GitHub base is compiled
  in, overridable via `TOPOS_INSTALL_BASE_URL` for a local mirror / air-gap (a non-HTTPS base is warned,
  the checksum still enforced). NOT a behavior verb — it touches no skills, no plane, no account, and
  mints no device identity.

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
    (`GET /proposals` per ws; author-message FIRST; the outbox is your own proposals, matched on
    `user.json`'s principal). A bare TARGET (`<skill>[@<hash>]`, a bare skill resolving to its one open
    proposal) DESCRIBES it — author, message, base, staleness, and the diff vs current (`current..<proposal>`
    through the same plane-diff machinery `diff` runs) — with the verdict next-actions; a target + a verdict
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
    the gate outcome (`open` → lands directly / `reviewed` → a proposal), the placements (`--to`, or
    `everyone` for a new skill), the audience (reach), the share line, the undo path, and the origin-demotion
    note; a no-op (the draft equals current) is a typed `NO_CHANGES`. The scan is local-first; the network is
    read only after it; the genesis / WAL apply paths stay byte-identical. The TTY success line leads with
    the skill NAME (`Published <name>@…` — `PublishData.name`; the opaque `skill_id` stays a `--json` key).
    `add -s/-a` accept MULTIPLE
    values (a remote import loops per skill × harness); `'*'` and the keep-as-yours re-adopt stay marked
    seams.
  - **The global `--workspace` selector** accepts the workspace's ADDRESS name as well as the opaque id —
    canonicalized ONCE at argv entry (`enroll::canonicalize_workspace_flag`, name → joined id, best-effort)
    so every consumer keeps id semantics; the selection/refusal guidance lists the joined ADDRESS names,
    never bare `w_…` ids.

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
concrete config bytes and Hermes's per-turn-injection claim stay pilot-pending behind their readiness
probes; only Claude Code guarantees the swap completes before skills resolve, so non-Claude adapters
leave a named, bounded multi-file-read residual). The identity step (sign-in) runs on the server's
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
edge), `anyhow`, `thiserror`. No key material and no crypto deps: the device authenticates with the
ONE bearer credential the device flow mints. None of these crates cross `check-arch`'s line (it bans
only `plane-store`/`sqlx`/`libsqlite3-sys`/`tokio`/`reqwest`/`hyper`); `topos-core` is signature-free
`no_std`.
