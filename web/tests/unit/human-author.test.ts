import { describe, expect, it } from "vitest";
import { humanAuthor } from "@/components/format";

/**
 * The review header's author gate: custody records the publishing installation's device id
 * (`d_` + 32 hex) as the commit author — machine identity, never rendered to humans. Anything
 * that is not exactly that shape is a recorded display string and passes through.
 */
describe("humanAuthor", () => {
  it("withholds a device-id-shaped author", () => {
    expect(humanAuthor("d_be703f314c6f4aad9f6f0d7ac1c561cd")).toBeUndefined();
  });

  it("stays absent when the candidate's meta is gone", () => {
    expect(humanAuthor(undefined)).toBeUndefined();
  });

  it("passes a human display string through untouched", () => {
    expect(humanAuthor("Mia")).toBe("Mia");
    expect(humanAuthor("mia@example.com")).toBe("mia@example.com");
  });

  it("only the exact shape counts as machine identity", () => {
    // Wrong length, wrong case, wrong prefix, extra text — all render as recorded.
    expect(humanAuthor("d_be703f31")).toBe("d_be703f31");
    expect(humanAuthor("D_BE703F314C6F4AAD9F6F0D7AC1C561CD")).toBe(
      "D_BE703F314C6F4AAD9F6F0D7AC1C561CD",
    );
    expect(humanAuthor("sn_b016c")).toBe("sn_b016c");
    expect(humanAuthor("d_be703f314c6f4aad9f6f0d7ac1c561cd (Mia)")).toBe(
      "d_be703f314c6f4aad9f6f0d7ac1c561cd (Mia)",
    );
  });
});
