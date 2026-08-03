import { readFile } from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";

/**
 * GET /compose-init-db.sh — the database's first-boot provisioning script, the second half of a
 * self-host install. `docker-compose.yml` mounts it into the postgres image's initdb hook, and it
 * is also the exact recipe to run once by hand against a managed Postgres, so it is a file rather
 * than something inlined into the compose YAML: one copy of the role/schema/grant logic, served
 * to both audiences.
 *
 * Same posture as `/install` and `/compose.yml`: the bytes come out of the running server's own
 * image, so what you copy from a server provisions exactly what that server expects.
 */

// Resolved against the working directory (an absolute path wins). In a checkout the web app runs
// from `web/`, so the default reaches the repo's own script.
async function initDbBytes(): Promise<Buffer> {
  const configured = serverEnv().COMPOSE_INIT_DB_PATH;
  return readFile(path.resolve(process.cwd(), configured));
}

// Immutable for the process lifetime — read once, not per curl. A failed read (missing file) is
// not memoized, so a misbuilt image fails loudly every time.
let initDbPromise: Promise<Buffer> | undefined;

export async function loader(): Promise<Response> {
  initDbPromise ??= initDbBytes().catch((error: unknown) => {
    initDbPromise = undefined;
    throw error;
  });
  const bytes = await initDbPromise;
  return new Response(new Uint8Array(bytes), {
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
