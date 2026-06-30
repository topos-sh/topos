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
- **The Claude Code adapter wiring** (`config_io` + the `&dyn HarnessAdapter` seam on `Ctx`) — `topos`
  drives `topos-harness::ClaudeCode` for discovery, adopt-in-place recognition, and the session-start
  currency hook. The adapter owns the strict-JSON `settings.json` merge; the durable write goes through a
  small `ConfigStore` port implemented here, which reuses the one `atomic_write` dance over `FsOps` (so
  the existing crash gate covers the config write too — never a second atomic-write to drift). The
  foreign-file writer adds the care a shared user file needs: ensure the parent dir, write through a
  symlink, a topos-namespaced temp, best-effort mode preservation.
- **The verbs** (`ops`) — `add` (mint id+name, scan + import, stage + publish with one rename — all-or-
  nothing; **recognize a Claude Code skill dir, tag it + arm the currency hook**; refuse re-adopting an
  already-tracked dir with `ALREADY_TRACKED`), `follow` (the device-flow enrollment + first-receive — see
  below), `list [--footprint]` (the tracked bucket; others render
  empty; footprint = the `~/.topos/` walk plus any harness config topos holds an entry in), `diff`
  (draft↔current via the gitstore `unified_diff` renderer), `log` (local actions + git history), `pull
  [<skill>[@<hash>]] [--quiet]` (the session-start currency entry point — see the sync engine below),
  `uninstall` (**scrub the currency hook**, then remove the binary + `~/.topos/`, touch no skill bytes).
- **The `follow` verb** (`ops/follow`, `enroll`, `plane_http::UreqEnroll`) — the two-call device-flow
  enrollment + first-receive. `follow <link>` reads the unauthenticated `/i/` **TOFU bootstrap**, pins the
  plane key (I-TOFU: absent → first pin; same base-url different key → `KEY_REPIN_REQUIRED`; cross-base-url
  → refused — one plane per install; the `alg` is a CLOSED enum, so a non-Ed25519 trust root fails the
  deserialize), starts a device authorization, writes a **`0600` WAL** (`identity/enrollment.json`), and
  returns `ENROLLMENT_PENDING` + the verification URL with the verified-domain provenance (the
  relay-phishing guard). `follow --resume` polls once; on a granted poll it signs the **enroll possession
  proof** (the device signer, binding `device_auth_id = user_code` + the offered-skill set + `grant_hash`),
  **redeems** the grant into per-skill read creds, records them in the WAL **before promotion** (the lockout
  fence — a single-use grant can't be re-redeemed; a re-`--resume` of a `Redeemed` WAL re-promotes without
  re-redeeming), then PROMOTES: `instance.json` (the pinned key + the workspace disclosure), `follows.json`
  (read-merge-write under the `identity` lock, **`0600`** — a second follow never clobbers the first), and
  `identity/user.json` (metadata, no secret), records the device key in `host.json`, and lays the
  **first-receive baseline** per skill. The agent only ever holds the opaque grant + the read creds — never a
  user token (I-NO-USER-TOKEN); the device code / grant / read tokens are redacted from every `Debug` and
  never reach a URL / log / error. `follow --approve <skill>[@<hash>]` drives the existing pull engine to
  place a disclosed first-receive offer (the I-TOFU "one --approve"). The enrollment transports (`UreqEnroll`
  + the read transport for the offer disclosure) are built per-base-URL behind an injectable factory, so the
  whole flow is tested over a **fake** with no HTTP (the real loopback proof lands with the test member next).
- **The pull/apply sync engine** (`ops/sync_engine`, `ops/pull`, `materialize`, `plane`) — the
  `checkForUpdates → plan → apply` machine over the kernel's four-state transition: a conditional read of
  the signed `current` pointer through the `PlaneSource` seam, signature + workspace/skill scope
  authentication, the anti-rollback floor (`observed` rises only on a verified strictly-higher record;
  never auto-applies a record at or below the floor) and the reused-tuple ALARM, a draft snapshot-on-touch
  before any decision, fetch + re-verify (digest == tree == `commit_id`) + an ancestor-backfilling durable
  record into the sidecar store, the post-fetch heal (a crash-after-swap advances `applied` with no second
  swap, never a false divergence), the consent decision (the kernel's one policy), and **crash-safe
  byte-writing materialization** (staging sibling → fsync → atomic dir-swap → fsync parent → `map → lock →
  sync` commit; `applied` advances only post-swap). `pull <skill>` accepts a pending update (the explicit
  command is the consent a confirm-each offer solicited); `pull <skill>@<hash>` goes back to a version
  locally (sets `held`, never lowers the floor). In tests the plane response + follow-state are
  **fixture-fed**; in production they now come from the real `ureq` transport + the on-disk follow-state that
  `follow` writes — so a bare `pull` with nothing followed stays an honest no-op. A **never-received**
  followed skill (the first-receive baseline `follow` lays: empty `recorded` at the genesis floor) is a
  state-② offer the engine OFFERS on a bare sweep (never auto-lands — I-TOFU, even for an `auto` follower)
  and PLACES on an explicit accept / `follow --approve`; the engine change is minimal + additive (a
  `known_current`/`first_receive` read of the baseline + a `FirstReceiveFromLink` row in the situation map).
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
  (`If-None-Match` + `Topos-Known-Version-Id`); `fetch_version` is a version-metadata GET + per-blob
  content-addressed bundle GETs that **re-verify each `sha256 == object_id`**. It is a dumb transport — the
  engine still verifies the pointer signature against the pinned key. `FileFollow` + the crash-safe
  `instance.json` (base URL + pinned plane **public** key) and `follows.json` (per-skill workspace + read
  token + mode) docs supply the transport creds + the consent state; the read token is a **secret** (redacted
  from `Debug`, never in an error message or URL). `app.rs` (via `load_enrollment`) selects the real transport
  + pinned key only when enrolled AND following ≥1 skill, else stays inert — and `load_enrollment` is **no
  longer inert in practice**, because `follow` now writes `instance.json` + `follows.json`. The end-to-end
  pull-over-loopback-HTTP proof lives in the `tests/` member; adding `ureq` keeps the client arch-clean (no
  `plane-store`/`sqlx`/`tokio` edge).
- **The device keypair + signer** (`device_signer`, `identity`) — the client's **only** private-key edge,
  mirroring the plane's in-process signer. An Ed25519 signing key is **load-or-generated** from a `0600`
  `identity/device.key` seed (refuse-on-permissive, exactly-32-bytes, a `Zeroizing` seed held only
  transiently, serialized under the identity lock; the `SigningKey` self-zeroizes on drop and a hand-written
  `Debug` redacts the key material). The **`device_key_id`** is derived byte-for-byte the way the plane
  derives it (`dk_` + the first 32 hex of `sha256(pubkey)`) — so the frames the client signs bind the SAME id
  the plane re-derives and verifies. Three concrete signers over `topos-core`'s frozen preimages —
  `sign_enroll` / `sign_governance` / `sign_device_op` (the last built now for the contribute verbs that land
  next) — each unit-proven to round-trip through the kernel's `verify_*` (one shared preimage, so signer +
  verifier agree by construction). `host.json` now carries a secret-free **`DeviceKeyRef`** (the PUBLIC key +
  a pointer to the sibling `0600` seed, NEVER the seed) via `set_device_key`. **`sign_enroll` is now wired**
  — `follow --resume` signs the enroll possession proof + records the device key in `host.json`;
  `sign_governance` / `sign_device_op` are wired by the invite / contribute verbs next.
- **The private-file FsOps primitives** (`fs_seam`, `atomic`, `doc`) — secrets need `0600`. The seam gains
  `write_private` (mode 0600 **from creation** — no world-readable window, no chmod-after-write race) +
  `private_perms_ok` (the refuse-on-permissive read gate), both threaded through the `FaultFs` crash gate;
  `atomic_write_private` is the crash-safe secret write (its temp is 0600 from creation, so a fault never
  leaves a world-readable partial), and `write_doc_private` / `read_doc_private` the typed secret-doc pair
  (`read_doc_private` fails closed on a group/other-accessible secret BEFORE parsing). The device seed,
  `follows.json`, **and** the enrollment WAL (`identity/enrollment.json`) now all go through these `0600`
  primitives.

Identity is the kernel's: `version_id`/`bundle_digest` depend only on the bytes + device id + a fixed
message, so injectable id/time sources make `add` deterministic. Golden `--json` fixtures (add/list/diff/log)
are asserted byte-equal in tests.

## Planned (lands later)

The `invite` verb (mint an `/i/` link — wires `sign_governance`) lands next; `unfollow` (stop following
`current`, keep the bytes) + signing-at-rest still later; the client `publish`/`review`/`revert` verbs that
WIRE the now-built `sign_device_op` (the **publish guard** over `conflict.json` is built + unit-tested now,
wired then); the `diff current..<hash>` + `log --team` plane halves; the OpenClaw/Hermes harness adapters
(Claude Code is the reference — only it guarantees the swap completes before skills resolve; the others
leave a named, bounded multi-file-read residual). The passcode / magic-link / OIDC identity steps run on the
plane's verification page (the agent only polls), so the client needs no UI for them.

## Architectural layering (enforced at the dependency graph)

**No edge to `plane-store`, no `sqlx`, no `libsqlite3-sys`.** The client is a thin sync tool, never an
authority — a per-target `cargo tree -p topos` assertion (`cargo xtask check-arch`) holds the line.

The sidecar keys skills by id; harness skill directories stay byte-pristine, so uninstall is a no-op for
your skills.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `topos-harness`, `clap`, `serde`/`serde_json`,
`uuid`, `rustix` (safe fsync/flock + the atomic dir-swap), `hex` (decode sidecar id fields), `base64`
(verify-side decode of the signed pointer's `Signature.value`, **and** encode-side for the enrollment wire —
the device public key + the enroll-possession signature header are base64url; neither is the private-key
signing edge `check-arch` forbids), `ureq` (the blocking rustls+ring plane + enrollment transport — self-contained, so no
`tokio`/`plane-store`/`sqlx` edge), `ed25519-dalek` (`std` + `zeroize` — the device-key SIGNER, the client's
only private-key edge), `getrandom` (first-run seed entropy) + `zeroize` (wipe the transient seed buffer),
`anyhow`, `thiserror`. None of the crypto crates cross `check-arch`'s line (it bans only
`plane-store`/`sqlx`/`libsqlite3-sys`/`tokio`/`reqwest`/`hyper`); `topos-core` stays verify-only `no_std`.
