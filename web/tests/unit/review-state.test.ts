import { describe, expect, it } from "vitest";
import {
  deriveDiffAvailability,
  deriveProposalPageState,
  type LiveCurrentLike,
  type ProposalDetailLike,
} from "@/lib/review/state";

/**
 * The page state machine as a truth table: STORED status × (base vs live generation) ×
 * (candidate vs live current id). Pure — the derivation the proposal page renders from.
 */

const CANDIDATE = "c".repeat(64);
const OTHER = "d".repeat(64);

function detail(status: string, epoch = 3, seq = 7): ProposalDetailLike {
  return { version_id: CANDIDATE, status, base_generation: { epoch, seq } };
}

function live(versionId: string, epoch = 3, seq = 7): LiveCurrentLike {
  return { versionId, generation: { epoch, seq } };
}

describe("deriveProposalPageState", () => {
  it("open on the live base is pending", () => {
    expect(deriveProposalPageState(detail("open"), live(OTHER))).toBe("pending");
  });

  it("open off the live base is stale — either half of the generation moving counts", () => {
    expect(deriveProposalPageState(detail("open", 3, 8), live(OTHER))).toBe("stale");
    expect(deriveProposalPageState(detail("open", 4, 7), live(OTHER))).toBe("stale");
  });

  it("accepted with the candidate AS current is accepted-live", () => {
    expect(deriveProposalPageState(detail("accepted"), live(CANDIDATE, 3, 8))).toBe(
      "accepted-live",
    );
  });

  it("accepted with current moved on is superseded — the base comparison is irrelevant", () => {
    expect(deriveProposalPageState(detail("accepted"), live(OTHER, 9, 9))).toBe("superseded");
    // Even a generation equal to the base cannot make an accepted row pending again.
    expect(deriveProposalPageState(detail("accepted"), live(OTHER))).toBe("superseded");
  });

  it("rejected is rejected regardless of the pointer", () => {
    expect(deriveProposalPageState(detail("rejected"), live(OTHER))).toBe("rejected");
    expect(deriveProposalPageState(detail("rejected"), live(CANDIDATE))).toBe("rejected");
  });

  it("a missing live pointer or an unrecognized status folds to unknown — never dressed up", () => {
    expect(deriveProposalPageState(detail("open"), undefined)).toBe("unknown");
    expect(deriveProposalPageState(detail("withdrawn"), live(OTHER))).toBe("unknown");
    expect(deriveProposalPageState(detail(""), live(OTHER))).toBe("unknown");
  });
});

describe("deriveDiffAvailability", () => {
  it("a successful candidate-meta read renders the full diff", () => {
    expect(deriveDiffAvailability({ ok: true })).toBe("full");
  });

  it("a 404 is the vault's RECLAMATION — the diff-less state surface, not a page error", () => {
    // keep == read: a rejected or staled candidate's bytes 404 by design.
    expect(deriveDiffAvailability({ ok: false, kind: "not_found" })).toBe("reclaimed");
  });

  it("any other failure is transient — unreadable, worth a reload", () => {
    expect(deriveDiffAvailability({ ok: false, kind: "unreachable" })).toBe("unreadable");
    expect(deriveDiffAvailability({ ok: false, kind: "plane_fault" })).toBe("unreadable");
    expect(deriveDiffAvailability({ ok: false })).toBe("unreadable");
  });
});
