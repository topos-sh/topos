# `topos` — the client CLI

**lib:** the local domain operations, the **sidecar** (an embedded-git store per skill + crash-safe JSON
docs holding identity / per-skill history / mappings), and the bundle scanner — all over a single
fault-injectable fs/syscall seam. **bin:** a thin `clap` wiring; `--json` (no prompts) + a thin TTY
renderer over the SAME typed outcomes (one value, two presentations).

## Implemented (the local, accountless core)

- **The fs/syscall seam** (`fs_seam`) — every durable mutation goes through one `FsOps` trait. `RealFs`
  uses `rustix` (safe; no `unsafe`): `F_FULLFSYNC` on macOS, `flock` for the per-skill writer lock. A
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
  already-tracked dir with `ALREADY_TRACKED`), `list [--footprint]` (the tracked bucket; others render
  empty; footprint = the `~/.topos/` walk plus any harness config topos holds an entry in), `diff`
  (draft↔current via the gitstore `unified_diff` renderer), `log` (local actions + git history), `pull
  [--quiet]` (the session-start currency entry point — a **no-op skeleton** that exits 0 and is byte-silent
  under `--quiet`, until the sync engine lands), `uninstall` (**scrub the currency hook**, then remove the
  binary + `~/.topos/`, touch no skill bytes).

Identity is the kernel's: `version_id`/`bundle_digest` depend only on the bytes + device id + a fixed
message, so injectable id/time sources make `add` deterministic. Golden `--json` fixtures (add/list/diff/log)
are asserted byte-equal in tests.

## Planned (lands later)

The plane + enrollment + signing-at-rest; the `pull` sync engine + the four-state sync machine; the
**byte-writing materialization** (the atomic dir-swap, for an *update* that overwrites a harness skill
dir — this increment writes nothing into a skill dir, so the swap is deferred); `publish`/`review`/
`revert`; the `diff current..<hash>` + `log --team` plane halves; the
OpenClaw/Hermes harness adapters (Claude Code is the reference).

## Architectural layering (enforced at the dependency graph)

**No edge to `plane-store`, no `sqlx`, no `libsqlite3-sys`.** The client is a thin sync tool, never an
authority — a per-target `cargo tree -p topos` assertion (`cargo xtask check-arch`) holds the line.

The sidecar keys skills by id; harness skill directories stay byte-pristine, so uninstall is a no-op for
your skills.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `topos-harness`, `clap`, `serde`/`serde_json`,
`uuid`, `rustix` (safe fsync/flock), `hex` (decode sidecar id fields), `anyhow`, `thiserror`. (The plane
transport + device-key signer land later.)
