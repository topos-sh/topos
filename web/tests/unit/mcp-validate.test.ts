import { Buffer } from "node:buffer";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { SECRET_ENTROPY, SECRET_PATTERNS } from "@/lib/mcp/secret-patterns.generated";
import {
  findSecret,
  findSecretDeep,
  MCP_ALLOWED_FILES,
  type McpValidation,
  suggestedNameFor,
  validateCandidateFiles,
  validateServerJson,
} from "@/lib/mcp/validate.server";

/**
 * The MCP server-document gate, driven by the SHARED vectors at the repo root
 * (`tests/fixtures/mcp/`) — the same files a client-side reader validates against, so the two
 * languages can never quietly disagree about what is accepted. Every vector names one verdict:
 * `ok`, or the exact refusal code. A rule change here fails until the vector changes with it.
 *
 * A vector arrives in one of three shapes, because a candidate is more than a readable document:
 * `file` is one fixture read as bytes, `raw_base64` is a document whose exact BYTES matter (an
 * invalid UTF-8 sequence has no fixture-file spelling that survives an editor), and `files` is a
 * whole candidate — the file set the allowlist judges and whose siblings the credential scan
 * reads.
 */

// tests/unit → web → repo root.
const FIXTURES = resolve(__dirname, "..", "..", "..", "tests", "fixtures", "mcp");

/** One file of a multi-file vector: a fixture on disk, or literal content spelled inline. */
interface VectorFile {
  path: string;
  file?: string;
  content?: string;
}

type Vector = { verdict: string; note: string } & (
  | { file: string }
  | { raw_base64: string }
  | { files: VectorFile[] }
);

const vectors = JSON.parse(readFileSync(join(FIXTURES, "vectors.json"), "utf8")) as Vector[];
const bytesOf = (file: string) => readFileSync(join(FIXTURES, file));

/** What a row is called in the report — the fixture path, the file set, or just "raw". */
function titleOf(vector: Vector): string {
  if ("file" in vector) {
    return vector.file;
  }
  if ("files" in vector) {
    return vector.files.map((f) => f.path).join(" + ");
  }
  return "raw";
}

/** Drive the gate the way this vector's shape asks to be driven. */
function runVector(vector: Vector): McpValidation {
  if ("file" in vector) {
    return validateServerJson(bytesOf(vector.file));
  }
  if ("raw_base64" in vector) {
    return validateServerJson(Buffer.from(vector.raw_base64, "base64"));
  }
  return validateCandidateFiles(
    vector.files.map((f) => ({
      path: f.path,
      bytes: f.file === undefined ? Buffer.from(f.content ?? "", "utf8") : bytesOf(f.file),
    })),
  );
}

describe("the shared refusal vectors", () => {
  it("covers both verdicts and every refusal code the gate can answer", () => {
    expect(vectors.length).toBeGreaterThan(0);
    const codes = new Set(vectors.map((v) => v.verdict));
    expect([...codes].sort()).toEqual([
      "MCP_INSECURE_URL",
      "MCP_INVALID",
      "MCP_LOCAL_REFUSED",
      "MCP_NO_STREAMABLE_REMOTE",
      "MCP_SECRET_REFUSED",
      "MCP_URL_TEMPLATE",
      "ok",
    ]);
    // Every vector carries a note: the file says WHY, not just what.
    for (const vector of vectors) {
      expect(vector.note.length).toBeGreaterThan(20);
    }
  });

  it.each(
    vectors.map((v) => [titleOf(v), v.verdict, v] as const),
  )("%s → %s", (title, verdict, vector) => {
    const result = runVector(vector);
    if (verdict === "ok") {
      expect(result.ok, `${title}: ${vector.note}`).toBe(true);
      return;
    }
    expect(result.ok, `${title}: ${vector.note}`).toBe(false);
    expect(result.ok === false && result.code).toBe(verdict);
    // A refusal always says something a human can act on.
    expect(result.ok === false && result.message.length).toBeGreaterThan(10);
  });
});

describe("the accepted summary", () => {
  it("reads the endpoint, the transport, and no auth claim when none is declared", () => {
    const result = validateServerJson(bytesOf("valid/remote-no-auth.json"));
    expect(result.ok).toBe(true);
    expect(result.ok === true && result.summary).toEqual({
      name: "io.github.acme/weather",
      description: "Current conditions and forecasts for a named place.",
      version: "1.4.0",
      url: "https://weather.acme.example/mcp",
      transport: "streamable-http",
      headers: [],
      // Declaring nothing is NOT the same claim as declaring "none".
      authHint: null,
    });
  });

  it("keeps literal headers verbatim", () => {
    const result = validateServerJson(bytesOf("valid/remote-literal-header.json"));
    expect(result.ok === true && result.summary.headers).toEqual([
      { name: "X-Region", value: "eu-west-1" },
      { name: "X-Client", value: "topos" },
    ]);
  });

  it("reports the publisher's oauth hint from _meta", () => {
    const result = validateServerJson(bytesOf("valid/remote-oauth-meta.json"));
    expect(result.ok === true && result.summary.authHint).toBe("oauth");
  });

  it("picks the streamable-http remote when an event-stream sibling is offered first", () => {
    const result = validateServerJson(bytesOf("valid/remote-sse-and-streamable.json"));
    expect(result.ok === true && result.summary.url).toBe("https://calendar.acme.example/mcp");
    expect(result.ok === true && result.summary.transport).toBe("streamable-http");
  });

  it("suggests the tail segment of the registry name as the catalog name", () => {
    expect(suggestedNameFor("io.github.acme/weather")).toBe("weather");
    expect(suggestedNameFor("weather")).toBe("weather");
  });
});

describe("the credential scan", () => {
  it("runs from the SAME JSON the vectors carry — value for value", () => {
    const source = JSON.parse(readFileSync(join(FIXTURES, "secret-patterns.json"), "utf8")) as {
      patterns: { name: string; regex: string }[];
      entropy: { minLength: number; threshold: number };
    };
    expect(SECRET_PATTERNS.map((p) => ({ name: p.name, regex: p.regex }))).toEqual(source.patterns);
    expect({ ...SECRET_ENTROPY }).toEqual(source.entropy);
  });

  it.each([
    ["a classic forge token", "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"],
    ["a fine-grained forge token", "github_pat_11ABCDEFG0abcdefghijklmnop"],
    ["a model-provider key", "sk-proj-9fJ2mQ8xVb3nT7kLp0WzYcRd5AeHgUiO"],
    ["a chat-platform token", "xoxb-1234567890-abcdefghijkl"],
    ["a cloud access key id", "AKIAIOSFODNN7EXAMPLE"],
    ["a repository-host token", "glpat-ABCdef123456789012345"],
    ["a search-provider key", "AIzaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6"],
    ["a presented bearer credential", "Bearer abcdefghijklmnopqrstuvwxyz012345"],
    ["a long hex key", "0123456789abcdef0123456789abcdef"],
  ])("names %s", (_label, token) => {
    expect(findSecret(`{"value":"${token}"}`)).not.toBeNull();
  });

  it.each([
    ["ordinary prose", "The quick brown fox jumps over the lazy dog near the river bank."],
    ["a reverse-DNS server name", "io.github.modelcontextprotocol/servers-everything"],
    ["an https endpoint", "https://api.githubcopilot.com/mcp/v1/streamable-endpoint"],
    ["a semantic version", "2026.8.3-rc.1"],
    ["a region slug", "eu-west-1-production-gateway"],
    ["a user-agent-ish string", "Mozilla-5-0-compatible-topos-registry"],
  ])("leaves %s alone", (_label, text) => {
    expect(findSecret(text)).toBeNull();
  });
});

describe("what the raw text alone would miss", () => {
  const DOCUMENT = {
    name: "io.github.acme/probe",
    description: "A server used to probe the gate.",
    version: "1.0.0",
  };
  /** The probe document with one remote, plus whatever this case wants said about it. */
  const withRemote = (remote: Record<string, unknown>) =>
    JSON.stringify({
      ...DOCUMENT,
      remotes: [{ type: "streamable-http", url: "https://probe.acme.example/mcp", ...remote }],
    });

  it("an escaped credential is caught by the decoded-string walk", () => {
    const token = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    const escaped = [...token]
      .map((ch) => `\\u${ch.charCodeAt(0).toString(16).padStart(4, "0")}`)
      .join("");
    // Hand-built rather than stringified: the point is that the BYTES spell `\uXXXX` and the
    // JSON parser is what turns them back into a token.
    const raw = `{"name":"io.github.acme/escaped","description":"An escaped credential.","version":"1.0.0","remotes":[{"type":"streamable-http","url":"https://probe.acme.example/mcp","headers":[{"name":"X-Trace","value":"${escaped}"}]}]}`;
    // The raw pass is blind to it — every escape breaks the token run, and no pattern matches.
    expect(findSecret(raw)).toBeNull();
    const result = validateServerJson(raw);
    expect(result.ok === false && result.code).toBe("MCP_SECRET_REFUSED");
  });

  it("credential-bearing header names refuse independent of value", () => {
    // Values chosen to be innocent on their own: the NAME is what refuses, and the case it is
    // written in makes no difference.
    for (const [name, value] of [
      ["Authorization", "Basic hello"],
      ["COOKIE", "session=1"],
    ]) {
      const result = validateServerJson(withRemote({ headers: [{ name, value }] }));
      expect(result.ok === false && result.code, name).toBe("MCP_SECRET_REFUSED");
      expect(result.ok === false && result.message).toContain(name);
    }
  });

  it("url userinfo refuses as a secret", () => {
    const result = validateServerJson(
      withRemote({ url: "https://svc:hunter2@probe.acme.example/mcp" }),
    );
    expect(result.ok === false && result.code).toBe("MCP_SECRET_REFUSED");
  });

  it("invalid utf-8 refuses invalid", () => {
    // A lone 0xE9 inside a string value. A lossy decode replaces it and the document parses as
    // if it were readable, so refusing depends entirely on decoding strictly.
    const bytes = Buffer.concat([
      Buffer.from('{"name":"io.github.acme/probe","description":"caf', "utf8"),
      Buffer.from([0xe9]),
      Buffer.from(
        '","version":"1.0.0","remotes":[{"type":"streamable-http","url":"https://probe.acme.example/mcp"}]}',
        "utf8",
      ),
    ]);
    expect(JSON.parse(new TextDecoder().decode(bytes)).name).toBe("io.github.acme/probe");
    const result = validateServerJson(bytes);
    expect(result.ok === false && result.code).toBe("MCP_INVALID");
    expect(result.ok === false && result.message).toContain("UTF-8");
  });

  it("the candidate allowlist refuses a stray file and scans sibling bytes", () => {
    const server = { path: "server.json", bytes: bytesOf("valid/remote-no-auth.json") };
    const stray = validateCandidateFiles([
      server,
      { path: "install.sh", bytes: Buffer.from("#!/bin/sh\necho hi\n", "utf8") },
    ]);
    expect(stray.ok === false && stray.code).toBe("MCP_INVALID");
    // The refusal names the WHOLE allowed set — the author learns the rule, not one violation.
    for (const allowed of MCP_ALLOWED_FILES) {
      expect(stray.ok === false && stray.message).toContain(allowed);
    }
    const readme = validateCandidateFiles([
      server,
      {
        path: "README.md",
        bytes: Buffer.from(
          "Export GITHUB_TOKEN=ghp_A1b2C3d4E5A1b2C3d4E5A1b2C3d4E5A1b2C3 first.\n",
          "utf8",
        ),
      },
    ]);
    expect(readme.ok === false && readme.code).toBe("MCP_SECRET_REFUSED");
    // The allowed set is a ceiling, not a demand: the exact trio, all clean, passes.
    const trio = validateCandidateFiles([
      server,
      { path: "README.md", bytes: Buffer.from("What this server does.\n", "utf8") },
      { path: "topos-mcp.toml", bytes: Buffer.from("# reserved\n", "utf8") },
    ]);
    expect(trio.ok).toBe(true);
  });
});

describe("edge shapes", () => {
  it("refuses empty bytes", () => {
    const result = validateServerJson(new Uint8Array(0));
    expect(result.ok === false && result.code).toBe("MCP_INVALID");
  });

  it("refuses a JSON array (a document is an object)", () => {
    const result = validateServerJson("[]");
    expect(result.ok === false && result.code).toBe("MCP_INVALID");
  });

  it("refuses an unparseable endpoint before it can be treated as an address", () => {
    const result = validateServerJson(
      JSON.stringify({
        name: "io.github.acme/broken",
        description: "An endpoint that is not a URL.",
        version: "1.0.0",
        remotes: [{ type: "streamable-http", url: "not a url" }],
      }),
    );
    expect(result.ok === false && result.code).toBe("MCP_INVALID");
  });

  it("refuses remote-level variables even when the URL is a plain address", () => {
    const result = validateServerJson(
      JSON.stringify({
        name: "io.github.acme/varied",
        description: "Variables with nothing left to fill.",
        version: "1.0.0",
        remotes: [
          {
            type: "streamable-http",
            url: "https://varied.acme.example/mcp",
            variables: { tenant: { description: "which tenant" } },
          },
        ],
      }),
    );
    expect(result.ok === false && result.code).toBe("MCP_SECRET_REFUSED");
  });

  it("accepts a document string as readily as its bytes", () => {
    const text = readFileSync(join(FIXTURES, "valid/remote-no-auth.json"), "utf8");
    expect(validateServerJson(text).ok).toBe(true);
  });
});

/**
 * ITEM PAIR (deep nesting, the web half): a document nested past the shared cap answers the
 * TYPED refusal — never a RangeError. On the Rust side serde_json refuses such a document at the
 * parse (its default recursion limit); this tier's `JSON.parse` is iterative and would accept
 * far deeper documents, so the explicit depth cap is what keeps the two gates vector-identical,
 * and the explicit-stack walk in `findSecretDeep` is what keeps the scan itself crash-free.
 * Before the fix the recursive walk blew the call stack: ~120k nested arrays under the byte cap
 * threw instead of refusing.
 */
describe("deep nesting answers a typed refusal, never a crash", () => {
  const nested = (n: number) => "[".repeat(n) + "]".repeat(n);

  it("a 120k-deep document under the byte cap is MCP_INVALID, not a RangeError", () => {
    const result = validateServerJson(nested(120_000));
    expect(result.ok).toBe(false);
    expect(result.ok === false && result.code).toBe("MCP_INVALID");
    expect(result.ok === false && result.message).toBe("document nesting too deep");
  });

  it("the cap sits exactly where serde_json's recursion limit does: 127 parses, 128 refuses", () => {
    // 127 containers pass the depth gate — the refusal is the later SHAPE check, which proves
    // the document was read past the cap.
    const at127 = validateServerJson(nested(127));
    expect(at127.ok === false && at127.message).toBe("a server.json document is a JSON object");
    // The 128th container is where serde stops on the Rust side — and where this cap stops too.
    const at128 = validateServerJson(nested(128));
    expect(at128.ok === false && at128.message).toBe("document nesting too deep");
  });

  it("findSecretDeep walks a pathological document on an explicit stack", () => {
    // No throw, no hit on empty nesting…
    expect(findSecretDeep(JSON.parse(nested(120_000)))).toBeNull();
    // …and a token buried in nested containers is still found, keys and values alike.
    const buried = { a: [{ b: { note: `ghp_${"A1b2C3d4E5".repeat(4)}` } }] };
    expect(findSecretDeep(buried)).toBe("github-token");
  });
});
