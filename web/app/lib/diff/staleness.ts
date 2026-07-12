/**
 * Proposal staleness, derived from the proposals listing + the current pointer's generation.
 * The generation values stay INTERNAL to this comparison — the return is the tri-state only;
 * nothing here (and nothing downstream) renders an epoch or a seq to a user.
 */

export interface GenerationLike {
  epoch: number;
  seq: number;
}

export interface ProposalListLike {
  proposals: readonly { version_id: string; base_generation: GenerationLike }[];
}

export type ProposalStatus = "open" | "moved" | "not-open";

export function deriveProposalStatus(
  list: ProposalListLike,
  versionId: string,
  currentGeneration: GenerationLike,
): ProposalStatus {
  const listed = list.proposals.find((p) => p.version_id === versionId);
  if (listed === undefined) {
    return "not-open";
  }
  const base = listed.base_generation;
  if (base.epoch === currentGeneration.epoch && base.seq === currentGeneration.seq) {
    return "open";
  }
  return "moved";
}
