import { gunzipSync } from "node:zlib";
import { sql } from "drizzle-orm";
import { getDb } from "@/lib/db/index.server";
import { commitVersion } from "@/lib/plane/custody.server";

/**
 * UPSTREAM — the fork-that-remembers-its-parent half of the GitHub story, server-side.
 *
 * A bundle may carry ONE upstream (`web.bundle_upstream`: host + owner/repo + subdir). The
 * CHECKER fetches the repo's current tree (the public codeload tarball — no token, no API),
 * and when the subdir's bytes differ from what was last seen it imports them as a CANDIDATE
 * version and opens an ordinary PROPOSAL — external changes ALWAYS propose, even on an
 * unprotected bundle: members publish directly, the outside world never moves `current`.
 * The proposal is attributed to no user (a system act); a review-thread comment carries the
 * provenance (`repo@commit`) so the review UI narrates where the bytes came from.
 *
 * Polling: [`armUpstreamChecker`] starts ONE process-wide interval (default hourly;
 * `TOPOS_UPSTREAM_CHECK_MS` tunes it, `0` disables) sweeping every upstream-carrying bundle;
 * the skill page's "Check for updates" arm runs [`checkBundleUpstream`] on demand.
 */

// ── The minimal tar reader (regular files only, path-safe) ──────────────────────────────────

interface TarFile {
  path: string;
  mode: number;
  bytes: Buffer;
}

/** The archive ceilings — a public repo is UNTRUSTED input, so every dimension is bounded. */
const MAX_ARCHIVE_FILES = 2000;

/** Read a POSIX/pax tarball's REGULAR files + the pax global `comment` (codeload stamps the
 * commit sha there). STRICT on structure — an invalid header checksum, a malformed size, or a
 * truncated body THROWS (a damaged archive must never import as partial content) — while
 * unsafe ENTRIES (`..` segments, absolute paths, links, devices) are skipped: the import wants
 * plain files, never a filesystem side effect. */
export function untar(tar: Buffer): { files: TarFile[]; comment: string | null } {
  const files: TarFile[] = [];
  let comment: string | null = null;
  let offset = 0;
  let paxPath: string | null = null;
  let sawEnd = false;
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((b) => b === 0)) {
      sawEnd = true; // the end-of-archive zero blocks
      break;
    }
    // The header checksum: the stored field read as spaces, summed bytewise. A mismatch is a
    // damaged or forged archive — refuse whole, never a partial import.
    const stored = parseOctal(cstr(header.subarray(148, 156)), "checksum");
    let sum = 0;
    for (let i = 0; i < 512; i++) {
      sum += i >= 148 && i < 156 ? 0x20 : (header[i] ?? 0);
    }
    if (stored !== sum) {
      throw new Error("malformed archive: header checksum mismatch");
    }
    const name = cstr(header.subarray(0, 100));
    const prefix = cstr(header.subarray(345, 500));
    const modeField = cstr(header.subarray(100, 108));
    const mode = modeField.length === 0 ? 0o644 : parseOctal(modeField, "mode");
    const sizeField = cstr(header.subarray(124, 136));
    const size = sizeField.length === 0 ? 0 : parseOctal(sizeField, "size");
    const typeflag = String.fromCharCode(header[156] ?? 0x30);
    if (offset + 512 + size > tar.length) {
      throw new Error("malformed archive: truncated entry body");
    }
    const body = tar.subarray(offset + 512, offset + 512 + size);
    offset += 512 + Math.ceil(size / 512) * 512;

    if (typeflag === "g" || typeflag === "x") {
      // pax bodies are LENGTH-FRAMED records: `<len> <key>=<value>\n` where <len> counts the
      // WHOLE record (digits, space, newline). Walk the frames from BYTES — anything
      // off-frame (no length prefix, a length that lies, a record without `=` or the trailing
      // newline) refuses whole rather than misparsing. The global header carries codeload's
      // commit comment; an extended header may carry a long `path` for the NEXT entry.
      let pos = 0;
      while (pos < body.length) {
        const space = body.indexOf(0x20, pos);
        const lenToken = space > pos ? body.subarray(pos, space).toString("utf8") : "";
        const len = /^\d+$/.test(lenToken) ? Number.parseInt(lenToken, 10) : Number.NaN;
        if (!Number.isFinite(len) || len <= space - pos + 1 || pos + len > body.length) {
          throw new Error("malformed archive: bad pax record");
        }
        const record = body.subarray(space + 1, pos + len).toString("utf8");
        const eq = record.indexOf("=");
        if (!record.endsWith("\n") || eq < 1) {
          throw new Error("malformed archive: bad pax record");
        }
        const key = record.slice(0, eq);
        const value = record.slice(eq + 1, -1);
        if (typeflag === "g" && key === "comment") {
          comment = value;
        }
        if (typeflag === "x" && key === "path") {
          paxPath = value;
        }
        pos += len;
      }
      continue;
    }
    const rawPath = paxPath ?? (prefix.length > 0 ? `${prefix}/${name}` : name);
    paxPath = null;
    if (typeflag !== "0" && typeflag !== "\0") {
      continue; // links, dirs, devices — never imported
    }
    const clean = rawPath.replaceAll("\\", "/");
    if (
      clean.length === 0 ||
      clean.startsWith("/") ||
      clean.split("/").some((seg) => seg === ".." || seg.length === 0)
    ) {
      continue; // unsafe or degenerate — skipped, never trusted
    }
    files.push({ path: clean, mode, bytes: Buffer.from(body) });
    if (files.length > MAX_ARCHIVE_FILES) {
      throw new Error("archive holds too many files");
    }
  }
  if (!sawEnd) {
    // The archive ran out without the zero-block end marker — a truncated download must never
    // import as the files that happened to arrive.
    throw new Error("malformed archive: missing end-of-archive marker");
  }
  return { files, comment };
}

/** Strict octal field parse — junk after (or inside) the digits refuses, never truncates. */
function parseOctal(raw: string, what: string): number {
  const token = raw.trim();
  if (!/^[0-7]+$/.test(token)) {
    throw new Error(`malformed archive: bad ${what} field`);
  }
  return Number.parseInt(token, 8);
}

function cstr(b: Buffer | Uint8Array): string {
  const buf = Buffer.from(b);
  const end = buf.indexOf(0);
  return buf
    .subarray(0, end < 0 ? buf.length : end)
    .toString("utf8")
    .trim();
}

// ── The GitHub tree fetch (public tarball; no token, no API) ────────────────────────────────

export interface UpstreamTree {
  /** The commit the tarball snapshots (codeload's pax comment), or null when unstamped. */
  commit: string | null;
  /** The subdir's files, paths relative to the SKILL root (the subdir stripped). */
  files: { path: string; executable: boolean; bytes: Buffer }[];
  /** A LICENSE file's leading identifier line, from the skill root or the repo root. */
  license: string | null;
}

/** The injectable fetcher seam — tests feed a fixture tarball, production dials codeload. */
export type TarballFetcher = (repo: string, ref: string) => Promise<Buffer>;

const MAX_TARBALL_BYTES = 32 * 1024 * 1024;
/** The DECOMPRESSED ceiling — a small, highly-compressible archive must not inflate without
 * bound (`gunzipSync` enforces it via `maxOutputLength`, throwing past it). */
const MAX_UNPACKED_BYTES = 128 * 1024 * 1024;

async function fetchCodeload(repo: string, ref: string): Promise<Buffer> {
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo)) {
    throw new Error("malformed repo");
  }
  const response = await fetch(
    `https://codeload.github.com/${repo}/tar.gz/${encodeURIComponent(ref)}`,
    { redirect: "follow", signal: AbortSignal.timeout(30_000) },
  );
  if (!response.ok) {
    throw new Error(`upstream fetch failed: ${response.status}`);
  }
  if (response.body === null) {
    throw new Error("upstream fetch failed: empty body");
  }
  // STREAM with a running cap — never buffer an unbounded body before checking its size.
  const chunks: Buffer[] = [];
  let total = 0;
  const reader = response.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    total += value.byteLength;
    if (total > MAX_TARBALL_BYTES) {
      await reader.cancel();
      throw new Error("upstream tarball too large");
    }
    chunks.push(Buffer.from(value));
  }
  return Buffer.concat(chunks);
}

/**
 * Fetch `owner/repo`'s tree at `ref` (default `HEAD` — the default branch) and slice the
 * skill's `subdir` ("" = the repo root, minus the tarball's own top-level folder).
 */
export async function fetchUpstreamTree(
  repo: string,
  subdir: string,
  ref = "HEAD",
  fetcher: TarballFetcher = fetchCodeload,
): Promise<UpstreamTree> {
  const gz = await fetcher(repo, ref);
  const { files, comment } = untar(gunzipSync(gz, { maxOutputLength: MAX_UNPACKED_BYTES }));
  // codeload prefixes every path with `<repo>-<ref-ish>/` — strip the ONE top segment.
  const stripped = files
    .map((f) => {
      const slash = f.path.indexOf("/");
      return slash < 0 ? null : { ...f, path: f.path.slice(slash + 1) };
    })
    .filter((f): f is TarFile => f !== null && f.path.length > 0);
  const want = subdir.length > 0 ? `${subdir.replace(/\/+$/, "")}/` : "";
  const inSubdir = stripped
    .filter((f) => f.path.startsWith(want))
    .map((f) => ({
      path: f.path.slice(want.length),
      executable: (f.mode & 0o111) !== 0,
      bytes: f.bytes,
    }))
    .filter((f) => f.path.length > 0);
  const license =
    licenseOf(inSubdir.map((f) => ({ path: f.path, bytes: f.bytes }))) ??
    licenseOf(stripped.map((f) => ({ path: f.path, bytes: f.bytes })));
  return { commit: comment, files: inSubdir, license };
}

/**
 * Resolve a pasted `/tree/<...>` remainder against the LIVE repo. The text alone is
 * ambiguous — branch names may contain `/` — but git forbids a ref named both `a` and
 * `a/b`, so probing shortest-prefix-first is unambiguous: the first prefix codeload serves
 * IS the ref, and the rest is the subdir. Non-404 failures surface immediately.
 */
export async function resolveTreeSource(
  repo: string,
  rest: string[],
  fetcher: TarballFetcher = fetchCodeload,
): Promise<{ tree: UpstreamTree; ref: string; subdir: string }> {
  let lastError: unknown = null;
  for (let i = 1; i <= rest.length; i++) {
    const ref = rest.slice(0, i).join("/");
    const subdir = rest.slice(i).join("/");
    try {
      return { tree: await fetchUpstreamTree(repo, subdir, ref, fetcher), ref, subdir };
    } catch (error) {
      if (!(error instanceof Error && error.message.includes("404"))) {
        throw error;
      }
      lastError = error;
    }
  }
  throw lastError instanceof Error ? lastError : new Error("upstream fetch failed");
}

function licenseOf(files: { path: string; bytes: Buffer }[]): string | null {
  const hit = files.find((f) => /^licen[cs]e(\.(md|txt))?$/i.test(f.path));
  if (hit === undefined) {
    return null;
  }
  const first = hit.bytes.toString("utf8").split("\n", 1)[0]?.trim() ?? "";
  return first.length > 0 ? first.slice(0, 120) : "present";
}

// ── The checker: compare, import, PROPOSE ───────────────────────────────────────────────────

export type UpstreamCheckOutcome =
  | { outcome: "no_upstream" }
  | { outcome: "unchanged"; commit: string | null }
  | { outcome: "already_current"; commit: string | null }
  | { outcome: "proposed"; commit: string | null; versionId: string }
  | { outcome: "error"; message: string };

/**
 * Check ONE bundle's upstream and open a proposal when it moved. External changes ALWAYS
 * propose (never a direct publish): the candidate is committed to the vault (rehash-verified
 * there), a proposal row opens attributed to NO user (a system act), a review comment carries
 * the `repo@commit` provenance, and `version_upstream` records which commit the candidate's
 * bytes came from. Idempotent: an unchanged upstream just stamps `last_checked_at`; a
 * re-check of the same moved commit converges on the one open proposal (the partial unique).
 */
export async function checkBundleUpstream(
  workspaceId: string,
  bundleId: string,
  fetcher: TarballFetcher = fetchCodeload,
): Promise<UpstreamCheckOutcome> {
  const db = getDb();
  // WORKSPACE-BOUND: the caller's authorization covered ONE workspace, so the lookup must
  // never resolve a bundle id from another one (a cross-workspace check would write proposals
  // where the caller holds no seat).
  const rows = await db.execute(sql`
    SELECT bu.workspace_id, bu.repo, bu.path, bu.last_seen_commit,
           cp.version_id AS current_version_id
    FROM web.bundle_upstream bu
    JOIN web.bundle b ON b.id = bu.bundle_id AND b.status = 'active'
    LEFT JOIN plane.current_pointer cp
      ON cp.workspace_id = bu.workspace_id AND cp.bundle_id = bu.bundle_id
    WHERE bu.bundle_id = ${bundleId} AND bu.workspace_id = ${workspaceId}
  `);
  const row = rows.rows[0] as
    | {
        workspace_id: string;
        repo: string;
        path: string;
        last_seen_commit: string | null;
        current_version_id: string | null;
      }
    | undefined;
  if (!row) {
    return { outcome: "no_upstream" };
  }
  let tree: UpstreamTree;
  try {
    tree = await fetchUpstreamTree(row.repo, row.path, "HEAD", fetcher);
  } catch (error) {
    return { outcome: "error", message: error instanceof Error ? error.message : "fetch failed" };
  }
  if (tree.files.length === 0) {
    return { outcome: "error", message: "upstream tree is empty at the recorded path" };
  }
  if (row.current_version_id === null) {
    return { outcome: "error", message: "the bundle has no published current to propose against" };
  }
  const stamp = async () => {
    await db.execute(sql`
      UPDATE web.bundle_upstream
      SET last_checked_at = now(), last_seen_commit = ${tree.commit}
      WHERE bundle_id = ${bundleId}
    `);
  };
  if (tree.commit !== null && tree.commit === row.last_seen_commit) {
    await stamp();
    return { outcome: "unchanged", commit: tree.commit };
  }

  // Import as a CANDIDATE (commit-only — `current` never moves from here). The vault rehashes;
  // the candidate's id is content-addressed, so byte-identical bytes converge on one version.
  const committed = await commitVersion(row.workspace_id, bundleId, {
    files: tree.files.map((f) => ({
      path: f.path,
      mode: f.executable ? "100755" : "100644",
      content_base64: f.bytes.toString("base64"),
    })),
    // The candidate parents on the CURRENT version, so the review diff reads as "what changes".
    parent: row.current_version_id,
    attribution: "upstream",
    message:
      tree.commit === null
        ? `upstream import: ${row.repo}`
        : `upstream import: ${row.repo}@${tree.commit.slice(0, 12)}`,
  });
  if (committed.kind !== "ok") {
    return {
      outcome: "error",
      message: committed.kind === "rejected" ? (committed.message ?? "rejected") : "vault fault",
    };
  }
  const versionId = committed.value.version_id;
  if (versionId === row.current_version_id) {
    // The upstream matches what the workspace already ships — nothing to review.
    await stamp();
    return { outcome: "already_current", commit: tree.commit };
  }

  await db.transaction(async (tx) => {
    // A SYSTEM act: no user id (proposed_by stays NULL); the ON CONFLICT partial unique
    // converges a re-check of the same commit on the one open proposal.
    const proposed = await tx.execute(sql`
      INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, status)
      VALUES (${`p_${crypto.randomUUID().replaceAll("-", "")}`}, ${row.workspace_id},
              ${bundleId}, ${versionId}, 'open')
      ON CONFLICT (workspace_id, bundle_id, candidate_version_id) WHERE status = 'open'
      DO NOTHING
      RETURNING id
    `);
    await tx.execute(sql`
      INSERT INTO web.version_upstream (workspace_id, bundle_id, version_id, commit)
      VALUES (${row.workspace_id}, ${bundleId}, ${versionId}, ${tree.commit ?? ""})
      ON CONFLICT (bundle_id, version_id) DO NOTHING
    `);
    // The provenance narration the review thread shows. The id is DERIVED (in Postgres —
    // hashing lives DB-side here) from (workspace, bundle, candidate): content-addressed
    // version ids repeat across workspaces, so the scope must be in the key — and two racing
    // checks of the same commit converge on ONE comment via the PK conflict, never a
    // duplicate thread line.
    await tx.execute(sql`
      INSERT INTO web.proposal_comment
        (id, workspace_id, bundle_id, version_id, author_display, body)
      VALUES (
        substr(encode(sha256(convert_to(
          ${`${row.workspace_id}\n${bundleId}\n${versionId}`}, 'UTF8')), 'hex'), 1, 32)::uuid,
        ${row.workspace_id}, ${bundleId}, ${versionId},
        'upstream watcher',
        ${`Imported from ${row.repo}${row.path.length > 0 ? `/${row.path}` : ""}${tree.commit === null ? "" : ` @ ${tree.commit.slice(0, 12)}`} — review before it ships.`})
      ON CONFLICT (id) DO NOTHING
    `);
    // The audit row rides the proposal's own insert: a converging duplicate check (a manual
    // check racing the sweep) lands NO second proposal, so it writes no second audit line.
    if (proposed.rows.length > 0) {
      await tx.execute(sql`
        INSERT INTO web.audit_event (workspace_id, actor_display, kind, subject, outcome, details)
        VALUES (${row.workspace_id}, 'upstream watcher', 'upstream_proposal', ${bundleId}, 'ok',
                ${JSON.stringify({ repo: row.repo, commit: tree.commit, versionId })}::jsonb)
      `);
    }
  });
  await stamp();
  return { outcome: "proposed", commit: tree.commit, versionId };
}

/** The upstream facts one skill page shows. */
export interface UpstreamView {
  repo: string;
  path: string;
  license: string | null;
  lastCheckedAt: Date | null;
  lastSeenCommit: string | null;
  /** The commit the CURRENT version's bytes came from, when recorded (null = locally edited
   * since the last import — divergence, readable from the history itself). */
  currentCommit: string | null;
}

export async function upstreamOf(
  workspaceId: string,
  bundleId: string,
): Promise<UpstreamView | null> {
  const rows = await getDb().execute(sql`
    SELECT bu.repo, bu.path, bu.license, bu.last_checked_at, bu.last_seen_commit,
           vu.commit AS current_commit
    FROM web.bundle_upstream bu
    LEFT JOIN plane.current_pointer cp
      ON cp.workspace_id = bu.workspace_id AND cp.bundle_id = bu.bundle_id
    LEFT JOIN web.version_upstream vu
      ON vu.bundle_id = bu.bundle_id AND vu.version_id = cp.version_id
    WHERE bu.bundle_id = ${bundleId} AND bu.workspace_id = ${workspaceId}
  `);
  const row = rows.rows[0] as
    | {
        repo: string;
        path: string;
        license: string | null;
        last_checked_at: string | null;
        last_seen_commit: string | null;
        current_commit: string | null;
      }
    | undefined;
  if (!row) {
    return null;
  }
  return {
    repo: row.repo,
    path: row.path,
    license: row.license,
    lastCheckedAt: row.last_checked_at === null ? null : new Date(row.last_checked_at),
    lastSeenCommit: row.last_seen_commit,
    currentCommit:
      row.current_commit === null || row.current_commit === "" ? null : row.current_commit,
  };
}

// ── The poller ──────────────────────────────────────────────────────────────────────────────

let checkerArmed = false;

/**
 * Arm the process-wide upstream sweep ONCE: every interval, check each upstream-carrying
 * active bundle (oldest-checked first, capped per tick so a large catalog spreads out).
 * Default hourly; `TOPOS_UPSTREAM_CHECK_MS` tunes, `0` disables. Failures are per-bundle and
 * silent-but-audited by the checker itself — the sweep never throws.
 */
export function armUpstreamChecker(): void {
  if (checkerArmed) {
    return;
  }
  checkerArmed = true;
  const raw = process.env.TOPOS_UPSTREAM_CHECK_MS;
  const interval = raw === undefined ? 3_600_000 : Number(raw);
  if (!Number.isFinite(interval) || interval <= 0) {
    return;
  }
  const timer = setInterval(async () => {
    try {
      // The CLAIM: stamp last_checked_at atomically, ONE row at a time, immediately before
      // its check — the stamp-to-check window stays a single fetch (~30 s ceiling), never a
      // whole batch, so a second poller instance can't reclaim rows still being processed
      // inside the 5-minute guard. SKIP LOCKED keeps two ticks off the same row entirely.
      for (let n = 0; n < 20; n++) {
        const rows = await getDb().execute(sql`
          UPDATE web.bundle_upstream bu SET last_checked_at = now()
          WHERE (bu.workspace_id, bu.bundle_id) = (
            SELECT bu2.workspace_id, bu2.bundle_id FROM web.bundle_upstream bu2
            JOIN web.bundle b ON b.id = bu2.bundle_id AND b.status = 'active'
            WHERE bu2.last_checked_at IS NULL
               OR bu2.last_checked_at < now() - interval '5 minutes'
            ORDER BY bu2.last_checked_at NULLS FIRST
            LIMIT 1
            FOR UPDATE OF bu2 SKIP LOCKED
          )
          RETURNING bu.workspace_id, bu.bundle_id
        `);
        const row = rows.rows[0] as { workspace_id: string; bundle_id: string } | undefined;
        if (row === undefined) {
          break; // nothing due — the tick is done
        }
        await checkBundleUpstream(row.workspace_id, row.bundle_id);
      }
    } catch {
      // The sweep is best-effort; the next tick retries.
    }
  }, interval);
  timer.unref?.();
}
