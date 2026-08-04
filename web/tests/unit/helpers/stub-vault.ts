import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";

/**
 * An in-process stand-in for the vault's internal custody lane — the workspace-export suite's
 * pattern, extracted so the MCP suites can share it.
 *
 * It is NOT a mock of the custody layer: the app's ONE transport is re-pointed at this server
 * through `PLANE_INTERNAL_URL`, so every call still goes through `vaultFetch`, the route
 * allowlist, and the typed wrappers. What it stands in for is the byte store: it keeps
 * published files in memory, serves the version listing and the objects back, and records the
 * write calls so a test can prove that a refusal reached NO custody route at all.
 *
 * It deliberately does not mirror into `plane.*` — the DB rows a page joins against are seeded
 * by the scratch-db helpers, exactly as the vault would have written them.
 */

export interface StubFile {
  path: string;
  content: string;
}

export interface StubCall {
  route: string;
  ws: string;
  bundle: string;
}

export interface StubVault {
  url: string;
  /** Register a version's files so a later read resolves them (the "already published" state). */
  seed(ws: string, bundleId: string, versionId: string, files: StubFile[]): void;
  /** The write calls this stub answered, oldest first. */
  calls: StubCall[];
  /** Files a `publish` call carried, oldest first — what actually crossed the boundary. */
  published: { ws: string; bundle: string; versionId: string; files: StubFile[] }[];
  close(): Promise<void>;
}

const key = (ws: string, bundle: string, version: string) => `${ws}\n${bundle}\n${version}`;

/** A deterministic 64-hex id per (key, index) — a seed, never a real content address. */
function idFor(prefix: string, n: number): string {
  return `${prefix}${String(n).padStart(4, "0")}`.padEnd(64, "0").slice(0, 64);
}

export async function startStubVault(): Promise<StubVault> {
  const versions = new Map<string, StubFile[]>();
  const objects = new Map<string, Buffer>();
  const calls: StubCall[] = [];
  const published: StubVault["published"] = [];
  let minted = 0;

  // Object ids must be stable per (version, path) so a listing and a read agree.
  const objectIdOf = (versionId: string, path: string): string => {
    let sum = 0;
    for (const ch of `${versionId}:${path}`) {
      sum = (sum * 31 + ch.charCodeAt(0)) % 0xffffffff;
    }
    return sum.toString(16).padStart(8, "0").repeat(8).slice(0, 64);
  };

  const seed: StubVault["seed"] = (ws, bundleId, versionId, files) => {
    versions.set(key(ws, bundleId, versionId), files);
    for (const file of files) {
      objects.set(objectIdOf(versionId, file.path), Buffer.from(file.content, "utf8"));
    }
  };

  const server = createServer((request, response) => {
    const url = request.url ?? "";
    const method = request.method ?? "GET";
    const json = (status: number, body: unknown): void => {
      const buf = Buffer.from(JSON.stringify(body));
      response.writeHead(status, {
        "content-type": "application/json",
        "content-length": String(buf.byteLength),
      });
      response.end(buf);
    };

    const write = url.match(
      /^\/internal\/v1\/workspaces\/([^/]+)\/bundles\/([^/]+)\/(publish|versions)$/,
    );
    if (method === "POST" && write?.[1] && write[2] && write[3]) {
      const [, ws, bundle, route] = write as unknown as [string, string, string, string];
      const chunks: Buffer[] = [];
      request.on("data", (chunk) => chunks.push(chunk as Buffer));
      request.on("end", () => {
        const body = JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}") as {
          files?: { path: string; content_base64: string }[];
        };
        calls.push({ route, ws, bundle });
        minted += 1;
        const versionId = idFor("ee", minted);
        const files = (body.files ?? []).map((f) => ({
          path: f.path,
          content: Buffer.from(f.content_base64, "base64").toString("utf8"),
        }));
        seed(ws, bundle, versionId, files);
        published.push({ ws, bundle, versionId, files });
        json(200, {
          version_id: versionId,
          commit_id: versionId,
          bundle_digest: "d".repeat(64),
          deduped: false,
          ...(route === "publish"
            ? {
                pointer: {
                  version_id: versionId,
                  generation: 1,
                  moved_at_ms: Date.now(),
                  moved_by_display: "stub",
                  replayed: false,
                },
              }
            : {}),
        });
      });
      return;
    }

    const meta = url.match(
      /^\/internal\/v1\/workspaces\/([^/]+)\/bundles\/([^/]+)\/versions\/([^/]+)$/,
    );
    if (method === "GET" && meta?.[1] && meta[2] && meta[3]) {
      const files = versions.get(key(meta[1], meta[2], meta[3]));
      if (files === undefined) {
        return json(404, { code: "NOT_FOUND" });
      }
      return json(200, {
        version_id: meta[3],
        parents: [],
        author: "stub",
        message: "stub",
        bundle_digest: "d".repeat(64),
        created_at_ms: 1_700_000_000_000,
        files: files.map((f) => ({
          path: f.path,
          mode: "100644",
          object_id: objectIdOf(meta[3] as string, f.path),
        })),
      });
    }

    const object = url.match(
      /^\/internal\/v1\/workspaces\/[^/]+\/bundles\/[^/]+\/objects\/([^/]+)$/,
    );
    if (method === "GET" && object?.[1]) {
      const bytes = objects.get(object[1]);
      if (bytes === undefined) {
        return json(404, { code: "NOT_FOUND" });
      }
      response.writeHead(200, {
        "content-type": "application/octet-stream",
        "content-length": String(bytes.byteLength),
      });
      return response.end(bytes);
    }

    return json(404, { code: "NOT_FOUND" });
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as AddressInfo).port;
  return {
    url: `http://127.0.0.1:${port}`,
    seed,
    calls,
    published,
    async close() {
      await new Promise<void>((resolve, reject) =>
        (server as Server).close((e) => (e ? reject(e) : resolve())),
      );
    },
  };
}
