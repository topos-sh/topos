import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { createRequire } from "node:module";
import { type AddressInfo, connect } from "node:net";
import { writeReadableStreamToWritable } from "@react-router/node";
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

/**
 * THE SAME INVARIANT, END TO END — the leak as production meets it, not as the middleware
 * describes itself.
 *
 * The cases above pin compression's own semantics; this one runs the whole serving path: the
 * middleware `react-router-serve` mounts, the write loop `@react-router/express` uses
 * (`writeReadableStreamToWritable`, which awaits one drain per chunk the response could not
 * take), and a client that reads slowly enough to make the gzip stream fill and drain over and
 * over. That combination is the only thing that reproduces the production line
 * (`MaxListenersExceededWarning: 11 drain listeners added to [Gzip]`), and it is what tells an
 * UNPATCHED install apart from a patched one — module semantics look fine either way until the
 * listeners actually pile up.
 */
describe("a streamed response written back under real backpressure", () => {
  /** Chunky, and compressible enough to be worth gzipping — a real page, scaled up. */
  const CHUNK = Buffer.from("<p>the quick brown fox jumps over the lazy dog</p>\n".repeat(1400));
  const CHUNKS = 400;

  it("leaves the Gzip stream with no accumulated drain listeners, and warns about none", async () => {
    const warnings: string[] = [];
    const onWarning = (w: Error) => {
      if (w.name === "MaxListenersExceededWarning") {
        warnings.push(w.message);
      }
    };
    process.on("warning", onWarning);

    let peakOnGzip = 0;
    const middleware = compression();
    const streaming = createServer((req, res) => {
      middleware(req, res, () => {
        res.writeHead(200, { "content-type": "text/html" });
        // `res.on('drain', …)` hands back whatever compression parked the listener on — the gzip
        // stream itself, which is the emitter the production warning names.
        const probe = () => undefined;
        const gzip = res.on("drain", probe) as unknown as NodeJS.EventEmitter;
        res.removeListener("drain", probe);
        const sample = setInterval(() => {
          peakOnGzip = Math.max(peakOnGzip, gzip.listenerCount("drain"));
        }, 2);

        let sent = 0;
        const body = new ReadableStream<Uint8Array>({
          pull(controller) {
            if (sent++ >= CHUNKS) {
              controller.close();
              return;
            }
            controller.enqueue(CHUNK);
          },
        });
        void writeReadableStreamToWritable(body, res).finally(() => clearInterval(sample));
      });
    });
    await new Promise<void>((resolve) => streaming.listen(0, "127.0.0.1", resolve));
    const streamingPort = (streaming.address() as AddressInfo).port;

    try {
      await new Promise<void>((resolve, reject) => {
        const socket = connect(streamingPort, "127.0.0.1", () => {
          socket.write(
            "GET / HTTP/1.1\r\nHost: x\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
          );
          // A slow reader: resume in short bursts so the socket — and behind it the gzip stream —
          // fills and drains repeatedly, which is the only way a per-chunk drain wait recurs.
          socket.pause();
          const sip = setInterval(() => {
            socket.resume();
            setTimeout(() => socket.pause(), 1);
          }, 4);
          socket.on("close", () => {
            clearInterval(sip);
            resolve();
          });
          socket.on("error", (err) => {
            clearInterval(sip);
            reject(err);
          });
        });
      });
    } finally {
      process.off("warning", onWarning);
      await new Promise<void>((resolve) => streaming.close(() => resolve()));
    }

    // One wait at a time is the whole shape: the loop adds a listener, the drain resolves it, and
    // the wait is cleaned up before the next chunk. Anything that climbs is the leak.
    expect(peakOnGzip).toBeLessThanOrEqual(2);
    expect(warnings).toEqual([]);
  }, 60000);
});
