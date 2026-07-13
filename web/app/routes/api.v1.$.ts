import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { forwardDeviceLane } from "@/lib/plane/client.server";

/**
 * The `/api/v1` PASS-THROUGH — every device-lane path this tier does not serve itself
 * (publish/propose/revert/review, the pointer/object/version reads, the whole enrollment and
 * governance surface, the operator policy route) forwards VERBATIM to the vault on the internal
 * network. No session, no guard, no pre-auth: the device's own bearer rides through and the
 * vault's in-transaction credential resolve stays the sole authority — see
 * `forwardDeviceLane` for exactly what may cross in each direction. An unknown `/api/v1` path
 * forwards too, so the vault's own fallback (the constant card / uniform 404) answers it —
 * posture parity by construction, not by reimplementation.
 */
export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  return checkBelt(request) ?? forwardDeviceLane(request);
}

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  return checkBelt(request) ?? forwardDeviceLane(request);
}
