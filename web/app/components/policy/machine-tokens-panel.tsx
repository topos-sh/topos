import { useFetcher } from "react-router";
import { Card, SectionHeading, buttonClasses } from "@/components/ui";

/** One token row as the settings loader serialized it (dates ride as ISO strings). */
export interface MachineTokenView {
  tokenId: string;
  name: string;
  createdAt: string;
  lastUsedAt: string | null;
  serviceSessions: number;
}

interface MintFetcherData {
  status: "ok" | "error";
  error?: string;
  /** The plaintext, present EXACTLY ONCE — on the mint reply that created the token. */
  secret?: string;
  tokenName?: string;
}

/**
 * Machine tokens — the workspace's headless read credential (CI, VMs, sandboxes). Owner-only
 * panel: mint (the secret shows once, on the reply), list with last-use and live service
 * sessions, revoke. Non-owners don't see this section at all — there is nothing for them here.
 */
export function MachineTokensPanel({ tokens }: { tokens: MachineTokenView[] }) {
  const mint = useFetcher<MintFetcherData>();
  const minted = mint.data?.status === "ok" ? mint.data : undefined;
  return (
    <section aria-labelledby="machine-tokens-heading" className="space-y-3">
      <SectionHeading>
        <span id="machine-tokens-heading">Machine tokens</span>
      </SectionHeading>
      <Card className="space-y-4 px-4 py-3">
        <p className="text-sm text-dim">
          A machine token lets CI jobs, VMs, and sandboxes install this workspace's bundles
          without a person's login. Tokens are read-only: they can fetch bundles and report what
          a machine applied, never publish or change anything. Each run appears under Sessions
          as a service machine and expires on its own.
        </p>
        {minted?.secret !== undefined && (
          <div className="space-y-1 rounded border border-line bg-panel px-3 py-2">
            <p className="text-ink text-sm font-medium">
              Token "{minted.tokenName}" created — copy it now. It won't be shown again.
            </p>
            <code className="block select-all break-all font-mono text-sm">{minted.secret}</code>
            <p className="text-sm text-dim">
              In CI, set it as the <code className="font-mono">TOPOS_TOKEN</code> environment
              variable.
            </p>
          </div>
        )}
        {tokens.length > 0 && (
          <ul className="divide-y divide-line-soft">
            {tokens.map((t) => (
              <TokenRow key={t.tokenId} token={t} />
            ))}
          </ul>
        )}
        <mint.Form method="post" className="flex items-end gap-2">
          <input type="hidden" name="intent" value="mint-token" />
          <label className="flex-1 space-y-1">
            <span className="block text-sm text-dim">New token name</span>
            <input
              name="token_name"
              required
              maxLength={80}
              placeholder="github-actions"
              className="w-full rounded border border-line bg-transparent px-2 py-1 font-mono text-sm"
            />
          </label>
          <button
            type="submit"
            disabled={mint.state !== "idle"}
            className={buttonClasses("primary")}
          >
            {mint.state === "idle" ? "Create token" : "Creating…"}
          </button>
        </mint.Form>
        {mint.data?.status === "error" && (
          <p className="text-sm text-red-600">{mint.data.error}</p>
        )}
      </Card>
    </section>
  );
}

function TokenRow({ token }: { token: MachineTokenView }) {
  const revoke = useFetcher<{ status: "ok" | "error"; error?: string }>();
  const busy = revoke.state !== "idle";
  return (
    <li className="flex items-center justify-between gap-3 py-2">
      <div className="min-w-0">
        <p className="truncate text-ink text-sm font-medium">{token.name}</p>
        <p className="text-sm text-dim">
          created {new Date(token.createdAt).toLocaleDateString()} ·{" "}
          {token.lastUsedAt === null
            ? "never used"
            : `last used ${new Date(token.lastUsedAt).toLocaleString()}`}
          {token.serviceSessions > 0 &&
            ` · ${token.serviceSessions} service ${token.serviceSessions === 1 ? "machine" : "machines"}`}
        </p>
        {revoke.data?.status === "error" && (
          <p className="text-sm text-red-600">{revoke.data.error}</p>
        )}
      </div>
      <revoke.Form method="post">
        <input type="hidden" name="intent" value="revoke-token" />
        <input type="hidden" name="token_id" value={token.tokenId} />
        <button type="submit" disabled={busy} className={buttonClasses("danger")}>
          {busy ? "Revoking…" : "Revoke"}
        </button>
      </revoke.Form>
    </li>
  );
}
