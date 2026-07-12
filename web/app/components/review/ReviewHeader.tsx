import { firstLine, relativeTime } from "@/components/format";
import { ShortId } from "@/components/ui";
import { ProposalStatusBanner, type ReviewStatus } from "./ProposalStatusBanner";

/**
 * The review page header: what change, by which device (and, when the session lane discloses
 * it, which teammate), against which skill — with the status banner integrated at the top (the
 * first thing a reviewer must know). Author, proposer, and message are server-recorded values
 * and render as TEXT NODES only. Author and message come from the candidate's version meta —
 * ABSENT when the server has reclaimed that candidate (the diff-less render), so the header
 * degrades to the proposal facts alone. `skillName` is the catalog display name (or name).
 */
export function ReviewHeader({
  skillName,
  versionId,
  author,
  message,
  createdAt,
  proposer,
  status,
}: {
  skillName: string;
  versionId: string;
  /** The authoring device id — absent when the candidate's meta is no longer readable. */
  author?: string;
  message?: string;
  createdAt?: string;
  /** The proposer's canonical email — session-lane disclosure; absent on a degraded read. */
  proposer?: string;
  status: ReviewStatus;
}) {
  const title = message !== undefined ? firstLine(message) : "";
  const opened = createdAt !== undefined ? relativeTime(createdAt) : "";
  const proposedLine =
    proposer !== undefined
      ? `proposed by ${proposer}${opened !== "" ? ` ${opened}` : ""}`
      : opened !== ""
        ? `proposed ${opened}`
        : "";
  return (
    <header className="flex flex-col gap-3">
      <ProposalStatusBanner status={status} />
      <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <h1 className="font-display font-semibold text-xl tracking-[-0.02em] text-ink">
          {skillName}
        </h1>
        <ShortId value={versionId} />
      </div>
      {title !== "" ? <p className="text-base text-dim">{title}</p> : null}
      {author !== undefined || proposedLine !== "" ? (
        <p className="text-sm text-faint">
          {author !== undefined ? (
            <>
              authored by device <ShortId value={author} />
            </>
          ) : null}
          {author !== undefined && proposedLine !== "" ? " · " : null}
          {proposedLine !== "" ? proposedLine : null}
        </p>
      ) : null}
    </header>
  );
}
