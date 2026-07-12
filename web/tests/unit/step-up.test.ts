import { describe, expect, it } from "vitest";
import { requireTypedName, STEP_UP_CONFIRM_FIELD } from "@/lib/auth/step-up.server";

/**
 * The PURE half of the step-up gate — `requireTypedName`, the destructive ceremonies' type-the-name
 * second factor. The password-verifying `requireStepUp` needs a live Better Auth session + request
 * headers, so it is covered by the e2e suite; this exercises the exact-match rule on its own.
 */

function confirmForm(value?: string): FormData {
  const form = new FormData();
  if (value !== undefined) {
    form.set(STEP_UP_CONFIRM_FIELD, value);
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
