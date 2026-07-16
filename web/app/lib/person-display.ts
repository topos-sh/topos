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
