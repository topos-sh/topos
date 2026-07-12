import { describe, expect, it } from "vitest";
import { languageForPath } from "@/lib/view/language";

describe("languageForPath", () => {
  it("maps markdown extensions to the markdown renderer", () => {
    expect(languageForPath("SKILL.md")).toEqual({ kind: "markdown" });
    expect(languageForPath("docs/guide.markdown")).toEqual({ kind: "markdown" });
  });

  it("maps code extensions to their shiki language id", () => {
    expect(languageForPath("src/index.ts")).toEqual({ kind: "code", lang: "typescript" });
    expect(languageForPath("app.tsx")).toEqual({ kind: "code", lang: "tsx" });
    expect(languageForPath("main.py")).toEqual({ kind: "code", lang: "python" });
    expect(languageForPath("lib.rs")).toEqual({ kind: "code", lang: "rust" });
    expect(languageForPath("data.json")).toEqual({ kind: "code", lang: "json" });
    expect(languageForPath("config.yaml")).toEqual({ kind: "code", lang: "yaml" });
    expect(languageForPath("config.yml")).toEqual({ kind: "code", lang: "yaml" });
    expect(languageForPath("Cargo.toml")).toEqual({ kind: "code", lang: "toml" });
  });

  it("collapses aliased extensions onto one shiki id", () => {
    expect(languageForPath("bundle.mjs")).toEqual({ kind: "code", lang: "javascript" });
    expect(languageForPath("bundle.cjs")).toEqual({ kind: "code", lang: "javascript" });
    expect(languageForPath("run.sh")).toEqual({ kind: "code", lang: "shellscript" });
    expect(languageForPath("run.bash")).toEqual({ kind: "code", lang: "shellscript" });
    expect(languageForPath("run.zsh")).toEqual({ kind: "code", lang: "shellscript" });
    expect(languageForPath("changes.patch")).toEqual({ kind: "code", lang: "diff" });
  });

  it("matches Dockerfile and Makefile by basename (case-sensitive)", () => {
    expect(languageForPath("Dockerfile")).toEqual({ kind: "code", lang: "dockerfile" });
    expect(languageForPath("build/Dockerfile")).toEqual({ kind: "code", lang: "dockerfile" });
    expect(languageForPath("Makefile")).toEqual({ kind: "code", lang: "makefile" });
    // lower-case spellings are NOT the toolchain's name → plain
    expect(languageForPath("dockerfile")).toEqual({ kind: "plain" });
    expect(languageForPath("makefile")).toEqual({ kind: "plain" });
  });

  it("is case-insensitive on the extension", () => {
    expect(languageForPath("README.MD")).toEqual({ kind: "markdown" });
    expect(languageForPath("Index.TS")).toEqual({ kind: "code", lang: "typescript" });
    expect(languageForPath("Data.JSON")).toEqual({ kind: "code", lang: "json" });
  });

  it("treats unknown, extensionless, and dotfiles as plain", () => {
    expect(languageForPath("notes.txt")).toEqual({ kind: "plain" });
    expect(languageForPath("table.csv")).toEqual({ kind: "plain" });
    expect(languageForPath("LICENSE")).toEqual({ kind: "plain" });
    expect(languageForPath("scripts/somebinary")).toEqual({ kind: "plain" });
    expect(languageForPath(".gitignore")).toEqual({ kind: "plain" });
    expect(languageForPath(".env")).toEqual({ kind: "plain" });
  });
});
