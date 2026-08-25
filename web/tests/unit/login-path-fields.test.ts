import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * WHO EACH FIELD ON THE LOGIN PATH IS FOR.
 *
 * Password managers ignore `autocomplete="off"` by design: they read a lone text field in a form
 * as a sign-in field and mount their own inline menu over it. The menu takes the first keypress,
 * so the first Enter after a page load never reaches the form — nothing leaves the browser,
 * nothing appears in the page, and only a second attempt gets through, which reads as "it refused
 * what I typed". On the login path that lands on the one page a person reaches with a waiting
 * terminal beside them.
 *
 * So every field there has to say which kind it is, and the two kinds are opposites:
 *
 *  - NOT a credential (a login-approval code, a recovery code, a workspace name or address):
 *    an honest `autoComplete` for what it IS, plus `NOT_A_CREDENTIAL` — the opt-out 1Password,
 *    LastPass, and Bitwarden each document.
 *  - A credential (email, password): untouched. Filling those is the whole point of a password
 *    manager, and opting one out would be a worse bug than the one this guards.
 *
 * Read from source rather than a rendered page because the create-workspace fields (on /verify's
 * chooser and on /new) exist only in MULTI tenancy, which the single-tenancy e2e cannot reach.
 * verify.spec.ts proves the rendered attributes on the one field the browser suite can see.
 */

const APP = resolve(__dirname, "..", "..", "app");

/** The one VISIBLE `<input …/>` carrying `name="<name>"` in a route file, as source text. */
function inputNamed(file: string, name: string): string {
  const source = readFileSync(resolve(APP, file), "utf8");
  const found = source
    .split("<input")
    .slice(1)
    .map((chunk) => chunk.slice(0, chunk.indexOf("/>")))
    .filter((chunk) => chunk.includes(`name="${name}"`) && !chunk.includes('type="hidden"'));
  expect(found, `${file} renders exactly one visible field named "${name}"`).toHaveLength(1);
  return found[0] as string;
}

describe("the login path's fields say who they are for", () => {
  it.each([
    ["routes/verify.tsx", "code", "one-time-code"],
    ["routes/verify.tsx", "displayName", "organization"],
    // No platform hint describes a workspace address, so `off` stands — the opt-out beside it is
    // what actually keeps a manager's menu off the field.
    ["routes/verify.tsx", "slug", "off"],
    ["routes/workspace-new.tsx", "displayName", "organization"],
    ["routes/workspace-new.tsx", "slug", "off"],
    ["routes/recovery.tsx", "code", "one-time-code"],
  ])("%s field %s is declared %s and opted out", (file, name, hint) => {
    const field = inputNamed(file, name);
    expect(field).toContain(`autoComplete="${hint}"`);
    expect(field).toContain("{...NOT_A_CREDENTIAL}");
  });

  it.each([
    ["routes/login.tsx", "email", "email"],
    ["routes/login.tsx", "password", null],
    ["routes/claim.tsx", "email", "email"],
    ["routes/claim.tsx", "password", "new-password"],
    ["routes/invite-redeem.tsx", "password", "new-password"],
  ])("%s field %s stays a password manager's business", (file, name, hint) => {
    const field = inputNamed(file, name);
    expect(field).not.toContain("NOT_A_CREDENTIAL");
    expect(field).not.toContain("data-1p-ignore");
    if (hint !== null) {
      expect(field).toContain(`autoComplete="${hint}"`);
    }
  });
});

describe("NOT_A_CREDENTIAL", () => {
  it("carries the opt-out each of the three common managers documents", async () => {
    const { NOT_A_CREDENTIAL } = await import("@/components/ui");
    expect(NOT_A_CREDENTIAL).toEqual({
      "data-1p-ignore": "true",
      "data-lpignore": "true",
      "data-bwignore": "true",
    });
  });
});
