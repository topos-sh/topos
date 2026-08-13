import type { ActionFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { type ReportedHarnessState, reportApplied } from "@/lib/db/queries.lane.server";

/**
 * `PUT /api/v1/workspaces/{ws}/report` — the session's post-reconcile applied snapshot
 * (`WireAppliedReport`) → 204. A report is the COMPLETE per-workspace snapshot of what the
 * installation holds (absence is meaningful), workspace-membership-checked per bundle.
 * Body-capped; a malformed body is a 400 BEFORE the credential resolve.
 */
const HEX_64 = /^[0-9a-f]{64}$/;
// The vault's own id rule, mirrored: lowercase path-safe charset, 1–128 bytes.
const SKILL_ID = /^[a-z0-9_-]{1,128}$/;
// The harness registry's slug spelling (`claude-code`, `cursor`) and the applied-state word.
// The state VOCABULARY is open — the client's word, kept verbatim — so only its shape is
// checked; a reader that meets an unknown state ignores it.
const HARNESS_SLUG = /^[a-z0-9-]{1,64}$/;
const HARNESS_STATE = /^[a-z-]{1,32}$/;
/**
 * More harnesses than any one installation runs — the shape cap on a client-asserted list. The
 * harness table itself grows (16 MCP-capable rows today), so the bound is sized to stay an abuse
 * fence, never a ceiling a full fleet report can reach.
 */
const MAX_HARNESSES = 64;
const MAX_NOTE = 200;

const MIB = 1024 * 1024;
/**
 * The worst JSON bytes ONE harness block can weigh: a slug and a state at the full length their
 * shapes allow (64 + 32), a note at its cap in the encoding that costs the most — every code unit
 * escaped `\uXXXX`, six bytes each — and the keys, quotes and commas around them.
 */
const WORST_HARNESS_BYTES = 64 + 32 + MAX_NOTE * 6 + 40;
/** The worst ONE applied row can weigh: a full-length id, a version id, its keys, and the blocks. */
const WORST_ROW_BYTES = 128 + 64 + 60 + MAX_HARNESSES * WORST_HARNESS_BYTES;
/** More bundles than one machine follows in one workspace — the breadth the cap is sized for. */
const FULL_BREADTH_ROWS = 128;
/**
 * The cap on the whole body, DERIVED from the shape rules above rather than guessed: 128 rows of
 * ~84 KiB is ~10.5 MiB, and the cap is the next whole mebibyte past it (12 MiB today).
 *
 * It is sized this way because a report lands ATOMICALLY — there is no partial one — so a cap a
 * legal report can reach is a fleet page that goes stale exactly when something is wrong with the
 * machine, and for as long as the condition persists. A real report is two orders of magnitude
 * under this (16 MCP-capable harness rows, most of them carrying no note at all). The fence is for
 * abuse, and it costs nothing to hold high: `readCappedBody` abandons the read mid-stream, so
 * nothing past the cap is ever buffered.
 */
const BODY_CAP = (Math.ceil((WORST_ROW_BYTES * FULL_BREADTH_ROWS) / MIB) + 1) * MIB;

/**
 * A note is DISPLAY TEXT, so an over-long one is trimmed rather than refused. The cut is taken on
 * a code-point boundary: slicing between a surrogate pair would leave a lone code unit, which is
 * not representable in jsonb and would turn a long note into a 500 further down.
 */
function cappedNote(note: string): string {
  if (note.length <= MAX_NOTE) {
    return note;
  }
  const cut = note.slice(0, MAX_NOTE);
  const last = cut.charCodeAt(MAX_NOTE - 1);
  return last >= 0xd800 && last <= 0xdbff ? cut.slice(0, -1) : cut;
}

/**
 * The optional per-harness block a config-placed ('mcp') bundle's row carries. Absent or empty
 * ⇒ null (a file bundle's one applied version says everything). This route refuses a malformed
 * SHAPE rather than trimming it: the report is the fleet's evidence, so a garbled row must be
 * fixed at the client, not silently half-stored.
 *
 * A note's LENGTH is the one exception, and deliberately so. A report is the session's whole
 * per-workspace snapshot, so refusing the PUT over one long note — a deep path plus an OS error
 * is all it takes, and the client cannot know it was too long — would knock the machine out of
 * the fleet picture entirely, for as long as the condition it is reporting persists. The note is
 * capped instead: the snapshot lands, and the note says as much of its story as it fits.
 */
function parseHarnesses(raw: unknown): ReportedHarnessState[] | null | string {
  if (raw === undefined || raw === null) {
    return null;
  }
  if (!Array.isArray(raw) || raw.length > MAX_HARNESSES) {
    return "malformed report entry: harnesses";
  }
  const states: ReportedHarnessState[] = [];
  for (const entry of raw as unknown[]) {
    const h = entry as { slug?: unknown; state?: unknown; note?: unknown };
    if (typeof h !== "object" || h === null) {
      return "malformed report entry: harnesses";
    }
    if (typeof h.slug !== "string" || !HARNESS_SLUG.test(h.slug)) {
      return "malformed report entry: harness slug";
    }
    if (typeof h.state !== "string" || !HARNESS_STATE.test(h.state)) {
      return "malformed report entry: harness state";
    }
    if (h.note !== undefined && h.note !== null && typeof h.note !== "string") {
      return "malformed report entry: harness note";
    }
    states.push({
      slug: h.slug,
      state: h.state,
      ...(typeof h.note === "string" ? { note: cappedNote(h.note) } : {}),
    });
  }
  return states.length === 0 ? null : states;
}

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  if (request.method !== "PUT") {
    return uniformNotFound();
  }
  const body = await readCappedBody(request, BODY_CAP, "report body");
  if (body instanceof Response) {
    return body;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return badRequest("malformed JSON body");
  }
  const report = parsed as { schema_version?: unknown; applied?: unknown };
  if (typeof parsed !== "object" || parsed === null || !Array.isArray(report.applied)) {
    return badRequest("malformed report body");
  }
  if (report.schema_version !== 1) {
    return badRequest("malformed report body: schema_version");
  }
  const applied: {
    skillId: string;
    versionId: string;
    harnesses: ReportedHarnessState[] | null;
  }[] = [];
  for (const entry of report.applied as unknown[]) {
    const row = entry as { skill_id?: unknown; version_id?: unknown; harnesses?: unknown };
    if (typeof row.skill_id !== "string" || !SKILL_ID.test(row.skill_id)) {
      return badRequest("malformed report entry: skill_id");
    }
    if (typeof row.version_id !== "string" || !HEX_64.test(row.version_id)) {
      return badRequest("malformed report entry: version_id must be 64-char lowercase hex");
    }
    const harnesses = parseHarnesses(row.harnesses);
    if (typeof harnesses === "string") {
      return badRequest(harnesses);
    }
    applied.push({ skillId: row.skill_id, versionId: row.version_id, harnesses });
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const outcome = await reportApplied(actor, applied);
  if (outcome === "session_ended") {
    // The session ended between the guard and the write (a revocation won the race) — the
    // same uniform miss the guard itself would have answered a moment later.
    return uniformNotFound();
  }
  return new Response(null, { status: 204 });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
