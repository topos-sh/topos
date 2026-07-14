/**
 * The fixture vault — a plain node http server replaying contract-shaped responses for the e2e
 * suite. It speaks the wire the app actually calls: the INTERNAL SESSION LANE
 * (`/internal/v1/...`, gated on `Authorization: Bearer <PLANE_INTERNAL_TOKEN>` and identified by
 * `X-Topos-Acting-Email`) plus two PUBLIC reads (`GET /v1/enroll/verify/{user_code}`,
 * `GET /i/{token}`). It mirrors the real vault's POSTURE — member-scoped reads answer 404 for
 * missing and unauthorized alike; the whole internal lane 404s without the configured internal
 * token — without any of its logic. State: it records every internal-lane WRITE for assertion via
 * `GET /__test/calls`, and its read/write scopes are (re)seeded via `POST /__test/seed`.
 */
import { createServer } from "node:http";
import {
  BINARY_MARKER,
  CAP_REASON,
  CAP_TRIGGER_NAME,
  CREATED_ADDRESS,
  CREATED_WS_ID,
  DENY_REASON,
  DENY_TRIGGER_NAME,
  ERROR_TRIGGER_NAME,
  initialScopes,
  OID_BIG,
  OID_BIN_NEW,
  OID_BIN_OLD,
  OID_DELETED,
  OID_FAIL,
  OID_MODE,
  OID_MOVE,
  OID_SAME,
  OID_SKILL_NEW,
  OID_SKILL_OLD,
  OID_XSS,
  RATE_LIMITED_CODE,
  REVERT_TARGET_DENIED_REASON,
  REVIEW_DENIED_REASON,
  VERIFY_CONTEXTS,
  XSS_CONTENT,
} from "./data.mjs";

const PORT = Number(process.env.PLANE_FIXTURE_PORT ?? "8791");
// The shared internal bearer the app injects on every internal-lane request. The real vault
// answers the whole lane 404 when its side is unset; the fixture mirrors that — an absent/wrong
// token makes /internal/ requests indistinguishable from an unrouted path.
const INTERNAL_TOKEN = process.env.PLANE_INTERNAL_TOKEN ?? "";

const text = (s) => Buffer.from(s, "utf8");
const BLOBS = {
  [OID_SKILL_OLD]: text("# Deploy runbook\n\nStep one: build.\nStep two: ship.\n"),
  [OID_SKILL_NEW]: text(
    "# Deploy runbook\n\nStep one: build.\nStep two: test.\nStep three: ship.\n",
  ),
  [OID_DELETED]: text("obsolete notes\n"),
  // The script blob backs the file browser's highlighted code view (the diff never fetches it —
  // scripts/deploy.sh is mode-only between the two seeded versions).
  [OID_MODE]: text('#!/bin/sh\nset -eu\necho "deploy"\n'),
  // The diff never fetches unchanged/moved bytes either, but the file BROWSER fetches every
  // manifest entry — and the real vault serves any object a version lists, so the fixture must
  // too (only OID_FAIL stays deliberately absent).
  [OID_SAME]: text(
    "# Guide\n\nHow the runbook is meant to be used:\n\n- read `SKILL.md` first\n- then run `scripts/deploy.sh`\n",
  ),
  [OID_MOVE]: text("Release notes template — fill in per release.\n"),
  [OID_BIN_OLD]: Buffer.concat([Buffer.from([0, 1, 2, 3]), text(`${BINARY_MARKER}_OLD`)]),
  [OID_BIN_NEW]: Buffer.concat([Buffer.from([0, 4, 5, 6]), text(`${BINARY_MARKER}_NEW`)]),
  // The ONE oversized blob: just past the 1 MiB per-blob cap.
  [OID_BIG]: Buffer.alloc(1024 * 1024 + 64, 0x78),
  [OID_XSS]: text(XSS_CONTENT),
  [OID_FAIL]: null, // listed in the candidate meta, but the bundle route 404s it
};

// Recorded internal-lane WRITE calls (the specs assert exact method / path / acting / body).
let calls = [];
// The read/write scopes (member-read content + review/revert mutable state). Rebuilt on seed.
let scopes = initialScopes();
// Same request_id ⇒ the stored success, byte-for-byte (the idempotent replay).
let replays = new Map();

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const HEX64_RE = /^[0-9a-f]{64}$/;
const SKILL_NAME_RE = /^[a-z0-9][a-z0-9-]*$/;
const A_LONG_TIME_AGO = "2026-07-01T00:00:00Z";

/** The acting identity the app derives from the session and sends verbatim on the internal lane.
 * Canonicalized ASCII-lowercase (matching the vault's principal fold). Absent = "". */
function actingEmail(req) {
  const header = req.headers["x-topos-acting-email"];
  return typeof header === "string" ? header.trim().toLowerCase() : "";
}

/** The internal lane requires the shared bearer; without it the whole lane is a uniform miss. */
function internalAuthed(req) {
  return INTERNAL_TOKEN !== "" && req.headers.authorization === `Bearer ${INTERNAL_TOKEN}`;
}

function json(res, status, body, headers = {}) {
  const buf = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    "content-type": "application/json",
    "content-length": buf.byteLength,
    ...headers,
  });
  res.end(buf);
}

/** The uniform not-found the read lane answers for missing and unauthorized alike. */
function notFound(res) {
  json(res, 404, { error: "not_found" });
}

function readBody(req, done) {
  let raw = "";
  req.on("data", (chunk) => {
    raw += chunk;
  });
  req.on("end", () => {
    let parsed = {};
    try {
      parsed = JSON.parse(raw || "{}");
    } catch {
      parsed = {};
    }
    done(parsed);
  });
}

/** keep == read (the vault's retention rule): a version's meta — and any blob its manifest lists
 * — is served only while the version is TRUNK-REACHABLE (current and its ancestry) or an OPEN
 * proposal whose base equals the live generation. A rejected/staled candidate's meta and bytes
 * 404: the vault reclaims them. */
function readableVersionIds(s) {
  const readable = new Set();
  const cursor = [s.currentId];
  while (cursor.length > 0) {
    const id = cursor.pop();
    if (readable.has(id)) continue;
    const meta = s.metas[id];
    if (meta === undefined) continue;
    readable.add(id);
    cursor.push(...(meta.parents ?? []));
  }
  for (const [id, meta] of Object.entries(s.proposalMeta ?? {})) {
    if (
      meta.status === "open" &&
      meta.base_generation.epoch === s.generation.epoch &&
      meta.base_generation.seq === s.generation.seq
    ) {
      readable.add(id);
    }
  }
  return readable;
}

// ── Wire serializers (the internal lane is byte-parity with the device /v1 wire shapes) ────────

function wireCurrent(ws, skill, s) {
  // The FROZEN nested pointer envelope (WireCurrentRecord): a versioned wrapper around the
  // (workspace, skill) scope and the `record` — the version id + its `(epoch, seq)` generation.
  // NO bundle_digest / created_at: the pointer names the version; the commit transitively pins the
  // bytes. Byte-parity with the device `/v1` current read.
  return {
    schema_version: 1,
    scope: { workspace_id: ws, skill_id: skill },
    record: {
      version_id: s.currentId,
      generation: { epoch: s.generation.epoch, seq: s.generation.seq },
    },
  };
}

function wireVersion(ws, skill, meta) {
  return {
    schema_version: 1,
    workspace_id: ws,
    skill_id: skill,
    version_id: meta.version_id,
    bundle_digest: meta.bundle_digest ?? null,
    author: meta.author,
    message: meta.message,
    created_at: A_LONG_TIME_AGO,
    parents: meta.parents ?? [],
    files: (meta.files ?? []).map((f) => ({
      path: f.path,
      mode: f.mode,
      size: BLOBS[f.object_id]?.byteLength ?? 0,
      object_id: f.object_id,
    })),
  };
}

function wireProposalList(s) {
  // The vault's list predicate (open ∧ base == current): a staled proposal vanishes.
  return {
    schema_version: 1,
    proposals: (s.proposals?.proposals ?? []).filter(
      (p) =>
        p.base_generation.epoch === s.generation.epoch &&
        p.base_generation.seq === s.generation.seq,
    ),
  };
}

function wireProposalDetail(versionId, meta) {
  return {
    version_id: versionId,
    status: meta.status,
    base_epoch: meta.base_generation.epoch,
    base_seq: meta.base_generation.seq,
    created_at: meta.created_at,
    proposer: meta.proposer,
    review_required: meta.review_required,
    resolved_by: meta.resolution?.resolved_by ?? null,
    resolved_reason: meta.resolution?.reason ?? null,
    resolved_at: meta.resolution?.resolved_at ?? null,
  };
}

/** Resolve the read/write scope for (ws, acting) — or answer the miss/deny and return undefined.
 * Every content read is member-gated: a non-member acting email is the uniform 404. */
function scopeFor(res, ws, acting) {
  const scope = scopes[ws];
  const extra = process.env.PLANE_FIXTURE_EXTRA_MEMBER;
  const admitted =
    scope !== undefined &&
    (scope.members.includes(acting) || (extra !== undefined && acting === extra));
  if (!admitted) {
    notFound(res);
    return undefined;
  }
  return scope;
}

const server = createServer((req, res) => {
  const url = new URL(req.url ?? "/", `http://127.0.0.1:${PORT}`);
  const path = decodeURIComponent(url.pathname);
  const method = req.method ?? "GET";

  // ── Test-only introspection / control (never gated on the internal token) ────────────────────
  if (method === "GET" && path === "/__test/calls") {
    return json(res, 200, calls);
  }
  if (method === "POST" && (path === "/__test/seed" || path === "/__test/reset")) {
    return readBody(req, (body) => {
      scopes = body?.scopes ? body.scopes : initialScopes();
      calls = [];
      replays = new Map();
      res.writeHead(204);
      res.end();
    });
  }

  // ── Public read: GET /v1/enroll/verify/{user_code} (rides bare, no internal token) ───────────
  const verify = path.match(/^\/v1\/enroll\/verify\/([^/]+)$/);
  if (method === "GET" && verify !== null) {
    if (verify[1] === RATE_LIMITED_CODE) {
      return json(
        res,
        429,
        { schema_version: 1, ok: false, error: { code: "rate_limited", retryable: true } },
        { "retry-after": "60" },
      );
    }
    const context = VERIFY_CONTEXTS[verify[1]];
    if (context === undefined) return notFound(res);
    return json(res, 200, context);
  }

  // ── Public read: GET /i/{token} — the one-time admin-claim passthrough, content-negotiated ───
  const claim = path.match(/^\/i\/([^/]+)$/);
  if (method === "GET" && claim !== null) {
    const accept = String(req.headers.accept ?? "");
    if (accept.includes("application/json")) {
      return json(
        res,
        200,
        { enrollment_method: "admin_claim", token: claim[1], offered_skills: [] },
        { "cache-control": "no-store", vary: "accept" },
      );
    }
    const doc = `# Add this device to your workspace\n\nRun this with your agent:\n\n    topos follow <origin>/i/${claim[1]}\n`;
    const buf = text(doc);
    res.writeHead(200, {
      "content-type": "text/plain; charset=utf-8",
      "content-length": buf.byteLength,
      "cache-control": "no-store",
      "x-robots-tag": "noindex",
      vary: "accept",
    });
    return res.end(buf);
  }

  // ── Everything under /internal/ requires the shared bearer, or it is a uniform miss ──────────
  if (path.startsWith("/internal/")) {
    if (!internalAuthed(req)) return notFound(res);
    return handleInternal(req, res, path, method);
  }

  return notFound(res);
});

/** The internal session lane. Reads are member-gated; writes record their exact payload BEFORE
 * answering, then replay the configured outcome. */
function handleInternal(req, res, path, method) {
  const acting = actingEmail(req);

  // ── Reads ──────────────────────────────────────────────────────────────────────────────────
  if (method === "GET") {
    // GET /internal/v1/workspaces/{ws}/skills/{skill}/current
    const mCurrent = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/current$/);
    if (mCurrent !== null) {
      const scope = scopeFor(res, mCurrent[1], acting);
      if (scope === undefined) return undefined;
      const s = scope.skills[mCurrent[2]];
      if (s === undefined) return notFound(res);
      return json(res, 200, wireCurrent(mCurrent[1], mCurrent[2], s));
    }

    // GET /internal/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}
    const mVersion = path.match(
      /^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/versions\/([^/]+)$/,
    );
    if (mVersion !== null) {
      const scope = scopeFor(res, mVersion[1], acting);
      if (scope === undefined) return undefined;
      const s = scope.skills[mVersion[2]];
      const meta = s?.metas[mVersion[3]];
      // keep == read: a reclaimed version (rejected/staled candidate) 404s even though the raw
      // fixture table still holds its meta — exactly the vault's retention.
      if (s === undefined || meta === undefined || !readableVersionIds(s).has(mVersion[3])) {
        return notFound(res);
      }
      return json(res, 200, wireVersion(mVersion[1], mVersion[2], meta));
    }

    // GET /internal/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}
    const mBundle = path.match(
      /^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/bundles\/([^/]+)$/,
    );
    if (mBundle !== null) {
      const scope = scopeFor(res, mBundle[1], acting);
      if (scope === undefined) return undefined;
      // keep == read on bytes too: a blob is served only while SOME readable version's manifest
      // lists it.
      const s = scope.skills[mBundle[2]];
      const listed =
        s !== undefined &&
        [...readableVersionIds(s)].some((id) =>
          (s.metas[id]?.files ?? []).some((f) => f.object_id === mBundle[3]),
        );
      const blob = listed ? BLOBS[mBundle[3]] : null;
      if (blob == null) return notFound(res);
      res.writeHead(200, {
        "content-type": "application/octet-stream",
        "content-length": blob.byteLength,
      });
      return res.end(blob);
    }

    // GET /internal/v1/workspaces/{ws}/skills/{skill}/proposals
    const mProposals = path.match(
      /^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/proposals$/,
    );
    if (mProposals !== null) {
      const scope = scopeFor(res, mProposals[1], acting);
      if (scope === undefined) return undefined;
      const s = scope.skills[mProposals[2]];
      if (s === undefined) return notFound(res);
      return json(res, 200, wireProposalList(s));
    }

    // GET /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}
    const mDetail = path.match(
      /^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/proposals\/([^/]+)$/,
    );
    if (mDetail !== null) {
      const scope = scopeFor(res, mDetail[1], acting);
      if (scope === undefined) return undefined;
      const meta = scope.skills[mDetail[2]]?.proposalMeta?.[mDetail[3]];
      if (meta === undefined) return notFound(res);
      return json(res, 200, wireProposalDetail(mDetail[3], meta));
    }

    return notFound(res);
  }

  // ── Writes (200-for-all-outcomes; every call recorded BEFORE it is answered) ──────────────────

  // POST /internal/v1/workspaces — create.
  if (method === "POST" && path === "/internal/v1/workspaces") {
    return readBody(req, (body) => {
      calls.push({ route: "create-workspace", method, path, acting, body });
      const name = body.display_name ?? body.name;
      if (name === CAP_TRIGGER_NAME) {
        return json(res, 200, { outcome: "denied", reason: CAP_REASON });
      }
      if (name === DENY_TRIGGER_NAME) {
        return json(res, 200, { outcome: "denied", reason: DENY_REASON });
      }
      return json(res, 200, {
        outcome: "created",
        workspace_id: CREATED_WS_ID,
        address: CREATED_ADDRESS,
      });
    });
  }

  // POST /internal/v1/device-sessions/{user_code}/approve — the enroll leg.
  const approve = path.match(/^\/internal\/v1\/device-sessions\/([^/]+)\/approve$/);
  if (method === "POST" && approve !== null) {
    return readBody(req, (body) => {
      calls.push({ route: "approve", method, path, key: approve[1], acting, body });
      const sessionIntent = VERIFY_CONTEXTS[approve[1]]?.intent;
      if (sessionIntent !== "enroll" && sessionIntent !== "login") {
        return json(res, 200, { outcome: "not_found" });
      }
      return json(res, 200, { outcome: "confirmed" });
    });
  }

  // POST /internal/v1/device-sessions/{user_code}/approve-standup — creates on approve.
  const approveStandup = path.match(/^\/internal\/v1\/device-sessions\/([^/]+)\/approve-standup$/);
  if (method === "POST" && approveStandup !== null) {
    return readBody(req, (body) => {
      calls.push({ route: "approve-standup", method, path, key: approveStandup[1], acting, body });
      if (VERIFY_CONTEXTS[approveStandup[1]]?.intent !== "standup") {
        return json(res, 200, { outcome: "not_found" });
      }
      const name = body.display_name ?? body.name;
      if (name === CAP_TRIGGER_NAME) {
        return json(res, 200, { outcome: "denied", reason: CAP_REASON });
      }
      if (name === ERROR_TRIGGER_NAME) {
        // A transient vault fault: the session stays live — the page must re-offer the approve.
        return json(res, 500, { ok: false, error: { code: "internal", retryable: true } });
      }
      return json(res, 200, {
        outcome: "approved",
        workspace_id: CREATED_WS_ID,
        display_name: name ?? "owner's workspace",
      });
    });
  }

  // POST /internal/v1/workspaces/{ws}/roster/remove — instant revoke.
  const removeSeat = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/roster\/remove$/);
  if (method === "POST" && removeSeat !== null) {
    return readBody(req, (body) => {
      calls.push({ route: "roster-remove", method, path, ws: removeSeat[1], acting, body });
      if (!UUID_RE.test(String(body.request_id ?? ""))) {
        return json(res, 200, { outcome: "denied", reason: "bad_request_id" });
      }
      if (typeof body.email !== "string" || body.email.length === 0) {
        return json(res, 200, { outcome: "denied", reason: "bad_email" });
      }
      return json(res, 200, { outcome: "removed" });
    });
  }

  // POST /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/{approve|reject}
  const reviewWrite = path.match(
    /^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/proposals\/([^/]+)\/(approve|reject)$/,
  );
  if (method === "POST" && reviewWrite !== null) {
    const [, ws, skill, versionId, verb] = reviewWrite;
    return readBody(req, (body) => {
      calls.push({ route: `review-${verb}`, method, path, ws, skill, versionId, acting, body });
      if (!UUID_RE.test(String(body.request_id ?? ""))) {
        return json(res, 200, { outcome: "denied", reason: "bad_request_id" });
      }
      const reason = verb === "reject" ? String(body.reason ?? "").trim() : "";
      if (verb === "reject" && (reason.length === 0 || reason.length > 2000)) {
        return json(res, 200, { outcome: "denied", reason: "bad_reason" });
      }
      const replay = replays.get(body.request_id);
      if (replay !== undefined) return json(res, 200, replay);

      const scope = scopes[ws];
      const s = scope?.skills?.[skill];
      const meta = s?.proposalMeta?.[versionId];
      if (
        scope === undefined ||
        !scope.members.includes(acting) ||
        s === undefined ||
        meta === undefined
      ) {
        return json(res, 200, { outcome: "not_found" });
      }
      const answer = (payload) => {
        if (payload.outcome === "approved" || payload.outcome === "rejected") {
          replays.set(body.request_id, payload);
        }
        return json(res, 200, payload);
      };
      if (!(scope.reviewers ?? []).includes(acting)) {
        return answer({ outcome: "denied", reason: REVIEW_DENIED_REASON.roleGate });
      }
      const now = new Date().toISOString();
      if (verb === "approve") {
        // The vault's order: the pointer CAS runs FIRST — an approve bound behind the live
        // generation is a conflict even when the proposal has since resolved.
        const boundMatches =
          body.expected_epoch === s.generation.epoch && body.expected_seq === s.generation.seq;
        if (s.alwaysConflict || !boundMatches) {
          if (s.alwaysConflict) {
            // The concurrent pointer move the conflict reports: the revalidated page must derive
            // `stale` (open proposal, base != current) and drop every form.
            s.generation = { epoch: s.generation.epoch, seq: s.generation.seq + 1 };
          }
          return answer({ outcome: "conflict" });
        }
        if (
          meta.status !== "open" ||
          meta.base_generation.epoch !== s.generation.epoch ||
          meta.base_generation.seq !== s.generation.seq
        ) {
          return answer({ outcome: "denied", reason: REVIEW_DENIED_REASON.notOpen });
        }
        if (meta.review_required && meta.proposer === acting) {
          return answer({ outcome: "denied", reason: REVIEW_DENIED_REASON.fourEyes });
        }
        meta.status = "accepted";
        meta.resolution = { resolved_by: acting, reason: null, resolved_at: now };
        s.currentId = versionId;
        s.generation = { epoch: s.generation.epoch, seq: s.generation.seq + 1 };
        s.updatedAtMs = Date.now();
        return answer({ outcome: "approved" });
      }
      // reject — no CAS arm: a mismatched base is the typed notOpen denial, never a conflict.
      if (meta.status === "accepted") {
        return answer({ outcome: "denied", reason: REVIEW_DENIED_REASON.alreadyAccepted });
      }
      if (meta.status === "rejected") {
        return answer({ outcome: "rejected" });
      }
      if (
        body.expected_epoch !== meta.base_generation.epoch ||
        body.expected_seq !== meta.base_generation.seq ||
        meta.base_generation.epoch !== s.generation.epoch ||
        meta.base_generation.seq !== s.generation.seq
      ) {
        return answer({ outcome: "denied", reason: REVIEW_DENIED_REASON.notOpen });
      }
      meta.status = "rejected";
      meta.resolution = { resolved_by: acting, reason, resolved_at: now };
      return answer({ outcome: "rejected" });
    });
  }

  // POST /internal/v1/workspaces/{ws}/skills/{skill}/reverts — roll current back to a good version.
  const revertWrite = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/reverts$/);
  if (method === "POST" && revertWrite !== null) {
    const [, ws, skill] = revertWrite;
    return readBody(req, (body) => {
      calls.push({
        route: "revert",
        method,
        path,
        ws,
        skill,
        good: body.good_version_id,
        acting,
        body,
      });
      if (!UUID_RE.test(String(body.request_id ?? ""))) {
        return json(res, 200, { outcome: "denied", reason: "bad_request_id" });
      }
      if (!HEX64_RE.test(String(body.good_version_id ?? ""))) {
        return json(res, 200, { outcome: "denied", reason: "bad_version_id" });
      }
      const replay = replays.get(body.request_id);
      if (replay !== undefined) return json(res, 200, replay);

      const scope = scopes[ws];
      const s = scope?.skills?.[skill];
      if (scope === undefined || !scope.members.includes(acting) || s === undefined) {
        return json(res, 200, { outcome: "not_found" });
      }
      const answer = (payload) => {
        if (payload.outcome === "reverted") replays.set(body.request_id, payload);
        return json(res, 200, payload);
      };
      if (!(scope.reviewers ?? []).includes(acting)) {
        return answer({ outcome: "denied", reason: REVIEW_DENIED_REASON.roleGate });
      }
      if (s.alwaysConflict) {
        // A concurrent pointer move: the conflict AND the bumped generation the reload rebinds to.
        s.generation = { epoch: s.generation.epoch, seq: s.generation.seq + 1 };
        return answer({ outcome: "conflict" });
      }
      if (
        body.good_version_id === s.currentId ||
        !readableVersionIds(s).has(body.good_version_id)
      ) {
        return answer({ outcome: "denied", reason: REVERT_TARGET_DENIED_REASON });
      }
      if (body.expected_epoch !== s.generation.epoch || body.expected_seq !== s.generation.seq) {
        return answer({ outcome: "conflict" });
      }
      // Reverted. The fixture deliberately leaves its pointer put — harness discipline (the DB-fed
      // catalog never tracks the vault's moves anyway), and a stable history keeps the mounted
      // control's success state assertable rather than racing an unmount against revalidation.
      return answer({ outcome: "reverted" });
    });
  }

  // ── Skill LIFECYCLE ceremonies (archive/unarchive/delete/purge/rename) ───────────────────────
  // Owner-gating lives in the WEB guard (the DB roster); the fixture models the confirmed-MEMBER
  // posture (a non-member acting is the uniform 200 {outcome:"not_found"}) plus the deterministic
  // per-op denial triggers the crib names. `{skill}` is the immutable skill_id (== name for seeded
  // skills). Every call is recorded BEFORE it is answered, keyed by route/ws/skill/acting/body.
  const lifecycleMember = (ws) => {
    const scope = scopes[ws];
    return scope?.members.includes(acting) ? scope : undefined;
  };

  const archive = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/archive$/);
  if (method === "POST" && archive !== null) {
    const [, ws, skill] = archive;
    return readBody(req, (body) => {
      calls.push({ route: "archive", method, path, ws, skill, acting, body });
      if (lifecycleMember(ws) === undefined) return json(res, 200, { outcome: "not_found" });
      // Archive renames to `<name>-archived-<date>`, freeing the base name.
      return json(res, 200, { outcome: "archived", archived_name: `${skill}-archived-2026-07-12` });
    });
  }

  const unarchive = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/unarchive$/);
  if (method === "POST" && unarchive !== null) {
    const [, ws, skill] = unarchive;
    return readBody(req, (body) => {
      calls.push({ route: "unarchive", method, path, ws, skill, acting, body });
      if (lifecycleMember(ws) === undefined) return json(res, 200, { outcome: "not_found" });
      // A skill id tagged `-name-taken` models the base name having been reused since archiving.
      if (skill.endsWith("-name-taken")) {
        return json(res, 200, { outcome: "denied", reason: "name_taken" });
      }
      return json(res, 200, { outcome: "unarchived", name: skill });
    });
  }

  const del = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/delete$/);
  if (method === "POST" && del !== null) {
    const [, ws, skill] = del;
    return readBody(req, (body) => {
      calls.push({ route: "delete", method, path, ws, skill, acting, body });
      if (lifecycleMember(ws) === undefined) return json(res, 200, { outcome: "not_found" });
      return json(res, 200, { outcome: "deleted" });
    });
  }

  const purge = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/purge$/);
  if (method === "POST" && purge !== null) {
    const [, ws, skill] = purge;
    return readBody(req, (body) => {
      calls.push({ route: "purge", method, path, ws, skill, acting, body });
      const scope = lifecycleMember(ws);
      if (scope === undefined) return json(res, 200, { outcome: "not_found" });
      const s = scope.skills?.[skill];
      // Purging the CURRENT version is refused; any other readable version drops its bytes.
      if (s !== undefined && body.version_id === s.currentId) {
        return json(res, 200, { outcome: "denied", reason: "is_current" });
      }
      return json(res, 200, { outcome: "purged" });
    });
  }

  const rename = path.match(/^\/internal\/v1\/workspaces\/([^/]+)\/skills\/([^/]+)\/rename$/);
  if (method === "POST" && rename !== null) {
    const [, ws, skill] = rename;
    return readBody(req, (body) => {
      calls.push({ route: "rename", method, path, ws, skill, acting, body });
      const scope = lifecycleMember(ws);
      if (scope === undefined) return json(res, 200, { outcome: "not_found" });
      const name = String(body.new_name ?? "");
      if (!SKILL_NAME_RE.test(name) || name.length > 64 || name.includes("-archived-")) {
        return json(res, 200, { outcome: "denied", reason: "bad_name" });
      }
      // A rename to a name a live skill already holds is refused.
      if (scope.skills?.[name] !== undefined) {
        return json(res, 200, { outcome: "denied", reason: "name_taken" });
      }
      return json(res, 200, { outcome: "renamed", name });
    });
  }

  return notFound(res);
}

server.listen(PORT, "127.0.0.1", () => {
  console.warn(`fixture vault listening on 127.0.0.1:${PORT}`);
});
