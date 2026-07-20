import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";

/**
 * The agent-skills discovery lane's ONE source of truth: the repo's own `skills/topos/` files
 * (BUILTIN_SKILL_DIR, resolved relative to the process working directory — the install.ts
 * posture), read ONCE per process. The `/.well-known/agent-skills/` routes serve these exact
 * bytes, and the index digest is computed here FROM THE SAME READ — no generation script, no
 * committed digest, so the advertised SHA-256 and the served bytes cannot drift.
 *
 * This is the one sanctioned digest computation in this tier (carved out as ONE exact
 * expression in check-boundary.mjs — anything past that pinned form fails the gate here too):
 * it hashes PUBLIC bytes this same process serves, for the READER to verify against — no
 * secret is hashed and nothing signs.
 */

/** The three files of the built-in `topos` skill — SKILL.md first (the index's one entry). */
const SKILL_FILES = ["SKILL.md", "INSTALL.md", "reference.md"] as const;

const INDEX_SCHEMA = "https://schemas.agentskills.io/discovery/0.2.0/schema.json";
/** Path-absolute per the discovery RFC (resolved against the index URL as base). */
const SKILL_MD_URL = "/.well-known/agent-skills/topos/SKILL.md";
/** The RFC caps a skill entry's description at 1024 characters. */
const DESCRIPTION_MAX = 1024;

export interface AgentSkillsData {
  /** filename → the exact bytes the file route serves. */
  files: ReadonlyMap<string, Buffer>;
  /** The index document, pre-serialized — the exact bytes the index routes serve. */
  indexJson: string;
}

/** The skill's own frontmatter `description:` scalar — the index entry must not drift from it. */
function frontmatterDescription(skillMd: string): string {
  const frontmatter = skillMd.match(/^---\n([\s\S]*?)\n---/)?.[1];
  const description = frontmatter?.match(/^description:[ \t]*(.+)$/m)?.[1]?.trim();
  if (!description) {
    throw new Error("skills/topos/SKILL.md: no frontmatter description to index");
  }
  if (description.length > DESCRIPTION_MAX) {
    throw new Error(
      `skills/topos/SKILL.md: frontmatter description exceeds the RFC's ${DESCRIPTION_MAX}-char cap`,
    );
  }
  return description;
}

async function loadData(): Promise<AgentSkillsData> {
  const dir = path.resolve(process.cwd(), serverEnv().BUILTIN_SKILL_DIR);
  const files = new Map<string, Buffer>();
  for (const name of SKILL_FILES) {
    files.set(name, await readFile(path.join(dir, name)));
  }
  const skillMd = files.get("SKILL.md") as Buffer;
  const index = {
    $schema: INDEX_SCHEMA,
    skills: [
      {
        name: "topos",
        type: "skill-md",
        description: frontmatterDescription(skillMd.toString("utf8")),
        url: SKILL_MD_URL,
        digest: `sha256:${createHash("sha256").update(skillMd).digest("hex")}`,
      },
    ],
  };
  return { files, indexJson: `${JSON.stringify(index, null, 2)}\n` };
}

// Read once, not per fetch — the bytes are immutable for the process lifetime. A failed read
// (missing file, bad frontmatter) is not memoized, so a misbuilt image fails loudly every time.
let dataPromise: Promise<AgentSkillsData> | undefined;

export function agentSkills(): Promise<AgentSkillsData> {
  dataPromise ??= loadData().catch((error: unknown) => {
    dataPromise = undefined;
    throw error;
  });
  return dataPromise;
}
