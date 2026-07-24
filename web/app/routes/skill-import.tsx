import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, Link, redirect, useActionData, useNavigation } from "react-router";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { requireMemberInScope } from "@/lib/auth/guards.server";
import { mintBundleId } from "@/lib/db/identity.server";
import { inFinalTx, registerGenesisBundleInTx } from "@/lib/db/queries.custody.server";
import { fetchUpstreamTree } from "@/lib/db/upstream.server";
import { publishVersion } from "@/lib/plane/custody.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta() {
  return [{ title: "Add from GitHub" }];
}

/**
 * ADD FROM GITHUB — the web import flow: paste a repository (or subfolder) reference, the
 * SERVER fetches the tree (the public tarball — no token), previews it (SKILL.md lead, file
 * list, license, the pinned commit), and one click publishes it into the workspace WITH
 * upstream provenance — a fork that remembers its parent. From then on the upstream checker
 * watches it: external changes arrive as ordinary proposals, never direct publishes.
 *
 * Two phases in one route: `intent=preview` fetches + discloses (nothing written);
 * `intent=publish` re-fetches AT THE PREVIEWED COMMIT (deterministic — the preview is exactly
 * what lands) and runs the ordinary genesis publish + provenance rows.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace } = await requireMemberInScope(request, params);
  return { wsName: workspace.name };
}

const REPO_SHAPE = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const NAME_SHAPE = /^[a-z0-9][a-z0-9-]*$/;
const COMMIT_SHAPE = /^[0-9a-f]{7,40}$/;

/** Parse the pasted source into (repo, subdir, ref): `owner/repo[/sub/dir]`, a github.com
 * URL, or a `/tree/<ref>/<subdir>` URL — whose REF is honored (the preview fetches that
 * branch/tag, and the publish still pins the resolved commit). Null on anything else. */
function parseSource(raw: string): { repo: string; subdir: string; ref: string } | null {
  let token = raw.trim();
  token = token.replace(/^https?:\/\//, "").replace(/^github\.com\//, "");
  token = token.replace(/\.git$/, "").replace(/\/+$/, "");
  const treeMatch = token.match(/^([^/]+\/[^/]+)\/tree\/([^/]+)(?:\/(.*))?$/);
  if (treeMatch?.[1] !== undefined && treeMatch[2] !== undefined) {
    const repo = treeMatch[1];
    return REPO_SHAPE.test(repo) ? { repo, subdir: treeMatch[3] ?? "", ref: treeMatch[2] } : null;
  }
  const segments = token.split("/").filter((s) => s.length > 0);
  if (segments.length < 2) {
    return null;
  }
  const repo = `${segments[0]}/${segments[1]}`;
  if (!REPO_SHAPE.test(repo)) {
    return null;
  }
  return { repo, subdir: segments.slice(2).join("/"), ref: "HEAD" };
}

interface PreviewData {
  form: "preview";
  repo: string;
  subdir: string;
  commit: string | null;
  license: string | null;
  suggestedName: string;
  files: { path: string; bytes: number }[];
  skillMdLead: string | null;
  error?: string;
}

interface PublishError {
  form: "publish";
  error: string;
}

export async function action({ request, params }: ActionFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");

  if (intent === "preview") {
    const source = parseSource(String(formData.get("source") ?? ""));
    if (source === null) {
      return data<PreviewData>(
        {
          form: "preview",
          repo: "",
          subdir: "",
          commit: null,
          license: null,
          suggestedName: "",
          files: [],
          skillMdLead: null,
          error:
            "That doesn't read as a GitHub reference — `owner/repo`, a repository URL, or a subfolder URL.",
        },
        { status: 400 },
      );
    }
    try {
      const tree = await fetchUpstreamTree(source.repo, source.subdir, source.ref);
      if (tree.files.length === 0) {
        return data<PreviewData>(
          {
            form: "preview",
            repo: source.repo,
            subdir: source.subdir,
            commit: null,
            license: null,
            suggestedName: "",
            files: [],
            skillMdLead: null,
            error: "Nothing there — the path holds no files.",
          },
          { status: 400 },
        );
      }
      const skillMd = tree.files.find((f) => f.path.toLowerCase() === "skill.md");
      const lastSegment =
        source.subdir.length > 0
          ? (source.subdir.split("/").at(-1) ?? "")
          : (source.repo.split("/")[1] ?? "");
      const suggestedName = lastSegment
        .toLowerCase()
        .replaceAll(/[^a-z0-9-]+/g, "-")
        .replaceAll(/^-+|-+$/g, "")
        .slice(0, 60);
      return data<PreviewData>({
        form: "preview",
        repo: source.repo,
        subdir: source.subdir,
        commit: tree.commit,
        license: tree.license,
        suggestedName,
        files: tree.files
          .map((f) => ({ path: f.path, bytes: f.bytes.length }))
          .sort((a, b) => a.path.localeCompare(b.path)),
        skillMdLead: skillMd === undefined ? null : skillMd.bytes.toString("utf8").slice(0, 1200),
      });
    } catch (error) {
      return data<PreviewData>(
        {
          form: "preview",
          repo: source.repo,
          subdir: source.subdir,
          commit: null,
          license: null,
          suggestedName: "",
          files: [],
          skillMdLead: null,
          error: `Fetch failed: ${error instanceof Error ? error.message : "unknown"}`,
        },
        { status: 400 },
      );
    }
  }

  if (intent === "publish") {
    const repo = String(formData.get("repo") ?? "");
    const subdir = String(formData.get("subdir") ?? "");
    const commit = String(formData.get("commit") ?? "");
    const name = String(formData.get("name") ?? "").trim();
    if (!REPO_SHAPE.test(repo) || !NAME_SHAPE.test(name) || !COMMIT_SHAPE.test(commit)) {
      return data<PublishError>(
        { form: "publish", error: "Malformed publish — re-run the preview." },
        { status: 400 },
      );
    }
    // Re-fetch AT the previewed commit: the preview IS what lands (codeload by sha is
    // deterministic), and the provenance records exactly that commit.
    let tree: Awaited<ReturnType<typeof fetchUpstreamTree>>;
    try {
      tree = await fetchUpstreamTree(repo, subdir, commit);
    } catch (error) {
      return data<PublishError>(
        {
          form: "publish",
          error: `Fetch failed: ${error instanceof Error ? error.message : "unknown"}`,
        },
        { status: 500 },
      );
    }
    if (tree.files.length === 0) {
      return data<PublishError>(
        { form: "publish", error: "Nothing there — the path holds no files." },
        { status: 400 },
      );
    }
    const bundleId = mintBundleId();
    const published = await publishVersion(workspace.id, bundleId, {
      files: tree.files.map((f) => ({
        path: f.path,
        mode: f.executable ? "100755" : "100644",
        content_base64: f.bytes.toString("base64"),
      })),
      attribution: actor.display,
      message: `imported from ${repo}@${commit.slice(0, 12)}`,
    });
    if (published.kind !== "ok") {
      return data<PublishError>(
        { form: "publish", error: "The publish did not land — try again." },
        { status: 500 },
      );
    }
    const { sql } = await import("drizzle-orm");
    const registered = await inFinalTx(async (tx) => {
      const registration = await registerGenesisBundleInTx(tx, actor, bundleId, name, null);
      await tx.execute(sql`
        INSERT INTO web.bundle_upstream (bundle_id, workspace_id, host, repo, path, license,
                                         last_seen_commit, last_checked_at)
        VALUES (${bundleId}, ${workspace.id}, 'github.com', ${repo}, ${subdir},
                ${tree.license}, ${commit}, now())
        ON CONFLICT (bundle_id) DO NOTHING
      `);
      await tx.execute(sql`
        INSERT INTO web.version_upstream (workspace_id, bundle_id, version_id, commit)
        VALUES (${workspace.id}, ${bundleId}, ${published.value.version_id}, ${commit})
        ON CONFLICT (bundle_id, version_id) DO NOTHING
      `);
      await tx.execute(sql`
        INSERT INTO web.audit_event (workspace_id, actor_user_id, actor_display, kind, subject,
                                     outcome, details)
        VALUES (${workspace.id}, ${actor.userId}, ${actor.display}, 'skill_imported',
                ${bundleId}, 'ok', ${JSON.stringify({ repo, subdir, commit })}::jsonb)
      `);
      return registration;
    });
    throw redirect(wsPathServer(workspace.name, `skills/${registered.name}`));
  }

  return data<PublishError>({ form: "publish", error: "Unknown action." }, { status: 400 });
}

export default function SkillImport() {
  const actionData = useActionData<typeof action>();
  const navigation = useNavigation();
  const wsPath = useWsPath();
  const busy = navigation.state !== "idle";
  const preview =
    actionData !== undefined && actionData.form === "preview" && actionData.error === undefined
      ? actionData
      : undefined;
  const error = actionData !== undefined && "error" in actionData ? actionData.error : undefined;
  return (
    <div className="space-y-8">
      <PageHeader
        title="Add from GitHub"
        actions={
          <Link to={wsPath("")} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <p className="max-w-2xl text-dim text-sm leading-relaxed">
        Import a skill from a public GitHub repository. The server fetches it, shows you exactly
        what would land, and publishes it into the workspace with its upstream recorded — when the
        repository changes later, the change arrives as a proposal in the review queue, never a
        silent update.
      </p>
      <Form method="post" className="flex max-w-2xl items-end gap-2">
        <input type="hidden" name="intent" value="preview" />
        <label className="block flex-1">
          <span className="mb-1 block font-medium text-dim text-sm">Repository or folder</span>
          <input
            type="text"
            name="source"
            required
            placeholder="vercel-labs/skills or https://github.com/owner/repo/tree/main/skills/x"
            className="block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink placeholder:text-faint focus:border-accent focus:outline-none"
          />
        </label>
        <button type="submit" disabled={busy} className={`${buttonClasses("primary")} min-h-11`}>
          {busy ? "Fetching…" : "Preview"}
        </button>
      </Form>
      {error !== undefined && (
        <p role="alert" className="text-red-700 text-sm">
          {error}
        </p>
      )}
      {preview !== undefined && <PreviewCard preview={preview} />}
    </div>
  );
}

function PreviewCard({ preview }: { preview: PreviewData }) {
  const navigation = useNavigation();
  const busy = navigation.state !== "idle";
  return (
    <section aria-labelledby="import-preview-heading" className="max-w-2xl space-y-3">
      <SectionHeading>
        <span id="import-preview-heading">What would land</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3" data-testid="import-preview">
        <p className="text-dim text-sm">
          <span className="font-mono text-[13px] text-ink">
            {preview.repo}
            {preview.subdir.length > 0 ? `/${preview.subdir}` : ""}
          </span>
          {preview.commit !== null && (
            <>
              {" "}
              @ <code className="font-mono text-[13px]">{preview.commit.slice(0, 12)}</code>
            </>
          )}
          {" · "}
          {preview.files.length === 1 ? "1 file" : `${preview.files.length} files`}
          {" · license: "}
          {preview.license ?? "none found"}
        </p>
        {preview.skillMdLead !== null ? (
          <pre className="max-h-64 overflow-auto rounded bg-panel2 p-3 font-mono text-[12px] text-dim leading-relaxed">
            {preview.skillMdLead}
          </pre>
        ) : (
          <p className="text-faint text-xs">No SKILL.md at the root — agents may not pick it up.</p>
        )}
        <ul className="max-h-40 overflow-auto text-faint text-xs">
          {preview.files.map((f) => (
            <li key={f.path} className="font-mono">
              {f.path} <span className="text-faint">({f.bytes} B)</span>
            </li>
          ))}
        </ul>
        {preview.commit === null ? (
          <p className="text-red-700 text-sm">
            The archive carried no commit stamp — try again (the publish pins an exact commit).
          </p>
        ) : (
          <Form method="post" className="flex items-end gap-2">
            <input type="hidden" name="intent" value="publish" />
            <input type="hidden" name="repo" value={preview.repo} />
            <input type="hidden" name="subdir" value={preview.subdir} />
            <input type="hidden" name="commit" value={preview.commit} />
            <label className="block flex-1">
              <span className="mb-1 block font-medium text-dim text-sm">Publish as</span>
              <input
                type="text"
                name="name"
                required
                defaultValue={preview.suggestedName}
                pattern="[a-z0-9][a-z0-9-]*"
                className="block h-11 w-full rounded-md border border-line px-3 font-mono text-[13px] text-ink focus:border-accent focus:outline-none"
              />
            </label>
            <button
              type="submit"
              disabled={busy}
              className={`${buttonClasses("primary")} min-h-11`}
            >
              {busy ? "Publishing…" : "Publish to the workspace"}
            </button>
          </Form>
        )}
      </Card>
    </section>
  );
}
