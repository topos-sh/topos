# `topos-harness` — the `HarnessAdapter` port

The `HarnessAdapter` trait + its three impls: **Claude Code** (the reference), **OpenClaw**, **Hermes**.
The one real client-side port (three shipping impls). Does discovery + byte-exact placement targeting +
currency-trigger install.

**ALL platform / harness-version dependencies live here** — the rest of the workspace stays
platform-agnostic.

**Content-blind: no translate, no project.** v0 places a skill's **exact bytes** into a harness's skill
directory — no frontmatter rewrite, no dialect translation between harnesses. Adding a harness is a new
impl (a directory mapping + a currency trigger), not a refactor anywhere else.

Dependencies: `topos-core`, `topos-types`, plus the platform crates.
