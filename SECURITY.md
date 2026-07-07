# Security policy

## Reporting a vulnerability

Please report security issues **privately** — do not open a public issue.

Use GitHub's private vulnerability reporting:

> **[Report a vulnerability](https://github.com/topos-sh/topos/security/advisories/new)**
> (repository → **Security** → **Advisories** → **Report a vulnerability**)

This opens a private advisory visible only to you and the maintainers. Where you can, include:

- the impact — what an attacker gains, and the trust boundary it crosses;
- the affected component (the `topos` CLI, the `topos-plane` server, or a specific crate) and version;
- a minimal reproduction — commands, a bundle, or a request sequence.

For how the trust model works — what the digest, signing, and consent chain protect and deliberately do not
— see [`ARCHITECTURE.md`](ARCHITECTURE.md) and the "Trust & security" and self-hosting sections of the
[`README`](README.md).
