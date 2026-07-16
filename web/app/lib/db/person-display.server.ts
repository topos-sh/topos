import { type AnyColumn, type SQL, sql } from "drizzle-orm";

/**
 * The SQL twin of `app/lib/person-display.ts`: a person's human-facing display is the profile
 * name, else the email. Magic-link sign-ups are born with `name = ''`, so every SQL-side display
 * composition (member lists, attribution snapshots, the device-lane actor) coalesces through this
 * ONE fragment rather than re-spelling the rule. Display only — email never authorizes.
 */

interface PersonColumns {
  name: AnyColumn;
  email: AnyColumn;
}

/** The display fragment over an INNER-joined user (the row exists, so this is never NULL). */
export function personDisplaySql(u: PersonColumns): SQL<string> {
  return sql`COALESCE(NULLIF(btrim(${u.name}), ''), ${u.email})`;
}

/**
 * The LEFT-join face of [`personDisplaySql`]: NULL when the user row is gone, so a caller's own
 * "former member" fallback keeps its meaning.
 */
export function personDisplayLeftSql(u: PersonColumns): SQL<string | null> {
  return personDisplaySql(u);
}
