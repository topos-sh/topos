// The composed product stack, out of process, for the meta-skill eval.
//
// Boots the REAL product topology the workspace e2e suite proves (tests/tests/common/mod.rs),
// but as shell processes so a headless agent can drive the real `topos` binary against it:
//   - a fresh database per run on the eval's own Postgres container (never a shared server),
//     provisioned with the two application roles + schemas exactly like scripts/compose-init-db.sh;
//   - the `topos-plane` vault binary (self-migrates its schema at boot; internal token armed;
//     loopback bind — no public face);
//   - the web app's production build served by node (the one public surface), first-boot setup
//     triggered by a document request, the workspace claimed through the real /claim ceremony.
//
// Everything here speaks the product's own wire: no test-fixtures feature, no in-process shortcut.

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, existsSync, openSync } from "node:fs";
import { createServer } from "node:net";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

export const PG_CONTAINER = "topos-bundle-d-pg";
export const PG_PORT = 5454;
export const WS_NAME = "acme";
export const SETUP_CODE = "eval-setup-code-0123456789abcdef";
export const OWNER_EMAIL = "owner@acme.test";
export const PASSWORD = "eval-password-1234";
export const INTERNAL_TOKEN = "eval-internal-token";

export function repoRoot() {
  // evals/meta-skill/stack.mjs → the repo root is two levels up.
  const here = path.dirname(new URL(import.meta.url).pathname);
  return path.dirname(path.dirname(here));
}

let dbCounter = 0;

/** Run SQL as the superuser inside the eval's Postgres container. */
export function psql(sql, db = "postgres") {
  const r = spawnSync(
    "docker",
    ["exec", "-i", PG_CONTAINER, "psql", "-U", "postgres", "-d", db, "-v", "ON_ERROR_STOP=1", "-tA"],
    { input: sql, encoding: "utf8" },
  );
  if (r.status !== 0) throw new Error(`psql failed: ${r.stderr}\nsql: ${sql}`);
  return r.stdout.trim();
}

/** Provision a fresh database with the production role/schema recipe (mirrors compose-init-db.sh). */
export function provisionDb() {
  const db = `topos_eval_${process.pid}_${++dbCounter}`;
  // Roles are cluster-wide; guard creation so repeated runs are idempotent — and swallow the
  // duplicate_object race two CONCURRENT lanes can hit between the existence check and the
  // CREATE (each cell is its own process under --jobs, so the check-then-create is not atomic).
  psql(`
    DO $$ BEGIN
      BEGIN
        IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_plane') THEN
          CREATE ROLE topos_plane LOGIN PASSWORD 'plane';
        END IF;
      EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
      END;
      BEGIN
        IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_web') THEN
          CREATE ROLE topos_web LOGIN PASSWORD 'web';
        END IF;
      EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
      END;
    END $$;`);
  psql(`CREATE DATABASE "${db}"`);
  psql(
    `
    REVOKE ALL ON DATABASE "${db}" FROM PUBLIC;
    GRANT CONNECT ON DATABASE "${db}" TO topos_plane;
    GRANT CONNECT ON DATABASE "${db}" TO topos_web;
    GRANT CREATE ON DATABASE "${db}" TO topos_web;
    ALTER ROLE topos_web IN DATABASE "${db}" SET search_path = web, plane;
    ALTER ROLE topos_plane IN DATABASE "${db}" SET search_path = plane;
  `,
    db,
  );
  psql(
    `
    CREATE SCHEMA IF NOT EXISTS web AUTHORIZATION topos_web;
    CREATE SCHEMA IF NOT EXISTS plane AUTHORIZATION topos_plane;
    GRANT USAGE ON SCHEMA plane TO topos_web;
    ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane GRANT SELECT ON TABLES TO topos_web;
  `,
    db,
  );
  return db;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
    srv.on("error", reject);
  });
}

async function waitHealthy(url, tries = 240) {
  for (let i = 0; i < tries; i++) {
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch {}
    await sleep(500);
  }
  throw new Error(`never healthy: ${url}`);
}

/** A minimal cookie-jar browser stand-in for the claim/verify ceremonies. */
export class Session {
  constructor(origin) {
    this.origin = origin;
    this.jar = new Map();
  }
  cookieHeader() {
    return [...this.jar.entries()].map(([k, v]) => `${k}=${v}`).join("; ");
  }
  absorb(res) {
    for (const c of res.headers.getSetCookie?.() ?? []) {
      const [pair] = c.split(";");
      const eq = pair.indexOf("=");
      this.jar.set(pair.slice(0, eq).trim(), pair.slice(eq + 1).trim());
    }
  }
  async get(pathname) {
    const res = await fetch(this.origin + pathname, {
      redirect: "manual",
      headers: { Cookie: this.cookieHeader(), Accept: "text/html,application/xhtml+xml" },
    });
    this.absorb(res);
    return res;
  }
  async postForm(pathname, fields) {
    const body = new URLSearchParams(fields).toString();
    const res = await fetch(this.origin + pathname, {
      method: "POST",
      redirect: "manual",
      headers: {
        Cookie: this.cookieHeader(),
        Origin: this.origin,
        Accept: "text/html,application/xhtml+xml",
        "Content-Type": "application/x-www-form-urlencoded",
      },
      body,
    });
    this.absorb(res);
    return res;
  }
  signedIn() {
    return [...this.jar.keys()].some((k) => k.includes("session_token"));
  }
  async postJson(pathname, body) {
    const res = await fetch(this.origin + pathname, {
      method: "POST",
      redirect: "manual",
      headers: {
        Cookie: this.cookieHeader(),
        Origin: this.origin,
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    });
    this.absorb(res);
    return res;
  }
}

export class Stack {
  constructor({ db, origin, planeBase, procs, scratch }) {
    this.db = db;
    this.origin = origin;
    this.planeBase = planeBase;
    this.procs = procs;
    this.scratch = scratch;
  }
  address() {
    return `${this.origin}/${WS_NAME}`;
  }
  /** Claim the boot-minted workspace: first account, first owner seat, signed in. */
  async claimOwner() {
    const s = new Session(this.origin);
    const g = await s.get(`/claim?code=${SETUP_CODE}`);
    if (g.status !== 200) throw new Error(`claim GET ${g.status}`);
    const p = await s.postForm(`/claim?code=${SETUP_CODE}`, {
      code: SETUP_CODE,
      name: OWNER_EMAIL.split("@")[0],
      email: OWNER_EMAIL,
      password: PASSWORD,
    });
    if (p.status !== 302) throw new Error(`claim POST ${p.status}: ${await p.text()}`);
    if (!s.signedIn()) throw new Error("claim did not sign in");
    this.owner = s;
    return s;
  }
  /** Approve a pending device flow at the real /verify ceremony (signed-in accept). */
  async approveDevice(userCode, session = this.owner) {
    const res = await session.postForm("/verify", { intent: "approve", code: userCode });
    const body = await res.text();
    if (res.status !== 200 || !body.includes("connected")) {
      throw new Error(`approve failed: ${res.status}`);
    }
  }
  /**
   * Mint a MEMBER account + seat, mail-lessly — the same arrangement the workspace e2e
   * harness uses (the OSS surface for this is the invitation rung, which needs SMTP; the
   * eval stack runs none): flip the registration knob open, sign the account up through
   * the real better-auth endpoint, then seat it directly and mark the email verified.
   * The knob goes back to gated afterwards. Returns a signed-in Session for the member.
   */
  async addMember(email, password) {
    psql(`UPDATE web.workspace SET registration = 'open'`, this.db);
    const s = new Session(this.origin);
    const r = await s.postJson("/api/auth/sign-up/email", {
      email,
      password,
      name: email.split("@")[0],
    });
    if (r.status !== 200) throw new Error(`member sign-up ${r.status}: ${await r.text()}`);
    psql(`UPDATE web."user" SET email_verified = true WHERE email = '${email}'`, this.db);
    psql(
      `INSERT INTO web.seat (workspace_id, user_id, role)
       SELECT w.id, u.id, 'member' FROM web.workspace w, web."user" u WHERE u.email = '${email}'`,
      this.db,
    );
    psql(`UPDATE web.workspace SET registration = 'invite_only'`, this.db);
    if (!s.signedIn()) throw new Error("member sign-up did not sign in");
    return s;
  }
  async teardown() {
    for (const p of this.procs) {
      try {
        p.kill("SIGTERM");
      } catch {}
    }
    await sleep(300);
    for (const p of this.procs) {
      try {
        p.kill("SIGKILL");
      } catch {}
    }
  }
}

/** Boot vault + web against a fresh database. `scratch` holds vault object roots + logs. */
export async function startStack(scratch) {
  const root = repoRoot();
  if (!existsSync(path.join(root, "web/build/server/index.js"))) {
    throw new Error("web production build missing — run: cd web && bun install && bun run build");
  }
  if (!existsSync(path.join(root, "target/debug/topos-plane"))) {
    throw new Error("plane binary missing — run: cargo build -p topos -p topos-plane");
  }
  mkdirSync(scratch, { recursive: true });
  const db = provisionDb();
  const planePort = await freePort();
  const appPort = await freePort();
  const planeBase = `http://127.0.0.1:${planePort}`;
  const origin = `http://127.0.0.1:${appPort}`;
  const procs = [];

  // The vault: self-migrates the plane schema at boot; internal lane armed; loopback only.
  const vault = spawn(path.join(root, "target/debug/topos-plane"), [], {
    env: {
      ...process.env,
      TOPOS_PLANE_BIND: `127.0.0.1:${planePort}`,
      DATABASE_URL: `postgres://topos_plane:plane@127.0.0.1:${PG_PORT}/${db}`,
      TOPOS_PLANE_GIT_ROOT: path.join(scratch, "vault-git"),
      TOPOS_PLANE_LARGE_ROOT: path.join(scratch, "vault-large"),
      TOPOS_PLANE_INTERNAL_TOKEN: INTERNAL_TOKEN,
    },
    stdio: ["ignore", "ignore", openSync(path.join(scratch, "vault.log"), "a")],
  });
  procs.push(vault);
  await waitHealthy(`${planeBase}/healthz`);

  // The web schema migrates via the app's own migrator (as topos_web), like the e2e harness.
  const webUrl = `postgres://topos_web:web@127.0.0.1:${PG_PORT}/${db}`;
  const mig = spawnSync("node", ["scripts/migrate.mjs"], {
    cwd: path.join(root, "web"),
    env: { ...process.env, DATABASE_URL: webUrl },
    encoding: "utf8",
  });
  if (mig.status !== 0) throw new Error(`web migrate failed: ${mig.stderr}`);

  // The web app production build — node-native spawn so kill() actually kills it.
  const web = spawn(
    "node",
    [path.join(root, "web/node_modules/@react-router/serve/bin.cjs"), "./build/server/index.js"],
    {
      cwd: path.join(root, "web"),
      env: {
        ...process.env,
        PORT: String(appPort),
        HOST: "127.0.0.1",
        DATABASE_URL: webUrl,
        PLANE_INTERNAL_URL: planeBase,
        PLANE_INTERNAL_TOKEN: INTERNAL_TOKEN,
        BETTER_AUTH_SECRET: "eval-secret-0123456789abcdef0123456789abcdef",
        BETTER_AUTH_URL: origin,
        APP_ENV: "test",
        TOPOS_WEB_RATELIMIT: "off",
        TOPOS_WORKSPACE_NAME: WS_NAME,
        TOPOS_SETUP_CODE: SETUP_CODE,
        TOPOS_SETUP_LINK_FILE: path.join(scratch, "setup-link.txt"),
      },
      stdio: ["ignore", "ignore", openSync(path.join(scratch, "web.log"), "a")],
    },
  );
  procs.push(web);
  await waitHealthy(`${origin}/healthz`);
  // First document request runs first-boot setup (workspace mint + claim code).
  await fetch(`${origin}/login`, { headers: { Accept: "text/html" } });

  return new Stack({ db, origin, planeBase, procs, scratch });
}

/** Read a file if it exists, else null. */
export function maybeRead(p) {
  try {
    return readFileSync(p, "utf8");
  } catch {
    return null;
  }
}
