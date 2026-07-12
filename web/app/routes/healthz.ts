import { getPool } from "@/lib/db/index.server";

/**
 * Own-database liveness ONLY — this route never calls the vault, so orchestration can't mistake
 * a vault outage for a web outage.
 */
export async function loader(): Promise<Response> {
  try {
    await getPool().query("select 1");
    return Response.json({ ok: true });
  } catch {
    return Response.json({ ok: false }, { status: 503 });
  }
}
