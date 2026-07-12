/**
 * The vault's INTERNAL session lane — the wire this tier speaks to the vault process. One
 * lane, one trust model: every request carries the shared internal bearer
 * (`Authorization: Bearer <PLANE_INTERNAL_TOKEN>`) plus the session-verified acting principal
 * (`X-Topos-Acting-Email`); the vault re-verifies the acting principal's roster row inside
 * every transaction (the same in-transaction gate the device lane runs), so this tier's
 * assertion is evidence, never authority. The vault answers the whole lane with 404 when no
 * internal token is configured.
 *
 * Read responses are byte-parity with the device lane's `/v1` wire shapes (the vault serializes
 * both through the same mappers); the shapes here are the fields this tier consumes.
 */

/** GET  /internal/v1/workspaces/{ws}/skills/{skill}/current */
export interface WireCurrentRecord {
  schema_version: number;
  workspace_id: string;
  skill_id: string;
  version_id: string;
  bundle_digest: string;
  epoch: number;
  seq: number;
  created_at: string;
}

/** GET  /internal/v1/workspaces/{ws}/skills/{skill}/versions/{versionId} */
export interface WireVersionMeta {
  schema_version: number;
  workspace_id: string;
  skill_id: string;
  version_id: string;
  bundle_digest: string | null;
  author: string;
  message: string;
  created_at: string;
  parents: string[];
  files: WireVersionFile[];
}

export interface WireVersionFile {
  path: string;
  mode: string;
  size: number;
  object_id: string;
}

/** GET  /internal/v1/workspaces/{ws}/skills/{skill}/proposals */
export interface WireProposalList {
  schema_version: number;
  proposals: WireOpenProposal[];
}

export interface WireOpenProposal {
  version_id: string;
  base_generation: { epoch: number; seq: number };
  created_at: string;
}

/** GET  /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{versionId} */
export interface WireProposalDetail {
  version_id: string;
  status: "open" | "accepted" | "rejected" | "closed";
  base_epoch: number;
  base_seq: number;
  created_at: string;
  proposer: string;
  review_required: boolean;
  resolved_by: string | null;
  resolved_reason: string | null;
  resolved_at: string | null;
}

/** POST /internal/v1/workspaces — body */
export interface CreateWorkspaceBody {
  request_id: string;
  display_name?: string;
  name?: string;
}

/** POST /internal/v1/workspaces — response */
export interface CreateWorkspaceOutcome {
  outcome: "created" | "replayed" | "denied";
  workspace_id?: string;
  address?: string;
  reason?: string;
}

/**
 * POST /internal/v1/device-sessions/{userCode}/approve — response. First-writer-wins: a
 * re-approve by the SAME email is the idempotent `confirmed`; anything else is the uniform miss.
 */
export interface ApproveSessionOutcome {
  outcome: "confirmed" | "not_found";
}

/** POST /internal/v1/device-sessions/{userCode}/approve-standup — body + response */
export interface ApproveStandupBody {
  display_name?: string;
  name?: string;
}
export interface ApproveStandupOutcome {
  outcome: "approved" | "already_approved" | "denied" | "not_found";
  workspace_id?: string;
  display_name?: string;
  reason?: string;
}

/** POST /internal/v1/workspaces — CreateWorkspaceOutcome extras on success. */
export interface CreatedWorkspace {
  workspace_id: string;
  display_name: string;
  address: string;
}

/** POST /internal/v1/workspaces/{ws}/roster/remove — body + response */
export interface RemoveMemberBody {
  request_id: string;
  email: string;
}
export interface RemoveMemberOutcome {
  outcome: "removed" | "denied";
  reason?: string;
}

/** POST /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{versionId}/approve|reject */
export interface ReviewDecisionBody {
  request_id: string;
  expected_epoch: number;
  expected_seq: number;
  /** Mandatory on reject; absent on approve. */
  reason?: string;
}
export interface ReviewDecisionOutcome {
  outcome: "approved" | "rejected" | "conflict" | "denied" | "not_found";
  reason?: string;
}

/** POST /internal/v1/workspaces/{ws}/skills/{skill}/reverts */
export interface RevertBody {
  request_id: string;
  good_version_id: string;
  expected_epoch: number;
  expected_seq: number;
}
export interface RevertOutcome {
  outcome: "reverted" | "conflict" | "denied" | "not_found";
  reason?: string;
}

/** GET /v1/enroll/verify/{userCode} — the PUBLIC verification-context read (device approval). */
export interface VerificationContext {
  intent?: "enroll" | "standup" | "login";
  machine_name: string;
  device_fingerprint: string;
  workspace_display_name: string;
  verified_domain?: string | null;
  verified_domain_status: string;
  offered_skills: string[];
}
