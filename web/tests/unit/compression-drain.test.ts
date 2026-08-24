import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { createRequire } from "node:module";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

/**
 * THE GZIP DRAIN-LISTENER LEAK — `MaxListenersExceededWarning: 11 drain listeners added to
 * [Gzip]`, four times in one production run.
 *
 * `@react-router/serve` puts `compression` in front of this app, and `compression` moves a
 * response's `drain` listeners onto the compression stream (`res.on('drain', …)` → `stream.on`)
 * so a writer sees the COMPRESSED stream's backpressure. Removal was never proxied: a
 * `res.removeListener('drain', …)` went to the response, which never held the listener, so the
 * stream kept it forever. `res.once('drain', …)` is caught by the same asymmetry — node's
 * once-wrapper registers through the patched `on` and unregisters through `removeListener`.
 *
 * That is exactly the write loop underneath this app: `@remix-run/node-fetch-server` awaits a
 * drain once per chunk a streamed response could not take, so a large document accumulated one
 * permanent listener per chunk. The fix is `patches/compression@1.8.1.patch` — a pinned patch of
 * the dependency, because the asymmetry is entirely inside it.
 *
 * The invariant, stated once: a listener added through the response and removed through the
 * response leaves nothing behind, wherever compression chose to keep it.
 */

// The middleware the production server mounts, resolved the way it resolves it — this test asserts
// against the very module `react-router-serve` loads at runtime, patch and all.
const require = createRequire(import.meta.url);
const compression = require("compression") as () => (
  req: IncomingMessage,
  res: ServerResponse,
  next: () => void,
) => void;

/** The gzip stream and the response in front of it, for one live request. */
interface Live {
  res: ServerResponse;
  /** What compression parks `drain` listeners on — `res.on('drain', …)` hands it back. */
  gzip: NodeJS.EventEmitter;
}

let server: Server;
let port = 0;
let live: ((value: Live) => void) | null = null;

beforeAll(async () => {
  const middleware = compression();
  server = createServer((req, res) => {
    middleware(req, res, () => {
      // No Content-Length and a compressible type: compression takes the response, so a `drain`
      // listener lands on its stream rather than on the socket.
      res.writeHead(200, { "content-type": "text/html" });
      const probe = () => undefined;
      const gzip = res.on("drain", probe) as unknown as NodeJS.EventEmitter;
      res.removeListener("drain", probe);
      live?.({ res, gzip });
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  port = (server.address() as AddressInfo).port;
});

afterAll(async () => {
  await new Promise<void>((resolve) => server.close(() => resolve()));
});

/** One live request, handed the response and the stream compression hid behind it. */
async function withLiveResponse(body: (l: Live) => void): Promise<void> {
  const seen = new Promise<Live>((resolve) => {
    live = resolve;
  });
  const request = fetch(`http://127.0.0.1:${port}/`, {
    headers: { "accept-encoding": "gzip" },
  });
  const l = await seen;
  try {
    body(l);
  } finally {
    l.res.end("x".repeat(2048));
    await request.then((r) => r.arrayBuffer());
  }
}

describe("a response compression is holding", () => {
  it("parks drain listeners on the compression stream, not on the response", async () => {
    await withLiveResponse(({ res, gzip }) => {
      // The response already carries compression's own drain handler (it resumes the stream);
      // what a CALLER adds must not join it there.
      const onResponse = res.listenerCount("drain");
      const listener = () => undefined;
      res.on("drain", listener);
      expect(gzip.listenerCount("drain")).toBe(1);
      expect(res.listenerCount("drain")).toBe(onResponse);
      res.removeListener("drain", listener);
    });
  });

  it("takes a listener off the stream when the response is asked to remove it", async () => {
    await withLiveResponse(({ res, gzip }) => {
      const listener = () => undefined;
      res.on("drain", listener);
      res.removeListener("drain", listener);
      expect(gzip.listenerCount("drain")).toBe(0);
    });
  });

  it("leaves nothing behind across a write loop's worth of once/remove pairs", async () => {
    // The shape `@remix-run/node-fetch-server` writes a streamed response in: await one drain per
    // chunk that did not fit, then clean the wait up. Twenty chunks is twice what it takes to
    // trip node's ten-listener warning.
    await withLiveResponse(({ res, gzip }) => {
      for (let i = 0; i < 20; i++) {
        const onDrain = () => undefined;
        res.once("drain", onDrain);
        res.removeListener("drain", onDrain);
      }
      expect(gzip.listenerCount("drain")).toBe(0);
    });
  });

  it("still routes non-drain listeners to the response itself", async () => {
    await withLiveResponse(({ res, gzip }) => {
      const listener = () => undefined;
      res.on("close", listener);
      expect(gzip.listenerCount("close")).toBe(0);
      res.removeListener("close", listener);
      expect(res.listenerCount("close")).toBe(0);
    });
  });
});
