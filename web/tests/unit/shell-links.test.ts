import { existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { links } from "@/root";

/**
 * EVERY URL THE SHELL LINKS IS A FILE THAT SHIPS IN web/public — never a built asset's hashed
 * path.
 *
 * The shell module is compiled TWICE: once into the server bundle that renders the document, and
 * once into the browser bundle that hydrates it. A build-time asset URL (`import href from
 * "./x.css?url"`) is resolved separately by each of those builds, so the two copies of the string
 * can name different files — and a document whose `<link>` the hydrating client cannot match is
 * thrown away and re-rendered whole (React #418), which leaves every form on the page dead until
 * React remounts it. That is not hypothetical: production shipped a document linking one compiled
 * stylesheet while the browser bundle asked for another, and /verify's approve form swallowed
 * clicks because of it.
 *
 * Hashed assets reach the document through the build manifest instead — a side-effect `import
 * "./app.css"`, which both sides read from the ONE manifest. So the shell's own link list stays
 * hand-written paths, and this test is the fence: a path that is not a real file under web/public
 * is a build-resolved URL that has no business being here.
 */

const PUBLIC_DIR = resolve(__dirname, "..", "..", "public");

describe("the root shell's links", () => {
  it("name only files that ship in web/public", () => {
    const descriptors = links();
    expect(descriptors.length).toBeGreaterThan(0);

    for (const descriptor of descriptors) {
      const href = "href" in descriptor ? descriptor.href : undefined;
      expect(href, `every shell link needs an href: ${JSON.stringify(descriptor)}`).toBeTypeOf(
        "string",
      );
      const path = String(href);
      expect(
        path.startsWith("/") && existsSync(join(PUBLIC_DIR, path)),
        `${JSON.stringify(path)} is not a file that ships in web/public — a build-resolved asset URL cannot be linked from the shell`,
      ).toBe(true);
    }
  });
});
