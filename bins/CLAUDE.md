# `bins/` — the two programs

Each is a **lib + a thin bin**: the lib holds the logic (unit-testable without a binary), the bin is a thin
composition root.

- **`topos/`** — the CLIENT. The CLI an agent drives — 14 behavior verbs + 2 maintenance groups
  (`self-update`, `auth`); the full reference is generated into `docs/cli.md`. Depends on the kernel +
  gitstore + the harness port. **Takes no dependency on `plane-store` / `sqlx`** — it is a thin sync tool,
  never an authority.
- **`topos-plane/`** — the OSS PLANE. A library (`plane-core`: the composable authority API + a
  `router(state)` builder) + a thin `axum` bin. A separate private product imports and composes this
  library; there is **no** extension hook here.
