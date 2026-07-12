import { readFile } from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";

/**
 * GET /install — the advertised `curl -fsSL … | sh` target, served as the bytes themselves. The
 * file is this repo's own `scripts/install.sh` (INSTALL_SH_PATH, resolved relative to the
 * process working directory), the same checksummed installer the release ships. This route only
 * ferries those bytes, verifying nothing itself. Plain text, not a download: "read it first"
 * renders it in the browser.
 */

// The configured path is resolved against the working directory (an absolute path wins). In a
// checkout the web app runs from `web/`, so the default `../scripts/install.sh` reaches the
// repo's own installer.
async function installerBytes(): Promise<Buffer> {
  const configured = serverEnv().INSTALL_SH_PATH;
  return readFile(path.resolve(process.cwd(), configured));
}

// The installer bytes are immutable for the process lifetime — read once, not per curl. A failed
// read (missing file) is not memoized, so a misbuilt image fails loudly every time.
let installerPromise: Promise<Buffer> | undefined;

export async function loader(): Promise<Response> {
  installerPromise ??= installerBytes().catch((error: unknown) => {
    installerPromise = undefined;
    throw error;
  });
  const bytes = await installerPromise;
  return new Response(new Uint8Array(bytes), {
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
