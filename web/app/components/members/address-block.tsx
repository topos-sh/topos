import { CommandBlock } from "@/components/command-block";

/**
 * The workspace ADDRESS as the share surface — the join hand-off's copy pattern (it replaces the
 * old standing "door link"). The address is the workspace's URL slug; pasting `topos follow
 * <origin>/<address>` to an agent walks it through installing topos and following the workspace.
 * The address is not itself an admission — the ROSTER is: joining still requires an invited email,
 * so the same address serves every invited teammate and the owner's own devices. `origin` is the
 * request URL's origin (loader-derived); `address` is the workspace slug from the DAL.
 */
export function AddressBlock({ origin, address }: { origin: string; address: string }) {
  const command = `topos follow ${origin}/${address}`;
  return (
    <div className="space-y-2">
      <p className="text-sm text-dim">
        Paste this workspace address to your agent and ask it to follow — it walks the agent through
        installing topos and joining. Joining still requires an invited email, so only people on the
        roster can complete it.
      </p>
      <CommandBlock command={command} copyLabel="Copy the workspace address" />
      <p className="text-sm text-dim">
        Prefer a terminal? Run <span className="font-mono text-[12px]">{command}</span>
      </p>
    </div>
  );
}
