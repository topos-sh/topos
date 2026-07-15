import type { LoaderFunctionArgs } from "react-router";
import { data, redirect } from "react-router";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import { theWorkspace } from "@/lib/db/identity.server";
import { membershipsFor } from "@/lib/db/queries.server";

export function meta() {
  return [{ title: "Workspaces · Topos" }];
}

/**
 * The single-tenant resolve — this install serves exactly ONE workspace, so /workspaces is a
 * router, not a picker: a seat goes straight to the dashboard; no seat renders the honest miss
 * below (status 404 — same posture as every membership check, with the one next step a
 * legitimate visitor needs); an install still awaiting its owner bounces home, where the
 * landing shows the claim hint. There is no create flow here — a hosted deployment layers
 * workspace creation on as its own surface.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  const seat = (await membershipsFor(actor))[0];
  if (seat !== undefined) {
    throw redirect(`/workspaces/${seat.id}`);
  }
  const workspace = await theWorkspace();
  if (workspace === null || workspace.claimedAt === null) {
    throw redirect("/");
  }
  return data(null, { status: 404 });
}

/** Signed in, seatless: the 404-shaped pane (inside the shell) that still says what to do. */
export default function WorkspacesIndex() {
  return (
    <div className="grid min-h-[60dvh] place-items-center px-6">
      <div className="text-center">
        <p className="font-mono text-faint text-sm">404</p>
        <h1 className="mt-2 font-display font-semibold text-2xl text-ink">No seat here</h1>
        <p className="mt-3 text-dim">
          You don’t have a seat in this workspace. Ask a member to invite you.
        </p>
      </div>
    </div>
  );
}
