import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { announceCeremony, CEREMONY_EVENT } from "@/lib/ceremony-event";
import { installTestEnv } from "./helpers/test-env";

/**
 * The create form's `workspace_created` announcement gate. A POST navigating away is NOT proof
 * of a landed create: the action's `requireSession` bounces a stale session to /login BEFORE
 * `createWorkspace` runs, and announcing there would celebrate a workspace that does not exist.
 * The gate (`isCreateSuccessNavigation`) confirms the destination is exactly the pathname THIS
 * submission's success redirect produces — the `next` weave, else `/<slug>` — and refuses
 * `/login` outright (`login` is a reserved segment, so no created workspace can land there).
 * There is no component-test harness in this suite (the vitest environment is node), so the
 * helper the effect drives is pinned here — the ceremony-event dispatch discipline itself is
 * pinned in ceremony-event.test.ts.
 */

// The route module's server-touching imports, stubbed so the pure helpers import in a node test.
vi.mock("@/lib/auth/guards.server", () => ({
  actorFromSession: vi.fn(),
  notFound: vi.fn(),
  requireSession: vi.fn(),
  safeNextPath: (next: string | undefined) => next ?? "/app",
}));
vi.mock("@/lib/auth/server", () => ({ getAuth: vi.fn() }));
vi.mock("@/lib/db/workspace-create.server", () => ({
  createWorkspace: vi.fn(),
  workspaceNameAvailable: vi.fn(),
}));
vi.mock("@/lib/plane/follow-base.server", () => ({ followBase: vi.fn() }));
vi.mock("@/lib/ws-url.server", () => ({ wsPathServer: vi.fn() }));
vi.mock("@/components/ui", () => ({
  buttonClasses: () => "",
  Card: () => null,
  PageHeader: () => null,
  SectionHeading: () => null,
}));

let isCreateSuccessNavigation: typeof import("@/routes/workspace-new").isCreateSuccessNavigation;
let createSuccessPathname: typeof import("@/routes/workspace-new").createSuccessPathname;

beforeAll(async () => {
  installTestEnv();
  ({ isCreateSuccessNavigation, createSuccessPathname } = await import("@/routes/workspace-new"));
});

afterEach(() => {
  vi.unstubAllGlobals();
});

/** A stand-in `window` recording every ceremony detail it hears (the ceremony-event pattern). */
function listeningWindow(): { heard: unknown[] } {
  const target = new EventTarget();
  const heard: unknown[] = [];
  target.addEventListener(CEREMONY_EVENT, (event) => {
    heard.push((event as CustomEvent).detail);
  });
  vi.stubGlobal("window", target);
  return { heard };
}

function form(entries: Record<string, string>): FormData {
  const data = new FormData();
  for (const [name, value] of Object.entries(entries)) {
    data.set(name, value);
  }
  return data;
}

/** The create POST's loading phase heading to `destination` — the shape the effect observes. */
function postLoadingTo(destination: string, formData: FormData) {
  return {
    state: "loading" as const,
    formMethod: "POST",
    formData,
    location: { pathname: destination },
  };
}

/** Mirrors the route effect's body: dispatch the ceremony iff the gate answers true. */
function driveEffect(navigation: Parameters<typeof isCreateSuccessNavigation>[0]): void {
  if (isCreateSuccessNavigation(navigation, "/new")) {
    announceCeremony("workspace_created");
  }
}

describe("isCreateSuccessNavigation", () => {
  it("a stale-session auth bounce (POST heading to /login) yields NO dispatch", () => {
    const { heard } = listeningWindow();
    driveEffect(postLoadingTo("/login", form({ displayName: "Acme", slug: "acme" })));
    expect(heard).toEqual([]);
  });

  it("the auth bounce stays silent even when the submitted slug spells the login segment", () => {
    // `/${slug}` for slug "login" IS "/login" — but that slug is a reserved route segment the
    // action refuses in place, so a navigation there can only ever be the auth bounce.
    const { heard } = listeningWindow();
    driveEffect(postLoadingTo("/login", form({ displayName: "Login Co", slug: "login" })));
    expect(heard).toEqual([]);
  });

  it("a landed create (POST heading to the submitted /<slug>) dispatches ONE announcement", () => {
    const { heard } = listeningWindow();
    driveEffect(postLoadingTo("/acme", form({ displayName: "Acme", slug: "acme" })));
    expect(heard).toEqual([{ kind: "workspace_created" }]);
  });

  it("the /verify weave announces on the next destination it carried", () => {
    const { heard } = listeningWindow();
    driveEffect(postLoadingTo("/verify", form({ slug: "acme", next: "/verify?code=AB12-CD34" })));
    expect(heard).toEqual([{ kind: "workspace_created" }]);
  });

  it("an in-place revalidation (a failed action) is not a success navigation", () => {
    expect(isCreateSuccessNavigation(postLoadingTo("/new", form({ slug: "acme" })), "/new")).toBe(
      false,
    );
  });

  it("a POST heading anywhere the success redirect cannot produce stays silent", () => {
    // A destination unrelated to the submitted form — not this submission's success shape.
    expect(isCreateSuccessNavigation(postLoadingTo("/other", form({ slug: "acme" })), "/new")).toBe(
      false,
    );
  });

  it("only our POST's loading phase qualifies — GET loads, submitting, idle never announce", () => {
    const formData = form({ slug: "acme" });
    expect(
      isCreateSuccessNavigation({ state: "loading", location: { pathname: "/acme" } }, "/new"),
    ).toBe(false);
    expect(
      isCreateSuccessNavigation(
        { state: "submitting", formMethod: "POST", formData, location: { pathname: "/new" } },
        "/new",
      ),
    ).toBe(false);
    expect(isCreateSuccessNavigation({ state: "idle" }, "/new")).toBe(false);
  });
});

describe("createSuccessPathname", () => {
  it("derives the success destination from the submission's own fields", () => {
    expect(createSuccessPathname(form({ slug: "acme" }))).toBe("/acme");
    expect(createSuccessPathname(form({ slug: "  acme  " }))).toBe("/acme");
    expect(createSuccessPathname(form({ slug: "acme", next: "/verify?code=X" }))).toBe("/verify");
    expect(createSuccessPathname(form({ slug: "acme", next: "/settings#tab" }))).toBe("/settings");
  });

  it("offers no success shape for a missing form, an empty slug, or the login pathname", () => {
    expect(createSuccessPathname(undefined)).toBeNull();
    expect(createSuccessPathname(form({}))).toBeNull();
    expect(createSuccessPathname(form({ slug: "   " }))).toBeNull();
    expect(createSuccessPathname(form({ slug: "login" }))).toBeNull();
    expect(createSuccessPathname(form({ slug: "acme", next: "/login?next=/x" }))).toBeNull();
  });
});
