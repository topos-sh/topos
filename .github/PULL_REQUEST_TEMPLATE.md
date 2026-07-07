<!-- Thanks for contributing to Topos! Keep PRs focused; open an issue first for anything large. -->

## What & why

<!-- What does this change do, and why? Link any related issue: Closes #123 -->

## Checklist

- [ ] `cargo xtask ci` passes locally (fmt, clippy, doc, drift gates, check-arch)
- [ ] `cargo test` passes (with a reachable `DATABASE_URL`) — or N/A, explain below
- [ ] Tests added/updated for behavior changes
- [ ] If wire types or SQL changed: regenerated the contract (`gen-schema` / `gen-fixtures`) and/or `.sqlx` metadata, and committed the result
- [ ] Living docs updated in this change (per-folder `CLAUDE.md` status)

## Notes for reviewers

<!-- Anything that helps the review: trade-offs, follow-ups, areas you're unsure about. -->

---

By opening this pull request I agree that my contribution is licensed to the project under the Apache-2.0
license (inbound = outbound; no CLA — see CONTRIBUTING.md).
