# Installing topos

`topos` is the CLI that delivers a team's shared skills to this machine and keeps them current.
One consent rule governs this whole page: propose the command and say what it does — the human
runs it, or gives an explicit yes before you do. Never install anything unasked.

The freshest version of this walkthrough is served live at <https://topos.sh/agent> (every
topos server serves its own at `/agent`); this page stands alone when you cannot fetch it.

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
`topos add topos` lets this machine's topos manage that downloaded copy and keep it current.

## Connect it to a workspace

```sh
topos login
```

Open the printed approval URL in a browser: sign in, pick the workspace — or create it right
there — and one click logs this machine in. Login is the acceptance: everything the workspace
gives you — its shared baseline, any channel you carry, anything assigned to you — lands
immediately and stays current from then on, along with whatever this folder's `topos.toml` asks
for (`topos update` sweeps on demand; `topos add <name>` records more). Nothing else needs
accepting per skill.

**Know where you're headed?** `topos login <workspace>` preselects it — and once this machine
is logged in to the server, that command connects a further workspace you belong to with no
browser at all. An invitation mail's terminal line works verbatim: `topos login <invite-url>`.

**Self-host.** The server ships in this same repository as a compose stack — see the Self-hosting
section of the README at <https://github.com/topos-sh/topos>. Your workspace then lives at your own
origin, and `topos login topos.example.com` (your server's address) logs in against it.
