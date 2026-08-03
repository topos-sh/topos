import { readFile } from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";

/**
 * GET /compose.yml — the self-host deployment file, served as the bytes themselves, so standing up
 * a server is `curl … -o docker-compose.yml && docker compose up -d`: no clone, no toolchain, no
 * build. The file is this repo's own `docker-compose.yml` (COMPOSE_YML_PATH, resolved relative to
 * the process working directory), the same one the compose smoke test boots — the `/install`
 * posture applied to the other install path.
 *
 * A useful consequence of serving it from the deployment rather than a static host: the bytes come
 * out of THIS running server's image, so the image tags they pin are the release this server is
 * itself built from. What you copy from a server is what that server runs.
 *
 * Plain text, not a download: a deployment file is something to read before running, and
 * `application/yaml` makes a browser save it unseen.
 */

// The configured path is resolved against the working directory (an absolute path wins). In a
// checkout the web app runs from `web/`, so the default `../docker-compose.yml` reaches the repo's
// own compose file.
async function composeBytes(): Promise<Buffer> {
  const configured = serverEnv().COMPOSE_YML_PATH;
  return readFile(path.resolve(process.cwd(), configured));
}

// Immutable for the process lifetime — read once, not per curl. A failed read (missing file) is
// not memoized, so a misbuilt image fails loudly every time.
let composePromise: Promise<Buffer> | undefined;

export async function loader(): Promise<Response> {
  composePromise ??= composeBytes().catch((error: unknown) => {
    composePromise = undefined;
    throw error;
  });
  const bytes = await composePromise;
  return new Response(new Uint8Array(bytes), {
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
