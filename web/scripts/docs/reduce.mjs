import { parseTag } from "./components.mjs";

/**
 * THE PLAIN-MARKDOWN TWIN. Every docs page is also served at its path + `.md`, and that copy is
 * for a reader with no renderer — an agent that fetched the URL. It must be the page's own prose,
 * not a description of it, and it must not make anyone parse JSX: each component tag is reduced
 * to the plainest markdown that carries the same meaning.
 *
 *   <Note>/<Warning>/<Tip>   a bold label line, then the aside's own prose
 *   <Steps>/<Step>           the steps in order, each titled "**1. …**"
 *   <Tabs>/<Tab>             each tab's title as a bold line, then its content
 *   <CardGrid>/<Card>        a markdown link list, each card's blurb indented under its link
 *
 * Everything else — headings, lists, tables, fenced code — is the author's markdown, untouched.
 * The input is the NORMALIZED source (tags at the margin, children dedented), so this is a
 * line pass with a small stack rather than a second parser.
 */

const FENCE = /^(\s*)(```|~~~)/;

/** The bold lead-in each aside is reduced to. */
const ASIDE_LABEL = { Note: "Note", Warning: "Warning", Tip: "Tip" };

export function reduceToMarkdown(body, meta, file) {
  const out = [];
  const stack = [];
  let fence = null;
  let stepNumber = 0;

  const push = (line) => {
    const inCard = stack.some((entry) => entry.name === "Card");
    out.push(inCard && line.trim() !== "" ? `  ${line}` : line);
  };
  const pushBlank = () => {
    if (out.length > 0 && out[out.length - 1].trim() !== "") {
      out.push("");
    }
  };

  for (const line of body.split("\n")) {
    const match = line.match(FENCE);
    if (fence !== null) {
      push(line);
      if (match !== null && match[2] === fence) {
        fence = null;
      }
      continue;
    }
    if (match !== null) {
      fence = match[2];
      push(line);
      continue;
    }

    const trimmed = line.trim();
    const tag = trimmed.startsWith("<") ? parseTag(trimmed, file) : null;
    if (tag === null) {
      push(line);
      continue;
    }
    if (tag.kind === "close") {
      stack.pop();
      pushBlank();
      continue;
    }
    if (tag.kind === "open") {
      stack.push({ name: tag.name });
    }
    if (tag.name === "Steps") {
      stepNumber = 0;
    }
    pushBlank();
    const reduced = reduceTag(tag, () => (stepNumber += 1));
    if (reduced !== null) {
      // A card's link is the list item; a nested blank keeps the following blurb attached.
      out.push(reduced);
    }
    if (tag.kind === "self") {
      stack.pop();
    }
  }

  const prose = out
    .join("\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
  return `# ${meta.title}\n\n${meta.description}\n\n${prose}\n`;
}

/** The one line (or nothing) a component tag reduces to. */
function reduceTag(tag, nextStep) {
  if (tag.kind === "close") {
    return null;
  }
  const label = ASIDE_LABEL[tag.name];
  if (label !== undefined) {
    return `**${label}**`;
  }
  if (tag.name === "Step") {
    return `**${nextStep()}. ${tag.attributes.title}**`;
  }
  if (tag.name === "Tab") {
    return `**${tag.attributes.title}**`;
  }
  if (tag.name === "Card") {
    return `- [${tag.attributes.title}](${tag.attributes.href})`;
  }
  if (tag.name === "Cta") {
    return `**[${tag.attributes.title}](${tag.attributes.href})**`;
  }
  if (tag.name === "Column") {
    return `**${tag.attributes.title}**`;
  }
  // Steps / Tabs / CardGrid / Columns are pure containers: their children carry the meaning.
  return null;
}
