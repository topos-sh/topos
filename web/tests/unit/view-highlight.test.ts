import { describe, expect, it } from "vitest";
import { renderCodeHTML } from "@/lib/view/highlight.server";

describe("renderCodeHTML", () => {
  it("highlights TypeScript with hex-color styled spans", async () => {
    const html = await renderCodeHTML("const answer = 42;\n", "typescript");
    expect(html).toContain("<pre");
    expect(html).toContain("<span");
    // shiki emits inline color styles the sanitizer keeps (hex only).
    expect(html).toMatch(/style="[^"]*color:#[0-9a-fA-F]{3,8}/);
  });

  it("emits only the allowlisted tags", async () => {
    const html = await renderCodeHTML("let x = 1;\n", "javascript");
    const tags = new Set([...html.matchAll(/<([a-z][a-z0-9-]*)/g)].map((m) => m[1]));
    for (const tag of tags) {
      expect(["pre", "code", "span"]).toContain(tag);
    }
  });

  it("falls back to escaped plaintext for an unknown language without throwing", async () => {
    const html = await renderCodeHTML("hello <there> & friends\n", "no-such-language");
    expect(html).toContain("<pre");
    // The angle brackets survive as escaped text, not as markup.
    expect(html).toContain("&lt;there&gt;");
    expect(html).not.toContain("<there>");
  });

  it("falls back to escaped plaintext when the language id is undefined", async () => {
    const html = await renderCodeHTML("plain text line\n", undefined);
    expect(html).toContain("<pre");
    expect(html).toContain("plain text line");
  });

  it("neutralizes an embedded closing-tag + script injection (text stays escaped)", async () => {
    const html = await renderCodeHTML("</pre><script>alert(1)</script>\n", "typescript");
    expect(html).not.toContain("<script");
    expect(html).not.toContain("</pre><script");
    // Highlighting tokenizes the payload across spans, but every angle bracket is escaped and the
    // text remains visible — `<` never survives as real markup.
    expect(html).toContain("&lt;");
    expect(html).toContain("&gt;");
    expect(html).toContain("script");
    expect(html).toContain("alert");
  });
});
