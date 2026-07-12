import { describe, expect, it } from "vitest";
import { deriveProposalStatus } from "@/lib/diff/staleness";

const V1 = "a".repeat(64);
const V2 = "b".repeat(64);

describe("deriveProposalStatus", () => {
  it("listed with a deep-equal base generation is open", () => {
    const list = { proposals: [{ version_id: V1, base_generation: { epoch: 2, seq: 7 } }] };
    expect(deriveProposalStatus(list, V1, { epoch: 2, seq: 7 })).toBe("open");
  });

  it("listed with a different seq is moved", () => {
    const list = { proposals: [{ version_id: V1, base_generation: { epoch: 2, seq: 6 } }] };
    expect(deriveProposalStatus(list, V1, { epoch: 2, seq: 7 })).toBe("moved");
  });

  it("listed with a different epoch is moved even when seq matches", () => {
    const list = { proposals: [{ version_id: V1, base_generation: { epoch: 1, seq: 7 } }] };
    expect(deriveProposalStatus(list, V1, { epoch: 2, seq: 7 })).toBe("moved");
  });

  it("absent from the list is not-open", () => {
    const list = { proposals: [{ version_id: V2, base_generation: { epoch: 2, seq: 7 } }] };
    expect(deriveProposalStatus(list, V1, { epoch: 2, seq: 7 })).toBe("not-open");
  });

  it("an empty list is not-open", () => {
    expect(deriveProposalStatus({ proposals: [] }, V1, { epoch: 0, seq: 0 })).toBe("not-open");
  });
});
