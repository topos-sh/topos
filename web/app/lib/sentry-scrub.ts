/**
 * Capability tokens ride URLs in this system: the `/i/<claim>` admin-claim links and Better Auth's
 * magic-link `?token=` verification. Any URL that can surface on an error report — request
 * context, fetch breadcrumbs, error messages — passes through this redaction before send. Pure
 * string → string, so every Sentry config can share it.
 */
export function redactTokenPaths(value: string): string {
  return value
    .replace(/\/i\/[^/?#\s"']+/g, "/i/[token]")
    .replace(/([?&]token=)[^&#\s"']+/g, "$1[token]");
}
