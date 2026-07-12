import { Chip } from "@/components/ui";

/**
 * The ONE place semantic colors appear in the product: the workspace's domain-verification
 * state, verified BY THE SERVER (never by the web). green=verified, amber=pending,
 * gray=unverified — nothing else may use these tones. `status` arrives as the wire's raw string;
 * anything that isn't `verified`/`pending` renders the honest unverified fallback.
 */
export function DomainBadge({
  domain,
  status,
}: {
  domain: string | null | undefined;
  status: string;
}) {
  if (status === "verified" && domain != null) {
    return <Chip tone="verified">✓ {domain} — domain verified by the server</Chip>;
  }
  if (status === "pending" && domain != null) {
    return (
      <div className="flex flex-col items-center gap-1">
        <Chip tone="pending">{domain} — verification pending</Chip>
        <p className="text-sm text-dim">Not yet verified. Treat this workspace as unverified.</p>
      </div>
    );
  }
  return (
    <div className="flex flex-col items-center gap-1">
      <Chip tone="unverified">No verified domain</Chip>
      <p className="text-sm text-dim">
        Confirm you recognize this workspace and machine by other means before continuing.
      </p>
    </div>
  );
}
