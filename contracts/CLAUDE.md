# `contracts/` — the generated, committed cross-language contract

The source of truth for the wire boundary, GENERATED from the Rust types, committed +
diff-reviewed. A hand-edited protocol drifts; a generated-and-reviewed artifact does not. Any
consumer in another language pins these by the exact commit. **Never hand-edit anything here.**

- **`schemas/`** — one JSON-Schema (2020-12) per wire type, per-verb `data` payload, and
  persisted client document, generated from `topos-types` (`cargo xtask gen-schema`;
  `--check` is the drift gate — stale / missing / orphan all fail).
- **`fixtures/json/`** — golden `--json` envelopes (success + failure), generated from the typed
  shapes (`cargo xtask gen-fixtures`), so they cannot drift from the contract. The digest golden
  vector, consent truth-table, and identity byte vectors live as `topos-core` known-answer tests.
- **`openapi/openapi.json`** — the public session-lane HTTP contract, generated from
  `topos_plane::openapi()`. It spans two serving tiers behind one public base: the vault's live
  handlers plus contract-only stubs (`bins/topos-plane/src/routes/door.rs`) for the routes the
  web app serves — one pinned artifact, riding the same `gen-schema` run and drift gate.
- **`docs/cli.md`** (a sibling generated artifact, outside `contracts/`) — the CLI reference
  rendered from the real clap tree (`cargo xtask gen-cli-ref`, byte-gated), same discipline.
