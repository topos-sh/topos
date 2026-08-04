import { STREAMABLE_HTTP } from "@/lib/mcp/validate.server";

/**
 * THE BUILT-IN LIST — the popular remote MCP servers the import page offers as a picker, so the
 * common case is choosing a name off a list instead of knowing an address by heart.
 *
 * Plain committed data. Nothing here is fetched at build time and no entry carries code: the
 * picker reads it, and choosing one hands the SAME preview → publish flow the same kind of bytes
 * a paste would have. Every entry goes through the ordinary gate
 * (app/lib/mcp/validate.server.ts) on the way in and again on the way out — there is no arm here
 * that skips it, and the unit suite drives every entry through that gate so a bad row fails CI
 * rather than a member's publish.
 *
 * TWO RULES decide membership, and they are the same two the gate enforces:
 *
 *  · REMOTE — the vendor documents an https `streamable-http` endpoint. Nothing installs locally.
 *  · SECRETLESS — a person gets in by signing in through their own agent (`oauth`), or the server
 *    needs no credential at all (`none`). A server whose documented setup is "paste this API key"
 *    is deliberately absent: this list only holds servers a whole team can receive as-is.
 *
 * The `name` is the bundle's IDENTITY and the key the registry read lane resolves by. Where the
 * official registry carries the server, the name here is the one it publishes, so a workspace's
 * copy and upstream agree on what the thing is called. Where it does not, the name is built from
 * the vendor's own domain in the registry's reverse-DNS grammar — the same shape a first-party
 * publication would take.
 *
 * The DOCUMENT is Topos's own minimal construction — name, description, version, the one remote —
 * not a mirror of anyone's release, which is why every entry carries the same [`CURATED_VERSION`]
 * rather than a vendor version number it would immediately fall behind. A team that wants
 * upstream's exact bytes still has the registry-name arm on the same page.
 */

/** What the picker renders, and what publishing needs. One row per server, no code. */
export interface CuratedMcpServer {
  /** The document's registry name — this bundle's identity, unique across the list. */
  name: string;
  /** The catalog name the publish step suggests (the tail of `name` is usually just "mcp"). */
  slug: string;
  /** The product's own name, as the picker's row title. */
  title: string;
  /** One line, ≤100 chars: what an agent reaches through it. Doubles as the document's own. */
  description: string;
  /**
   * How a person gets in once the bundle lands on their machine: `oauth` — the agent runs its own
   * authorization dance on first use, which happens on that machine and never here — or `none`.
   * It rides the document as `_meta["sh.topos/auth"]`, so the preview says the same thing.
   */
  auth: "oauth" | "none";
  /** The remote `streamable-http` endpoint, as the vendor documents it. */
  url: string;
}

/**
 * The version every curated document carries. It is the version of THIS document — a minimal
 * construction that names one endpoint — and deliberately not the vendor's release number, which
 * this file could only ever hold a stale copy of.
 */
export const CURATED_VERSION = "1.0.0";

/**
 * The servers themselves, alphabetical by title so the picker's default order is one a person can
 * scan. Adding one is a data edit plus a green unit suite; there is nothing else to wire.
 */
export const CURATED_MCP_SERVERS: readonly CuratedMcpServer[] = [
  {
    name: "com.airtable/mcp",
    slug: "airtable",
    title: "Airtable",
    description: "Bases, tables and records in Airtable.",
    auth: "oauth",
    url: "https://mcp.airtable.com/mcp",
  },
  {
    name: "com.asana/mcp",
    slug: "asana",
    title: "Asana",
    description: "Tasks, projects and portfolios in Asana.",
    auth: "oauth",
    url: "https://mcp.asana.com/v2/mcp",
  },
  {
    name: "com.atlassian/atlassian-mcp-server",
    slug: "atlassian",
    title: "Atlassian",
    description: "Jira issues and Confluence pages, through Atlassian's Rovo server.",
    auth: "oauth",
    url: "https://mcp.atlassian.com/v1/mcp/authv2",
  },
  {
    name: "com.canva/mcp",
    slug: "canva",
    title: "Canva",
    description: "Designs, folders and brand assets in Canva.",
    auth: "oauth",
    url: "https://mcp.canva.com/mcp",
  },
  {
    name: "com.clickup/mcp",
    slug: "clickup",
    title: "ClickUp",
    description: "Tasks, lists and docs in a ClickUp workspace.",
    auth: "oauth",
    url: "https://mcp.clickup.com/mcp",
  },
  {
    name: "com.cloudflare.mcp/mcp",
    slug: "cloudflare-docs",
    title: "Cloudflare Docs",
    description: "Search Cloudflare's product documentation.",
    auth: "none",
    url: "https://docs.mcp.cloudflare.com/mcp",
  },
  {
    name: "com.context7/mcp",
    slug: "context7",
    title: "Context7",
    description: "Up-to-date documentation and code examples for libraries.",
    auth: "none",
    url: "https://mcp.context7.com/mcp",
  },
  {
    name: "com.deepwiki/mcp",
    slug: "deepwiki",
    title: "DeepWiki",
    description: "Ask questions about any public GitHub repository.",
    auth: "none",
    url: "https://mcp.deepwiki.com/mcp",
  },
  {
    name: "com.dropbox/mcp",
    slug: "dropbox",
    title: "Dropbox",
    description: "Files and folders in a Dropbox account.",
    auth: "oauth",
    url: "https://mcp.dropbox.com/mcp",
  },
  {
    name: "com.figma.mcp/mcp",
    slug: "figma",
    title: "Figma",
    description: "Design files, components and variables in Figma.",
    auth: "oauth",
    url: "https://mcp.figma.com/mcp",
  },
  {
    name: "com.github/mcp",
    slug: "github",
    title: "GitHub",
    description: "Repositories, issues, pull requests and code search on GitHub.",
    auth: "oauth",
    url: "https://api.githubcopilot.com/mcp/",
  },
  {
    name: "com.gitlab/mcp",
    slug: "gitlab",
    title: "GitLab",
    description: "Projects, issues and merge requests on GitLab.com.",
    auth: "oauth",
    url: "https://gitlab.com/api/v4/mcp",
  },
  {
    name: "com.grafana/mcp",
    slug: "grafana",
    title: "Grafana Cloud",
    description: "Dashboards, datasources and alerts in Grafana Cloud.",
    auth: "oauth",
    url: "https://mcp.grafana.com/mcp",
  },
  {
    name: "com.intercom/mcp",
    slug: "intercom",
    title: "Intercom",
    description: "Conversations, contacts and help articles in Intercom.",
    auth: "oauth",
    url: "https://mcp.intercom.com/mcp",
  },
  {
    name: "app.linear/linear",
    slug: "linear",
    title: "Linear",
    description: "Issues, projects and cycles in Linear.",
    auth: "oauth",
    url: "https://mcp.linear.app/mcp",
  },
  {
    name: "com.microsoft.learn/mcp",
    slug: "microsoft-learn",
    title: "Microsoft Learn",
    description: "Search Microsoft and Azure technical documentation.",
    auth: "none",
    url: "https://learn.microsoft.com/api/mcp",
  },
  {
    name: "tech.neon/mcp",
    slug: "neon",
    title: "Neon",
    description: "Postgres projects, branches and queries on Neon.",
    auth: "oauth",
    url: "https://mcp.neon.tech/mcp",
  },
  {
    name: "com.netlify/mcp",
    slug: "netlify",
    title: "Netlify",
    description: "Sites, deploys and build logs on Netlify.",
    auth: "oauth",
    url: "https://netlify-mcp.netlify.app/mcp",
  },
  {
    name: "com.notion/mcp",
    slug: "notion",
    title: "Notion",
    description: "Pages, databases and comments in a Notion workspace.",
    auth: "oauth",
    url: "https://mcp.notion.com/mcp",
  },
  {
    name: "com.pagerduty/mcp",
    slug: "pagerduty",
    title: "PagerDuty",
    description: "Incidents, services and on-call schedules in PagerDuty.",
    auth: "oauth",
    url: "https://mcp.pagerduty.com/mcp",
  },
  {
    name: "io.sentry/mcp",
    slug: "sentry",
    title: "Sentry",
    description: "Errors, issues and releases in Sentry.",
    auth: "oauth",
    url: "https://mcp.sentry.dev/mcp",
  },
  {
    name: "com.slack/mcp",
    slug: "slack",
    title: "Slack",
    description: "Messages, channels and files in a Slack workspace.",
    auth: "oauth",
    url: "https://mcp.slack.com/mcp",
  },
  {
    name: "com.stripe/mcp",
    slug: "stripe",
    title: "Stripe",
    description: "Customers, payments, subscriptions and invoices in Stripe.",
    auth: "oauth",
    url: "https://mcp.stripe.com",
  },
  {
    name: "com.supabase/mcp",
    slug: "supabase",
    title: "Supabase",
    description: "Projects, database schema and logs on Supabase.",
    auth: "oauth",
    url: "https://mcp.supabase.com/mcp",
  },
  {
    name: "com.vercel/vercel-mcp",
    slug: "vercel",
    title: "Vercel",
    description: "Projects, deployments and logs on Vercel.",
    auth: "oauth",
    url: "https://mcp.vercel.com",
  },
];

/** By name, for the arm that turns a picked row back into a document. */
const BY_NAME = new Map(CURATED_MCP_SERVERS.map((entry) => [entry.name, entry]));

/** The entry a picked row names, or undefined — an unknown id is refused, never guessed at. */
export function curatedServerByName(name: string): CuratedMcpServer | undefined {
  return BY_NAME.get(name);
}

/**
 * The server document one curated entry stands for — the object, not the bytes: the caller
 * canonicalizes it exactly as it canonicalizes a fetched or pasted one, so a curated import and a
 * hand-pasted copy of the same server converge on one version id.
 */
export function curatedServerDocument(entry: CuratedMcpServer): Record<string, unknown> {
  return {
    name: entry.name,
    description: entry.description,
    version: CURATED_VERSION,
    remotes: [{ type: STREAMABLE_HTTP, url: entry.url }],
    _meta: { "sh.topos/auth": entry.auth },
  };
}

/** One row as the picker renders it — the host is shown so the address is visible before a click. */
export interface CuratedMcpRow {
  name: string;
  title: string;
  description: string;
  auth: "oauth" | "none";
  host: string;
}

/** What the loader hands the page: display fields only, in the list's own order. */
export function curatedServerRows(): CuratedMcpRow[] {
  return CURATED_MCP_SERVERS.map((entry) => ({
    name: entry.name,
    title: entry.title,
    description: entry.description,
    auth: entry.auth,
    host: new URL(entry.url).host,
  }));
}
