/**
 * Capability codes ride URLs in this system: the setup claim link's `?code=` (and the verify
 * page's short device code, same query name) and Better Auth's magic-link/verification
 * `?token=`. Any URL that can surface on an error report — request context, fetch breadcrumbs,
 * error messages — passes through this redaction before send. Pure string → string, so every
 * Sentry config can share it.
 */
export function redactTokenPaths(value: string): string {
  return value
    .replace(/([?&]token=)[^&#\s"']+/g, "$1[token]")
    .replace(/([?&]code=)[^&#\s"']+/g, "$1[code]");
}
