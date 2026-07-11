# Security policy

## Trust model

A behavior you follow is code and prose that runs inside your agent, so integrity and consent are the
whole point of the tool. Topos borrows the trust model of a git host plus CI — nothing new is invented.

- **TLS everywhere; every request is made by an authenticated principal** — a web session, an enrolled
  device, or an opaque per-skill credential, each resolved against live database rows.
- **Access is database policy.** Roster, role, and device rows decide every authorization; revocation is a
  row change, effective immediately — a revoke committed before a promotion blocks that promotion inside the
  same transaction.
- **Versions are content-addressed.** A version's identity IS the sha256 of its bytes; what you pin is
  exactly what you get. The client re-verifies the byte-exact digest on every apply, so corruption or
  tampering in transit or storage is structurally visible — a mismatch is a loud integrity error, never a
  silent overwrite.
- **No anonymous writes.** Every mutation is attributed to a device or session identity and recorded as a
  durable receipt plus an audit event, with all-outcome idempotency — a retried write replays its original
  result and never double-applies.
- **Server-side gates.** Review-required protection, four-eyes approval (a proposer cannot approve their own
  proposal), and last-owner lockout are enforced in the authority, not the client.

Assurance is **visibility, not cryptography**: an inspectable history, durable receipts, and a one-command
revert let a team catch and undo a bad change. Topos proves provenance and consent — it does not judge
whether an approved behavior is safe to run, and a followed behavior runs with your harness's permissions.

### What Topos deliberately does not do

There is **no signing** anywhere in Topos: no signed pointers, no key pinning, no client-side signature
verification, no anti-rollback cryptography. The accepted consequence is explicit — a compromised server can
distribute bad content — and it is the same risk every team already accepts from its git host and CI.
Content addressing keeps optional signing available as a future layer without redesign.

## Reporting a vulnerability

Please report security issues **privately** — do not open a public issue.

Use GitHub's private vulnerability reporting:

> **[Report a vulnerability](https://github.com/topos-sh/topos/security/advisories/new)**
> (repository → **Security** → **Advisories** → **Report a vulnerability**)

This opens a private advisory visible only to you and the maintainers. Where you can, include:

- the impact — what an attacker gains, and the trust boundary it crosses;
- the affected component (the `topos` CLI, the `topos-plane` server, or a specific crate) and version;
- a minimal reproduction — commands, a bundle, or a request sequence.

For how the trust model works, see [`ARCHITECTURE.md`](ARCHITECTURE.md) and the self-hosting section of the
[`README`](README.md).
