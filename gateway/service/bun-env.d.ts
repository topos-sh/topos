/// <reference types="node" />

/**
 * The service runs on Bun but the shared tsconfig deliberately includes no runtime type set (the
 * engine is host-neutral). The node types ride the reference above (the service uses node:*
 * modules Bun implements); the one Bun-only API the service touches is declared here, minimally,
 * instead of dragging bun-types into a program the engine shares.
 */

interface BunServeOptions {
  hostname: string;
  port: number;
  /** Seconds; 0 disables — proxied SSE streams outlive any sane default. */
  idleTimeout?: number;
  fetch(request: Request): Response | Promise<Response>;
}

interface BunServer {
  stop(closeActiveConnections?: boolean): Promise<void>;
  readonly port: number;
  readonly hostname: string;
}

declare const Bun: {
  serve(options: BunServeOptions): BunServer;
};
