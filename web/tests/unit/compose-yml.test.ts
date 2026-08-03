import { readFile } from "node:fs/promises";
import { beforeAll, describe, expect, it } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * GET /compose.yml — the self-host deployment file, served as bytes. The invariants under test are
 * the promises the quickstart makes: the route hands back THIS repo's compose file unchanged, and
 * that file PULLS both application images at ONE pinned release rather than building them. A
 * `build:` block sneaking back into the published file would turn the advertised
 * `curl … && docker compose up -d` into a source build on the reader's machine, silently.
 *
 * Vitest runs with cwd = web/, so the COMPOSE_YML_PATH default resolves to the repo's own
 * docker-compose.yml — the route reads the real committed file here.
 */

let composeLoader: () => Promise<Response>;
let initDbLoader: () => Promise<Response>;
let onDisk: string;
let initDbOnDisk: string;

beforeAll(async () => {
  installTestEnv();
  ({ loader: composeLoader } = await import("@/routes/compose-yml"));
  ({ loader: initDbLoader } = await import("@/routes/compose-init-db"));
  onDisk = await readFile("../docker-compose.yml", "utf8");
  initDbOnDisk = await readFile("../scripts/compose-init-db.sh", "utf8");
});

describe("the self-host compose file", () => {
  it("serves the repo's own docker-compose.yml as readable plain text", async () => {
    const res = await composeLoader();
    expect(res.status).toBe(200);
    // Plain text, not application/yaml: a deployment file is meant to be read in the browser
    // before it is run, not saved unseen.
    expect(res.headers.get("content-type")).toBe("text/plain; charset=utf-8");
    expect(await res.text()).toBe(onDisk);
  });

  it("pins both application images to one release and builds neither", async () => {
    const served = await (await composeLoader()).text();

    // Both images, both pinned through the SAME variable — one TOPOS_VERSION moves the pair, and
    // they are only ever tested together.
    const pins = [
      ...served.matchAll(/image:\s*ghcr\.io\/topos-sh\/(\S+):\$\{TOPOS_VERSION:-([^}]+)\}/g),
    ];
    expect(pins.map(([, image]) => image).sort()).toEqual(["topos-plane", "topos-web"]);

    // ONE version across both, and a release tag rather than a floating one: `latest` is a choice
    // a self-hoster makes, never the default a fresh install lands on.
    const versions = pins.map(([, , version]) => version);
    expect(new Set(versions).size).toBe(1);
    for (const version of versions) {
      expect(version).toMatch(/^v\d+\.\d+\.\d+$/);
    }

    // Nothing in the PUBLISHED file builds — that lives in docker-compose.build.yml.
    expect(served).not.toMatch(/^\s*build:/m);
  });

  it("serves every host path it mounts, so a fetched deployment is complete", async () => {
    const served = await (await composeLoader()).text();

    // Any `./…` bind mount is a file the operator must ALSO have. Each one must therefore be
    // fetchable from this server — otherwise the quickstart hands out a compose file that cannot
    // stand up, and Docker turns the missing path into a directory rather than an error.
    const hostMounts = [...served.matchAll(/^\s*-\s+\.\/(\S+?):/gm)].map(([, p]) => p);
    expect(hostMounts).toEqual(["scripts/compose-init-db.sh"]);

    const res = await initDbLoader();
    expect(res.status).toBe(200);
    expect(await res.text()).toBe(initDbOnDisk);
  });

  it("pins the project name, so the documented volume names are the real ones", async () => {
    // Without this, the volumes are prefixed with whatever directory the file was downloaded to,
    // and the backup commands in the docs would silently create-and-tar empty volumes.
    expect(await (await composeLoader()).text()).toMatch(/^name:\s*topos\s*$/m);
  });
});
