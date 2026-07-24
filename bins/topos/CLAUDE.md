# `topos` — the client CLI

**lib:** the local domain operations, the **sidecar** (an embedded-git store per skill + crash-safe JSON
docs holding identity / per-skill history / mappings), and the bundle scanner — all over a single
fault-injectable fs/syscall seam. **bin:** a thin `clap` wiring; `--json` (no prompts) + a thin TTY
renderer over the SAME typed outcomes (one value, two presentations).

The client runs the **manifest architecture**: what a folder's agents should have IS its `topos.toml`
manifest (nearest one wins, walking up like git), the person's server-stored PROFILE covers everywhere
else, and a **session** (user × workspace × installation, minted by `topos login`) is the standing
acceptance — delivery is silent from login on, npm-style. There is no subscribe verb, no per-skill
consent step, and no device-link lane; `follow`/`unfollow`/`channel` are gone.

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
  serde and never deleted).
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

- **The MANIFEST layer** (`manifest/{file,refs,resolve,walk}`, `ops/manifest_edit`) — a scope IS a
  manifest. Resolution stacks four layers NEAREST-FIRST: the folder's `topos.toml` → each ancestor's →
  every live session's server-stored profile (delivered ready-made) → the LOCAL personal manifest
  (`~/.topos/topos.toml`). The nearest layer that names an item NAME wins whole; an `[exclude]` line is
  the ONE negative state (subtracting a broader layer's delivery). The **reference grammar**
  (`manifest/refs`) is shape-determined: a bare name (a connected catalog's skill, unique-across-catalogs),
  `@<ws>/<name>` and `@<ws>/channels/<name>` (workspace-qualified; the CANONICAL host-qualified form
  `host/ws/name` is what manifests store), `owner/repo` (GitHub import; pins are 7–40-hex commits),
  `./path` (a local adopt; out-of-tree sources store the absolute path), with `*`/64-hex version pins as
  entry values. The `#` sigil is banned and a bare `@name` is a typed refusal. `edit_target` picks the
  manifest an `add`/`remove` edits: the nearest covering the cwd, else a fresh one at the enclosing git
  root (npm-init precedent), else the cwd; `-g` (path refs) targets the personal manifest; `init` creates
  the folder's manifest from the commented template.
- **SESSIONS** (`sessions`, `ops/login`, `enroll`) — `topos login <address>` runs the RFC-8628-shaped
  flow against `/v1/login/authorize|token` (the constant protocol card re-roots onto the declared API
  base, same-security only; the `0600` WAL holds the flow; re-invoking IS the resume; the loopback
  listener auto-opens the approval page where a browser is plausible). The granted poll mints ONE
  **workspace-scoped bearer credential** persisted as a session row in `identity/sessions.json` (`0600`;
  statuses `active`/`pending`/`ended`) — login IS the acceptance event (the receipt discloses what the
  profile delivers; no offer step follows). Further workspaces are further logins; `logout
  [<ws>|--all]` runs the server-side self-end (`DELETE /v1/session` under that session's OWN credential;
  the uniform 404 = already ended) then deletes the local row — skills, drafts, and manifests stay. A
  session the server no longer answers (owner-ended, seat removed, workspace gone — indistinguishable)
  is marked `ended` locally by the sweep, prints its ONE typed `SESSION_ENDED` line, and freezes that
  workspace's items in place; `login` reconnects.
- **The MANIFEST RECONCILE** (`ops/reconcile` — what `update` runs) — dial each live session's
  `GET /v1/workspaces/{ws}/delivery` ONCE (a pending session skips quietly; NotFound = the ended-session
  line + local flip; unreachable degrades that session's profile layer to the OFFLINE CACHE so the local
  converge keeps working), build the layers for the cwd's scope chain, resolve, then reconcile each
  resolved item BY KIND: profile items sync against the delivery's pre-resolved target (installed
  SILENTLY under their catalog names — login was the acceptance); project/personal workspace refs
  resolve through the ref's session (catalog index; channels expand via the channel index; a manifest
  pin overrides the served current, falling back honestly when the pinned version is gone); GitHub refs
  install at their pin and re-import on a pin bump (refusing over local edits); path refs are
  adopt-in-place presence checks. Placement is PER-SCOPE (see below); after the fan-out,
  `clean_undemanded` retires what no layer demands any more (a profile drop withdraws the person-scope
  placements snapshot-first and resets to never-received; a project drop cleans the stale in-chain dirs)
  and each session gets the applied report (`PUT …/report` — a complete snapshot per session). The
  **delivery cache** (`state/sync_status.json`) records host/workspace_name per workspace +
  name/review_required/served_version per skill, so `status`/`list` answer offline and `CacheFollow`
  (the FollowSource over the cache) + `SessionRoutedPlane` (the PlaneSource routing each skill to its
  session's lane) wire the composition root. Notices ACK by id after an interactive `update`; the quiet
  hook fetches without acking.
- **The verbs** (`ops`) — `add <source> [-s <name>] [-a <slug>] [-g]`: ONE source-polymorphic positional —
  a workspace-shaped reference (`@ws/name`, `@ws/channels/x`, canonical `host/ws/name`, a bare catalog
  name when sessions exist) edits the nearest manifest (or, `-g`, PUTs the server-stored profile route)
  and DELIVERS in the same invocation; `add topos` is the built-in's restore; a PATH adopts a directory
  in place (mint id+name, scan + import, stage + publish with one rename — all-or-nothing; the manifest
  records the dir-relative `./path` line; recognize a Claude Code skill dir, tag it + arm the hook); a
  bare NAME with no sessions resolves against `list`'s untracked discovery; a REMOTE
  `owner/repo`/github URL is fetched + imported (`add_remote` over the injectable `GitTarballSource`,
  `..`/symlink-safe, `origin.json` provenance adjunct) and recorded as a pinned GitHub ref.
  `remove <targets…> [-g]` is `add`'s inverse: drop the manifest line, or record an EXCLUDE when a
  broader layer still provides the name; `-g` DELETEs the profile row (the answer's `data.status` —
  `removed`/`excluded`/`not_in_profile` — phrases the receipt); a tracked never-published local (or an
  untracked agent-dir copy) keeps the two-phase permanent delete with the loss-guard describe;
  `remove topos --yes` is the built-in's durable opt-out. `update [<target>…] [--quiet]` is the manifest
  reconcile above (targeted forms narrow it; `--reset` keeps the loss-led two-phase discard;
  `--goback`/`--onto-current` keep the per-skill engine). `list [--footprint] [--tracked] [--remote]`
  (tracked bucket + untracked discovery across the baked ~73-harness registry; `--remote` reads each
  session's catalog, session-routed). `diff`, `log` (the plane half rides the skill's session lane),
  `init`, `status` (below), `uninstall [--yes]` (two-phase teardown: hook scrub + sidecar delete; the
  sessions go with it; skill files stay).
- **The placement engine** (`placement`, `topos-harness::{coverage,registry}`) — WHERE a managed skill's
  bytes land, computed each sync FOR ITS SCOPE. **Person scope** (`plan_for_skill`/`plan_targets`) is
  shared-dir-first over the home: one `~/.agents/skills` copy where a detected harness is covered, plus a
  native user-dir copy per detected-but-uncovered harness (the active adapter keeps its richer
  `placement_for`); prior-(kind, agent) stability never reuses a dir recorded INSIDE a project checkout
  (`under_project_manifest` — the mirror of the project plan's project-local rule). **Project scope**
  (`project_plan`) mirrors the policy ROOTED AT THE CHECKOUT: `<proj>/.agents/skills` for covered
  agents, the registry's project dirs for the rest, the Claude-Code-shaped `.claude/skills` default when
  nothing is detected; a manifest `[placement]` override pins ONE project-relative dir; every landed
  project dir gets its `.git/info/exclude` line (idempotent; worktree gitdir/commondir resolved).
  Placements are reconciled each sync (new targets appended and converged from the LOCAL store; a
  record leaves only through an explicit act; detection loss freezes, never deletes). `map.json` is
  schema v2 (per-placement `placement_state` rows).
- **The pull/apply sync engine** (`ops/sync_engine`, `materialize`, `plane`) — the `checkForUpdates →
  plan → apply` machine over the kernel's four-state transition, now PLAN-THREADED: `sync_one_planned`
  takes an optional `PlanFn` so the reconcile drives per-scope placement through the ONE engine (no
  fork). The served record IS the sync target (adopted in ANY direction); draft snapshot-on-touch;
  fetch + re-verify (digest == tree == `commit_id`); crash-safe dir-swap materialization into every
  managed placement; draft-anywhere (one edited copy IS the draft; several divergent copies freeze
  typed with `update --reset` the way out); the ancestor backfill shallow-stops at purged history.
- **The author-merge resolution** (`ops/merge_resolve`) — resolves a DIVERGED draft behind the
  `DivergedWitness` capability token (followers never reach merge code). Kernel-planned diff3; a clean
  merge lands a publishable draft-on-current; a conflict writes the complete marker tree + the durable
  `conflict.json` (publish-block + recovery journal); the disclosed escape (`--onto-current`) always
  works offline. An `auto` item's bare sweep resolves unattended.
- **The transport** (`plane_http`) — blocking `ureq` (rustls+ring). `UreqPlane` (conditional `current`
  GET, verified per-blob bundle fetch, delivery + report) and `UreqDeviceClient` (the directory reads,
  the profile row ops `PUT/DELETE /v1/workspaces/{ws}/profile/{skills|channels}/…` with the
  status-carrying DELETE, the contribute writes, invitations, notices, `DELETE /v1/session`, and the
  creds-free login flow) — every credentialed call under the SESSION's workspace-scoped Bearer. The
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
  reference (`PublishData.manifest`/`reference`/`converted_from` disclose it). `--to` accepts channel
  references (workspace-checked); a curated `everyone` withholds a member's default placement,
  disclosed. A logged-out publish refuses typed toward `topos login`. **`review`** (inbox across
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
- **The `status` verb + the bare `topos` orientation** (`ops/status`) — the offline trust rail: the
  server + each session (host/workspace/status — pending and ended annotated), THIS directory's
  manifest chain with each item's source manifest + scope (a connected workspace ref reads "not yet
  reconciled" — offline honesty, currency is the reconcile's answer), followed-skill counts, per-agent
  trigger state probed read-only, and the binary version. No network, no writes; it dispatches ahead of
  the recovery sweep and leaves a pending-recovery sidecar byte-identical. A bare `topos` on a TTY
  renders the same snapshot (a fresh machine gets the welcome); piped bare invocations keep the usage
  error.
- **The hook posture + notices** (`ops/reconcile`, `sync_status`, `ops/quiet_gate`) — the reconcile
  stamps `state/sync_status.json` on every delivery/report; `update --quiet` (the session-start hook
  path) self-throttles (single-flight lock + TTL, default 300 s) and stays near-byte-silent: a
  no-change sweep emits nothing; a changed sweep emits the ONE SessionStart hook JSON
  (`reloadSkills`) with the ended-session freeze and unreachable-AND-stale facts riding
  `additionalContext`. An auth/transport failure warns and exits 0; a genuinely local failure exits
  nonzero.
- **The BUILT-IN `topos` skill** (`ops/builtin`, `cli_ref`, the repo-top-level `skills/topos/` source) —
  the meta-skill teaching an agent the manifest surface, the contribute loop, distillation, and the
  team-genesis runbook. The binary embeds the three files (`SKILL.md` + `INSTALL.md` + the generated
  `reference.md` = the same bytes as `docs/cli.md`, one renderer `cli_ref::cli_ref_md()`, both copies
  drift-gated) and places them through the ORDINARY engine at the trigger-arming moments, force-synced
  on every bare sweep (changes ride `reloadSkills`). A pre-existing `topos` dir is never written by the
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
