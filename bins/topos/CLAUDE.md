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
- **The verbs** (`ops`) — `add` (mint id+name, scan + import, stage + publish with one rename — all-or-
  nothing), `list [--footprint]` (the tracked bucket; others render empty), `diff` (draft↔current, a
  vendored unified diff), `log` (local actions + git history), `uninstall` (remove the binary + `~/.topos/`,
  touch no skill bytes).

Identity is the kernel's: `version_id`/`bundle_digest` depend only on the bytes + device id + a fixed
message, so injectable id/time sources make `add` deterministic. Golden `--json` fixtures (add/list/diff/log)
are asserted byte-equal in tests.

## Planned (lands later)

The plane + enrollment + signing-at-rest; `follow`/`pull` + the four-state sync machine; the harness
adapters + placement (atomic dir-swap); `publish`/`review`/`revert`; the `diff current..<hash>` + `log
--team` plane halves; the large-object offload.

## Architectural layering (enforced at the dependency graph)

**No edge to `plane-store`, no `sqlx`, no `libsqlite3-sys`.** The client is a thin sync tool, never an
authority — a per-target `cargo tree -p topos` assertion (`cargo xtask check-arch`) holds the line.

The sidecar keys skills by id; harness skill directories stay byte-pristine, so uninstall is a no-op for
your skills.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `topos-harness`, `clap`, `serde`/`serde_json`,
`uuid`, `rustix` (safe fsync/flock), `anyhow`, `thiserror`. (The plane transport + device-key signer land later.)
