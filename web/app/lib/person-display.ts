/**
 * A person's human-facing display label: the profile name, else the email address. Magic-link
 * sign-ups are born with an EMPTY name, so every place a person is shown to a human must trim +
 * fall back — one rule, written once, shared by the TS compositions (the session actor mint, the
 * invited-seat binding) and mirrored by the SQL twin (`app/lib/db/person-display.server.ts`).
 * Display only — email NEVER authorizes (check:email).
 */
export function personDisplay(name: string | null | undefined, email: string): string {
  return name !== null && name !== undefined && name.trim().length > 0 ? name : email;
}

/**
 * A person as a COMMIT AUTHOR reads — `Robert <robert@topos.sh>`, the shape a version's author
 * line has carried since git. Falls back to the address alone when there is no name, so a
 * magic-link account never renders `robert@topos.sh <robert@topos.sh>`.
 *
 * Display only, like [`personDisplay`] it builds on — email NEVER authorizes (check:email).
 */
export function personAttribution(name: string | null | undefined, email: string): string {
  const display = personDisplay(name, email);
  return display === email ? email : `${display} <${email}>`;
}
