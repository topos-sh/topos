# Security policy

## Trust model

A behavior you follow is code and prose that runs inside your agent, so integrity and consent are the
whole point of the tool. Topos borrows the trust model of a git host plus CI — nothing new is invented.

- **TLS everywhere; every request is authenticated.** A request is either a signed-in person (a Better
  Auth session) or an enrolled device (its one bearer credential); the app↔vault internal lane carries its
  own shared bearer. There are no anonymous requests and no per-skill credentials.
- **Access is database policy.** Membership is a **seat** row keyed by a person's identity; the seat, its
  role, and the device rows decide every authorization, re-resolved from the trusted rows on every request
  (never a caller-asserted id). Revocation is a row change, effective immediately — and a device revoke is
  **final**, enforced by a database trigger so no ordinary code path can un-revoke it; rotation is revoke +
  re-enroll.
- **Versions are content-addressed.** A version's identity IS the sha256 of its bytes; what you pin is
  exactly what you get. The client re-verifies the byte-exact digest on every apply, so corruption or
  tampering in transit or storage is structurally visible — a mismatch is a loud integrity error, never a
  silent overwrite.
- **No anonymous writes.** Every mutation is attributed to a person or device identity and recorded as a
  durable receipt plus an audit event, with all-outcome idempotency — a retried write replays its original
  result and never double-applies.
- **Server-side gates.** Review-required protection, four-eyes approval (a proposer cannot approve their
  own proposal), and last-owner lockout are enforced in the app tier inside the database transaction, not
  in the client.

Assurance is **visibility, not cryptography**: an inspectable history, durable receipts, and a one-command
revert let a team catch and undo a bad change. Topos proves provenance and consent — it does not judge
whether an approved behavior is safe to run, and a followed behavior runs with your harness's permissions.

## Identity

There is **one identity**: a person's `user.id`. Email is a mutable attribute and a login name, never an
authorization key — nothing anywhere compares an email to decide access, so an address change (or a
lookalike) can never become an authority event. Every seat, device, subscription, and audit row references
a `user.id`.

Credentials are **hash-stored, and the hash is computed in the database** (Postgres' built-in SHA-256 over
a presented code or credential; passwords use the auth library's own hasher). No plaintext secret lands in
a table, a log, or an error, and the web tier itself computes no digest.

The ceremonies that mint identity:

- **First boot** creates the workspace and prints **one** claim link to the server logs
  (`→ Finish setup: <origin>/claim?code=…`). Whoever opens it creates the first account (email + password)
  and is seated as the owner. A preset `TOPOS_SETUP_CODE` makes the link reproducible for CI/IaC; only the
  code's SHA-256 is stored, and the code dies on first use.
- **Registration is never open.** A sign-up succeeds only with the setup claim code, **or** a pending
  invitation on a deployment whose SMTP is armed (the invited seat binds only after the mailbox round-trip,
  so an unverifiable address admits nothing), **or** the off-by-default `registration = 'open'` operator
  knob. Every other attempt gets one constant, non-enumerating refusal — the same answer whether the
  address is unknown, uninvited, expired, or already taken. The rule runs as a database hook under every
  sign-up path, so a new auth method cannot reopen it by accident.
- **Device enrollment** is a GitHub-style flow: the CLI prints "open `<origin>/verify` and enter AB12-CD34";
  the signed-in person approves it with **a plain signed-in accept** — the live session plus the explicit
  approve click — which mints the device (owned by that person) and its one bearer credential. Revocation is
  self-service, immediate, and final.
- **Password recovery** sends reset mail when SMTP is armed; a mail-less solo owner runs a one-shot
  container command that prints a single-use recovery code (`web/scripts/mint-recovery-code.mjs`) — machine
  control on the box is the proof.

**Confirmation** guards every admin ceremony in proportion to its reach — a live authenticated session
plus the role gate is what authorizes the act; there is no separate re-authentication step to steal.
Destructive ceremonies (skill delete, version purge, channel delete) additionally require typing the
resource's exact name; acts with cross-person reach carry an explicit in-place confirmation in the UI; and
routine policy saves are plain submits. Every attempt, refusals included, lands an audit row.

**Outbound mail is logged metadata-only.** The one mail transport records a `mail_event` for every send
attempt — the kind, the recipient, and the outcome (ok or a coarse failure code) — and never the subject,
the body, or the relay response, because a message body can carry a live credential (an invite token, a
reset link).

## The vault

The vault (the Rust plane) is **pure byte custody** and **identity-free**: it holds the content-addressed
version index, the single-generation compare-and-set `current` pointer, and the object upload/lifecycle
bookkeeping — no identity, membership, or policy row exists below it. It runs **internal-network-only** with
exactly **one caller** (the web app), authenticated by the internal bearer, and treats every request as
already authorized; the attribution it records is pass-through display text the request carries. All
authorization happens in the app tier, once, before a byte op is forwarded.

## The gates that pin this

Each invariant above is held by an automated gate, not by convention:

- **Vault vocabulary + schema boundary** (`cargo xtask check-arch`): no identity vocabulary and no
  app-schema table name may appear anywhere in the vault's code or SQL — the identity-freedom is mechanical.
- **Cross-lane grants shape** (`scripts/check-db-grants.sh`): two database roles, one per application, each
  owning its schema; the web role cannot write or alter the vault's schema, and the vault role cannot read
  the app's. The gate proves it by **logging in as each role** (not `SET ROLE`, which would not adopt the
  role's real connection settings).
- **Email-authorization** (`web/scripts/check-email-authz.mjs`): fails the build on any email-equality
  branch or the retired email-canonicalization defenses — the one-identity rule, made executable.
- **Trust boundary** (`web/scripts/check-boundary.mjs`): the web tier holds no signing machinery and
  computes no digest; every vault byte rides one allowlisted transport; every data-reading route carries an
  auth guard; the app ships zero client-side env.

## What Topos deliberately does not do

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
- the affected component (the `topos` CLI, the `topos-plane` vault, the web app, or a specific crate) and
  version;
- a minimal reproduction — commands, a bundle, or a request sequence.

For how the trust model works, see [`ARCHITECTURE.md`](ARCHITECTURE.md) and the self-hosting section of the
[`README`](README.md).
