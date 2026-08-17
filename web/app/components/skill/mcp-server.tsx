import { Form, useNavigation } from "react-router";
import { McpMark } from "@/components/mcp-mark";
import { BusyFields, buttonClasses, Card, Chip, SectionHeading } from "@/components/ui";
import type { McpProbeRecord } from "@/lib/mcp/probe-state";
import { probeStateLine } from "@/lib/mcp/probe-state";

/**
 * WHAT A CONNECTED SERVER IS — the face of a `kind: 'mcp'` bundle.
 *
 * A skill's face shows its files, because a skill IS files. A server is an address in a catalog,
 * so this shows the address, how a person gets in, whether anything answered when this plane last
 * asked, and which revision the workspace's machines are actually being handed. Nothing here is a
 * file tree, and nothing pretends a version is a commit.
 */

export interface McpServerView {
  serverId: string;
  isPrivate: boolean;
  registryName: string | null;
  displayName: string;
  description: string | null;
  websiteUrl: string | null;
  icon: string | null;
  authMode: "oauth" | "none" | "manual" | null;
  authNote: string | null;
  pinnedRevisionId: string | null;
  currentRevisionId: string | null;
  resolved: {
    revisionId: string;
    upstreamVersion: string;
    status: string;
    url: string | null;
    transport: string | null;
    document: string;
    probe: McpProbeRecord | null;
  } | null;
  revisions: {
    revisionId: string;
    seq: number;
    upstreamVersion: string;
    status: string;
    source: string;
    publishedAt: string | null;
    publishedBy: string | null;
  }[];
}

/** The sign-in tier, worded once. A row where nobody established one carries no chip: silence is
 *  the honest answer, not "no sign-in". */
function AuthChip({ auth }: { auth: McpServerView["authMode"] }) {
  if (auth === null) {
    return null;
  }
  if (auth === "manual") {
    return <Chip tone="pending">manual setup</Chip>;
  }
  return auth === "oauth" ? (
    <Chip tone="accent">oauth</Chip>
  ) : (
    <Chip tone="neutral">no sign-in</Chip>
  );
}

export function McpServerPanel({ server, isOwner }: { server: McpServerView; isOwner: boolean }) {
  return (
    <div className="space-y-6">
      <section aria-labelledby="mcp-server-heading" className="space-y-3">
        <SectionHeading>
          <span id="mcp-server-heading">The server</span>
        </SectionHeading>
        <Card className="space-y-3 px-4 py-3">
          <div
            data-testid="mcp-server-panel"
            className="flex flex-wrap items-center gap-x-2 gap-y-1"
          >
            <McpMark logo={server.icon ?? undefined} className="size-4" />
            <span className="font-mono text-[13px] text-ink">
              {server.registryName ?? server.displayName}
            </span>
            {server.resolved?.transport !== null && server.resolved !== null && (
              <Chip tone="neutral">{server.resolved.transport}</Chip>
            )}
            <AuthChip auth={server.authMode} />
            {server.isPrivate && <Chip tone="neutral">this workspace only</Chip>}
          </div>
          {server.description !== null && <p className="text-dim text-sm">{server.description}</p>}
          {server.resolved?.url != null && (
            <p className="break-all font-mono text-[13px] text-dim" data-testid="mcp-server-url">
              {server.resolved.url}
            </p>
          )}
          {server.authMode === "manual" && server.authNote !== null && (
            <p className="text-faint text-xs leading-relaxed" data-testid="mcp-server-auth-note">
              {server.authNote}
            </p>
          )}
          <p className="text-dim text-sm" data-testid="mcp-probe">
            {probeStateLine(server.resolved?.probe ?? null)}
          </p>
        </Card>
      </section>

      <section aria-labelledby="mcp-connection-heading" className="space-y-3">
        <SectionHeading>
          <span id="mcp-connection-heading">This workspace</span>
        </SectionHeading>
        <Card className="space-y-2 px-4 py-3">
          <div data-testid="mcp-connection" className="space-y-2">
            {server.resolved === null ? (
              <p className="text-dim text-sm">
                Nothing is published for this server yet, so it delivers nothing.
              </p>
            ) : (
              <>
                <p className="text-dim text-sm">
                  {server.pinnedRevisionId === null
                    ? `Following the current version, ${server.resolved.upstreamVersion}.`
                    : `Pinned to ${server.resolved.upstreamVersion}.`}
                </p>
                {server.resolved.status === "revoked" && (
                  <p className="text-amber-800 text-sm" data-testid="mcp-revoked">
                    This version was withdrawn after it was published. It is still being delivered
                    here, because the pin asked for exactly it.
                  </p>
                )}
              </>
            )}
          </div>
        </Card>
      </section>

      <RevisionList server={server} />
      {isOwner && server.isPrivate && server.resolved !== null && (
        <EditServerForm document={server.resolved.document} />
      )}
    </div>
  );
}

/** The version history — what exists, and which one this workspace receives. */
function RevisionList({ server }: { server: McpServerView }) {
  return (
    <section aria-labelledby="mcp-revisions-heading" className="space-y-3">
      <SectionHeading>
        <span id="mcp-revisions-heading">Versions</span>
      </SectionHeading>
      <Card className="divide-y divide-line-soft">
        <div data-testid="mcp-revisions" className="divide-y divide-line-soft">
          {server.revisions.map((revision) => (
            <div
              key={revision.revisionId}
              className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2.5"
            >
              <span className="font-mono text-[13px] text-ink">{revision.upstreamVersion}</span>
              {revision.revisionId === server.resolved?.revisionId && (
                <Chip tone="accent">delivered here</Chip>
              )}
              {revision.status !== "published" && <Chip tone="neutral">{revision.status}</Chip>}
              <span className="ml-auto text-faint text-xs">
                {revision.publishedBy === null
                  ? `revision ${revision.seq}`
                  : `revision ${revision.seq} · ${revision.publishedBy}`}
                {revision.publishedAt === null ? "" : ` · ${revision.publishedAt.slice(0, 10)}`}
              </span>
            </div>
          ))}
        </div>
      </Card>
    </section>
  );
}

/**
 * THE OWNER'S EDIT, for a server this workspace wrote down itself. Every save is a NEW revision
 * and a pointer move — the document somebody already received is never rewritten, so a machine
 * that took the old one can still be told which it holds.
 */
function EditServerForm({ document }: { document: string }) {
  const navigation = useNavigation();
  const busy =
    navigation.state !== "idle" && navigation.formData?.get("intent") === "edit-mcp-server";
  return (
    <section aria-labelledby="mcp-edit-heading" className="space-y-3">
      <SectionHeading>
        <span id="mcp-edit-heading">Edit</span>
      </SectionHeading>
      <Card className="px-4 py-3">
        <Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="edit-mcp-server" />
          <BusyFields busy={busy} className="space-y-3">
            <label className="block">
              <span className="mb-1 block font-medium text-dim text-sm">server.json</span>
              <textarea
                name="document"
                rows={12}
                defaultValue={document}
                data-testid="mcp-edit-document"
                className="block w-full rounded-md border border-line px-3 py-2 font-mono text-[12px] text-ink focus:border-accent focus:outline-none"
              />
            </label>
            <p className="text-faint text-xs">
              Saving publishes a new version. Machines that already hold the old one move to it on
              their next update.
            </p>
            <button
              type="submit"
              data-testid="mcp-edit-save"
              className={`${buttonClasses("primary")} min-h-11`}
            >
              {busy ? "Saving…" : "Save a new version"}
            </button>
          </BusyFields>
        </Form>
      </Card>
    </section>
  );
}
