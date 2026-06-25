# `topos` — the client CLI

**lib:** the 12-verb domain operations, the `pull` state machine (check-for-update → plan → apply), the
**sidecar** (a partial git store + crash-safe JSON docs holding identity / per-skill history / mappings),
namespace-atomic materialization (a private fs/syscall seam, fault-injectable), and the device-flow client
+ the device-key signer.

**bin:** `clap` wiring of the 12 verbs; `--json` (no prompts) + a thin TTY renderer over the SAME typed
outcomes (one value, two presentations).

## Architectural layering (enforced at the dependency graph)

**No edge to `plane-store`, no `sqlx`, no `libsqlite3-sys`.** The client is a thin sync tool, never an
authority. The intended gate is a per-target `cargo tree -p topos` assertion — those crates legitimately
exist elsewhere in the workspace via the plane, so this is a dependency-tree check, not a global ban.

The sidecar keys skills by id; harness skill directories stay byte-pristine, so uninstall is a no-op for
your skills.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `topos-harness`, `clap`, an HTTP client
(`ureq` default — blocking, rustls, no tokio), `ed25519-dalek`.
