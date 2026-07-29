import { useNavigation } from "react-router";

/**
 * The `intent` of a submission from THIS page currently on the wire, or null when nothing of
 * ours is.
 *
 * `useNavigation()` reports EVERY navigation — a sidebar link, a breadcrumb, the redirect that
 * lands somewhere else — so a form keyed on a bare `state !== "idle"` goes inert and announces
 * "Creating…" while you are merely leaving the page. Scoping to `formMethod === "POST"` keeps it
 * to real submissions. It deliberately stays non-null through the post-submit `loading` phase,
 * because a form that lands by redirect must stay inert until the new page actually arrives.
 *
 * The value is the submitted `intent` field, or `""` for a form that posts none — so a route
 * serving several arms from one action can name the wait for the arm that was clicked instead of
 * making every arm claim it. FETCHER-driven forms need none of this: a fetcher only ever reports
 * its own submission, so `fetcher.state` is already scoped.
 */
export function useSubmittingIntent(): string | null {
  const navigation = useNavigation();
  if (navigation.state === "idle" || navigation.formMethod !== "POST") {
    return null;
  }
  return String(navigation.formData?.get("intent") ?? "");
}
