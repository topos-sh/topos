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

**Join an existing team.** Ask a teammate for the workspace address — each publish prints it, and
it looks like `https://topos.sh/<workspace>`. Then:

```sh
topos login <workspace-address>
```

Open the printed approval URL in a browser and approve — that logs this machine in. Login is the
acceptance: what your profile and this folder's manifest demand lands immediately and stays
current from then on (`topos update` sweeps on demand; `topos add <name>` records more).

**Start fresh.** Sign up at <https://topos.sh> and create a workspace in the browser, then log
this machine in against your own address the same way: `topos login https://topos.sh/<your-workspace>`
— approve in the browser, and the workspace's deliveries follow.

**Self-host.** The server ships in this same repository as a compose stack — see the Self-hosting
section of the README at <https://github.com/topos-sh/topos>. Your workspace then lives at your own
origin, and `topos login https://topos.example.com` (your server's address) logs in against it.
