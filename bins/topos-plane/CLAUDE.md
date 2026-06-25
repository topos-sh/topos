# `topos-plane` — the OSS plane (library-first)

## lib (`plane-core`) — the composable surface a downstream plane builds on

- the authority operations (over `plane-store`): publish / set-current / review / revert / roster / enroll /
  read-object / get-current;
- a `router(state) -> Router` builder;
- the **`review-required` workspace policy** (an authoritative row read + locked inside the publish txn);
- the enrollment state machine (device-flow / passcode / magic-link / invite-chain / one generic OSS OIDC
  connector — concrete modules, behind a cargo feature, deferred).

## bin

A thin `axum` `main` that serves `router(state)` — no trust decision, no raw object read in a handler.

## The litmus for what belongs in this lib

*Would you expose it even if there were no private product?* `router()`, the ops, the policy — **yes**
(self-hosters, integrators, and the tests all want them). An extension/callback hook that serves only a
closed product — **no**, and there is none here. A separate private program IMPORTS this library and
composes it (its routes call these public ops; its middleware sits in front); the library never calls back
into it. Anything that must run inside the publish transaction is, by definition, trust logic and lives
here, parameterized by config (like the `review-required` boolean) — never injected from outside.

Dependencies: `plane-store`, `topos-core`, `topos-types`, `axum`, tower middleware, `oauth2` /
`openidconnect` (feature-gated), `lettre`, `tracing`.
