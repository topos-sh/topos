import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * The server-tier env is parsed LAZILY and memoized on first `serverEnv()` — a CI build runs
 * without production secrets, so a top-level parse would fail the build. These cases stub the
 * WHOLE schema per run (absent ⇒ unset, so ambient/CI env can't leak in) and re-import a fresh
 * module registry each time.
 */

const REQUIRED: Record<string, string> = {
  DATABASE_URL: "postgres://user:pass@localhost:5439/db",
  BETTER_AUTH_SECRET: "0123456789abcdef0123456789abcdef",
  BETTER_AUTH_URL: "http://localhost:3000",
  PLANE_INTERNAL_URL: "http://vault.internal:8080",
  PLANE_INTERNAL_TOKEN: "internal-token-value",
};

/** Every schema key — each case stubs ALL of them so ambient env never leaks into a parse. */
const SCHEMA_KEYS = [...Object.keys(REQUIRED), "INSTALL_SH_PATH", "APP_ENV"];

// serverEnv() memoizes per module instance, so each case gets a fresh module registry.
async function parseWith(env: Record<string, string | undefined>) {
  vi.resetModules();
  for (const key of SCHEMA_KEYS) {
    vi.stubEnv(key, env[key]); // undefined ⇒ the variable is removed for this case
  }
  const { serverEnv } = await import("@/env.server");
  return serverEnv();
}

afterEach(() => {
  vi.unstubAllEnvs();
});

describe("serverEnv", () => {
  it("parses with the required vars; APP_ENV + INSTALL_SH_PATH take their defaults", async () => {
    const env = await parseWith(REQUIRED);
    expect(env.APP_ENV).toBe("development");
    expect(env.INSTALL_SH_PATH).toBe("../scripts/install.sh");
    expect(env.PLANE_INTERNAL_TOKEN).toBe("internal-token-value");
  });

  it("accepts an explicit APP_ENV", async () => {
    const env = await parseWith({ ...REQUIRED, APP_ENV: "production" });
    expect(env.APP_ENV).toBe("production");
  });

  it("rejects a missing PLANE_INTERNAL_TOKEN, naming it", async () => {
    await expect(parseWith({ ...REQUIRED, PLANE_INTERNAL_TOKEN: undefined })).rejects.toSatisfy(
      (error: unknown) => {
        const message = error instanceof Error ? error.message : String(error);
        expect(message).toContain("PLANE_INTERNAL_TOKEN");
        return true;
      },
    );
  });

  it("rejects a missing DATABASE_URL", async () => {
    await expect(parseWith({ ...REQUIRED, DATABASE_URL: undefined })).rejects.toSatisfy(
      (error: unknown) => {
        const message = error instanceof Error ? error.message : String(error);
        expect(message).toContain("DATABASE_URL");
        return true;
      },
    );
  });

  it("rejects a too-short BETTER_AUTH_SECRET", async () => {
    await expect(parseWith({ ...REQUIRED, BETTER_AUTH_SECRET: "tooshort" })).rejects.toThrow();
  });

  it("rejects a non-URL BETTER_AUTH_URL and PLANE_INTERNAL_URL", async () => {
    await expect(parseWith({ ...REQUIRED, BETTER_AUTH_URL: "not-a-url" })).rejects.toThrow();
    await expect(parseWith({ ...REQUIRED, PLANE_INTERNAL_URL: "not-a-url" })).rejects.toThrow();
  });
});
