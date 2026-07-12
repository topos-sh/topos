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
  below), `invite` (an owner mints an `/i/` link by POSTing the governance Invite op under the workspace credential — see below),
  `list [--footprint] [--tracked] [--remote]` (the tracked bucket + **untracked discovery** — skills sitting
  in any known harness's skill dir, across a baked registry ported from `vercel-labs/skills`, deduped against
  tracked placements by canonical path; `--tracked` suppresses discovery; `--remote` is the **catalog read** —
  a `GET /v1/workspaces/{ws}/skills` (under the workspace Bearer credential) per followed workspace, merged
  with local follow-state
  (Available / Following / FollowingBehind), a per-workspace transport fault degrading to a warning;
  `followed`/`published_by_you` still render empty; footprint = the `~/.topos/` walk plus any harness config
  topos holds an entry in), `diff`
  (draft↔current via the gitstore `unified_diff` renderer), `log` (local actions + git history), `pull
  [<skill>[@<hash>]] [--quiet]` (the session-start currency entry point — see the sync engine below),
  `uninstall` (**scrub the currency hook**, then remove the binary + `~/.topos/`, touch no skill bytes).
- **The `follow` verb** (`ops/follow`, `enroll`, `plane_http::UreqDeviceClient`) — the two-call device-flow
  enrollment + first-receive. `follow <link>` reads the unauthenticated `/i/` **TOFU bootstrap** (fetched
  with an explicit `Accept: application/json` — the same URL serves an agent-instruction markdown to
  everything else), **re-roots onto the bootstrap's declared `plane.base_url`** (a share link may ride the
  team's web origin; the declared API base — normalized, same URL gate, https-may-never-downgrade — is
  what the device flow, the redeem, every pull, and `instance.json` ride; disclosed as
  `FollowData.plane_base_url`), guards one-plane-per-install (a bootstrap declaring a DIFFERENT plane than
  the one already enrolled is refused; there is **no trust root to pin** — the `current` pointer is
  unsigned, its authority the database row and its integrity the content-addressed version id), starts a
  device authorization, writes a **`0600` WAL** (`identity/enrollment.json`), and
  returns `ENROLLMENT_PENDING` + the SERVER-built verification URL with the verified-domain provenance
  (the relay-phishing guard; there is no client-side URL reconstruction — a WAL without the persisted
  URL restarts typed). Re-invoking `follow` while a pending WAL exists (with any target, or none) polls
  once — the "re-invoking IS the resume" idiom (the BIN re-invokes it automatically on a cadence for an
  interactive run or a `--wait [<seconds>]` run, so a person never re-runs by hand; a headless `--json` run
  without `--wait` returns the pending state and never hangs); on a granted poll it **redeems** the grant
  (the bearer credential — the redeem body carries `device_public_key`, which the server checks against the
  grant's bound pubkey; nothing is signed) into the ONE **workspace credential**, records it in the WAL
  **before promotion** (the lockout
  fence — a single-use grant can't be re-redeemed; a re-invoked `follow` over a `Redeemed` WAL re-promotes without
  re-redeeming), then PROMOTES: `instance.json` (the plane), `identity/credentials.json` (the workspace
  credential, `0600`, upsert under the `identity` lock), `follows.json` (the followed set from the WAL's
  offered skills — read-merge-write under the `identity` lock, `0600`; pure subscription state, no secret),
  and `identity/user.json` (metadata, no secret), records the device key in `host.json`, and lays the
  **first-receive baseline** per offered skill. The agent only ever holds the opaque grant + the workspace
  credential — never a user token (I-NO-USER-TOKEN); the device code / grant / workspace credential are
  redacted from every `Debug` and
  never reach a URL / log / error. The promote also **arms the session-start currency hook** — best-effort
  + idempotent, mirroring `add` (a pure follower never runs `add`, so enrollment is their one arm point; a
  degraded config edit is disclosed on the result's `currency` field, never a rolled-back enrollment).
  A KNOWN followed-skill positional — `follow <skill>[@<hash>]` — drives the existing pull engine to
  place a disclosed first-receive offer (the I-TOFU "one accept"), and RESUMES a retained entry
  `unfollow` paused (flips `following` back on; a still-pending first-receive offer is placed, else the
  next `pull` lands current). The positional is dispatched by shape (a pending WAL wins; `@` forces the
  skill path; a known skill name is the skill path; else it is an `/i/` link or a bare invite token). The enrollment transports (`UreqDeviceClient`
  + the read transport for the offer disclosure) are built per-base-URL behind an injectable factory, so the
  whole flow is tested over a **fake** with no HTTP (the real loopback proof lives in
  `tests/tests/follow_e2e.rs`).
- **The `invite` verb** (`ops/invite`, `plane_http::UreqDeviceClient`) — an OWNER mints an `/i/<token>` invite link
  by POSTing the governance Invite op. Requires prior enrollment: the plane (`base_url` from `instance.json`)
  and the workspace (`workspace_id` from `identity/user.json`) come from what `follow`
  wrote (absent ⇒ a typed "run follow first" error). It mints an `op_id` (the canonical hyphenated UUID
  rides the wire, the plane re-parses it to the SAME 16 bytes for idempotency) and POSTs the body under the
  **workspace Bearer credential** — the plane resolves the credential's non-revoked registry row → principal
  → role matrix (a non-owner is DENIED); the acting device is never a body field. **Nothing is signed**
  (git/GitHub-level trust). The role rides the wire body (an
  omitted `--role` defaults to member, matching the plane); the emails are folded to
  `topos_core::identity::canonical_principal`'s ASCII-lowercase form ONCE before the wire body (the plane
  re-folds at its parse boundary), so the roster rows carry one identity per human; the skill **ids** (never
  names) are what the invite pre-offers. The POST rides through the `UreqDeviceClient` behind a
  `GovernanceSource` seam, mapping the all-outcome **200 envelope** (`ok` ⇒ `InviteData`; a role-DENIED
  `!ok` ⇒ a typed "not authorized"); the link never carries a role. A unit test proves the wire body's
  shape (workspace / role / folded emails / skill ids) over a **fake** with no HTTP.
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
  key pinning** — the trust level is the same a team extends to its git host + CI.
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
  (`GET /v1/workspaces/{ws}/skills/{skill}/current` with `If-None-Match` + `Topos-Known-Version-Id`);
  `fetch_version` is a version-metadata GET + per-blob
  content-addressed bundle GETs that **re-verify each `sha256 == object_id`** — all under the workspace
  **Bearer credential**. It is a dumb transport — the
  engine scope-checks the served (unsigned) pointer and re-verifies the fetched bytes against the version
  id. `FileFollow` + the crash-safe `instance.json` (the plane base URL — no trust root), `follows.json`
  (per-skill workspace + mode — pure subscription state), and `identity/credentials.json` (the per-workspace
  Bearer credentials — the **secret**, redacted from `Debug`, never in an error message or URL, joined onto
  the follow-state by `skill_creds`) supply the transport creds + the consent state. `app.rs` (via
  `load_enrollment`) selects the real transport only when `instance.json` is present, else stays inert — and
  `load_enrollment` is **no longer inert in practice**, because `follow` now writes `instance.json` +
  `credentials.json` + `follows.json`. The end-to-end
  pull-over-loopback-HTTP proof lives in the `tests/` member; adding `ureq` keeps the client arch-clean (no
  `plane-store`/`sqlx`/`tokio` edge).
- **The device keypair** (`device_signer`, `identity`) — the device's **keygen-only identity**. An Ed25519
  keypair is **load-or-generated** from a `0600` `identity/device.key` seed (refuse-on-permissive,
  exactly-32-bytes, a `Zeroizing` seed held only transiently, serialized under the identity lock; the
  `SigningKey` self-zeroizes on drop and a hand-written `Debug` redacts the key material). The public key
  REGISTERS the device at enroll; the **`device_key_id`** (`dk_` + the first 32 hex of `sha256(pubkey)`,
  the ONE kernel derivation `topos_core::identity::device_key_id`) is the device's stable, non-secret NAME
  (the receipts/audit actor) — the plane re-derives the SAME id from the registered public key. **Nothing
  signs with the private key** (git/GitHub-level trust): every plane request — reads AND writes AND
  governance — authenticates with the **workspace credential** the redeem mints (Bearer), never the key.
  `host.json` carries a secret-free
  **`DeviceKeyRef`** (the PUBLIC key + a pointer to the sibling `0600` seed, NEVER the seed) via
  `set_device_key`. A KAT pins `device_key_id` against `topos_core::identity::device_key_id`.
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
  CONFLICT / APPROVAL_REQUIRED / DENIED — is an `Ok(WriteReceipt)`; only a transport/non-200/malformed body
  is an `Err`; the served pointer (`wire_record`) is parsed leniently because an OK `review --reject`
  carries `data: {}`). **`publish [--propose] <target>[@<digest>]`** first runs the
  **auto-add pre-step** (`ensure_tracked`): an EXACT tracked name wins straight through, else the target is
  an untracked LOCAL source it adopts before publishing — a discovered `<name>` / `<name>@<harness>`
  (reusing `add`'s `resolve_add_target`) or a `<dir>` (adopted in place via `ops::add`); a remote
  `owner/repo`/URL is refused (add it first), a `@<harness>` disagreeing with an already-tracked skill is
  `HARNESS_MISMATCH`, and `--propose` while un-enrolled is refused BEFORE any adoption. A folded-in add is
  disclosed on the receipt (`PublishData`/`ProposeData` `added`), and the standup resume argv self-heals to
  the adopted `<name>@<digest>`. Then it scans the draft
  (the same source `diff` uses), and when the target pins a `@<digest>` runs the **optional consent gate**
  (recompute the digest over the scanned bytes; refuse on mismatch — never a silent mode-flip; without a pin
  the computed digest just ships), computes the byte-identical `commit_id`/`bundle_digest`
  via the kernel (**I-COMMIT-PARITY** — author = `ctx.device_id`, message = a fixed `"topos: publish"`), pins
  the candidate in the store, persists an **op-WAL** (the extended `OpRecord`, `0600`) BEFORE the first send,
  POSTs, and maps the outcome (OK advances local state read-your-writes; APPROVAL_REQUIRED surfaces the
  `publish --propose` next-action; CONFLICT surfaces rebase; a genesis publish folds in a best-effort,
  owner-gated `/i/` link). **`review <skill>@<hash> --approve|--reject`** binds the proposal's re-derived
  identity at `expected` = the FRESH `current` (a reviewable proposal's base). **`revert --to <good>`** binds
  the forward commit `{parents:[FRESH current], tree: good.tree}` (a stale local parent would be a DENIED, so
  it reads the live current). An UNCERTAIN send keeps the WAL so the next attempt **replays the SAME `op_id`**
  (no double-advance); a settled op deletes it. **`diff <skill> <ref>`** gained the plane half
  (`current..<hash>` / `<hash>` / `<a>..<b>` — a plane endpoint fetches + re-verifies). The commit-id parity
  (I-COMMIT-PARITY) is proven by `topos-core`'s `commit_id` KAT; the op_id-replay test lives in
  `ops/contribute`; the full loop is proven e2e over loopback HTTP in `tests/`.

- **The workspace-standup client** (`ops/publish`'s standup branch + `ops/follow`'s claim door) — the two
  self-serve doors onto the server's genesis seat. **The un-enrolled direct `publish`** stands the
  workspace up instead of failing: the FULL pre-flight (skill resolution, scan, digest, the optional
  `@<digest>` gate) runs BEFORE any network, then a standup device authorization against the hosted base
  (`TOPOS_PLANE_URL` override, else the compiled-in `https://api.topos.sh` — consulted ONLY on this
  branch), a one-plane-per-install guard against the response's declared plane base, a `0600`
  `AuthorizingStandup` WAL, and an `ok`
  PENDING receipt (`PublishData.pending` = `signin_required` + the SERVER-built
  `verification_uri_complete` verbatim + the code + an RFC-3339 expiry) whose `ENROLL_RESUME` next-action
  argv is THE SAME publish command. Re-invoking it polls ONCE (when the target pins a `@<digest>` the
  consent re-derives from it, so drifted bytes are refused before any poll); granted ⇒ redeem (the grant is
  the bearer credential; nothing is signed) → `Redeemed` WAL BEFORE promotion (the shared crash fence) →
  promote → the publish CONTINUES in
  the same invocation, disclosing `workspace <name> — owner
  <principal>` on both surfaces (hijack visibility). The op is unchanged either way — it polls once and
  returns; the BIN (`app.rs`) is what turns that into ONE command: it re-invokes on a fixed cadence until
  the sign-in settles for an INTERACTIVE run (or a `--wait [<seconds>]` `--json` run), and a headless
  `--json` run without `--wait` still returns the PENDING receipt immediately (never hangs). `--propose`
  keeps the typed not-enrolled error; an enrolled device never reaches the branch. **`follow <claim-link>`**
  enrolls in ONE invocation: the
  bootstrap's `enrollment_method` branches (`admin_claim` ⇒ pin → pre-send `ClaimPending` WAL (`0600`,
  token redacted) → POST `/v1/admin-claim` → promote; an unknown method fails CLOSED typed); an uncertain
  send retries the POST directly from the WAL on the next invocation — never refetching the
  possibly-consumed `/i/` link (the server's same-device replay re-answers Redeemed). The seated
  `principal` persists into `user.json` (+ `email` when email-shaped), the WAL context records the
  enrollment ROOT (invite / standup / claim ⇒ an honest `invite_rooted`), a DENIED grant redeem is the
  typed ask-an-owner error (`REQUEST_ACCESS`), and the invite follow now persists + re-emits the
  server-built `verification_uri_complete` verbatim (reconstruction is only the older-plane fallback).

- **The `unfollow` verb** (`ops/unfollow`) — stop following `current`, KEEP the bytes. Local-only and
  byte-inert: it flips `following = false` in `follows.json` via the same identity-locked read-merge-write
  the enrollment uses (retaining the workspace / mode so a later
  `follow <skill>` resumes — flipping the flag back on and, if a first-receive offer is still
  pending, placing it; the workspace credential stays in `credentials.json`),
  and touches nothing else — never a skill file, never the sync state or a `held` pin, never the currency
  hook (the per-install hook's sweep simply skips an unfollowed skill; `load_enrollment` keeps the pinned
  plane key loaded even with zero active follows, so an enrolled author who unfollowed everything can
  still publish/revert/review). Idempotent: not-followed / already-unfollowed is the same clean success;
  an explicit local `pull <skill>@<hash>` (a user-initiated go-back) remains available on an unfollowed
  copy. Golden `--json` fixture + a byte-identity test (the placement bytes hash equal across unfollow).

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

## Planned (lands later)

The **workspace-credential model is now in place**: enrollment mints ONE Bearer **workspace credential**
per (workspace × device) that authenticates EVERY plane request — reads AND writes AND governance — and
authorization server-side is workspace membership. The device keypair remains **keygen-only identity** (the
non-secret `device_key_id` names the device; nothing signs). `follows.json` is pure subscription state;
`identity/credentials.json` (a `0600` secret) holds the per-workspace credentials; a first-run migration in
`read_follows` scrubs any legacy `read_token` field without losing a follow. Still to come:
**Multi-reviewer
governance** (reviewer roles / N-approver / a rendered diff UI — single-approver, plain unified diff only) +
the **`review-required` policy toggle verb** (enforcement is built; the policy row is a plane/console
setting) + `log --team`'s plane half; harness *selection* in the composition root (v0 constructs Claude
Code only; both the OpenClaw and Hermes adapters are built + wired into `adapter_for` — OpenClaw's
concrete config bytes and Hermes's per-turn-injection claim stay pilot-pending behind their readiness
probes; only Claude Code guarantees the swap completes before skills resolve, so non-Claude adapters
leave a named, bounded multi-file-read residual). The passcode / magic-link / OIDC identity steps run on
the plane's verification page (the agent only polls), so the client needs no UI for them.

## Architectural layering (enforced at the dependency graph)

**No edge to `plane-store`, no `sqlx`, no `libsqlite3-sys`.** The client is a thin sync tool, never an
authority — a per-target `cargo tree -p topos` assertion (`cargo xtask check-arch`) holds the line.

The sidecar keys skills by id; harness skill directories stay byte-pristine, so uninstall is a no-op for
your skills.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `topos-harness`, `clap`, `serde`/`serde_json`,
`uuid`, `rustix` (safe fsync/flock + the atomic dir-swap), `hex` (decode sidecar id fields), `base64`
(encode-side for the enrollment wire — the device public key in the authorize/redeem bodies is base64url —
and the contribute candidate's `content_base64`; wire encoding only, not a signing edge), `ureq` (the
blocking rustls+ring plane + enrollment transport — self-contained, so no `tokio`/`plane-store`/`sqlx`
edge), `ed25519-dalek` (`std` + `zeroize` — the device **keypair** custody: keygen only, NOTHING signs;
the public key is the device's registered identity), `getrandom` (first-run seed entropy) + `zeroize`
(wipe the transient seed buffer), `anyhow`, `thiserror`. None of these crates cross `check-arch`'s line
(it bans only `plane-store`/`sqlx`/`libsqlite3-sys`/`tokio`/`reqwest`/`hyper`); `topos-core` is
signature-free `no_std`.
