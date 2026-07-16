import type { LoaderFunctionArgs } from "react-router";
import { Outlet, useLoaderData } from "react-router";
import { ShellChrome } from "@/components/shell/shell-chrome";
import { actorFromSession } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { loadChrome } from "@/lib/shell/chrome.server";

/**
 * The layout for the three shareable FACES (workspace root · a skill · a channel) — where a
 * resource address and its canonical page are ONE route. Unlike shell.tsx there is NO login-bounce
 * middleware: anonymous is a VALID state here (the child face renders the constant teaser / landing
 * with no existence oracle). The chrome loads only for a signed-in visitor; the face module itself
 * decides member (canonical page) vs signed-in non-member (house 404).
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (actor === null) {
    return { signedIn: false as const };
  }
  return { signedIn: true as const, chrome: await loadChrome(request, actor) };
}

export default function FaceShell() {
  const data = useLoaderData<typeof loader>();
  if (!data.signedIn) {
    // Anonymous: the child face renders its own full-page teaser/landing — no chrome.
    return <Outlet />;
  }
  return (
    <ShellChrome {...data.chrome}>
      <Outlet />
    </ShellChrome>
  );
}
