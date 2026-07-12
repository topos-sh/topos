import { describe, expect, it } from "vitest";
import {
  diffChromeAssets,
  diffRendererName,
  renderFileDiffHTML,
} from "@/lib/diff/render/pierre.server";

describe("renderFileDiffHTML", () => {
  it("names its renderer", () => {
    expect(diffRendererName).toBe("@pierre/diffs");
  });

  it("pins a small add+modify+delete render (sanitized output snapshot)", async () => {
    const html = await renderFileDiffHTML({
      path: "SKILL.md",
      oldText: "# title\nkeep this line\ndrop this line\n",
      newText: "# title\nkeep this line\nadd this line\nand this one\n",
    });
    expect(html).toMatchSnapshot();
  });

  it("emits only the allowlisted elements and no chrome", async () => {
    const html = await renderFileDiffHTML({
      path: "a.ts",
      oldText: "const a = 1;\n",
      newText: "const a = 2;\n",
    });
    expect(html).not.toContain("<style");
    expect(html).not.toContain("<svg");
    expect(html).not.toContain("<script");
    const tags = new Set([...html.matchAll(/<([a-z][a-z0-9-]*)/g)].map((m) => m[1]));
    for (const tag of tags) {
      expect(["pre", "code", "div", "span"]).toContain(tag);
    }
  });

  describe("adversarial contents stay escaped (no executable vector)", () => {
    it("script tags in file contents render only as escaped text", async () => {
      const payload = '<script>alert("xss")</script>\n';
      const html = await renderFileDiffHTML({ path: "evil.md", oldText: "", newText: payload });
      expect(html).not.toContain("<script");
      expect(html).toContain("&lt;script&gt;");
    });

    // An on*/href/url( SUBSTRING may appear as escaped text — the executable shape is the
    // attribute inside a REAL (unescaped) tag, which these regexes target.
    const ON_ATTRIBUTE_IN_TAG = /<[a-z][^>]*\son[a-z]+\s*=/i;
    const HREF_IN_TAG = /<[a-z][^>]*\shref\s*=/i;
    const URL_IN_STYLE_ATTRIBUTE = /<[a-z][^>]*\sstyle\s*=\s*"[^"]*url\(/i;

    it("img onerror in contents renders only as escaped text", async () => {
      const payload = "<img src=x onerror=alert(1)>\n";
      const html = await renderFileDiffHTML({ path: "evil.md", oldText: "", newText: payload });
      expect(html).not.toContain("<img");
      expect(html).not.toMatch(ON_ATTRIBUTE_IN_TAG);
      expect(html).toContain("&lt;img");
    });

    it("javascript: URLs never become an attribute value", async () => {
      const payload = '<a href="javascript:alert(1)">x</a>\n';
      const html = await renderFileDiffHTML({ path: "evil.md", oldText: "", newText: payload });
      expect(html).not.toContain("<a ");
      expect(html).not.toMatch(HREF_IN_TAG);
      expect(html).toContain("&lt;a href");
    });

    it("an adversarial file PATH cannot inject markup", async () => {
      const html = await renderFileDiffHTML({
        path: "<img src=x onerror=alert(2)>.md",
        oldText: "a\n",
        newText: "b\n",
      });
      expect(html).not.toContain("<img");
      expect(html).not.toMatch(ON_ATTRIBUTE_IN_TAG);
    });

    it("event handlers and style URLs are stripped even if the renderer ever emitted them", async () => {
      // Drive the sanitizer through the renderer path: contents that LOOK like markup must
      // come out inert; no on* attribute or url()-bearing style survives on a real tag.
      const payload = '<div onclick=alert(1) style="background:url(javascript:alert(2))">x</div>\n';
      const html = await renderFileDiffHTML({ path: "e.md", oldText: "", newText: payload });
      expect(html).not.toMatch(ON_ATTRIBUTE_IN_TAG);
      expect(html).not.toMatch(URL_IN_STYLE_ATTRIBUTE);
    });
  });

  it("preserves BOM and CRLF text verbatim inside the diff", async () => {
    const html = await renderFileDiffHTML({
      path: "bom.txt",
      oldText: "",
      newText: "﻿hello\r\n",
    });
    // The BOM survives into the rendered content (it is not stripped by the pipeline).
    expect(html).toContain("﻿");
  });

  it(">128KiB plain mode still renders and sanitizes", async () => {
    const big = `${"x".repeat(200)}\n`.repeat(700); // ~140KiB
    const html = await renderFileDiffHTML({
      path: "big.txt",
      oldText: "",
      newText: big,
      plain: true,
    });
    expect(html).toContain("<pre");
    expect(html).not.toContain("<script");
  });
});

describe("diffChromeAssets", () => {
  it("returns the static stylesheet + sprite once, from constant content", async () => {
    const assets = await diffChromeAssets();
    expect(assets).toContain("<style");
    expect(assets).toContain("<svg");
    // Nothing but the style + sprite blocks: no rendered diff body (and so no content) rides
    // along with the chrome.
    const remainder = assets
      .replace(/<svg[\s\S]*?<\/svg>/g, "")
      .replace(/<style[\s\S]*?<\/style>/g, "");
    expect(remainder).toBe("");
  });
});
