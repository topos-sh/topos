# Security policy

## Trust model

A behavior you receive is code and prose that runs inside your agent, so integrity and consent are the
whole point of the tool. Topos borrows the trust model of a git host plus CI — nothing new is invented.

- **TLS everywhere; every request is authenticated.** A request is either a signed-in person (a Better
  Auth session) or a logged-in CLI session (its workspace-scoped bearer credential); the app↔vault internal
  lane carries its own shared bearer. There are no anonymous requests and no per-skill credentials.
- **Access is database policy.** Membership is a **seat** row keyed by a person's identity; the seat, its
  role, and the CLI-session rows decide every authorization, re-resolved from the trusted rows on every
  request (never a caller-asserted id). Revocation is a row change, effective immediately — an ended
  session is gone the instant the row commits, severable by both sides (the client's `logout`, the owner's
  sessions page); rotation is logout + login.
- **File-bundle versions are content-addressed.** A skill version's identity IS the sha256 of its bytes;
  what you pin is exactly what you get. The client re-verifies the byte-exact digest on every apply, so
  corruption or tampering in transit or storage is structurally visible — a mismatch is a loud integrity
  error, never a silent overwrite. MCP server definitions are not byte artifacts and carry no digest:
  each is a database row with an append-only revision history, every change attributed and audited, and
  what a hash could not promise for them — that the server behind a URL behaves tomorrow as it did
  today — nothing else can either.
- **No anonymous writes.** Every mutation is attributed to a person or session identity and recorded as a
  durable receipt plus an audit event, with all-outcome idempotency — a retried write replays its original
  result and never double-applies.
- **Server-side gates.** Review-required protection, four-eyes approval (a proposer cannot approve their
  own proposal), and last-owner lockout are enforced in the app tier inside the database transaction, not
  in the client.

Assurance is **visibility, not cryptography**: an inspectable history, durable receipts, and a one-command
revert let a team catch and undo a bad change. Topos proves provenance and consent — it does not judge
whether an approved behavior is safe to run, and a received behavior runs with your harness's permissions.

## Identity

There is **one identity**: a person's `user.id`. Email is a mutable attribute and a login name, never an
authorization key — nothing anywhere compares an email to decide access, so an address change (or a
lookalike) can never become an authority event. Every seat, CLI session, profile, and audit row references
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
- **CLI login** is a GitHub-style flow: the CLI prints "open `<origin>/verify` and enter AB12-CD34"; the
  signed-in person approves it with **a plain signed-in accept** — the live session plus the explicit
  approve click — which mints the CLI session (user × workspace × installation, owned by that person) and
  its workspace-scoped bearer credential. Ending it is self-service (client or owner side) and immediate.
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

**Bundle content is not signed:** no signed pointers, no key pinning, no client-side signature verification
of bundle bytes, no anti-rollback cryptography. The accepted consequence is explicit — a compromised server
can distribute bad content — and it is the same risk every team already accepts from its git host and CI.
Content addressing keeps optional signing available as a future layer without redesign.

**The CLI's own release artifacts are the one signed channel.** Every published asset carries a minisign
signature whose trusted comment binds it to its tag and filename. `topos self-update` compiles in the
release public key and verification is **mandatory and fail-closed** — the `.minisig` is fetched and
verified over the downloaded bytes *before* the checksum gate and long before the installed binary is
touched; a missing or invalid signature aborts the update. `scripts/install.sh` embeds the same key and
**always** fetches the `.minisig` (a signature it cannot download is a hard failure), but it can only
*verify* when the `minisign` tool is present on the machine; without it the installer says so loudly and
falls back to the never-skippable SHA-256 check, which rides the same origin as the asset — transit
integrity, not origin integrity.

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
