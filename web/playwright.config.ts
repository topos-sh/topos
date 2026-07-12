import { defineConfig, devices } from "@playwright/test";
import {
  APP_PORT,
  appEnv,
  BASE_URL,
  PLANE_INTERNAL_TOKEN,
  PLANE_PORT,
  STORAGE_STATE,
} from "./tests/e2e/env";

/**
 * E2E: the app runs against the fixture vault (tests/fixtures/plane/server.mjs) with a synthetic
 * env. `globalSetup` (tests/e2e/db-setup.mjs) provisions the `topos_e2e` database + the plane
 * schema; the `setup` project then signs a member in through the REAL email+password flow and
 * seeds the directory rows (tests/e2e/auth.setup.ts).
 *
 * The app server is `react-router dev` (the reliable dev choice — a build+start would need the
 * client assets copied alongside). The DB must be reachable at DATABASE_URL (a local Postgres;
 * sessions must not share the unit lane's database — hence the separate `topos_e2e`).
 */
export default defineConfig({
  testDir: "tests/e2e",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? "line" : "list",
  globalSetup: "./tests/e2e/db-setup.mjs",
  use: {
    baseURL: BASE_URL,
    trace: "retain-on-failure",
  },
  projects: [
    { name: "setup", testMatch: /.*\.setup\.ts/ },
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"], storageState: STORAGE_STATE },
      dependencies: ["setup"],
      testMatch: /.*\.spec\.ts/,
    },
  ],
  webServer: [
    {
      command: "node tests/fixtures/plane/server.mjs",
      url: `http://127.0.0.1:${PLANE_PORT}/__test/calls`,
      reuseExistingServer: !process.env.CI,
      env: {
        PLANE_FIXTURE_PORT: String(PLANE_PORT),
        PLANE_INTERNAL_TOKEN,
      },
      timeout: 30_000,
    },
    {
      // The PRODUCTION build, served — deterministic startup (the dev server's on-demand compile
      // has no ready signal a headless runner can wait on), and the e2e exercises the same bundle
      // a deployment runs. Migrations apply lazily on the first request (the healthz probe).
      command: `bun run build && bun run start`,
      // Probe by explicit IPv4 loopback — a runner's `localhost` may resolve to ::1 while the
      // server binds only the IPv4 side; the browser's BASE_URL stays hostname-based.
      url: `http://127.0.0.1:${APP_PORT}/healthz`,
      reuseExistingServer: !process.env.CI,
      env: { ...appEnv(), PORT: String(APP_PORT), HOST: "0.0.0.0" },
      timeout: 180_000,
      stdout: "pipe",
      stderr: "pipe",
    },
  ],
});
