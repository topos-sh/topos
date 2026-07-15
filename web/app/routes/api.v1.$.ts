import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { uniformNotFound } from "@/lib/api/wire.server";

/**
 * The `/api/v1` CATCH-ALL — every device-lane path now TERMINATES in this tier (the vault has
 * no public face and the splat forwarder died with it). An unmatched path answers the ONE
 * uniform wire 404, the same envelope every auth/existence miss on the lane speaks — no path
 * echo, no existence signal, no react-router 400/405.
 */
export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  return checkBelt(request) ?? uniformNotFound();
}

export async function action({ request }: LoaderFunctionArgs): Promise<Response> {
  return checkBelt(request) ?? uniformNotFound();
}
