/**
 * Contract-shaped fixture data shared by the fixture vault server and the e2e specs. Every
 * content id is 64-hex like the real wire. The vault the app calls speaks TWO surfaces: the
 * internal session lane (`/internal/v1/...`, Bearer + acting-email headers) and two public reads
 * (`/v1/enroll/verify/{user_code}`, `/i/{token}`). The scopes below back the internal-lane reads
 * (current / version metadata / bundle bytes / proposals / proposal-detail); membership, the
 * catalog, roster, and workspace addresses live in the DIRECTORY's own Postgres tables, seeded
 * separately (auth.setup.ts) — the app reads those directly, never over this vault.
 */

export const WS = "ws-e2e";
export const SKILL = "deploy-runbook";

export const CURRENT_ID = "c0".repeat(32);
export const CANDIDATE_ID = "ca".repeat(32);
export const MOVED_ID = "b1".repeat(32);
export const NOTOPEN_ID = "d2".repeat(32);
export const GENESIS_ID = "e0".repeat(32);

export const OID_SKILL_OLD = "11".repeat(32);
export const OID_SKILL_NEW = "22".repeat(32);
export const OID_SAME = "33".repeat(32);
export const OID_MODE = "44".repeat(32);
export const OID_MOVE = "55".repeat(32);
export const OID_BIN_OLD = "66".repeat(32);
export const OID_BIN_NEW = "77".repeat(32);
export const OID_BIG = "88".repeat(32);
export const OID_XSS = "99".repeat(32);
export const OID_DELETED = "aa".repeat(32);
export const OID_FAIL = "bb".repeat(32);

export const XSS_PATH = "notes/<img src=x onerror=alert(1)>.md";
export const XSS_CONTENT = '<script>alert("xss-e2e")</script>\n[click](javascript:alert(1))\n';
export const BINARY_MARKER = "RAW_BINARY_MARKER_MUST_NOT_RENDER";

export const CURRENT_GENERATION = { epoch: 1, seq: 5 };

export const CURRENT_FILES = [
  { path: "SKILL.md", mode: "100644", object_id: OID_SKILL_OLD },
  { path: "docs/guide.md", mode: "100644", object_id: OID_SAME },
  { path: "scripts/deploy.sh", mode: "100644", object_id: OID_MODE },
  { path: "docs/old-name.md", mode: "100644", object_id: OID_MOVE },
  { path: "assets/blob.bin", mode: "100644", object_id: OID_BIN_OLD },
  { path: "notes/removed.md", mode: "100644", object_id: OID_DELETED },
];

export const CANDIDATE_FILES = [
  { path: "SKILL.md", mode: "100644", object_id: OID_SKILL_NEW },
  { path: "docs/guide.md", mode: "100644", object_id: OID_SAME },
  { path: "scripts/deploy.sh", mode: "100755", object_id: OID_MODE },
  { path: "docs/new-name.md", mode: "100644", object_id: OID_MOVE },
  { path: "assets/blob.bin", mode: "100644", object_id: OID_BIN_NEW },
  { path: "data/big.dat", mode: "100644", object_id: OID_BIG },
  { path: XSS_PATH, mode: "100644", object_id: OID_XSS },
  { path: "notes/broken.md", mode: "100644", object_id: OID_FAIL },
];

export const VERSION_METAS = {
  [CURRENT_ID]: {
    version_id: CURRENT_ID,
    parents: [GENESIS_ID],
    author: "dev-aaaa1111",
    message: "current version",
    bundle_digest: "e3".repeat(32),
    files: CURRENT_FILES,
  },
  // The genesis ancestor of CURRENT_ID — readable (trunk-reachable), so the ws-e2e history walk
  // shows a genuine NON-current row. A plain member sees NO roll-back control on it (the
  // affordance is owner|reviewer-only); the revert e2e's member-posture check rides that row.
  [GENESIS_ID]: {
    version_id: GENESIS_ID,
    parents: [],
    author: "dev-aaaa1111",
    message: "genesis: seed the deploy runbook",
    bundle_digest: "d0".repeat(32),
    files: [{ path: "SKILL.md", mode: "100644", object_id: OID_SKILL_OLD }],
  },
  [CANDIDATE_ID]: {
    version_id: CANDIDATE_ID,
    parents: [CURRENT_ID],
    author: "dev-bbbb2222",
    message: "Tighten the deploy steps\n\nAdds an explicit test step before shipping.",
    bundle_digest: "f4".repeat(32),
    files: CANDIDATE_FILES,
  },
  [MOVED_ID]: {
    version_id: MOVED_ID,
    parents: [GENESIS_ID],
    author: "dev-cccc3333",
    message: "An older proposal whose base moved",
    bundle_digest: "a5".repeat(32),
    files: [{ path: "SKILL.md", mode: "100644", object_id: OID_SKILL_NEW }],
  },
  [NOTOPEN_ID]: {
    version_id: NOTOPEN_ID,
    parents: [CURRENT_ID],
    author: "dev-dddd4444",
    message: "A proposal that is no longer open",
    bundle_digest: "b6".repeat(32),
    files: [{ path: "SKILL.md", mode: "100644", object_id: OID_SKILL_NEW }],
  },
};

export const PROPOSALS = {
  proposals: [
    {
      version_id: CANDIDATE_ID,
      base_generation: CURRENT_GENERATION,
      created_at: "2026-07-01T18:00:00Z",
    },
    {
      version_id: MOVED_ID,
      base_generation: { epoch: 1, seq: 4 },
      created_at: "2026-06-30T09:00:00Z",
    },
  ],
};

// ---------------------------------------------------------------------------------------------
// The session-review byte contract (the pinned `denied` reasons the vault relays). Deliberately
// REDECLARED here rather than imported from app/ — the fixture pins the vault's wire bytes
// independently, so a drift in either tier's copy fails a spec instead of hiding.
// ---------------------------------------------------------------------------------------------
export const REVIEW_DENIED_REASON = {
  roleGate: "approving or rejecting needs an owner or reviewer seat",
  fourEyes: "the proposer may not approve their own proposal under review-required",
  notOpen: "no open proposal for this candidate and base",
  alreadyAccepted: "the proposal is already accepted",
};
export const REVERT_TARGET_DENIED_REASON = "revert target is not an accepted version";

/**
 * The proposal-detail metas for ws-e2e's deploy-runbook — STATIC (the suite's default viewer
 * holds a plain member seat there, so no write can ever mutate them): the open candidate (base
 * == current → pending), the open-but-moved proposal (derived stale), and the rejected one
 * (terminal, with full resolution facts). Shape mirrors the detail read's internal model; the
 * fixture flattens it onto the wire.
 */
export const WS_PROPOSAL_DETAILS = {
  [CANDIDATE_ID]: {
    status: "open",
    base_generation: CURRENT_GENERATION,
    created_at: "2026-07-01T18:00:00Z",
    proposer: "dev-bbbb2222",
    review_required: false,
    resolution: null,
  },
  [MOVED_ID]: {
    status: "open",
    base_generation: { epoch: 1, seq: 4 },
    created_at: "2026-06-30T09:00:00Z",
    proposer: "dev-cccc3333",
    review_required: false,
    resolution: null,
  },
  [NOTOPEN_ID]: {
    status: "rejected",
    base_generation: CURRENT_GENERATION,
    created_at: "2026-06-29T09:00:00Z",
    proposer: "dev-dddd4444",
    review_required: false,
    resolution: {
      resolved_by: "dev-aaaa1111",
      reason: "Superseded by a cleaner run of the same change.",
      resolved_at: "2026-06-29T12:00:00Z",
    },
  },
};

// The public verification-context read. `offered_skills` is a plain string list on this wire.
export const VERIFY_CONTEXTS = {
  APPROVED1: {
    machine_name: "roberts-macbook",
    device_fingerprint: "9f3a7c21b4d8e650",
    workspace_display_name: "Acme Platform",
    verified_domain: "acme.dev",
    verified_domain_status: "verified",
    offered_skills: ["Deploy runbook"],
    intent: "enroll",
  },
  PENDING99: {
    machine_name: "build-box",
    device_fingerprint: "17e2c4a90b6d3f58",
    workspace_display_name: "Acme Platform",
    verified_domain: "acme.dev",
    verified_domain_status: "pending",
    offered_skills: [],
    intent: "enroll",
  },
  NODOMAIN7: {
    machine_name: "shared-laptop",
    device_fingerprint: "c8d1f0347a5e92b6",
    workspace_display_name: "Side Project",
    verified_domain: null,
    verified_domain_status: "unverified",
    offered_skills: ["Deploy runbook"],
    intent: "enroll",
  },
  // A LOGIN session: workspace-less by design (the device re-mints its credentials across every
  // confirmed seat) — the page must render the sign-in consent, never a join framing.
  LOGIN77: {
    machine_name: "travel-laptop",
    device_fingerprint: "a1b2c3d4e5f60718",
    workspace_display_name: "",
    verified_domain: null,
    verified_domain_status: "unverified",
    offered_skills: [],
    intent: "login",
  },
  // A STANDUP session: no workspace exists yet (the vault fills the required name with ""), the
  // page branches its copy on the intent.
  STANDUP42: {
    machine_name: "founders-laptop",
    device_fingerprint: "d4e5f60718293041",
    workspace_display_name: "",
    verified_domain: null,
    verified_domain_status: "unverified",
    offered_skills: [],
    intent: "standup",
  },
};

export const RATE_LIMITED_CODE = "RATELIMIT";

// ---------------------------------------------------------------------------------------------
// Workspace standup / create (the internal-lane genesis writes).
// ---------------------------------------------------------------------------------------------

/** The workspace the fixture "creates"; the ADDRESS is what the paste block must show. */
export const CREATED_WS_ID = "w_created01";
export const CREATED_ADDRESS = "acme-platform";
/** Submitting this display name simulates the per-owner cap denial. */
export const CAP_TRIGGER_NAME = "CAP TEST";
export const CAP_REASON = "workspace creation limit reached";
/** Submitting this display name simulates a non-cap denial (the form stays for a retry). */
export const DENY_TRIGGER_NAME = "DENY TEST";
export const DENY_REASON = "synthetic denial (e2e)";
/** Submitting this display name simulates a transient vault fault (a 500) on the approve. */
export const ERROR_TRIGGER_NAME = "ERROR TEST";

// ---------------------------------------------------------------------------------------------
// The directory (Postgres) seed. auth.setup.ts writes plane.workspace + plane.workspace_member +
// the catalog/current/skill_commit/proposals + workspace_policy rows from THESE constants — the
// single source the vault scopes below also derive from, or the app's DB-read surfaces and the
// vault's HTTP surface disagree mid-test. All principals CANONICAL lowercase: the 0010/0015 CHECKs
// (principal = lower(principal COLLATE "C")) make a non-canonical seed a loud constraint violation.
// ---------------------------------------------------------------------------------------------

/** The suite's default signed-in identity (tests/e2e/env.ts MEMBER_EMAIL — keep in sync). */
export const DEFAULT_MEMBER_EMAIL = "reviewer@example.com";
/** Invited-only (never confirmed): index shows the invited framing, direct nav 404s. */
export const INVITED_EMAIL = "invited@example.com";
/** The enrolled joiner: a confirmed seat and ZERO web-tier rows — the seat alone must put the
 * workspace on their dashboard. */
export const JOINER_EMAIL = "joiner@example.com";
/** A signed-in identity with NO seat anywhere: the uniform-404 subject. */
export const OUTSIDER_EMAIL = "outsider@example.com";

// The workspace ADDRESSES (unique URL slugs; charset ^[a-z0-9][a-z0-9-]*$, none reserved).
export const WS_ADDRESS = "e2e";
export const ROSTER_WS = "w_roster";
export const ROSTER_ADDRESS = "roster-ws";
export const ROSTER_OWNER_EMAIL = "owner-roster@example.com";
export const ROSTER_MEMBER_EMAIL = "plain-roster@example.com";
export const ROSTER_REMOVABLE_EMAIL = "carol@example.com";
/** The plane-member-but-not-owner workspace: the same owner email holds a confirmed MEMBER seat,
 * so its settings page renders without any owner controls. */
export const NOT_OWNER_WS = "w_notowner";
export const NOT_OWNER_ADDRESS = "notowner-ws";

// ---------------------------------------------------------------------------------------------
// The session-review workspace: its OWN workspace + its OWN signed-in identity, because the
// suite's default member must keep exactly ONE membership total (the /app fast-path assertion in
// app-entry.spec.ts is a seed-state invariant) and their ws-e2e seat must stay a plain MEMBER (the
// member-cannot-decide postures ride it). REVIEWER_EMAIL holds a confirmed REVIEWER seat here —
// page admission + `canDecide` both derive from that DB row; the vault scope only gates the
// internal-lane reads/writes and plays back the configured responses.
// ---------------------------------------------------------------------------------------------

export const REVIEW_WS = "w_review";
export const REVIEW_ADDRESS = "review-ws";
export const REVIEWER_EMAIL = "decider@example.com";

/** One skill per mutating scenario, so the write specs never share fixture state. */
export const APPROVE_SKILL = "approve-runbook";
export const STALE_SKILL = "stale-runbook";
export const REJECT_SKILL = "reject-runbook";
export const SELF_SKILL = "self-runbook";

export const R_APPROVE_CUR = "a1".repeat(32);
export const R_APPROVE_CAND = "c1".repeat(32);
export const R_STALE_CUR = "a2".repeat(32);
export const R_STALE_CAND = "c2".repeat(32);
export const R_REJECT_CUR = "a3".repeat(32);
export const R_REJECT_CAND = "c3".repeat(32);
export const R_SELF_CUR = "a4".repeat(32);
export const R_SELF_CAND = "c4".repeat(32);

// Distinct (epoch, seq) pairs per skill — an epoch/seq swap in the wire payload fails loudly.
export const APPROVE_GENERATION = { epoch: 2, seq: 7 };
export const STALE_GENERATION = { epoch: 1, seq: 9 };
export const REJECT_GENERATION = { epoch: 3, seq: 4 };
export const SELF_GENERATION = { epoch: 1, seq: 13 };

// The team-revert skills (their own current + a known-good ancestor to roll back to). REVERT_SKILL
// is the happy path; REVERT_STALE_SKILL's `alwaysConflict` makes every revert the stale-CAS
// conflict.
export const REVERT_SKILL = "revert-runbook";
export const REVERT_STALE_SKILL = "revert-stale-runbook";
export const R_REVERT_CUR = "a5".repeat(32);
export const R_REVERT_GOOD = "c5".repeat(32);
export const R_REVERT_STALE_CUR = "a6".repeat(32);
export const R_REVERT_STALE_GOOD = "c6".repeat(32);
export const REVERT_GENERATION = { epoch: 4, seq: 2 };
export const REVERT_STALE_GENERATION = { epoch: 1, seq: 8 };

const REVIEW_UPDATED_AT_MS = Date.parse("2026-07-05T09:00:00Z");

/** One review skill's MUTABLE fixture state: a 2-version story (current → candidate, the same
 * OLD/NEW SKILL.md blobs the ws-e2e diff uses) plus its proposal detail meta. `alwaysConflict`
 * makes every approve the stale-CAS conflict AND moves the generation (simulating the concurrent
 * pointer move the conflict reports), so the revalidated page derives `stale`. */
function reviewSkillState({
  currentId,
  candidateId,
  generation,
  proposer,
  reviewRequired,
  alwaysConflict,
  message,
}) {
  const createdAt = "2026-07-05T10:00:00Z";
  return {
    currentId,
    generation: { ...generation },
    updatedAtMs: REVIEW_UPDATED_AT_MS,
    bundleDigest: currentId,
    alwaysConflict: alwaysConflict === true,
    metas: {
      [currentId]: {
        version_id: currentId,
        parents: [],
        author: "dev-aaaa1111",
        message: "current version",
        bundle_digest: currentId,
        files: [{ path: "SKILL.md", mode: "100644", object_id: OID_SKILL_OLD }],
      },
      [candidateId]: {
        version_id: candidateId,
        parents: [currentId],
        author: "dev-bbbb2222",
        message,
        bundle_digest: candidateId,
        files: [{ path: "SKILL.md", mode: "100644", object_id: OID_SKILL_NEW }],
      },
    },
    proposals: {
      proposals: [
        { version_id: candidateId, base_generation: { ...generation }, created_at: createdAt },
      ],
    },
    proposalMeta: {
      [candidateId]: {
        status: "open",
        base_generation: { ...generation },
        created_at: createdAt,
        proposer,
        review_required: reviewRequired,
        resolution: null,
      },
    },
  };
}

/** One revert skill's MUTABLE fixture state: a 2-version LINEAR history (goodId → currentId, the
 * same OLD/NEW SKILL.md blobs the diff uses) — so the history walk shows the current head plus one
 * readable NON-current ancestor (the roll-back target). `alwaysConflict` makes every revert the
 * stale-CAS conflict AND bumps the generation, so the reloaded page rebinds against the moved
 * current. */
function revertSkillState({ currentId, goodId, generation, alwaysConflict }) {
  return {
    currentId,
    generation: { ...generation },
    updatedAtMs: REVIEW_UPDATED_AT_MS,
    bundleDigest: currentId,
    alwaysConflict: alwaysConflict === true,
    metas: {
      [currentId]: {
        version_id: currentId,
        parents: [goodId],
        author: "dev-aaaa1111",
        message: "current version",
        bundle_digest: currentId,
        files: [{ path: "SKILL.md", mode: "100644", object_id: OID_SKILL_NEW }],
      },
      [goodId]: {
        version_id: goodId,
        parents: [],
        author: "dev-bbbb2222",
        message: "The known-good version to roll back to",
        bundle_digest: goodId,
        files: [{ path: "SKILL.md", mode: "100644", object_id: OID_SKILL_OLD }],
      },
    },
    proposals: { proposals: [] },
    proposalMeta: {},
  };
}

// ---------------------------------------------------------------------------------------------
// The DB catalog seed (plane.catalog / plane.current / plane.skill_commit / plane.proposals).
// auth.setup.ts writes THESE rows, and the dashboard's DB-first skill index renders from them
// with no vault call. The internal-lane read scopes (below) serve the SAME conventions re-keyed
// by (ws, skill). NOTE: skill_id === catalog name for every seeded skill (the app resolves the URL
// catalog name to the immutable skill_id and every vault call keys on it; the identity mapping
// keeps the two consistent and the recorded-call assertions legible).
// ---------------------------------------------------------------------------------------------

/** The HERO skill: published (a `current` row) with NO web-tier row anywhere — the catalog must
 * show it to any confirmed member from the seed alone. */
export const HERO_SKILL = "release-checklist";
export const HERO_CURRENT_ID = "5e".repeat(32);
export const HERO_BUNDLE_DIGEST = "6f".repeat(32);
/** A published skill whose provenance row carries a NULL bundle_digest — the catalog renders an
 * em-dash, never a fake value. */
export const NULLDIGEST_SKILL = "legacy-notes";
export const NULLDIGEST_CURRENT_ID = "7b".repeat(32);

// `current.updated_at` is BIGINT epoch-MILLISECONDS (the server clock stamps ms).
export const SKILL_UPDATED_AT_MS = Date.parse("2026-07-01T18:30:00Z");
export const HERO_UPDATED_AT_MS = Date.parse("2026-07-02T09:00:00Z");
export const NULLDIGEST_UPDATED_AT_MS = Date.parse("2026-07-02T10:00:00Z");

export const PLANE_SEED = {
  workspaces: [
    {
      workspaceId: WS,
      displayName: "E2E Workspace",
      address: WS_ADDRESS,
      createdAt: "2026-07-01T09:00:00Z",
    },
    {
      workspaceId: ROSTER_WS,
      displayName: "Roster Workspace",
      address: ROSTER_ADDRESS,
      createdAt: "2026-07-02T08:00:00Z",
    },
    {
      workspaceId: NOT_OWNER_WS,
      displayName: "Not The Workspace Owner",
      address: NOT_OWNER_ADDRESS,
      createdAt: "2026-07-02T08:30:00Z",
    },
    {
      workspaceId: REVIEW_WS,
      displayName: "Review Workspace",
      address: REVIEW_ADDRESS,
      createdAt: "2026-07-03T08:00:00Z",
    },
  ],
  members: [
    // The default member: confirmed MEMBER of ws-e2e (never owner — the member-cannot-flip-policy
    // assertion rides this seat), and their ONLY membership (the /app fast-path invariant).
    {
      workspaceId: WS,
      principal: DEFAULT_MEMBER_EMAIL,
      role: "member",
      status: "confirmed",
      invitedBy: null,
      addedAt: "2026-07-01T10:00:00Z",
    },
    {
      workspaceId: WS,
      principal: JOINER_EMAIL,
      role: "member",
      status: "confirmed",
      invitedBy: null,
      addedAt: "2026-07-01T10:05:00Z",
    },
    // The roster spec's workspace: a confirmed OWNER plus two confirmed members + one invited.
    {
      workspaceId: ROSTER_WS,
      principal: ROSTER_OWNER_EMAIL,
      role: "owner",
      status: "confirmed",
      invitedBy: null,
      addedAt: "2026-07-02T09:00:00Z",
    },
    {
      workspaceId: ROSTER_WS,
      principal: ROSTER_MEMBER_EMAIL,
      role: "member",
      status: "confirmed",
      invitedBy: ROSTER_OWNER_EMAIL,
      addedAt: "2026-07-02T09:05:00Z",
    },
    {
      workspaceId: ROSTER_WS,
      principal: ROSTER_REMOVABLE_EMAIL,
      role: "member",
      status: "confirmed",
      invitedBy: ROSTER_OWNER_EMAIL,
      addedAt: "2026-07-02T09:10:00Z",
    },
    {
      workspaceId: ROSTER_WS,
      principal: INVITED_EMAIL,
      role: "member",
      status: "invited",
      invitedBy: ROSTER_OWNER_EMAIL,
      addedAt: "2026-07-02T09:15:00Z",
    },
    // The honest-state subject: a confirmed MEMBER (not owner) of w_notowner.
    {
      workspaceId: NOT_OWNER_WS,
      principal: ROSTER_OWNER_EMAIL,
      role: "member",
      status: "confirmed",
      invitedBy: null,
      addedAt: "2026-07-02T10:00:00Z",
    },
    // The session-review decider: a confirmed REVIEWER seat (their ONLY membership) — page
    // admission and the decision panel's `canDecide` both ride this row.
    {
      workspaceId: REVIEW_WS,
      principal: REVIEWER_EMAIL,
      role: "reviewer",
      status: "confirmed",
      invitedBy: null,
      addedAt: "2026-07-03T09:00:00Z",
    },
  ],
};

export const PLANE_SKILL_SEED = {
  // Catalog rows: the name→skill identity surface (skill_id === name here). Every skill that
  // renders on a DB-first surface (dashboard, skill header) needs one.
  catalog: [
    { ws: WS, skillId: SKILL, name: SKILL },
    { ws: WS, skillId: HERO_SKILL, name: HERO_SKILL },
    { ws: WS, skillId: NULLDIGEST_SKILL, name: NULLDIGEST_SKILL },
    { ws: REVIEW_WS, skillId: APPROVE_SKILL, name: APPROVE_SKILL },
    { ws: REVIEW_WS, skillId: STALE_SKILL, name: STALE_SKILL },
    { ws: REVIEW_WS, skillId: REJECT_SKILL, name: REJECT_SKILL },
    { ws: REVIEW_WS, skillId: SELF_SKILL, name: SELF_SKILL },
    { ws: REVIEW_WS, skillId: REVERT_SKILL, name: REVERT_SKILL },
    { ws: REVIEW_WS, skillId: REVERT_STALE_SKILL, name: REVERT_STALE_SKILL },
  ],
  // Provenance rows first: `current` and `proposals` both FK onto (workspace_id, commit_id).
  commits: [
    { ws: WS, skillId: SKILL, commitId: CURRENT_ID, bundleDigest: "e3".repeat(32) },
    { ws: WS, skillId: SKILL, commitId: CANDIDATE_ID, bundleDigest: "f4".repeat(32) },
    { ws: WS, skillId: SKILL, commitId: MOVED_ID, bundleDigest: "a5".repeat(32) },
    { ws: WS, skillId: SKILL, commitId: NOTOPEN_ID, bundleDigest: "b6".repeat(32) },
    { ws: WS, skillId: HERO_SKILL, commitId: HERO_CURRENT_ID, bundleDigest: HERO_BUNDLE_DIGEST },
    { ws: WS, skillId: NULLDIGEST_SKILL, commitId: NULLDIGEST_CURRENT_ID, bundleDigest: null },
    // The review workspace's catalog rows: the proposal PAGE's existence probe is the DB `current`
    // row (skillIndexRow), so every review skill needs one. The vault's HTTP state moves during the
    // write specs; these rows deliberately do NOT (harness discipline — the DB-fed surfaces are
    // never asserted against fixture moves).
    { ws: REVIEW_WS, skillId: APPROVE_SKILL, commitId: R_APPROVE_CUR, bundleDigest: R_APPROVE_CUR },
    {
      ws: REVIEW_WS,
      skillId: APPROVE_SKILL,
      commitId: R_APPROVE_CAND,
      bundleDigest: R_APPROVE_CAND,
    },
    { ws: REVIEW_WS, skillId: STALE_SKILL, commitId: R_STALE_CUR, bundleDigest: R_STALE_CUR },
    { ws: REVIEW_WS, skillId: STALE_SKILL, commitId: R_STALE_CAND, bundleDigest: R_STALE_CAND },
    { ws: REVIEW_WS, skillId: REJECT_SKILL, commitId: R_REJECT_CUR, bundleDigest: R_REJECT_CUR },
    { ws: REVIEW_WS, skillId: REJECT_SKILL, commitId: R_REJECT_CAND, bundleDigest: R_REJECT_CAND },
    { ws: REVIEW_WS, skillId: SELF_SKILL, commitId: R_SELF_CUR, bundleDigest: R_SELF_CUR },
    { ws: REVIEW_WS, skillId: SELF_SKILL, commitId: R_SELF_CAND, bundleDigest: R_SELF_CAND },
    { ws: REVIEW_WS, skillId: REVERT_SKILL, commitId: R_REVERT_CUR, bundleDigest: R_REVERT_CUR },
    { ws: REVIEW_WS, skillId: REVERT_SKILL, commitId: R_REVERT_GOOD, bundleDigest: R_REVERT_GOOD },
    {
      ws: REVIEW_WS,
      skillId: REVERT_STALE_SKILL,
      commitId: R_REVERT_STALE_CUR,
      bundleDigest: R_REVERT_STALE_CUR,
    },
    {
      ws: REVIEW_WS,
      skillId: REVERT_STALE_SKILL,
      commitId: R_REVERT_STALE_GOOD,
      bundleDigest: R_REVERT_STALE_GOOD,
    },
  ],
  currents: [
    {
      ws: WS,
      skillId: SKILL,
      commitId: CURRENT_ID,
      epoch: CURRENT_GENERATION.epoch,
      seq: CURRENT_GENERATION.seq,
      updatedAtMs: SKILL_UPDATED_AT_MS,
    },
    {
      ws: WS,
      skillId: HERO_SKILL,
      commitId: HERO_CURRENT_ID,
      epoch: 1,
      seq: 1,
      updatedAtMs: HERO_UPDATED_AT_MS,
    },
    {
      ws: WS,
      skillId: NULLDIGEST_SKILL,
      commitId: NULLDIGEST_CURRENT_ID,
      epoch: 1,
      seq: 2,
      updatedAtMs: NULLDIGEST_UPDATED_AT_MS,
    },
    {
      ws: REVIEW_WS,
      skillId: APPROVE_SKILL,
      commitId: R_APPROVE_CUR,
      epoch: APPROVE_GENERATION.epoch,
      seq: APPROVE_GENERATION.seq,
      updatedAtMs: REVIEW_UPDATED_AT_MS,
    },
    {
      ws: REVIEW_WS,
      skillId: STALE_SKILL,
      commitId: R_STALE_CUR,
      epoch: STALE_GENERATION.epoch,
      seq: STALE_GENERATION.seq,
      updatedAtMs: REVIEW_UPDATED_AT_MS,
    },
    {
      ws: REVIEW_WS,
      skillId: REJECT_SKILL,
      commitId: R_REJECT_CUR,
      epoch: REJECT_GENERATION.epoch,
      seq: REJECT_GENERATION.seq,
      updatedAtMs: REVIEW_UPDATED_AT_MS,
    },
    {
      ws: REVIEW_WS,
      skillId: SELF_SKILL,
      commitId: R_SELF_CUR,
      epoch: SELF_GENERATION.epoch,
      seq: SELF_GENERATION.seq,
      updatedAtMs: REVIEW_UPDATED_AT_MS,
    },
    {
      ws: REVIEW_WS,
      skillId: REVERT_SKILL,
      commitId: R_REVERT_CUR,
      epoch: REVERT_GENERATION.epoch,
      seq: REVERT_GENERATION.seq,
      updatedAtMs: REVIEW_UPDATED_AT_MS,
    },
    {
      ws: REVIEW_WS,
      skillId: REVERT_STALE_SKILL,
      commitId: R_REVERT_STALE_CUR,
      epoch: REVERT_STALE_GENERATION.epoch,
      seq: REVERT_STALE_GENERATION.seq,
      updatedAtMs: REVIEW_UPDATED_AT_MS,
    },
  ],
  // Mirrors the vault's PROPOSALS with the STORED fields the count join reads. The MOVED row is the
  // DELIBERATELY STALE one (open, base (1,4) != current (1,5)): the index count must exclude it
  // exactly as the vault's own list does — the count-tripwire.
  proposals: [
    {
      ws: WS,
      id: "11111111-1111-4111-8111-111111111111",
      skillId: SKILL,
      commitId: CANDIDATE_ID,
      baseCommitId: CURRENT_ID,
      baseEpoch: CURRENT_GENERATION.epoch,
      baseSeq: CURRENT_GENERATION.seq,
      status: "open",
      proposer: "dev-bbbb2222",
      resolvedBy: null,
      createdAt: "2026-07-01T18:00:00Z",
    },
    {
      ws: WS,
      id: "22222222-2222-4222-8222-222222222222",
      skillId: SKILL,
      commitId: MOVED_ID,
      baseCommitId: GENESIS_ID,
      baseEpoch: 1,
      baseSeq: 4,
      status: "open",
      proposer: "dev-cccc3333",
      resolvedBy: null,
      createdAt: "2026-06-30T09:00:00Z",
    },
    {
      ws: WS,
      id: "33333333-3333-4333-8333-333333333333",
      skillId: SKILL,
      commitId: NOTOPEN_ID,
      baseCommitId: CURRENT_ID,
      baseEpoch: CURRENT_GENERATION.epoch,
      baseSeq: CURRENT_GENERATION.seq,
      status: "rejected",
      proposer: "dev-dddd4444",
      resolvedBy: "dev-aaaa1111",
      createdAt: "2026-06-29T09:00:00Z",
    },
  ],
};

// ---------------------------------------------------------------------------------------------
// The internal-lane read/write SCOPES, keyed ws → { members, reviewers, skills }. `members`
// mirrors the vault's posture (only a listed acting_email — a confirmed member — reads; every
// other miss is the uniform 404); `reviewers` gates the review/revert writes; the content bodies
// serve the metas/blobs keyed by (ws, skill). REBUILT per run via POST /__test/seed.
// ---------------------------------------------------------------------------------------------
export function initialScopes() {
  return {
    [WS]: {
      members: [DEFAULT_MEMBER_EMAIL, JOINER_EMAIL],
      reviewers: [],
      skills: {
        [SKILL]: {
          currentId: CURRENT_ID,
          generation: { ...CURRENT_GENERATION },
          updatedAtMs: SKILL_UPDATED_AT_MS,
          bundleDigest: "e3".repeat(32),
          metas: structuredClone(VERSION_METAS),
          proposals: structuredClone(PROPOSALS),
          proposalMeta: structuredClone(WS_PROPOSAL_DETAILS),
        },
        [HERO_SKILL]: {
          currentId: HERO_CURRENT_ID,
          generation: { epoch: 1, seq: 1 },
          updatedAtMs: HERO_UPDATED_AT_MS,
          bundleDigest: HERO_BUNDLE_DIGEST,
          metas: {},
          proposals: { proposals: [] },
          proposalMeta: {},
        },
      },
    },
    [REVIEW_WS]: {
      members: [REVIEWER_EMAIL],
      reviewers: [REVIEWER_EMAIL],
      skills: {
        [REVERT_SKILL]: revertSkillState({
          currentId: R_REVERT_CUR,
          goodId: R_REVERT_GOOD,
          generation: REVERT_GENERATION,
        }),
        [REVERT_STALE_SKILL]: revertSkillState({
          currentId: R_REVERT_STALE_CUR,
          goodId: R_REVERT_STALE_GOOD,
          generation: REVERT_STALE_GENERATION,
          alwaysConflict: true,
        }),
        [APPROVE_SKILL]: reviewSkillState({
          currentId: R_APPROVE_CUR,
          candidateId: R_APPROVE_CAND,
          generation: APPROVE_GENERATION,
          proposer: "dev-bbbb2222",
          reviewRequired: false,
          message: "Tighten the deploy steps\n\nAdds an explicit test step before shipping.",
        }),
        [STALE_SKILL]: reviewSkillState({
          currentId: R_STALE_CUR,
          candidateId: R_STALE_CAND,
          generation: STALE_GENERATION,
          proposer: "dev-bbbb2222",
          reviewRequired: false,
          alwaysConflict: true,
          message: "A proposal whose base will move mid-review",
        }),
        [REJECT_SKILL]: reviewSkillState({
          currentId: R_REJECT_CUR,
          candidateId: R_REJECT_CAND,
          generation: REJECT_GENERATION,
          proposer: "dev-bbbb2222",
          reviewRequired: false,
          message: "A change the reviewer will turn down",
        }),
        [SELF_SKILL]: reviewSkillState({
          currentId: R_SELF_CUR,
          candidateId: R_SELF_CAND,
          generation: SELF_GENERATION,
          proposer: REVIEWER_EMAIL,
          reviewRequired: true,
          message: "The reviewer's own proposal under review-required",
        }),
      },
    },
  };
}
