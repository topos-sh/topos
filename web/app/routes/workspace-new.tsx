import { useEffect, useRef, useState } from "react";
import {
  type ActionFunctionArgs,
  data,
  Form,
  type LoaderFunctionArgs,
  type MetaFunction,
  redirect,
  useActionData,
  useFetcher,
  useLoaderData,
  useLocation,
  useNavigation,
} from "react-router";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { actorFromSession, notFound, requireSession, safeNextPath } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { announceCeremony } from "@/lib/ceremony-event";
import { createWorkspace, workspaceNameAvailable } from "@/lib/db/workspace-create.server";
import { destinationPathname } from "@/lib/destination-path";
import { followBase } from "@/lib/plane/follow-base.server";
import { isWorkspaceNameShape, toWorkspaceSlug, WORKSPACE_NAME_MAX } from "@/lib/workspace-name";
import { wsPathServer } from "@/lib/ws-url.server";

export const meta: MetaFunction = () => [{ title: "Create your workspace · Topos" }];

/**
 * Self-serve workspace creation AND onboarding — ONE page for both arrivals: a brand-new
 * signed-in person with no workspace (routed here from the door) and someone deliberately making
 * another. It mounts only in MULTI tenancy (a single-tenant install IS its one workspace, born at
 * boot — there is nothing to create), so the module is never reachable single-tenant.
 *
 * The person's display name for the workspace derives an address SLUG live, editable, with a
 * live-availability read under it. The same route answers the availability probe: a `check` query
 * returns `{ name, available }` (the form's debounced fetcher reads it), where a RESERVED slug and
 * a TAKEN one are both simply `false` — indistinguishable, so the reserved list never leaks.
 */

const ADDRESS_TAKEN = "That address is taken — try another.";
const CREATE_RATE_LIMITED =
  "You’ve created several workspaces recently — wait a while before creating another.";
const NAME_REQUIRED = "Enter a name for your workspace (1–100 characters).";
const SLUG_SHAPE =
  "The address uses lowercase letters, numbers, and hyphens (up to 100 characters).";

/** The create form's typed reply on a NON-redirect (a landed create redirects away). */
interface ActionData {
  error: string;
  displayName: string;
  slug: string;
}

export async function loader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  // The loader owns its own /login bounce (requireSession's would drop the query): the current
  // path — including any `next` a device-approval carried in — rides back as `next`, so a
  // sign-in returns here to finish creating.
  const actor = actorFromSession(await getAuth().api.getSession({ headers: request.headers }));
  if (actor === null) {
    // The DESTINATION path, never the raw single-fetch URL: a client-side arrival loads this
    // loader from `/new.data`, and a `next` built from that raw pathname would land the
    // post-login browser on the data endpoint as a document.
    const here = `${destinationPathname(request)}${url.search}`;
    throw redirect(`/login?next=${encodeURIComponent(here)}`);
  }
  // The live-availability probe: the debounced fetcher hits this same route with `?check=`.
  const check = url.searchParams.get("check");
  if (check !== null) {
    return { name: check, available: await workspaceNameAvailable(check) };
  }
  // A `name` prefill hint is honored only when it is already a valid slug; otherwise ignored.
  const nameHint = url.searchParams.get("name") ?? "";
  const prefillName = isWorkspaceNameShape(nameHint) ? nameHint : "";
  // Where to return after creating (e.g. a /verify approval that needed a workspace first).
  const nextParam = url.searchParams.get("next");
  const next = nextParam === null ? null : safeNextPath(nextParam);
  return { origin: followBase(request), prefillName, next };
}

export async function action({ request }: ActionFunctionArgs) {
  // A POST carries no query to preserve — the plain guard's /login bounce is right here.
  const actor = actorFromSession(await requireSession(request));
  if (actor === null) {
    notFound();
  }
  const form = await request.formData();
  const displayName = String(form.get("displayName") ?? "").trim();
  const slug = String(form.get("slug") ?? "").trim();
  const nextRaw = form.get("next");
  const next = typeof nextRaw === "string" && nextRaw.length > 0 ? safeNextPath(nextRaw) : null;

  if (displayName.length < 1 || displayName.length > 100) {
    return data<ActionData>({ error: NAME_REQUIRED, displayName, slug }, { status: 400 });
  }
  if (!isWorkspaceNameShape(slug)) {
    return data<ActionData>({ error: SLUG_SHAPE, displayName, slug }, { status: 400 });
  }
  const result = await createWorkspace(actor, { name: slug, displayName });
  if (result.outcome === "taken") {
    // A reserved slug and an already-taken one land the SAME refusal — one string, one status.
    return data<ActionData>({ error: ADDRESS_TAKEN, displayName, slug }, { status: 400 });
  }
  if (result.outcome === "off") {
    // The composition switched self-serve creation off — the surface does not exist.
    notFound();
  }
  if (result.outcome === "rate-limited") {
    // Honest and disclosed, unlike `taken` — a floor is not a secret.
    return data<ActionData>({ error: CREATE_RATE_LIMITED, displayName, slug }, { status: 429 });
  }
  throw redirect(next ?? wsPathServer(result.name));
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

/**
 * The one pathname the action's SUCCESS redirect produces for a create POST, derived from the
 * submission's own form data (the same fields the action reads): the `next` weave's pathname
 * when the form carried one, else the created workspace's path — `/<slug>` (this module mounts
 * only in MULTI tenancy). `null` when the form offers no success shape. `/login` is never a
 * success destination: it is the action's stale-session auth bounce, and `login` is a reserved
 * route segment so no created workspace can land there — it answers `null` even when the
 * submitted slug spells it.
 */
export function createSuccessPathname(formData: FormData | undefined): string | null {
  if (formData === undefined) {
    return null;
  }
  const next = formData.get("next");
  const slug = formData.get("slug");
  const destination =
    typeof next === "string" && next.length > 0
      ? (next.split(/[?#]/, 1)[0] ?? "")
      : typeof slug === "string" && slug.trim().length > 0
        ? `/${slug.trim()}`
        : "";
  return destination.length === 0 || destination === "/login" ? null : destination;
}

/**
 * TRUE exactly for the navigation moment that proves THIS page's create POST landed: our POST's
 * loading phase heading to the pathname the action's success redirect produces for the submitted
 * form. Navigation shape alone is NOT that proof — the action's `requireSession` bounces a stale
 * session to /login before `createWorkspace` ever runs, and that bounce is ALSO a POST navigating
 * away — so the destination must additionally match the success shape computed from the same
 * form data the action reads. A failed action revalidates THIS location instead (never a
 * navigation away), and the availability probe is a fetcher (never navigation state).
 */
export function isCreateSuccessNavigation(
  navigation: {
    state: "idle" | "loading" | "submitting";
    formMethod?: string;
    formData?: FormData;
    location?: { pathname: string };
  },
  herePathname: string,
): boolean {
  if (
    navigation.state !== "loading" ||
    navigation.formMethod !== "POST" ||
    navigation.location === undefined ||
    navigation.location.pathname === herePathname
  ) {
    return false;
  }
  return navigation.location.pathname === createSuccessPathname(navigation.formData);
}

export default function WorkspaceNew() {
  const loaderData = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  // The route component only ever renders for a PAGE navigation — the availability probe is a
  // fetcher.load that never re-renders it — so the page shape (an `origin`) is guaranteed here.
  if (typeof loaderData.origin !== "string") {
    return null;
  }
  return (
    <CreateForm
      origin={loaderData.origin}
      prefillName={loaderData.prefillName ?? ""}
      next={loaderData.next ?? null}
      actionData={actionData}
    />
  );
}

function CreateForm({
  origin,
  prefillName,
  next,
  actionData,
}: {
  origin: string;
  prefillName: string;
  next: string | null;
  actionData: ActionData | undefined;
}) {
  const [displayName, setDisplayName] = useState(actionData?.displayName ?? prefillName);
  const [slug, setSlug] = useState(actionData?.slug ?? toWorkspaceSlug(prefillName));
  // Once the person edits the address by hand we stop re-deriving it from the display name.
  const [slugEdited, setSlugEdited] = useState(false);
  const check = useFetcher<typeof loader>();

  // The `workspace_created` ceremony announcement. A landed create REDIRECTS away (the action
  // throws), so success is observable here only through the navigation — but ONLY when the
  // destination is the one this submission's success redirect produces
  // (isCreateSuccessNavigation): a stale session's auth bounce is also a POST heading elsewhere,
  // and a false announcement of a workspace that does not exist is worse than none. Ref-guarded
  // so dev strict-mode's doubled effect and re-renders within the same navigation dispatch
  // exactly once; the form unmounts on arrival, so the destination never replays it, and a
  // failed attempt leaves the guard unset for the retry that does land.
  const navigation = useNavigation();
  const location = useLocation();
  const announcedCreate = useRef(false);
  useEffect(() => {
    if (announcedCreate.current || !isCreateSuccessNavigation(navigation, location.pathname)) {
      return;
    }
    announcedCreate.current = true;
    announceCeremony("workspace_created");
  }, [navigation, location.pathname]);

  // Debounced live-availability read: one request per settled slug, and the answer carries the
  // slug it is for (`name`) so a stale reply for an earlier keystroke is ignored.
  useEffect(() => {
    if (!isWorkspaceNameShape(slug)) {
      return;
    }
    const id = setTimeout(() => {
      check.load(`/new?check=${encodeURIComponent(slug)}`);
    }, 300);
    return () => clearTimeout(id);
  }, [slug, check.load]);

  const checkData = check.data && "available" in check.data ? check.data : undefined;
  const forCurrent = checkData !== undefined && checkData.name === slug;
  const checking = check.state !== "idle";

  function onDisplayNameChange(value: string) {
    setDisplayName(value);
    if (!slugEdited) {
      setSlug(toWorkspaceSlug(value));
    }
  }

  function onSlugChange(value: string) {
    setSlugEdited(true);
    setSlug(toWorkspaceSlug(value));
  }

  return (
    <div className="mx-auto max-w-xl space-y-8">
      <PageHeader
        title="Create your workspace"
        meta="A workspace is where your team follows skills from — one address, one roster."
      />
      <section aria-labelledby="create-workspace-heading" className="space-y-3">
        <SectionHeading>
          <span id="create-workspace-heading">Name it</span>
        </SectionHeading>
        <Card className="space-y-4 px-4 py-4">
          <Form method="post" className="space-y-4">
            {next !== null && <input type="hidden" name="next" value={next} />}
            <label className="block">
              <span className="mb-1 block font-medium text-dim text-sm">Workspace name</span>
              <input
                type="text"
                name="displayName"
                required
                autoComplete="off"
                spellCheck={false}
                placeholder="Acme Engineering"
                maxLength={100}
                value={displayName}
                onChange={(e) => onDisplayNameChange(e.target.value)}
                className={INPUT}
              />
            </label>

            <label className="block">
              <span className="mb-1 block font-medium text-dim text-sm">Address</span>
              <input
                type="text"
                name="slug"
                required
                autoComplete="off"
                spellCheck={false}
                placeholder="acme-engineering"
                pattern="[a-z0-9][a-z0-9-]*"
                maxLength={WORKSPACE_NAME_MAX}
                value={slug}
                onChange={(e) => onSlugChange(e.target.value)}
                className={`${INPUT} font-mono`}
              />
              <AddressStatus
                slug={slug}
                origin={origin}
                checking={checking}
                available={forCurrent ? checkData?.available : undefined}
              />
            </label>

            <button
              type="submit"
              disabled={slug.length === 0 || displayName.trim().length === 0}
              className={`${buttonClasses("primary")} min-h-11`}
            >
              Create workspace
            </button>
          </Form>

          {actionData !== undefined && actionData.error.length > 0 && (
            <p className="text-red-600 text-sm" role="alert">
              {actionData.error}
            </p>
          )}
        </Card>
      </section>
    </div>
  );
}

/** The live address preview + availability line under the slug field. */
function AddressStatus({
  slug,
  origin,
  checking,
  available,
}: {
  slug: string;
  origin: string;
  checking: boolean;
  available: boolean | undefined;
}) {
  if (slug.length === 0) {
    return <p className="mt-1 text-faint text-xs">Your team will follow from an address here.</p>;
  }
  return (
    <div className="mt-1 space-y-1">
      <p className="text-faint text-xs">
        <span className="font-mono">
          {origin}/{slug}
        </span>
      </p>
      {!isWorkspaceNameShape(slug) ? (
        <p className="text-faint text-xs">
          Use lowercase letters, numbers, and hyphens for the address.
        </p>
      ) : checking ? (
        <p className="text-faint text-xs" role="status">
          Checking availability…
        </p>
      ) : available === true ? (
        <p className="text-green-700 text-xs" role="status">
          Available.
        </p>
      ) : available === false ? (
        <p className="text-red-600 text-xs" role="status">
          {ADDRESS_TAKEN}
        </p>
      ) : null}
    </div>
  );
}
