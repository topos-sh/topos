import { Card, Chip } from "@/components/ui";

/**
 * Trust values are DISPLAYED with the server named as their source — the web computes none of
 * them and never uses "verified" as its own verb. The one displayable trust value is the
 * server-recorded bundle consent hash: content addressing, not a signature (nothing signs). The
 * digest is schema-honest — NULLABLE on the wire — so an absent one renders an em-dash chip.
 */
export function TrustPanel({ bundleDigest }: { bundleDigest: string | null }) {
  return (
    <Card className="flex flex-col gap-2 p-4">
      <div className="flex flex-wrap items-center gap-2">
        <Chip tone="neutral">
          <code className="font-mono">
            {bundleDigest !== null ? `sha-256:${bundleDigest.slice(0, 12)}…` : "sha-256:—"}
          </code>
        </Chip>
        <span className="text-sm text-dim">
          recorded by the server when this version was uploaded
        </span>
      </div>
      <p className="text-sm text-faint">
        The byte-exact consent hash over every file in this candidate. Enrolled devices re-hash the
        bytes they fetch and must reproduce it before anything lands.
      </p>
      <p className="text-sm text-faint">
        The version IS this hash — content-addressed, so what a device pins is exactly the bytes it
        gets.
      </p>
    </Card>
  );
}
