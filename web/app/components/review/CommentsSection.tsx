import { relativeTime } from "@/components/format";
import { Card, SectionHeading } from "@/components/ui";
import type { ProposalCommentRow } from "@/lib/db/queries.server";
import { CommentForm } from "./CommentForm";

/**
 * The proposal's comment thread — web-only state, open to every confirmed member in EVERY page
 * state (a resolved proposal's thread is part of its record). The route loader reads the bounded
 * window (via `proposalCommentsFor`) and mints the form's next comment id (the retry-idempotency
 * key), handing both here. The thread is keyed by the CANDIDATE version id on purpose: it follows
 * the bytes, so a real rebase re-parents into a different candidate id and gets a fresh thread.
 * Bodies are plain text rendered as React text nodes: no markdown, no sanitizer, no raw HTML —
 * there is nothing to get wrong. Append-only; the render is bounded, with an honest note when older
 * comments exist beyond the window.
 */
export function CommentsSection({
  ws,
  skill,
  versionId,
  comments,
  truncated,
  commentId,
}: {
  ws: string;
  skill: string;
  versionId: string;
  comments: readonly ProposalCommentRow[];
  truncated: boolean;
  /** The loader-minted id for the next comment (the retry-idempotency key). */
  commentId: string;
}) {
  return (
    <section aria-labelledby="comments-heading" className="flex flex-col gap-3">
      <SectionHeading>
        <span id="comments-heading">Comments</span>
      </SectionHeading>
      {truncated ? (
        <p className="text-faint text-xs">
          Showing the latest {comments.length} comments — older ones are kept, just not shown here.
        </p>
      ) : null}
      {comments.length === 0 ? (
        <Card className="px-4 py-3">
          <p className="text-sm text-faint">No comments yet — anything a reviewer should know?</p>
        </Card>
      ) : (
        <ul className="flex flex-col gap-2">
          {comments.map((comment) => (
            <li key={comment.id} className="rounded-lg border border-line-soft bg-panel px-4 py-3">
              <p className="text-faint text-xs">
                <span className="text-dim">{comment.authorDisplay}</span>
                {" · "}
                {relativeTime(comment.createdAt)}
              </p>
              <p className="mt-1 whitespace-pre-wrap text-ink text-sm">{comment.body}</p>
            </li>
          ))}
        </ul>
      )}
      <CommentForm ws={ws} skill={skill} versionId={versionId} commentId={commentId} />
    </section>
  );
}
