import { canonicalServerJson } from "@/lib/mcp/fetch.server";
import { STREAMABLE_HTTP } from "@/lib/mcp/validate.server";

/**
 * THE BUILT-IN LIST — the popular remote MCP servers the add-a-server page offers as a picker, so
 * the common case is choosing a name off a list instead of knowing an address by heart.
 *
 * Plain committed data. Nothing here is fetched at build time and no entry carries code: the
 * picker reads it, and choosing one publishes the SAME kind of bytes a paste would have. Every
 * entry goes through the ordinary gate (app/lib/mcp/validate.server.ts) on the way out — there is
 * no arm here that skips it, and the unit suite drives every entry through that gate so a bad row
 * fails CI rather than a member's publish.
 *
 * TWO RULES decide membership:
 *
 *  · REMOTE — the vendor documents an https `streamable-http` endpoint. Nothing installs locally.
 *    This one the gate enforces on the way out, like it does for a pasted document.
 *  · HOW A PERSON GETS IN, told in the row rather than discovered on the machine. Two tiers, and
 *    the difference between them is the difference between a team receiving a server and a team
 *    receiving a chore:
 *
 *      SELF-SERVICE is the default and most of this list. Either the server asks for nothing
 *      (`none`), or an agent can complete the entire sign-in by itself (`oauth`). `oauth` here is
 *      a claim about the WIRE, not a reading of anyone's docs: it means the endpoint's own
 *      discovery chain was walked and its authorization server was found to advertise dynamic
 *      client registration, so an agent registers itself and finishes the dance with nobody's
 *      help. That check is a live one, run against the vendor before a row lands here and re-run
 *      when a verdict is in doubt — nothing in this file touches the network, at build time or
 *      ever, so the word is only ever as good as the last check behind it.
 *
 *      MANUAL is the other tier: a token each person mints, or an app an admin registers first.
 *      Those servers are here because a team already living in GitHub or Slack is not served by
 *      pretending they do not exist — but a `manual` row is admitted ONLY carrying an
 *      [`authNote`] saying in one line what the person has to do. The picker prints it on the
 *      row and the confirm dialog repeats it, so the work is visible before the click rather
 *      than discovered by an agent that cannot sign in. The type refuses both mistakes — a
 *      `manual` row with no note, a note on a row that needs none — and the unit suite says the
 *      same thing again at runtime.
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

/**
 * What the picker renders and what publishing needs, minus the sign-in half — one row per server,
 * no code. The two halves are separate types because only one of them has a rule to hold.
 */
interface CuratedMcpServerFields {
  /** The document's registry name — this bundle's identity, unique across the list. */
  name: string;
  /** The catalog name the publish step suggests (the tail of `name` is usually just "mcp"). */
  slug: string;
  /** The product's own name, as the picker's row title. */
  title: string;
  /** One line, ≤100 chars: what an agent reaches through it. Doubles as the document's own. */
  description: string;
  /** The remote `streamable-http` endpoint, as the vendor documents it. */
  url: string;
  /**
   * The vendored brand mark this row flies, keyed into [`MCP_BRAND_MARKS`]. OPTIONAL on purpose:
   * the icon set carries most of these brands and not all of them, and a row the set has no mark
   * for stays without one rather than acquiring a drawn-here approximation. Absent is a stated
   * fact about the icon set, never a gap to fill.
   */
  logo?: string;
}

/**
 * A row nobody has to prepare anything for. `oauth` — the agent runs its own authorization dance
 * on first use, which happens on that machine and never here — or `none`, no credential at all.
 * The word rides the document as `_meta["sh.topos/auth"]`, so the preview says the same thing.
 *
 * `authNote` is typed `never` rather than left off: a note on a self-service row would be copy
 * describing work that does not exist, and this is the cheapest place to make that unwritable.
 */
interface CuratedSelfServiceServer extends CuratedMcpServerFields {
  auth: "oauth" | "none";
  authNote?: never;
}

/**
 * A row that costs a person a one-time step on each machine — a token to mint, an app an admin
 * registers — because the vendor runs no self-registration an agent could use. The note is
 * REQUIRED and it is the whole reason the row is allowed to be here: one line, in the second
 * person's terms, naming the thing that has to happen. It is picker copy and nothing more —
 * it does not ride the document, because it is this list's editorial answer and not the
 * publisher's declaration.
 */
interface CuratedManualServer extends CuratedMcpServerFields {
  auth: "manual";
  authNote: string;
}

export type CuratedMcpServer = CuratedSelfServiceServer | CuratedManualServer;

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
    logo: "airtable",
  },
  {
    name: "co.alphavantage/mcp",
    slug: "alpha-vantage",
    title: "Alpha Vantage",
    description: "Market data and financial indicators from Alpha Vantage.",
    auth: "oauth",
    url: "https://mcp.alphavantage.co/mcp",
  },
  {
    name: "com.amplitude/mcp-server",
    slug: "amplitude",
    title: "Amplitude",
    description: "Product analytics in Amplitude.",
    auth: "oauth",
    url: "https://mcp.amplitude.com/mcp",
  },
  {
    name: "com.apify/mcp",
    slug: "apify",
    title: "Apify",
    description: "Run scrapers and automation actors on Apify.",
    auth: "oauth",
    url: "https://mcp.apify.com/",
  },
  {
    name: "com.asana/mcp",
    slug: "asana",
    title: "Asana",
    description: "Tasks, projects and portfolios in Asana.",
    auth: "manual",
    authNote:
      "Needs an OAuth app your admin registers with Asana — agents can't sign in by themselves.",
    url: "https://mcp.asana.com/v2/mcp",
    logo: "asana",
  },
  {
    name: "com.atlassian/atlassian-mcp-server",
    slug: "atlassian",
    title: "Atlassian",
    description: "Jira issues and Confluence pages, through Atlassian's Rovo server.",
    auth: "oauth",
    url: "https://mcp.atlassian.com/v1/mcp/authv2",
    logo: "atlassian",
  },
  {
    name: "com.amazon.aws/knowledge",
    slug: "aws-knowledge",
    title: "AWS Knowledge",
    description: "AWS documentation and API references.",
    auth: "none",
    url: "https://knowledge-mcp.global.api.aws",
  },
  {
    name: "com.brightdata/mcp",
    slug: "bright-data",
    title: "Bright Data",
    description: "Web data collection through Bright Data.",
    auth: "oauth",
    url: "https://mcp.brightdata.com/mcp",
  },
  {
    name: "com.calendly/mcp",
    slug: "calendly",
    title: "Calendly",
    description: "Scheduling links and booked events in Calendly.",
    auth: "oauth",
    url: "https://mcp.calendly.com/",
    logo: "calendly",
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
    logo: "clickup",
  },
  {
    name: "com.cloudflare.mcp/mcp",
    slug: "cloudflare-docs",
    title: "Cloudflare Docs",
    description: "Search Cloudflare's product documentation.",
    auth: "none",
    url: "https://docs.mcp.cloudflare.com/mcp",
    logo: "cloudflare",
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
    name: "com.datadoghq/mcp",
    slug: "datadog",
    title: "Datadog",
    description: "Metrics, monitors and incidents in Datadog.",
    auth: "oauth",
    url: "https://mcp.datadoghq.com/v1/mcp",
    logo: "datadog",
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
    logo: "dropbox",
  },
  {
    name: "ai.exa/exa",
    slug: "exa",
    title: "Exa",
    description: "Semantic web search via Exa.",
    auth: "none",
    url: "https://mcp.exa.ai/mcp",
  },
  {
    name: "com.figma.mcp/mcp",
    slug: "figma",
    title: "Figma",
    description: "Design files, components and variables in Figma.",
    auth: "oauth",
    url: "https://mcp.figma.com/mcp",
    logo: "figma",
  },
  {
    name: "dev.firecrawl/mcp",
    slug: "firecrawl",
    title: "Firecrawl",
    description: "Scrape and crawl web pages into clean, agent-readable text.",
    auth: "none",
    url: "https://mcp.firecrawl.dev/mcp",
  },
  {
    name: "com.github/mcp",
    slug: "github",
    title: "GitHub",
    description: "Repositories, issues, pull requests and code search on GitHub.",
    auth: "manual",
    authNote:
      "Needs a GitHub personal access token per person — agents can't sign in by themselves.",
    url: "https://api.githubcopilot.com/mcp/",
    logo: "github",
  },
  {
    name: "com.gitlab/mcp",
    slug: "gitlab",
    title: "GitLab",
    description: "Projects, issues and merge requests on GitLab.com.",
    auth: "oauth",
    url: "https://gitlab.com/api/v4/mcp",
    logo: "gitlab",
  },
  {
    name: "com.grafana/mcp",
    slug: "grafana",
    title: "Grafana Cloud",
    description: "Dashboards, datasources and alerts in Grafana Cloud.",
    auth: "oauth",
    url: "https://mcp.grafana.com/mcp",
    logo: "grafana",
  },
  {
    name: "com.heroku/mcp",
    slug: "heroku",
    title: "Heroku",
    description: "Apps, pipelines and add-ons on Heroku.",
    auth: "oauth",
    url: "https://mcp.heroku.com/mcp",
  },
  {
    name: "co.huggingface/hf-mcp-server",
    slug: "hugging-face",
    title: "Hugging Face",
    description: "Models, datasets and Spaces on the Hugging Face Hub.",
    auth: "none",
    url: "https://huggingface.co/mcp",
    logo: "huggingface",
  },
  {
    name: "com.intercom/mcp",
    slug: "intercom",
    title: "Intercom",
    description: "Conversations, contacts and help articles in Intercom.",
    auth: "oauth",
    url: "https://mcp.intercom.com/mcp",
    logo: "intercom",
  },
  {
    name: "com.langchain/langsmith",
    slug: "langsmith",
    title: "LangSmith",
    description: "Traces, datasets and prompts in LangSmith.",
    auth: "oauth",
    url: "https://api.smith.langchain.com/mcp",
    logo: "langchain",
  },
  {
    name: "app.linear/linear",
    slug: "linear",
    title: "Linear",
    description: "Issues, projects and cycles in Linear.",
    auth: "oauth",
    url: "https://mcp.linear.app/mcp",
    logo: "linear",
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
    name: "com.mixpanel/mcp",
    slug: "mixpanel",
    title: "Mixpanel",
    description: "Product analytics in Mixpanel.",
    auth: "oauth",
    url: "https://mcp.mixpanel.com/mcp",
    logo: "mixpanel",
  },
  {
    name: "com.monday/monday.com",
    slug: "monday",
    title: "monday.com",
    description: "Boards, items and docs in monday.com.",
    auth: "oauth",
    url: "https://mcp.monday.com/mcp",
  },
  {
    name: "tech.neon/mcp",
    slug: "neon",
    title: "Neon",
    description: "Postgres projects, branches and queries on Neon.",
    auth: "oauth",
    url: "https://mcp.neon.tech/mcp",
    logo: "neon",
  },
  {
    name: "com.netlify/mcp",
    slug: "netlify",
    title: "Netlify",
    description: "Sites, deploys and build logs on Netlify.",
    auth: "oauth",
    url: "https://netlify-mcp.netlify.app/mcp",
    logo: "netlify",
  },
  {
    name: "com.newrelic/mcp-server",
    slug: "new-relic",
    title: "New Relic",
    description: "Telemetry, alerts and dashboards in New Relic.",
    auth: "oauth",
    url: "https://mcp.newrelic.com/mcp",
    logo: "newrelic",
  },
  {
    name: "com.notion/mcp",
    slug: "notion",
    title: "Notion",
    description: "Pages, databases and comments in a Notion workspace.",
    auth: "oauth",
    url: "https://mcp.notion.com/mcp",
    logo: "notion",
  },
  {
    name: "com.pagerduty/mcp",
    slug: "pagerduty",
    title: "PagerDuty",
    description: "Incidents, services and on-call schedules in PagerDuty.",
    auth: "manual",
    authNote: "Needs a PagerDuty API token per person — agents can't sign in by themselves.",
    url: "https://mcp.pagerduty.com/mcp",
    logo: "pagerduty",
  },
  {
    name: "com.paypal.mcp/mcp",
    slug: "paypal",
    title: "PayPal",
    description: "Payments, invoices and orders in PayPal.",
    auth: "oauth",
    url: "https://mcp.paypal.com/mcp",
    logo: "paypal",
  },
  {
    name: "com.posthog/mcp",
    slug: "posthog",
    title: "PostHog",
    description: "Product analytics, feature flags and session replay in PostHog.",
    auth: "oauth",
    url: "https://mcp.posthog.com/mcp",
    logo: "posthog",
  },
  {
    name: "com.postman/postman-mcp-server",
    slug: "postman",
    title: "Postman",
    description: "Collections, APIs and workspaces in Postman.",
    auth: "oauth",
    url: "https://mcp.postman.com/mcp",
    logo: "postman",
  },
  {
    name: "com.railway/mcp",
    slug: "railway",
    title: "Railway",
    description: "Services, deployments and logs on Railway.",
    auth: "oauth",
    url: "https://mcp.railway.com/",
    logo: "railway",
  },
  {
    name: "io.sanity.www/mcp",
    slug: "sanity",
    title: "Sanity",
    description: "Content, schemas and datasets in Sanity.",
    auth: "oauth",
    url: "https://mcp.sanity.io",
    logo: "sanity",
  },
  {
    name: "io.sentry/mcp",
    slug: "sentry",
    title: "Sentry",
    description: "Errors, issues and releases in Sentry.",
    auth: "oauth",
    url: "https://mcp.sentry.dev/mcp",
    logo: "sentry",
  },
  {
    name: "com.slack/mcp",
    slug: "slack",
    title: "Slack",
    description: "Messages, channels and files in a Slack workspace.",
    auth: "manual",
    authNote: "Needs a Slack app your admin registers — agents can't sign in by themselves.",
    url: "https://mcp.slack.com/mcp",
  },
  {
    name: "com.squareup/mcp",
    slug: "square",
    title: "Square",
    description: "Payments, catalog and customers in Square.",
    auth: "oauth",
    url: "https://mcp.squareup.com/mcp",
    logo: "square",
  },
  {
    name: "com.stripe/mcp",
    slug: "stripe",
    title: "Stripe",
    description: "Customers, payments, subscriptions and invoices in Stripe.",
    auth: "oauth",
    url: "https://mcp.stripe.com",
    logo: "stripe",
  },
  {
    name: "com.supabase/mcp",
    slug: "supabase",
    title: "Supabase",
    description: "Projects, database schema and logs on Supabase.",
    auth: "oauth",
    url: "https://mcp.supabase.com/mcp",
    logo: "supabase",
  },
  {
    name: "com.tavily/mcp",
    slug: "tavily",
    title: "Tavily",
    description: "Web search and content extraction for agents via Tavily.",
    auth: "oauth",
    url: "https://mcp.tavily.com/mcp",
  },
  {
    name: "net.todoist/mcp",
    slug: "todoist",
    title: "Todoist",
    description: "Tasks and projects in Todoist.",
    auth: "oauth",
    url: "https://ai.todoist.net/mcp",
    logo: "todoist",
  },
  {
    name: "com.vercel/vercel-mcp",
    slug: "vercel",
    title: "Vercel",
    description: "Projects, deployments and logs on Vercel.",
    auth: "oauth",
    url: "https://mcp.vercel.com",
    logo: "vercel",
  },
  {
    name: "com.webflow/mcp",
    slug: "webflow",
    title: "Webflow",
    description: "Sites, CMS collections and pages in Webflow.",
    auth: "oauth",
    url: "https://mcp.webflow.com/mcp",
    logo: "webflow",
  },
  {
    name: "com.wix/mcp",
    slug: "wix",
    title: "Wix",
    description: "Sites and business data in Wix.",
    auth: "oauth",
    url: "https://mcp.wix.com/mcp",
    logo: "wix",
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

/**
 * One row as the picker renders it, everything but the sign-in half below — the host is shown so
 * the address is visible before a click, and the row carries its own canonical `server.json`
 * bytes. The bytes ride along deliberately:
 * choosing a row asks "add this?" and answers "here is exactly what would land" in the same
 * instant, and a round trip to fetch a document this process has had committed since build time
 * is a round trip in front of a question the page can already answer. Nothing here is secret —
 * this file is source — and the publish arm re-derives the bytes from the list rather than
 * trusting the ones that come back.
 */
interface CuratedMcpRowFields {
  name: string;
  slug: string;
  title: string;
  description: string;
  host: string;
  url: string;
  version: string;
  transport: typeof STREAMABLE_HTTP;
  document: string;
  /** The brand mark key, when the icon set carries this brand — see [`CuratedMcpServer.logo`]. */
  logo?: string;
}

/**
 * The row carries the entry's sign-in half as the SAME union the list holds it in, rather than
 * flattening it into two independent fields. A page holding `auth: "manual"` can then read the
 * note without a null check, and no code path can render a manual row that forgot to say what
 * the person must do.
 */
export type CuratedMcpRow = CuratedMcpRowFields &
  ({ auth: "oauth" | "none"; authNote?: never } | { auth: "manual"; authNote: string });

/** What the loader hands the page: every row, in the list's own order. */
export function curatedServerRows(): CuratedMcpRow[] {
  return CURATED_MCP_SERVERS.map((entry) => {
    const row: CuratedMcpRowFields = {
      name: entry.name,
      slug: entry.slug,
      title: entry.title,
      description: entry.description,
      host: new URL(entry.url).host,
      url: entry.url,
      version: CURATED_VERSION,
      transport: STREAMABLE_HTTP,
      document: canonicalServerJson(curatedServerDocument(entry)),
      // Spread rather than assigned: `exactOptionalPropertyTypes` aside, a row for a brand the
      // icon set does not carry should have no `logo` key at all, not one holding `undefined`.
      ...(entry.logo === undefined ? {} : { logo: entry.logo }),
    };
    // Branched, not spread: the discriminant and its note travel together or the union would be
    // satisfiable by a row that claims `manual` and carries nothing to show for it.
    return entry.auth === "manual"
      ? { ...row, auth: entry.auth, authNote: entry.authNote }
      : { ...row, auth: entry.auth };
  });
}

/**
 * The exact bytes a picked row publishes, or `null` when this list holds no such row. The publish
 * arm reads this instead of the form's document field: a pick posts an id, and an id this list
 * does not hold is refused here rather than turned into a document.
 */
export function curatedDocumentFor(name: string): string | null {
  const entry = curatedServerByName(name);
  return entry === undefined ? null : canonicalServerJson(curatedServerDocument(entry));
}
