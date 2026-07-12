/**
 * Complete, valid server env for unit tests. serverEnv() parses the WHOLE schema on first call
 * and memoizes, so tests must install this BEFORE the first module call that touches env
 * (vitest isolates modules per test file, so a per-file beforeAll is enough).
 */
export function installTestEnv(overrides: Record<string, string> = {}): void {
  Object.assign(process.env, {
    DATABASE_URL: "postgres://unit:unit@localhost:5439/unit",
    BETTER_AUTH_SECRET: "0123456789abcdef0123456789abcdef",
    BETTER_AUTH_URL: "http://localhost:3000",
    PLANE_INTERNAL_URL: "http://vault.internal:8080",
    PLANE_INTERNAL_TOKEN: "internal-token-unit",
    APP_ENV: "test",
    ...overrides,
  });
}
