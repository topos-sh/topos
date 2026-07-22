import { describe, expect, it } from "vitest";
import { CONFIRM_NAME_FIELD, requireTypedName } from "@/lib/auth/ceremony.server";

/**
 * The destructive ceremonies' confirmation of intent — `requireTypedName`, the type-the-name
 * gate (ceremonies confirm, they don't re-authenticate). This exercises the exact-match rule
 * on its own; the route wiring is covered by the e2e suites.
 */

function confirmForm(value?: string): FormData {
  const form = new FormData();
  if (value !== undefined) {
    form.set(CONFIRM_NAME_FIELD, value);
  }
  return form;
}

describe("requireTypedName", () => {
  it("accepts an exact match", () => {
    expect(requireTypedName(confirmForm("deploy-runbook"), "deploy-runbook")).toEqual({ ok: true });
  });

  it("trims surrounding whitespace but is otherwise exact", () => {
    expect(requireTypedName(confirmForm("  deploy-runbook \n"), "deploy-runbook")).toEqual({
      ok: true,
    });
  });

  it("is case- and hyphen-sensitive (case and hyphens are part of the name)", () => {
    expect(requireTypedName(confirmForm("Deploy-Runbook"), "deploy-runbook").ok).toBe(false);
    expect(requireTypedName(confirmForm("deploy runbook"), "deploy-runbook").ok).toBe(false);
  });

  it("refuses a mismatch and names the exact expected value in the error", () => {
    const result = requireTypedName(confirmForm("nope"), "deploy-runbook");
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toContain("deploy-runbook");
    }
  });

  it("refuses an absent field (an empty confirm never matches a real name)", () => {
    expect(requireTypedName(confirmForm(), "deploy-runbook").ok).toBe(false);
  });
});
