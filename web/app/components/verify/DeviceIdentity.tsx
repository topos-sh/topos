/**
 * The device being authorized — machine name + key fingerprint — as a COMPACT, secondary block.
 *
 * The action and its button are the page's focus (this is a consent screen, like `gh` / `claude` login):
 * the machine name and fingerprint are the "what am I approving" context, small and off to the side, not
 * the hero. The fingerprint stays legible (grouped in 4s) for the anti-phishing cross-check — "does it
 * match what my terminal printed?" — it just no longer dominates the page. A display aid only, never an
 * authority input.
 */
export function DeviceIdentity({
  machineName,
  fingerprint,
}: {
  machineName: string;
  fingerprint: string;
}) {
  const grouped = fingerprint.match(/.{1,4}/g)?.join(" ") ?? fingerprint;
  return (
    <div className="flex flex-col gap-0.5 rounded-md border border-line-soft bg-ground px-3 py-2 text-left">
      <p className="text-faint text-xs">You&apos;re approving this device</p>
      <p className="break-all text-ink text-sm">{machineName}</p>
      {/* The fingerprint stays legible (text-sm, not faint) — it's the one string a human must actively
          eyeball-compare against their terminal to defeat device-code phishing; the block is secondary,
          the affordance is not throwaway. */}
      <p className="break-all font-mono text-dim text-sm">{grouped}</p>
      <p className="text-faint text-xs">confirm it matches your terminal</p>
    </div>
  );
}
