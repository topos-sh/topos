import { DocsContentError } from "./frontmatter.mjs";

/**
 * THE COMPONENT SET — a closed list, and nothing else. Anything a page reaches for beyond this
 * list is a build failure, never a silently-empty div: the set is small on purpose, so a page
 * either says something in plain markdown or says it in one of these shapes.
 *
 *   <Note> / <Warning> / <Tip>          asides — Warning is the hazard voice (loss, reach,
 *                                       irreversibility), Note is worth-reading, Tip is optional
 *   <Steps> wrapping <Step title="…">   an ordered procedure
 *   <Tabs> wrapping <Tab title="…">     the same task by two paths — JS-free: the labels drive
 *                                       one radio group and `:checked` shows the panel
 *   <CardGrid> wrapping <Card …>        a small set of next destinations
 *   <Cta title="…" href="…" />          ONE button-styled link — the page's primary action
 *   <Columns> wrapping <Column …>       two labelled blocks side by side on a wide screen
 *
 * Components are folded at the MDAST level, before hast. The tags arrive as `html` leaf nodes
 * (markdown's HTML blocks), flat among their siblings; this plugin pairs each opener with its
 * closer and wraps the nodes between them — which are ALREADY-PARSED markdown — into the design
 * system's markup. Folding here rather than over raw HTML is what makes a component's children
 * ordinary markdown (lists, tables, fenced code) instead of an unparsed string.
 */

/** Thrown when a component is unknown, misplaced, unclosed, or missing a required attribute. */
export { DocsContentError };

/**
 * Each entry: the element the component renders as, its classes, the attributes it requires,
 * the component it must sit directly inside (`childOf`), and what it may contain (`contains`).
 */
export const COMPONENTS = {
  Note: { tag: "aside", classes: ["docs-aside", "docs-aside--note"], label: "Note" },
  Warning: { tag: "aside", classes: ["docs-aside", "docs-aside--warning"], label: "Warning" },
  Tip: { tag: "aside", classes: ["docs-aside", "docs-aside--tip"], label: "Tip" },
  Steps: { tag: "ol", classes: ["docs-steps"], contains: ["Step"] },
  Step: { tag: "li", classes: ["docs-step"], attributes: ["title"], childOf: "Steps" },
  Tabs: { tag: "div", classes: ["docs-tabs"], contains: ["Tab"] },
  Tab: { tag: "div", classes: ["docs-tab"], attributes: ["title"], childOf: "Tabs" },
  CardGrid: { tag: "div", classes: ["docs-cards"], contains: ["Card"] },
  Card: { tag: "a", classes: ["docs-card"], attributes: ["title", "href"], childOf: "CardGrid" },
  Cta: { tag: "a", classes: ["docs-cta"], attributes: ["title", "href"] },
  Columns: { tag: "div", classes: ["docs-columns"], contains: ["Column"] },
  Column: { tag: "div", classes: ["docs-column"], attributes: ["title"], childOf: "Columns" },
};

export const COMPONENT_NAMES = Object.keys(COMPONENTS);

/** How many tabs the JS-free switch is styled for (app.css enumerates the `:checked` rules). */
export const MAX_TABS = 6;

/** A line that is nothing but one component tag — the shape the source normalizer works on. */
const TAG_LINE = new RegExp(`^<(/?)(${COMPONENT_NAMES.join("|")})(\\s[^>]*?)?\\s*(/?)>$`);
const ATTRIBUTE = /([a-zA-Z][a-zA-Z0-9-]*)\s*=\s*"([^"]*)"/g;
/** Any capitalised tag — an unknown component, which must fail rather than vanish. */
const COMPONENT_LIKE = /^<\/?([A-Z][A-Za-z0-9]*)/;

/**
 * Read one component tag. Returns null when `text` is not one of ours, so callers can tell
 * "not a component" from "a component, malformed" (which throws).
 */
export function parseTag(text, file) {
  const match = text.trim().match(TAG_LINE);
  if (!match) {
    return null;
  }
  const [, closing, name, rawAttributes, selfClosing] = match;
  if (closing && selfClosing) {
    throw new DocsContentError(`${file}: malformed tag ${text.trim()}`);
  }
  const kind = closing ? "close" : selfClosing ? "self" : "open";
  const attributes = {};
  if (rawAttributes && rawAttributes.trim() !== "") {
    if (kind === "close") {
      throw new DocsContentError(`${file}: closing tag </${name}> must carry no attributes`);
    }
    if (rawAttributes.replace(ATTRIBUTE, "").trim() !== "") {
      throw new DocsContentError(
        `${file}: ${text.trim()} — attributes must be spelled name="value" with double quotes`,
      );
    }
    ATTRIBUTE.lastIndex = 0;
    for (const attribute of rawAttributes.matchAll(ATTRIBUTE)) {
      attributes[attribute[1]] = attribute[2];
    }
  }
  return { kind, name, attributes };
}

/** The one thing a typo looks like: a component the set does not have. */
export function assertKnownComponent(text, file) {
  const match = text.trim().match(COMPONENT_LIKE);
  if (match && !(match[1] in COMPONENTS)) {
    throw new DocsContentError(
      `${file}: <${match[1]}> is not a docs component — the set is ${COMPONENT_NAMES.join(", ")}`,
    );
  }
}

/** An mdast node that renders as `tag` with `properties`, wrapping `children`. */
function element(tag, properties, children) {
  return { type: "docsElement", data: { hName: tag, hProperties: properties }, children };
}

function textNode(value) {
  return { type: "text", value };
}

/** A folded component: the marker the second phase turns into markup. */
function marker(name, attributes) {
  return { component: name, attributes, children: [] };
}

function isMarker(node) {
  return node !== null && typeof node === "object" && typeof node.component === "string";
}

/** Whitespace-only text is the gap between two tags, not content. */
function isBlank(node) {
  return node.type === "text" && node.value.trim() === "";
}

/**
 * Phase one: fold the flat `html` leaves of one parent into nested markers. Every parent in the
 * tree is folded, so a component inside a blockquote or a list item works the same way.
 */
function foldChildren(parent, file, counters) {
  const output = [];
  const stack = [];
  const push = (node) => {
    (stack.length > 0 ? stack[stack.length - 1].children : output).push(node);
  };

  for (const child of parent.children) {
    if (child.type !== "html") {
      push(child);
      continue;
    }
    const value = child.value.trim();
    if (value.startsWith("<!--")) {
      continue; // an HTML comment says nothing to a reader; it says nothing here either
    }
    assertKnownComponent(value, file);
    const tag = parseTag(value, file);
    if (tag === null) {
      // Not a component and not a comment: stray markup — an un-quoted `<placeholder>` in prose.
      // It renders as the literal text it reads as, never as live markup.
      push(textNode(child.value));
      continue;
    }
    if (tag.kind === "close") {
      const open = stack.pop();
      if (open === undefined || open.component !== tag.name) {
        throw new DocsContentError(
          `${file}: </${tag.name}> closes ${open === undefined ? "nothing" : `<${open.component}>`}`,
        );
      }
      push(open);
      continue;
    }
    const opened = marker(tag.name, tag.attributes);
    if (tag.name === "Tabs") {
      opened.group = `docs-tabs-${counters.tabs++}`;
    }
    if (tag.kind === "self") {
      push(opened);
      continue;
    }
    stack.push(opened);
  }

  if (stack.length > 0) {
    throw new DocsContentError(`${file}: <${stack[stack.length - 1].component}> is never closed`);
  }
  parent.children = output;
}

/** Phase two: check placement, then turn every marker into mdast elements, deepest first. */
function materialize(node, parentComponent, file) {
  const children = node.children ?? [];
  const spec = parentComponent === null ? null : COMPONENTS[parentComponent];
  const materialized = [];

  for (const child of children) {
    if (!isMarker(child)) {
      if (spec?.contains && !isBlank(child)) {
        throw new DocsContentError(
          `${file}: <${parentComponent}> may only contain <${spec.contains.join("> or <")}>`,
        );
      }
      materialize(child, null, file);
      materialized.push(child);
      continue;
    }
    const childSpec = COMPONENTS[child.component];
    if (childSpec.childOf !== undefined && childSpec.childOf !== parentComponent) {
      throw new DocsContentError(
        `${file}: <${child.component}> must sit directly inside <${childSpec.childOf}>`,
      );
    }
    if (spec?.contains && !spec.contains.includes(child.component)) {
      throw new DocsContentError(
        `${file}: <${parentComponent}> may only contain <${spec.contains.join("> or <")}>`,
      );
    }
    materialize(child, child.component, file);
    materialized.push(child);
  }

  node.children = materialized;
  if (parentComponent !== null) {
    // Replace the marker in place with the element it renders as. A <Tab>'s title outlives the
    // marker: its <Tabs> parent materializes AFTER it and needs the title for the label.
    const title = node.attributes.title;
    const rendered = renderComponent(node, file);
    node.type = rendered.type;
    node.data = rendered.data;
    node.children = rendered.children;
    delete node.component;
    delete node.attributes;
    delete node.group;
    if (title !== undefined) {
      node.tabTitle = title;
    }
  }
}

/** The markup one component becomes, once its children are materialized. */
function renderComponent(node, file) {
  const name = node.component;
  const spec = COMPONENTS[name];
  for (const required of spec.attributes ?? []) {
    if (!node.attributes[required]) {
      throw new DocsContentError(`${file}: <${name}> requires a ${required}="…" attribute`);
    }
  }
  const children = node.children.filter((child) => !isBlank(child));

  if (spec.label !== undefined) {
    return element(spec.tag, { className: spec.classes }, [
      element("p", { className: ["docs-aside__label"] }, [textNode(spec.label)]),
      element("div", { className: ["docs-aside__body"] }, children),
    ]);
  }
  if (name === "Step") {
    return element(spec.tag, { className: spec.classes }, [
      element("p", { className: ["docs-step__title"] }, [textNode(node.attributes.title)]),
      ...children,
    ]);
  }
  if (name === "Card") {
    return element("a", { className: spec.classes, href: node.attributes.href }, [
      element("span", { className: ["docs-card__title"] }, [textNode(node.attributes.title)]),
      element("span", { className: ["docs-card__body"] }, children),
    ]);
  }
  if (name === "Cta") {
    // A self-closing button-styled link: the title IS the label, so the reader's next action
    // renders as one object rather than a sentence.
    return element("a", { className: spec.classes, href: node.attributes.href }, [
      textNode(node.attributes.title),
    ]);
  }
  if (name === "Column") {
    return element(spec.tag, { className: spec.classes }, [
      element("p", { className: ["docs-column__title"] }, [textNode(node.attributes.title)]),
      ...children,
    ]);
  }
  if (name === "Tabs") {
    return renderTabs(node, children, file);
  }
  if (name === "Tab") {
    // A tab's markup is assembled by its <Tabs> parent (label and panel are siblings, so the
    // CSS-only `:checked ~` rule can reach across); standing alone it is just its children.
    return element(spec.tag, { className: spec.classes }, children);
  }
  return element(spec.tag, { className: spec.classes }, children);
}

/**
 * `<Tabs>` without JavaScript: one radio group per tab set, every label and panel a sibling of
 * the radios, and `:checked ~` doing the showing (see app.css). The first tab is checked, so a
 * panel is always visible — including for a reader whose browser never runs a line of script.
 */
function renderTabs(node, children, file) {
  if (children.length > MAX_TABS) {
    throw new DocsContentError(
      `${file}: <Tabs> holds ${children.length} tabs — the JS-free switch is styled for at most ${MAX_TABS}`,
    );
  }
  const controls = [];
  const panels = [];
  for (const [index, child] of children.entries()) {
    if (child.data?.hProperties?.className?.[0] !== "docs-tab") {
      throw new DocsContentError(`${file}: <Tabs> may only contain <Tab> elements`);
    }
    const id = `${node.group}-${index}`;
    const radio = { type: "radio", name: node.group, id, className: ["docs-tabs__radio"] };
    if (index === 0) {
      radio.checked = true;
    }
    controls.push(element("input", radio, []));
    controls.push(
      element("label", { className: ["docs-tabs__label"], htmlFor: id }, [
        textNode(child.tabTitle ?? ""),
      ]),
    );
    panels.push(child);
  }
  return element("div", { className: COMPONENTS.Tabs.classes }, [...controls, ...panels]);
}

/**
 * The plugin. Fold every parent's html leaves into markers, then materialize the whole tree.
 * Anything left unfolded is a content error by construction: an unmatched tag, an unknown
 * component, or raw HTML the docs contract does not allow.
 */
export function remarkDocsComponents(options) {
  const file = options.file;
  const counters = { tabs: 0 };
  return (tree) => {
    const foldDeep = (node) => {
      if (!Array.isArray(node.children)) {
        return;
      }
      for (const child of node.children) {
        foldDeep(child);
      }
      foldChildren(node, file, counters);
    };
    foldDeep(tree);
    materialize(tree, null, file);
  };
}
