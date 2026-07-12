import { Card, SectionHeading } from "@/components/ui";

/**
 * The display-only pending state that replaces the decision panel for a member seat. Not the
 * authority: the server's in-transaction gate refuses a member-seat decision regardless — this
 * note just says so up front instead of after a click. (The proposer's own pending proposal no
 * longer gets a note of its own: the decision panel renders with Approve withheld and withdraw
 * live — the four-eyes gate applies to approve only.)
 */

/** The viewer holds a member seat — reading and commenting, not deciding. */
export function MemberReadOnlyNote() {
  return (
    <Card className="flex flex-col gap-1 p-4">
      <SectionHeading>Awaiting review</SectionHeading>
      <p className="text-sm text-dim">
        An owner or reviewer seat decides this proposal — your member seat reads and comments. Add
        what you know below.
      </p>
    </Card>
  );
}
