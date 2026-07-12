import { describe, expect, it } from "vitest";
import {
  deleteDeniedCopy,
  isValidSkillName,
  purgeDeniedCopy,
  renameDeniedCopy,
  SKILL_NAME_MAX,
  unarchiveDeniedCopy,
} from "@/lib/plane/lifecycle-copy";

/**
 * The lifecycle write helpers' PURE parts — the name-charset belt and the denied-outcome → copy
 * maps. The fetch-bearing helpers are exercised by the e2e; these classifiers are the cheap unit
 * surface, and the exact `name_taken` copy the crib pins is asserted verbatim.
 */

describe("isValidSkillName", () => {
  it("accepts the catalog charset (lowercase, digits, interior hyphens)", () => {
    for (const name of ["a", "deploy-runbook", "x1", "0abc", "a-b-c-1"]) {
      expect(isValidSkillName(name)).toBe(true);
    }
  });

  it("rejects uppercase, leading hyphen, empty, spaces, and over-length", () => {
    for (const name of [
      "",
      "-lead",
      "Upper",
      "has space",
      "under_score",
      "a".repeat(SKILL_NAME_MAX + 1),
    ]) {
      expect(isValidSkillName(name)).toBe(false);
    }
  });

  it("accepts exactly the max length and rejects one past it", () => {
    expect(isValidSkillName("a".repeat(SKILL_NAME_MAX))).toBe(true);
    expect(isValidSkillName("a".repeat(SKILL_NAME_MAX + 1))).toBe(false);
  });
});

describe("renameDeniedCopy", () => {
  it("maps each outcome code to distinct inline copy", () => {
    expect(renameDeniedCopy("name_taken")).toMatch(/already taken/i);
    expect(renameDeniedCopy("bad_name")).toMatch(/lowercase/i);
    expect(renameDeniedCopy("not_active")).toMatch(/isn't active/i);
    expect(renameDeniedCopy("owner_role_required")).toMatch(/owner/i);
  });

  it("an unknown/absent reason degrades to a generic declined line", () => {
    expect(renameDeniedCopy(undefined)).toMatch(/declined/i);
    expect(renameDeniedCopy("surprise")).toMatch(/declined/i);
  });
});

describe("unarchiveDeniedCopy", () => {
  it("pins the name-reuse copy verbatim (the way out is named)", () => {
    expect(unarchiveDeniedCopy("name_taken")).toBe(
      "the name was reused — rename after unarchiving",
    );
  });

  it("maps the other codes and degrades an unknown one", () => {
    expect(unarchiveDeniedCopy("not_archived")).toMatch(/isn't archived/i);
    expect(unarchiveDeniedCopy("owner_role_required")).toMatch(/owner/i);
    expect(unarchiveDeniedCopy(undefined)).toMatch(/declined/i);
  });
});

describe("deleteDeniedCopy", () => {
  it("maps the codes and degrades an unknown one", () => {
    expect(deleteDeniedCopy("not_archived")).toMatch(/archive/i);
    expect(deleteDeniedCopy("owner_role_required")).toMatch(/owner/i);
    expect(deleteDeniedCopy(undefined)).toMatch(/declined/i);
  });
});

describe("purgeDeniedCopy", () => {
  it("maps is_current and already_purged to distinct copy", () => {
    expect(purgeDeniedCopy("is_current")).toMatch(/current version/i);
    expect(purgeDeniedCopy("already_purged")).toMatch(/already purged/i);
    expect(purgeDeniedCopy("owner_role_required")).toMatch(/owner/i);
    expect(purgeDeniedCopy(undefined)).toMatch(/declined/i);
  });
});
