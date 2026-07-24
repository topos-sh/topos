import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { gzipSync } from "node:zlib";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The upstream module: the minimal tar reader (path-safe, pax-aware), the tree slice, and the
 * checker's ALWAYS-PROPOSE discipline — an upstream change becomes a candidate + an OPEN
 * proposal attributed to no user, never a direct publish; an unchanged upstream just stamps.
 * The tarball is SYNTHESIZED in-test (codeload's shape: a pax global comment carrying the
 * commit, one top-level folder prefixing every path); the vault is a stub HTTP server.
 */

// ── A tiny tar writer (the test's fixture builder — mirrors what codeload emits) ────────────

function tarEntry(path: string, bytes: Buffer, typeflag = "0", mode = 0o644): Buffer {
  const header = Buffer.alloc(512);
  header.write(path, 0, 100, "utf8");
  header.write(`${mode.toString(8).padStart(7, "0")}\0`, 100);
  header.write("0000000\0", 108);
  header.write("0000000\0", 116);
  header.write(`${bytes.length.toString(8).padStart(11, "0")}\0`, 124);
  header.write("00000000000\0", 136);
  header.write("        ", 148); // checksum spaces while summing
  header.write(typeflag, 156);
  header.write("ustar", 257);
  header.write("00", 263);
  let sum = 0;
  for (const b of header) {
    sum += b;
  }
  header.write(`${sum.toString(8).padStart(6, "0")}\0 `, 148);
  const body = Buffer.alloc(Math.ceil(bytes.length / 512) * 512);
  bytes.copy(body);
  return Buffer.concat([header, body]);
}

function paxGlobal(comment: string): Buffer {
  const record = `comment=${comment}\n`;
  const line = `${record.length + 3} ${record}`;
  return tarEntry("pax_global_header", Buffer.from(line), "g");
}

function fixtureTarball(commit: string, files: Record<string, string>): Buffer {
  const parts = [paxGlobal(commit)];
  for (const [path, content] of Object.entries(files)) {
    parts.push(tarEntry(`repo-${commit.slice(0, 7)}/${path}`, Buffer.from(content)));
  }
  parts.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(parts));
}

// ── The scratch DB + stub vault ─────────────────────────────────────────────────────────────

let db: ScratchDb;
let wsId = "";
let stub: Server;
/** The stub vault mints deterministic version ids from the candidate's SKILL.md bytes. */
const committedBodies: string[] = [];

beforeAll(async () => {
  stub = createServer((request, response) => {
    if (request.method === "POST" && /\/versions$/.test(request.url ?? "")) {
      let raw = "";
      request.on("data", (c) => {
        raw += c;
      });
      request.on("end", () => {
        committedBodies.push(raw);
        const body = JSON.parse(raw) as { files: { content_base64: string }[] };
        const seed = Buffer.from(body.files[0]?.content_base64 ?? "", "base64").toString("utf8");
        // A stable fake id derived from the content, hex-shaped.
        const id = Buffer.from(seed).toString("hex").padEnd(64, "0").slice(0, 64);
        response.writeHead(200, { "content-type": "application/json" });
        response.end(
          JSON.stringify({
            version_id: id,
            commit_id: id,
            bundle_digest: id,
            deduped: false,
          }),
        );
      });
      return;
    }
    response.writeHead(404, { "content-type": "application/json" });
    response.end(JSON.stringify({ code: "NOT_FOUND" }));
  });
  await new Promise<void>((resolve) => stub.listen(0, "127.0.0.1", resolve));
  const port = (stub.address() as AddressInfo).port;
  db = await createScratchDb("web_upstream", {
    TOPOS_SETUP_CODE: "upstream-setup-code",
    PLANE_INTERNAL_URL: `http://127.0.0.1:${port}`,
    TOPOS_UPSTREAM_CHECK_MS: "0",
  });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup("http://localhost:3000");
  wsId = (await identity.theWorkspace())?.id ?? "";
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
}, 60000);

afterAll(async () => {
  // db.drop() ends the pool itself — a second end() here would throw.
  await db.drop();
  await new Promise<void>((resolve) => {
    stub.close(() => resolve());
  });
});

describe("the tar reader", () => {
  it("reads regular files + the pax commit; skips traversal and links", async () => {
    const { untar } = await import("@/lib/db/upstream.server");
    const { gunzipSync } = await import("node:zlib");
    const commit = "a".repeat(40);
    const gz = fixtureTarball(commit, { "SKILL.md": "# Hi\n", "sub/tool.sh": "echo hi\n" });
    const { files, comment } = untar(gunzipSync(gz));
    expect(comment).toBe(commit);
    expect(files.map((f) => f.path).sort()).toEqual([
      `repo-${commit.slice(0, 7)}/SKILL.md`,
      `repo-${commit.slice(0, 7)}/sub/tool.sh`,
    ]);
    // Hostile entries are skipped, never surfaced.
    const hostile = Buffer.concat([
      tarEntry("../escape.txt", Buffer.from("nope")),
      tarEntry("/abs.txt", Buffer.from("nope")),
      tarEntry("link", Buffer.alloc(0), "2"),
      Buffer.alloc(1024),
    ]);
    const parsed = untar(hostile);
    expect(parsed.files).toHaveLength(0);
  });

  it("a damaged archive REFUSES whole — checksum, truncation, malformed pax", async () => {
    const { untar } = await import("@/lib/db/upstream.server");
    // A corrupted checksum byte.
    const good = tarEntry("ok.txt", Buffer.from("hi"));
    const corrupted = Buffer.concat([good, Buffer.alloc(1024)]);
    corrupted[150] = 0x39;
    expect(() => untar(corrupted)).toThrow(/checksum/);
    // A truncated body (the header claims more bytes than the archive holds).
    const truncated = tarEntry("big.txt", Buffer.from("x".repeat(600))).subarray(0, 700);
    expect(() => untar(Buffer.from(truncated))).toThrow(/truncated/);
    // A malformed pax record (no length prefix).
    const badPax = Buffer.concat([
      tarEntry("pax_global_header", Buffer.from("comment=abc\n"), "g"),
      Buffer.alloc(1024),
    ]);
    expect(() => untar(badPax)).toThrow(/pax/);
  });
});

describe("fetchUpstreamTree", () => {
  it("strips the tarball's top folder and slices the subdir", async () => {
    const { fetchUpstreamTree } = await import("@/lib/db/upstream.server");
    const commit = "b".repeat(40);
    const gz = fixtureTarball(commit, {
      "skills/deploy/SKILL.md": "# Deploy\n",
      "skills/deploy/run.sh": "run\n",
      "skills/other/SKILL.md": "# Other\n",
      LICENSE: "MIT License\n",
    });
    const tree = await fetchUpstreamTree("owner/repo", "skills/deploy", "HEAD", async () => gz);
    expect(tree.commit).toBe(commit);
    expect(tree.files.map((f) => f.path).sort()).toEqual(["SKILL.md", "run.sh"]);
    // The license falls back to the repo root when the subdir carries none.
    expect(tree.license).toBe("MIT License");
  });
});

describe("checkBundleUpstream — external changes ALWAYS propose", () => {
  it("proposes on a moved upstream; converges on re-check; stamps on unchanged", async () => {
    const upstream = await import("@/lib/db/upstream.server");
    // A bundle whose current is v1 (seeded custody rows), with an upstream recorded.
    const { versionId } = await seedBundle(db, wsId, "s_up", "up-skill");
    await db.q(
      `INSERT INTO web.bundle_upstream (bundle_id, workspace_id, host, repo, path, last_seen_commit)
       VALUES ('s_up', $1, 'github.com', 'owner/repo', 'skills/deploy', $2)`,
      [wsId, "c".repeat(40)],
    );

    // The upstream moved: new bytes at a new commit.
    const moved = "d".repeat(40);
    const gz = fixtureTarball(moved, { "skills/deploy/SKILL.md": "# Deploy v2\n" });
    const outcome = await upstream.checkBundleUpstream(wsId, "s_up", async () => gz);
    expect(outcome.outcome).toBe("proposed");
    // ONE open proposal, attributed to NO user, provenance in the thread + version_upstream.
    const proposals = await db.q<{ proposed_by: string | null; status: string }>(
      `SELECT proposed_by, status FROM web.proposal WHERE bundle_id = 's_up'`,
    );
    expect(proposals).toHaveLength(1);
    expect(proposals[0]?.proposed_by).toBeNull();
    expect(proposals[0]?.status).toBe("open");
    const comments = await db.q<{ author_display: string; body: string }>(
      `SELECT author_display, body FROM web.proposal_comment WHERE bundle_id = 's_up'`,
    );
    expect(comments[0]?.author_display).toBe("upstream watcher");
    expect(comments[0]?.body).toContain("owner/repo/skills/deploy");
    const vu = await db.q<{ commit: string }>(
      `SELECT commit FROM web.version_upstream WHERE bundle_id = 's_up' AND version_id <> $1`,
      [versionId],
    );
    expect(vu[0]?.commit).toBe(moved);
    // The candidate parents on the CURRENT version (the review diff reads as "what changes").
    const lastBody = JSON.parse(committedBodies.at(-1) ?? "{}") as { parent?: string };
    expect(lastBody.parent).toBe(versionId);

    // A re-check of the SAME commit is a fast no-op (last_seen matches).
    const again = await upstream.checkBundleUpstream(wsId, "s_up", async () => gz);
    expect(again.outcome).toBe("unchanged");
    expect(
      await db.q(`SELECT 1 FROM web.proposal WHERE bundle_id = 's_up' AND status = 'open'`),
    ).toHaveLength(1);
    // The bookkeeping stamped.
    const stamped = await db.q<{ last_seen_commit: string }>(
      `SELECT last_seen_commit FROM web.bundle_upstream WHERE bundle_id = 's_up'`,
    );
    expect(stamped[0]?.last_seen_commit).toBe(moved);
  });

  it("a bundle with no upstream answers no_upstream; upstreamOf reads the panel facts", async () => {
    const upstream = await import("@/lib/db/upstream.server");
    await seedBundle(db, wsId, "s_plain", "plain-skill");
    expect((await upstream.checkBundleUpstream(wsId, "s_plain")).outcome).toBe("no_upstream");
    // A cross-workspace probe resolves nothing — the check is workspace-bound.
    expect((await upstream.checkBundleUpstream("w_other", "s_up")).outcome).toBe("no_upstream");
    const view = await upstream.upstreamOf(wsId, "s_up");
    expect(view?.repo).toBe("owner/repo");
    expect(view?.path).toBe("skills/deploy");
    // The CURRENT version carries no upstream commit (locally published) — divergence readable.
    expect(view?.currentCommit).toBeNull();
  });
});
