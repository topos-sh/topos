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
const SCHEMA_KEYS = [
  ...Object.keys(REQUIRED),
  "INSTALL_SH_PATH",
  "APP_ENV",
  "TOPOS_WEB_RATELIMIT",
  "TOPOS_PUBLIC_URL",
  "TOPOS_WORKSPACE_NAME",
  "TOPOS_SETUP_CODE",
  "TOPOS_SETUP_LINK_FILE",
  "TOPOS_GTM_CONTAINER_ID",
];

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

  it("TOPOS_WORKSPACE_NAME defaults to 'team' and refuses a value outside the slug charset", async () => {
    const env = await parseWith(REQUIRED);
    expect(env.TOPOS_WORKSPACE_NAME).toBe("team");
    // The address-slug regex: lowercase alphanumerics + interior hyphens, no leading hyphen.
    await expect(parseWith({ ...REQUIRED, TOPOS_WORKSPACE_NAME: "Acme Corp" })).rejects.toThrow();
    const named = await parseWith({ ...REQUIRED, TOPOS_WORKSPACE_NAME: "acme-corp" });
    expect(named.TOPOS_WORKSPACE_NAME).toBe("acme-corp");
  });

  it("the /api/v1 rate belt defaults ON (the unit suites must turn it off explicitly)", async () => {
    const env = await parseWith(REQUIRED);
    expect(env.TOPOS_WEB_RATELIMIT).toBe("on");
  });

  it("TOPOS_GTM_CONTAINER_ID: unset and empty both spell unset; the container-id shape is enforced", async () => {
    const unset = await parseWith(REQUIRED);
    expect(unset.TOPOS_GTM_CONTAINER_ID).toBeUndefined();
    const empty = await parseWith({ ...REQUIRED, TOPOS_GTM_CONTAINER_ID: "  " });
    expect(empty.TOPOS_GTM_CONTAINER_ID).toBeUndefined();
    const set = await parseWith({ ...REQUIRED, TOPOS_GTM_CONTAINER_ID: "GTM-NMXMFBSF" });
    expect(set.TOPOS_GTM_CONTAINER_ID).toBe("GTM-NMXMFBSF");
    // A value that could break out of the inline snippet never parses.
    await expect(
      parseWith({ ...REQUIRED, TOPOS_GTM_CONTAINER_ID: "GTM-X');alert(1);//" }),
    ).rejects.toThrow();
  });
});
