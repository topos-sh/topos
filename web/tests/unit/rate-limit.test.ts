import { describe, expect, it } from "vitest";
import { allowCommentWrite, allowRevertWrite, clientKeyFromXff } from "@/lib/rate-limit.server";

/**
 * The comment-write token bucket, driven with an injected clock (module state persists across
 * calls by design — each test uses its OWN key so buckets never interfere). Burst 5, one token
 * back every ~10 s (0.1/s): a human conversation never notices, a runaway loop does.
 */

const T0 = 1_700_000_000_000;

describe("allowCommentWrite", () => {
  it("allows the burst of 5, then refuses the 6th", () => {
    const key = "burst@example.com";
    for (let i = 0; i < 5; i++) {
      expect(allowCommentWrite(key, T0)).toBe(true);
    }
    expect(allowCommentWrite(key, T0)).toBe(false);
  });

  it("refills one token per ~10 s — a drained bucket admits exactly one more", () => {
    const key = "refill@example.com";
    for (let i = 0; i < 5; i++) {
      allowCommentWrite(key, T0);
    }
    expect(allowCommentWrite(key, T0)).toBe(false);
    // 5 s: half a token — still refused.
    expect(allowCommentWrite(key, T0 + 5_000)).toBe(false);
    // 10.5 s after the drain (denied touches update the clock, not the tokens): one token back.
    expect(allowCommentWrite(key, T0 + 15_500)).toBe(true);
    expect(allowCommentWrite(key, T0 + 15_500)).toBe(false);
  });

  it("keys are independent — one actor's burst never taxes another", () => {
    const hot = "hot@example.com";
    for (let i = 0; i < 6; i++) {
      allowCommentWrite(hot, T0);
    }
    expect(allowCommentWrite(hot, T0)).toBe(false);
    expect(allowCommentWrite("calm@example.com", T0)).toBe(true);
  });

  it("never refills past the burst ceiling", () => {
    const key = "ceiling@example.com";
    expect(allowCommentWrite(key, T0)).toBe(true);
    // A day later: back to a FULL burst (5), not an accumulated hoard.
    const later = T0 + 86_400_000;
    for (let i = 0; i < 5; i++) {
      expect(allowCommentWrite(key, later)).toBe(true);
    }
    expect(allowCommentWrite(key, later)).toBe(false);
  });
});

describe("allowRevertWrite", () => {
  it("allows the burst of 5, then refuses the 6th (per acting email)", () => {
    const key = "revert-burst@example.com";
    for (let i = 0; i < 5; i++) {
      expect(allowRevertWrite(key, T0)).toBe(true);
    }
    expect(allowRevertWrite(key, T0)).toBe(false);
  });

  it("refills one token per ~10 s — a drained bucket admits exactly one more", () => {
    const key = "revert-refill@example.com";
    for (let i = 0; i < 5; i++) {
      allowRevertWrite(key, T0);
    }
    expect(allowRevertWrite(key, T0)).toBe(false);
    // 5 s: half a token — still refused; 10.5 s after the drain: one token back.
    expect(allowRevertWrite(key, T0 + 5_000)).toBe(false);
    expect(allowRevertWrite(key, T0 + 15_500)).toBe(true);
    expect(allowRevertWrite(key, T0 + 15_500)).toBe(false);
  });

  it("keys are independent, and it is a bucket distinct from the comment belt", () => {
    const hot = "revert-hot@example.com";
    for (let i = 0; i < 6; i++) {
      allowRevertWrite(hot, T0);
    }
    expect(allowRevertWrite(hot, T0)).toBe(false);
    expect(allowRevertWrite("revert-calm@example.com", T0)).toBe(true);
    // Draining the revert belt for a key never taxes the same key's comment belt (separate maps).
    const shared = "shared@example.com";
    for (let i = 0; i < 5; i++) {
      allowRevertWrite(shared, T0);
    }
    expect(allowRevertWrite(shared, T0)).toBe(false);
    expect(allowCommentWrite(shared, T0)).toBe(true);
  });
});

describe("clientKeyFromXff", () => {
  it("takes the LAST hop — the one address the trusted edge appended", () => {
    expect(clientKeyFromXff("forged, 10.0.0.1, 203.0.113.9")).toBe("203.0.113.9");
    expect(clientKeyFromXff(null)).toBe("unknown");
    expect(clientKeyFromXff("  ")).toBe("unknown");
  });
});
