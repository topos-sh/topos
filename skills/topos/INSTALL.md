# Installing topos

`topos` is the CLI that delivers a team's shared skills to this machine and keeps them current.
One consent rule governs this whole page: propose the command and say what it does — the human
runs it, or gives an explicit yes before you do. Never install anything unasked.

## Install the CLI

```sh
curl -fsSL https://topos.sh/install | sh
```

Installs the `topos` binary to `~/.local/bin` (no sudo) on macOS (Apple Silicon and Intel) and
Linux (x86_64 and arm64); on Windows, run it inside WSL2. The installer downloads the release's
SHA-256 manifest over TLS, prints the expected and actual checksums, and refuses to install on a
mismatch.

Manual alternative: download `topos-<target>.tar.gz` and `SHA256SUMS` from
<https://github.com/topos-sh/topos/releases>, check the archive's SHA-256 against its manifest
entry, and unpack the `topos` binary onto your PATH. Either way, `topos self-update` replaces the
binary in place from then on (same checksum discipline).

If this page arrived as part of a downloaded `topos` skill, add one step after the install:
`topos follow topos --yes` lets this machine's topos manage that downloaded copy and keep it
current (the bare `topos follow topos` only describes).

## Connect it to a workspace

**Join an existing team.** Ask a teammate for the workspace address — each publish prints it, and
it looks like `https://topos.sh/<workspace>`. Then:

```sh
topos follow <workspace-address>
```

Open the printed approval URL in a browser and approve — that enrolls this machine. The command
then describes what the team offers and prints a `--yes` next action; running that `--yes` is
what lands the skills, and they stay current from then on.

**Start fresh.** Sign up at <https://topos.sh> and create a workspace in the browser, then enroll
this machine against your own address the same way: `topos follow https://topos.sh/<your-workspace>`
— approve in the browser, then run the printed `--yes` next action to land what the workspace
offers.

**Self-host.** The server ships in this same repository as a compose stack — see the Self-hosting
section of the README at <https://github.com/topos-sh/topos>. Your workspace then lives at your own
origin, and `topos follow https://topos.example.com` (your server's address) enrolls against it.
