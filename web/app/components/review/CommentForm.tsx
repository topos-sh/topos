import { useEffect, useRef } from "react";
import { useFetcher } from "react-router";
import { buttonClasses } from "@/components/ui";

/** The review route's typed reply for `intent=comment`. */
interface CommentActionData {
  status: "posted" | "empty" | "too_long" | "slow_down" | "thread_full" | "error";
  submittedBody?: string;
}

/**
 * The comment form — posts `intent=comment` to the review route's action. `commentId` is minted by
 * the loader and rides a hidden field: a retried submit replays the same id, so the row lands
 * exactly once; a SUCCESSFUL post revalidates the page, which re-renders this component with a
 * fresh id for the next comment and resets the textarea. `version_id` names the candidate the
 * thread hangs off.
 */
export function CommentForm({
  ws: _ws,
  skill: _skill,
  versionId,
  commentId,
}: {
  /** Carried for the section's API shape; the route action reads ws/skill from its own params. */
  ws: string;
  skill: string;
  versionId: string;
  commentId: string;
}) {
  const fetcher = useFetcher<CommentActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const formRef = useRef<HTMLFormElement>(null);

  // React Router does not reset a fetcher form after submit; clear the field once, on a landed
  // success. A non-success keeps the typed text via the echoed submittedBody below.
  useEffect(() => {
    if (fetcher.state === "idle" && state?.status === "posted") {
      formRef.current?.reset();
    }
  }, [fetcher.state, state]);

  return (
    <div className="flex flex-col gap-2">
      <fetcher.Form ref={formRef} method="post" className="flex flex-col gap-2">
        <input type="hidden" name="intent" value="comment" />
        <input type="hidden" name="version_id" value={versionId} />
        <input type="hidden" name="comment_id" value={commentId} />
        <label className="block">
          <span className="sr-only">Comment</span>
          <textarea
            name="body"
            required
            rows={2}
            maxLength={4000}
            placeholder="Comment on this proposal — plain text, visible to the workspace."
            // The echoed submittedBody (keyed, so the node remounts) keeps the typed text through a
            // non-success re-render.
            key={state?.submittedBody ?? "initial"}
            defaultValue={state?.submittedBody ?? ""}
            className="block w-full rounded-md border border-line px-3 py-2 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
          />
        </label>
        <div>
          <button type="submit" disabled={pending} className={`${buttonClasses("quiet")} min-h-11`}>
            {pending ? "Posting…" : "Comment"}
          </button>
        </div>
      </fetcher.Form>
      {state?.status === "empty" && (
        <p className="text-red-600 text-sm" role="alert">
          A comment needs some text.
        </p>
      )}
      {state?.status === "too_long" && (
        <p className="text-red-600 text-sm" role="alert">
          That comment is over the 4000-character limit — trim it and post again.
        </p>
      )}
      {state?.status === "slow_down" && (
        <p className="text-red-600 text-sm" role="alert">
          You&apos;re posting faster than this thread accepts — wait a moment, your text is still
          here.
        </p>
      )}
      {state?.status === "thread_full" && (
        <p className="text-red-600 text-sm" role="alert">
          This thread is full — it holds 500 comments and can&apos;t take another.
        </p>
      )}
      {state?.status === "error" && (
        <p className="text-red-600 text-sm" role="alert">
          That didn&apos;t post — try again (a retry is safe: it resumes this same comment).
        </p>
      )}
    </div>
  );
}
