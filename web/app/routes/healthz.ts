import { serverEnv } from "@/env.server";
import { getPool } from "@/lib/db/index.server";

/**
 * Own-database liveness ONLY — this route never calls the vault, so orchestration can't mistake
 * a vault outage for a web outage.
 *
 * `build` identifies the running build when the deployment stamps `TOPOS_BUILD` (deploy tooling
 * needs to tell old from new during a rolling replacement, where "the site answers 200" no
 * longer proves which build answered). Unset, the field is simply absent.
 */
export async function loader(): Promise<Response> {
  const build = serverEnv().TOPOS_BUILD;
  const body = build ? { ok: true, build } : { ok: true };
  try {
    await getPool().query("select 1");
    return Response.json(body);
  } catch {
    return Response.json({ ...body, ok: false }, { status: 503 });
  }
}
