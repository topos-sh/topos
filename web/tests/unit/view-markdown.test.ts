import { describe, expect, it } from "vitest";
import { renderMarkdownHTML } from "@/lib/view/markdown.server";

describe("renderMarkdownHTML", () => {
  it("renders a GFM table", async () => {
    const md = ["| a | b |", "| - | - |", "| 1 | 2 |"].join("\n");
    const html = await renderMarkdownHTML(md);
    expect(html).toContain("<table");
    expect(html).toContain("<th");
    expect(html).toContain("<td");
  });

  it("renders a GFM task list as disabled checkbox inputs", async () => {
    const html = await renderMarkdownHTML("- [x] done\n- [ ] todo\n");
    expect(html).toContain("<input");
    expect(html).toContain("disabled");
  });

  it("drops raw HTML in the source (no script tag)", async () => {
    const html = await renderMarkdownHTML("hello\n\n<script>alert(1)</script>\n");
    expect(html).not.toContain("<script");
  });

  it("strips a javascript: link scheme", async () => {
    const html = await renderMarkdownHTML("[click](javascript:alert(1))\n");
    expect(html).not.toContain("javascript:");
  });

  it("drops images entirely", async () => {
    const html = await renderMarkdownHTML("![alt text](https://example.com/x.png)\n");
    expect(html).not.toContain("<img");
  });

  it("keeps an https link and adds rel=noopener noreferrer", async () => {
    const html = await renderMarkdownHTML("[site](https://example.com)\n");
    expect(html).toContain('href="https://example.com"');
    expect(html).toContain('rel="noopener noreferrer"');
  });

  it("keeps a relative link between bundle files", async () => {
    const html = await renderMarkdownHTML("[sibling](other.md)\n");
    expect(html).toContain('href="other.md"');
  });

  it("highlights a fenced code block with a known language", async () => {
    const html = await renderMarkdownHTML("```ts\nconst x = 1;\n```\n");
    expect(html).toContain("<pre");
    expect(html).toContain("<span");
    expect(html).toMatch(/style="[^"]*color:#[0-9a-fA-F]{3,8}/);
  });

  it("does not throw on a fence with an unknown language", async () => {
    const html = await renderMarkdownHTML("```no-such-lang\nplain body\n```\n");
    expect(html).toContain("<pre");
    expect(html).toContain("plain body");
  });
});
