import { useEffect, useState } from "react";
import { CopyButton } from "@/components/copy-button";

/**
 * The founder's address, kept OUT of every byte this app serves. Harvesters read served HTML —
 * and `mailto:` hrefs above all — far more reliably than they run scripts, so the address exists
 * nowhere as a literal: not in the markup, not in this module, and never in an href. It is
 * assembled from parts in the browser AFTER mount (this app renders on the server, so a
 * render-time assembly would land in the document), and everything that shows or copies it reads
 * that one runtime value. A reader with no JavaScript gets a plain human instruction rather than
 * an empty line.
 */
const LOCAL_PART = ["ro", "bert"];
const HOST_PARTS = ["topos", "sh"];
/** What a scriptless reader sees — a sentence a person can act on, and a harvester cannot. */
const SCRIPTLESS = "email robert at this domain";

function assemble() {
  return `${LOCAL_PART.join("")}${String.fromCharCode(64)}${HOST_PARTS.join(".")}`;
}

/**
 * The address, or null until the browser has built it. Null on the server AND on the first client
 * render — that pairing is what keeps hydration quiet while keeping the document address-free.
 */
function useFounderAddress() {
  const [address, setAddress] = useState<string | null>(null);
  useEffect(() => {
    setAddress(assemble());
  }, []);
  return address;
}

/**
 * The command-block-shaped address row: one light inset field carrying the address in mono with a
 * copy affordance — the same grammar the install command wears, because copying it is the action.
 * The copy button appears only once there is something to copy, so a scriptless page shows no
 * button that would do nothing.
 */
export function FounderAddressBlock() {
  const address = useFounderAddress();
  return (
    <div className="flex max-w-[400px] items-center gap-3 rounded-md border border-line-soft bg-panel2 px-4 py-2.5">
      <span className="min-w-0 flex-1 break-words font-mono text-[13px] text-ink">
        {address ?? SCRIPTLESS}
      </span>
      {address ? (
        <CopyButton
          text={address}
          ariaLabel="Copy the founder's email address"
          tone="light"
          compact
        />
      ) : null}
    </div>
  );
}

/** The footer's quiet copy of the same runtime address, sized to the link row beside it. */
export function FounderAddressText() {
  const address = useFounderAddress();
  return <span className="font-mono text-[13px] text-faint">{address ?? SCRIPTLESS}</span>;
}
