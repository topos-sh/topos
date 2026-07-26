import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { compileDocs } from "../../scripts/docs/compile.mjs";
import { DocsContentError, parseFrontmatter } from "../../scripts/docs/frontmatter.mjs";

/**
 * The docs CONTENT CONTRACT, red-tested. Every rule the generator enforces exists so a broken
 * page fails the build instead of shipping blank, unreachable, or half-rendered — so each one is
 * driven here over a fixture tree with the violation planted, and must FIRE.
 *
 * The happy path runs against tests/fixtures/docs — a small, stable content set this suite owns,
 * never the live documentation (which changes for reasons that have nothing to do with the
 * renderer).
 */

const FIXTURE_ROOT = resolve(__dirname, "..", "fixtures", "docs");

const PAGE = (title: string, body = "Body copy.\n") =>
  `---\ntitle: ${title}\ndescription: A one-line description.\n---\n\n${body}`;

let scratches: string[] = [];

/** A throwaway content root: `files` is a path → contents map, written under a temp dir. */
function contentRoot(files: Record<string, string>): string {
  const dir = mkdtempSync(join(tmpdir(), "topos-docs-"));
  scratches.push(dir);
  for (const [rel, text] of Object.entries(files)) {
    const full = join(dir, rel);
    mkdirSync(dirname(full), { recursive: true });
    writeFileSync(full, text);
  }
  return dir;
}

/** Compile a throwaway root and return the DocsContentError message it fails with. */
async function compileFailure(files: Record<string, string>): Promise<string> {
  try {
    await compileDocs({ contentRoot: contentRoot(files) });
  } catch (error) {
    expect(error).toBeInstanceOf(DocsContentError);
    return (error as Error).message;
  }
  throw new Error("expected the compile to fail, but it succeeded");
}

afterEach(() => {
  for (const dir of scratches) {
    rmSync(dir, { recursive: true, force: true });
  }
  scratches = [];
});

describe("frontmatter", () => {
  it("reads the three contract keys and hands back the body", () => {
    const parsed = parseFrontmatter(
      '---\ntitle: "Publish a skill"\ndescription: One sentence.\nsidebar_label: Publish\n---\n\nBody.\n',
      "publish.mdx",
    );
    expect(parsed).toMatchObject({
      title: "Publish a skill",
      description: "One sentence.",
      sidebarLabel: "Publish",
      body: "Body.\n",
    });
  });

  it("leaves sidebarLabel null when the optional key is absent", () => {
    expect(parseFrontmatter(PAGE("Quickstart"), "quickstart.mdx").sidebarLabel).toBeNull();
  });

  it.each([
    ["no frontmatter at all", "Just prose.\n", /missing frontmatter/],
    ["an unclosed block", "---\ntitle: X\n", /never closed/],
    ["a missing title", "---\ndescription: One.\n---\n\nBody.\n", /missing the required "title"/],
    [
      "a missing description",
      "---\ntitle: X\n---\n\nBody.\n",
      /missing the required "description"/,
    ],
    ["an empty value", "---\ntitle:\ndescription: One.\n---\n\nB.\n", /"title" is empty/],
    [
      "an unknown key",
      "---\ntitle: X\ndescription: One.\nicon: rocket\n---\n\nB.\n",
      /unknown frontmatter key "icon"/,
    ],
  ])("refuses %s", (_case, source, expected) => {
    expect(() => parseFrontmatter(source, "page.mdx")).toThrow(expected);
  });

  it("names the file in the failure, so a build error points at the page", () => {
    expect(() => parseFrontmatter("Body.\n", "self-hosting/backups.mdx")).toThrow(
      /^self-hosting\/backups\.mdx:/,
    );
  });
});

describe("nav.json ↔ the pages on disk", () => {
  const nav = (groups: unknown) => JSON.stringify(groups);

  it("fails when the nav lists a page that does not exist", async () => {
    const message = await compileFailure({
      "nav.json": nav([{ group: "Start", pages: ["index", "ghost"] }]),
      "index.mdx": PAGE("Index"),
    });
    expect(message).toMatch(/lists pages that do not exist: ghost\.mdx/);
  });

  it("fails when a page on disk is missing from the nav — nothing would link to it", async () => {
    const message = await compileFailure({
      "nav.json": nav([{ group: "Start", pages: ["index"] }]),
      "index.mdx": PAGE("Index"),
      "orphan.mdx": PAGE("Orphan"),
    });
    expect(message).toMatch(/missing from nav\.json .*: orphan\.mdx/);
  });

  it("fails when a page is listed twice", async () => {
    const message = await compileFailure({
      "nav.json": nav([
        { group: "A", pages: ["index"] },
        { group: "B", pages: ["index"] },
      ]),
      "index.mdx": PAGE("Index"),
    });
    expect(message).toMatch(/"index" is listed more than once/);
  });

  it("fails when there is no index page — /docs would have nothing to serve", async () => {
    const message = await compileFailure({
      "nav.json": nav([{ group: "Start", pages: ["quickstart"] }]),
      "quickstart.mdx": PAGE("Quickstart"),
    });
    expect(message).toMatch(/no "index" page/);
  });

  it("fails when nav.json is absent altogether", async () => {
    const message = await compileFailure({ "index.mdx": PAGE("Index") });
    expect(message).toMatch(/nav\.json is missing/);
  });

  it.each([
    ["a group with no title", [{ pages: ["index"] }], /missing a "group" title/],
    ["a group with no pages", [{ group: "Start", pages: [] }], /lists no pages/],
    [
      "a page id that is a filename",
      [{ group: "Start", pages: ["index.mdx"] }],
      /is not a page id/,
    ],
  ])("refuses %s", async (_case, groups, expected) => {
    expect(await compileFailure({ "nav.json": nav(groups), "index.mdx": PAGE("I") })).toMatch(
      expected,
    );
  });
});

describe("the component set is closed", () => {
  const withBody = (body: string) => ({
    "nav.json": JSON.stringify([{ group: "Start", pages: ["index"] }]),
    "index.mdx": PAGE("Index", body),
  });

  it("refuses a component that is not in the set", async () => {
    expect(await compileFailure(withBody("<Accordion>\nHidden.\n</Accordion>\n"))).toMatch(
      /<Accordion> is not a docs component/,
    );
  });

  it("refuses an unclosed component", async () => {
    expect(await compileFailure(withBody("<Note>\nUnclosed.\n"))).toMatch(/is never closed/);
  });

  it("refuses a <Step> outside <Steps>", async () => {
    expect(await compileFailure(withBody('<Step title="One">\nDo it.\n</Step>\n'))).toMatch(
      /<Step> must sit directly inside <Steps>/,
    );
  });

  it("refuses a <Step> with no title", async () => {
    expect(await compileFailure(withBody("<Steps>\n<Step>\nDo it.\n</Step>\n</Steps>\n"))).toMatch(
      /<Step> requires a title="…" attribute/,
    );
  });

  it("refuses prose sitting directly inside <Steps>", async () => {
    expect(await compileFailure(withBody("<Steps>\nLoose prose.\n</Steps>\n"))).toMatch(
      /<Steps> may only contain <Step>/,
    );
  });

  it("refuses an h1 in the body — the frontmatter title is the page's only h1", async () => {
    expect(await compileFailure(withBody("# Second title\n"))).toMatch(/the body carries an h1/);
  });
});

describe("the fixture content set compiles", () => {
  it("renders every page the nav lists, in nav order", async () => {
    const { pages, order, nav } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    expect(order).toEqual(["index", "quickstart", "motions/publish", "reference/cli"]);
    expect(pages.map((page) => page.id)).toEqual(order);
    expect(nav[0]).toEqual({ group: "Start here", pages: ["index", "quickstart"] });
  });

  it("carries frontmatter into the page record, sidebar_label falling back to the title", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const index = pages.find((page) => page.id === "index");
    const publish = pages.find((page) => page.id === "quickstart");
    expect(index).toMatchObject({ title: "Topos documentation", sidebarLabel: "Overview" });
    expect(publish?.sidebarLabel).toBe("Quickstart");
  });

  it("renders each component into its own markup, and nothing into raw JSX", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const html = pages.map((page) => page.html).join("");
    expect(html).toContain('class="docs-aside docs-aside--note"');
    expect(html).toContain('class="docs-aside docs-aside--warning"');
    expect(html).toContain('class="docs-aside docs-aside--tip"');
    expect(html).toContain('class="docs-steps"');
    expect(html).toContain('class="docs-tabs"');
    expect(html).toContain('class="docs-cards"');
    expect(html).not.toMatch(/<(Note|Warning|Tip|Steps|Step|Tabs|Tab|Card|CardGrid)[\s>]/);
  });

  it("keeps a component's children as MARKDOWN, not as an unparsed string", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const quickstart = pages.find((page) => page.id === "quickstart");
    // The install command inside <Step> is a fenced block: it must have become a <pre>, and the
    // inline code in the same step must have become a <code>.
    expect(quickstart?.html).toMatch(/<li class="docs-step">[\s\S]*?<pre/);
    expect(quickstart?.html).toContain("<code>~/.local/bin</code>");
  });

  it("highlights fenced code at BUILD time, and frames a fence that carries a title", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const quickstart = pages.find((page) => page.id === "quickstart");
    // Both themes render at build time: the light colours inline, the dark set as variables the
    // command frame switches to. Shiki's own inline <pre> page colours are stripped — the docs
    // stylesheet owns the frames.
    expect(quickstart?.html).toContain('<pre class="shiki shiki-themes github-light github-dark"');
    expect(quickstart?.html).not.toMatch(/<pre[^>]*style=/);
    expect(quickstart?.html).toContain(
      '<figure class="docs-code"><figcaption class="docs-code__title">Log in</figcaption>',
    );
  });

  it("frames every fence with a copy button, and sets command fences on the glass", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const html = pages.find((page) => page.id === "quickstart")?.html ?? "";
    // The fixture's fences are `sh` — command language — so the frame carries the glass class.
    expect(html).toContain('<div class="docs-codeblock docs-codeblock--command">');
    expect(html).toContain(
      '<button type="button" class="docs-copy" data-copy="" aria-label="Copy to clipboard">',
    );
    // Command tokens were recoloured to the dark set at generate time — no !important needed.
    expect(html).toMatch(/docs-codeblock--command[\s\S]*?color:var\(--shiki-dark\)/);
  });

  it("switches tabs without JavaScript: one radio group, the first tab checked", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const html = pages.find((page) => page.id === "quickstart")?.html ?? "";
    expect(html).toMatch(/<input type="radio" name="docs-tabs-0" id="docs-tabs-0-0"[^>]*checked>/);
    expect(html).toContain('<label class="docs-tabs__label" for="docs-tabs-0-0">Agent</label>');
    expect(html).toContain('<label class="docs-tabs__label" for="docs-tabs-0-1">Human</label>');
    expect(html).not.toContain("<script");
  });

  it("gives every h2/h3 an anchor and collects them as the page's contents", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const publish = pages.find((page) => page.id === "motions/publish");
    expect(publish?.headings).toEqual([
      { depth: 2, id: "publish", text: "Publish" },
      { depth: 3, id: "what-the-receipt-tells-you", text: "What the receipt tells you" },
    ]);
    expect(publish?.html).toContain('<h2 id="publish">');
    expect(publish?.html).toContain('<a class="docs-anchor" href="#publish"');
  });

  it("expands the generated CLI reference in place, headed by the page title alone", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const cli = pages.find((page) => page.id === "reference/cli");
    expect(cli?.html).not.toContain("GENERATED-CLI-REFERENCE");
    // The reference's own H1 and its "GENERATED" provenance quote are stripped on splice — the
    // page provides both — and the reference's headings keep their levels, so each command (h3)
    // lands in the page TOC.
    expect(cli?.html).not.toContain("topos-command-reference");
    expect(cli?.html).not.toContain("GENERATED from");
    expect(cli?.html).toContain("topos status");
    expect(cli?.headings.some((heading) => heading.text === "Global options")).toBe(true);
    expect(cli?.headings.some((heading) => heading.text === "topos status")).toBe(true);
  });
});

describe("the plain-markdown twin", () => {
  it("leads with the page's title and description", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const quickstart = pages.find((page) => page.id === "quickstart");
    expect(quickstart?.markdown.startsWith("# Quickstart\n\nInstall the CLI, log in,")).toBe(true);
  });

  it("reduces every component tag to plain markdown — no JSX to parse", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    for (const page of pages) {
      expect(page.markdown).not.toMatch(/<\/?(Note|Warning|Tip|Steps|Step|Tabs|Tab|Card|CardGrid)/);
    }
  });

  it("keeps the aside's voice, numbers the steps, and titles each tab", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const markdown = pages.find((page) => page.id === "quickstart")?.markdown ?? "";
    expect(markdown).toContain("**Warning**");
    expect(markdown).toContain("**1. Install the CLI**");
    expect(markdown).toContain("**2. Log in to your workspace**");
    expect(markdown).toContain("**Agent**");
    expect(markdown).toContain("**Human**");
  });

  it("turns cards into a link list", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const markdown = pages.find((page) => page.id === "index")?.markdown ?? "";
    expect(markdown).toContain("- [Quickstart](/docs/quickstart)");
    expect(markdown).toContain("- [Publish a skill](/docs/motions/publish)");
  });

  it("leaves the author's own markdown alone — fences, tables and headings survive verbatim", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const markdown = pages.find((page) => page.id === "quickstart")?.markdown ?? "";
    expect(markdown).toContain("```sh\ncurl -fsSL https://topos.sh/install | sh\n```");
    expect(markdown).toContain("| Column | Meaning |");
    expect(markdown).toContain("## Install and log in");
  });

  it("carries the whole CLI reference, so an agent can fetch one page and be done", async () => {
    const { pages } = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const markdown = pages.find((page) => page.id === "reference/cli")?.markdown ?? "";
    expect(markdown).not.toContain("GENERATED-CLI-REFERENCE");
    expect(markdown).toContain("### `topos status`");
  });
});

describe("determinism", () => {
  it("compiles byte-identically twice — the drift gate cannot flap", async () => {
    const first = await compileDocs({ contentRoot: FIXTURE_ROOT });
    const second = await compileDocs({ contentRoot: FIXTURE_ROOT });
    expect(JSON.stringify(second)).toBe(JSON.stringify(first));
  });
});
