import type { GatewayContext, GatewayStore } from "../core/ports";
import { probeServerTools, type ProbeOutcome, type ProbeTarget } from "../core/probe";
import type { Logger } from "./log";

/**
 * THE HOST'S SIDE OF THE TOOL PROBE — the deadline, the dropping usage sink, and the one log line.
 *
 * The engine owns no clock beyond `now()` and schedules no timer of its own, so the bound on a
 * probe is the host's to impose. Two halves, both needed:
 *
 *  - an ABORT SIGNAL merged into every fetch the probe makes, which is what actually stops the work;
 *  - a RACE against the same deadline, because one upstream path (a 2024-11-05 request waiting on
 *    its SSE pump) resolves only when an answer arrives, so aborting the socket alone would leave
 *    the caller waiting forever.
 *
 * Nothing here can fail the act it follows. A probe that times out, refuses, or throws answers
 * `unreachable`, the tool list stays exactly as it was, and the sign-in that just landed is still
 * connected. A raced-out probe may still finish in the background and write the truth it found a
 * moment late; that is the only trace it leaves.
 */

/** How long a connect waits on the probe before giving up and leaving the list as it was. */
export const PROBE_DEADLINE_MS = 8_000;

export interface ToolProbeDeps {
  store: GatewayStore;
  guardedFetch: typeof fetch;
  log: Logger;
}

/** Every probe fetch carries the deadline, on top of whatever signal the caller already passed. */
function deadlineFetch(inner: typeof fetch, deadline: AbortSignal): typeof fetch {
  return ((input: RequestInfo | URL, init?: RequestInit) =>
    inner(input, {
      ...init,
      signal: init?.signal ? AbortSignal.any([init.signal, deadline]) : deadline,
    })) as typeof fetch;
}

/**
 * Probe one server's tools, bounded. A probe is not an agent's call, so the context it runs under
 * carries a sink that DROPS — there is no session to attribute a usage row to and none is invented.
 */
export async function probeToolsForServer(
  deps: ToolProbeDeps,
  target: ProbeTarget,
): Promise<ProbeOutcome> {
  const abort = new AbortController();
  const ctx: GatewayContext = {
    store: deps.store,
    usage: { record: () => {} },
    env: {
      fetch: deadlineFetch(deps.guardedFetch, abort.signal),
      now: () => Date.now(),
      log: deps.log,
    },
  };
  let expire: (outcome: ProbeOutcome) => void = () => {};
  const expired = new Promise<ProbeOutcome>((resolve) => {
    expire = resolve;
  });
  const timer = setTimeout(() => {
    abort.abort();
    expire({ kind: "unreachable" });
  }, PROBE_DEADLINE_MS);
  try {
    const outcome = await Promise.race([probeServerTools(ctx, target), expired]);
    deps.log("info", "tool probe finished", {
      serverId: target.serverId,
      outcome: outcome.kind,
      ...(outcome.kind === "recorded" ? { tools: outcome.tools } : {}),
    });
    return outcome;
  } catch (error) {
    deps.log("warn", "tool probe failed", {
      serverId: target.serverId,
      error: error instanceof Error ? error.name : "unknown",
    });
    return { kind: "unreachable" };
  } finally {
    clearTimeout(timer);
    abort.abort();
  }
}
