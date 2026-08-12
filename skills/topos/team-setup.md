# Setting up topos for a team (no workspace yet)

When your human says "set up topos for our team", run the whole path — it is three steps, and
the only browser moments are theirs:

1. Log THIS machine in: `topos login` (self-hosting: `topos login <server>` — see
   `INSTALL.md`). Show your human the printed approval URL — they sign in there, name the new
   workspace (or pick one), and approve; never approve in their place. Piped runs print the
   approval URL and return; re-invoke `topos login` to poll, `--wait <seconds>` to block with
   a cap. The workspace's address — `topos.sh/<name>` — is its one handle: login, invites,
   and every publish receipt all speak it.
2. Seat teammates: `topos invite <email>` per person (bare describes, `--yes` sends). Add
   `--skill <name>` or `--channel <name>` to set someone up from their first day.
3. Hand each teammate the join line for their own agent — an invite seats them, but only this
   line brings their machine in:

   Ask your agent: "Set up Topos for us: fetch <server-origin>/agent and follow it. Our workspace: <address>"

   Fill in real values (`https://topos.sh/agent` and `topos.sh/<name>` on the hosted server) —
   every publish receipt prints the line ready-made. Do not hand out a skill-page URL instead:
   it answers only for members.
