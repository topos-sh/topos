import { describe, expect, it } from "vitest";
import { deriveDiffAvailability, deriveProposalPageState } from "@/lib/review/state";

/**
 * The proposal page's ONE state derivation as a truth table: the STORED status × (candidate vs
 * live current id). Pure — the derivation the proposal page renders from. There is no staleness
 * state anymore: a proposal stays decidable until someone decides it, and the approve action's
 * CAS binding refuses a moved pointer as a fresh OUTCOME, never a page state.
 */

const CANDIDATE = "c".repeat(64);
const OTHER = "d".repeat(64);

describe("deriveProposalPageState", () => {
  it("open is pending — decidable whatever the pointer says", () => {
    expect(deriveProposalPageState("open", CANDIDATE, OTHER)).toBe("pending");
    expect(deriveProposalPageState("open", CANDIDATE, CANDIDATE)).toBe("pending");
    expect(deriveProposalPageState("open", CANDIDATE, null)).toBe("pending");
  });

  it("approved with the candidate AS current is accepted-live", () => {
    expect(deriveProposalPageState("approved", CANDIDATE, CANDIDATE)).toBe("accepted-live");
  });

  it("approved with current moved on is superseded — a missing pointer counts as moved", () => {
    expect(deriveProposalPageState("approved", CANDIDATE, OTHER)).toBe("superseded");
    expect(deriveProposalPageState("approved", CANDIDATE, null)).toBe("superseded");
  });

  it("rejected is rejected regardless of the pointer", () => {
    expect(deriveProposalPageState("rejected", CANDIDATE, OTHER)).toBe("rejected");
    expect(deriveProposalPageState("rejected", CANDIDATE, CANDIDATE)).toBe("rejected");
  });

  it("withdrawn is closed — a real terminal state, not an anchoring failure", () => {
    expect(deriveProposalPageState("withdrawn", CANDIDATE, OTHER)).toBe("closed");
    expect(deriveProposalPageState("withdrawn", CANDIDATE, null)).toBe("closed");
  });

  it("an unrecognized stored status folds to unknown — never dressed up", () => {
    expect(deriveProposalPageState("", CANDIDATE, OTHER)).toBe("unknown");
    expect(deriveProposalPageState("accepted", CANDIDATE, CANDIDATE)).toBe("unknown");
    expect(deriveProposalPageState("surprise", CANDIDATE, null)).toBe("unknown");
  });
});

describe("deriveDiffAvailability", () => {
  it("a successful candidate-meta read renders the full diff", () => {
    expect(deriveDiffAvailability({ ok: true })).toBe("full");
  });

  it("a 404 is the vault's RECLAMATION — the diff-less state surface, not a page error", () => {
    // keep == read: a rejected or withdrawn candidate's bytes 404 by design.
    expect(deriveDiffAvailability({ ok: false, kind: "not_found" })).toBe("reclaimed");
  });

  it("any other failure is transient — unreadable, worth a reload", () => {
    expect(deriveDiffAvailability({ ok: false, kind: "unreachable" })).toBe("unreadable");
    expect(deriveDiffAvailability({ ok: false, kind: "plane_fault" })).toBe("unreadable");
    expect(deriveDiffAvailability({ ok: false })).toBe("unreadable");
  });
});
