/**
 * Capability codes ride URLs in this system: the setup claim link's `?code=` (and the verify
 * page's short device code, same query name), Better Auth's magic-link/verification `?token=`,
 * and — as a PATH segment, not a query param — the single-use invitation link and the reset
 * link. Any URL that can surface on an error report — request context, fetch breadcrumbs,
 * error messages — passes through this redaction before send. Pure string → string, so every
 * Sentry config can share it.
 *
 * The path forms matter as much as the query ones: an invitation token IS the authorization
 * (it seats its holder, and mints a born-verified account for the invited address), so a fault
 * on the invite page would otherwise ship a live, replayable capability to a third party.
 */
export function redactTokenPaths(value: string): string {
  return value
    .replace(/([?&]token=)[^&#\s"']+/g, "$1[token]")
    .replace(/([?&]code=)[^&#\s"']+/g, "$1[code]")
    .replace(/(\/invite\/)[^/?#\s"']+/g, "$1[token]")
    .replace(/(\/reset-password\/)[^/?#\s"']+/g, "$1[token]");
}
