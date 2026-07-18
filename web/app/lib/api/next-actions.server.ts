/**
 * The ONE web-side owner of next-action construction + safety metadata — the mirror of the CLI's
 * rules module (`bins/topos/src/actions.rs`), which owns the vocabulary. Every next-action object
 * this tier serializes is built here, so the optional safety facts (`mutates` / `needs_network` /
 * `risk_note` — absent = unknown) are stated ONCE per action code and cannot drift between call
 * sites, or between this tier and the CLI: the shared golden
 * (`contracts/fixtures/json/publish.conflict.json`) pins the `REBASE_AND_RETRY` row cross-tier,
 * and the unit suite reconstructs it through this module.
 *
 * The web emits a small subset of the vocabulary; each row below restates the Rust table's facts
 * verbatim. A code outside the table serializes with NO safety fields — unknown is the honest
 * default, exactly as the CLI's rules module answers.
 */

export interface NextAction {
  code: string;
  argv: string[];
  mutates?: boolean;
  needs_network?: boolean;
  risk_note?: string;
}

type Safety = Pick<NextAction, "mutates" | "needs_network" | "risk_note">;

/** The safety facts per web-emitted action code — one row each, mirroring the CLI's table. */
const SAFETY: Record<string, Safety> = {
  // Rebase-then-retry lands bytes on the caller's machine from the plane.
  REBASE_AND_RETRY: { mutates: true, needs_network: true },
  // Not self-service: no argv executes — asking a human changes nothing here.
  REQUEST_ACCESS: { mutates: false, needs_network: false },
  CONTACT_ADMIN: { mutates: false, needs_network: false },
  // RETRY re-runs the CALLER's own previous command (the argv is empty by design), so whether it
  // mutates is that command's story — unknown. It only ever rides a retryable transport outcome,
  // so the retry certainly dials.
  RETRY: { needs_network: true },
};

/** Build a next-action object, filling the safety metadata from the one table above. */
export function nextAction(code: string, argv: string[]): NextAction {
  return { code, argv, ...(SAFETY[code] ?? {}) };
}
