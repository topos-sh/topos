import { useState } from "react";
import { useFetcher } from "react-router";
import { type LastSetLine, LastSetNote } from "@/components/policy/last-set-line";
import { SaveControls } from "@/components/policy/save-controls";
import { Card, SectionHeading } from "@/components/ui";

interface McpGatewayFetcherData {
  error?: string;
}

type McpGatewaySwitch = "off" | "on";

/**
 * The workspace-wide gateway switch. Off overrides every per-server setting and member choice:
 * nothing in this workspace routes through the gateway. On (the default) hands the ruling to
 * each connection's own Gateway setting. Owner-only, a plain dirty-reveal save like every
 * policy knob.
 */
export function McpGatewayPolicyPanel({
  isOwner,
  mcpGateway,
  lastSet,
}: {
  isOwner: boolean;
  mcpGateway: McpGatewaySwitch;
  lastSet: LastSetLine | null;
}) {
  return (
    <section aria-labelledby="mcp-gateway-heading" className="space-y-3">
      <SectionHeading>
        <span id="mcp-gateway-heading">MCP gateway</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        {isOwner ? (
          <McpGatewayControl current={mcpGateway} />
        ) : (
          <p className="text-ink text-sm">
            The MCP gateway is currently{" "}
            <span className="font-medium">{mcpGateway === "on" ? "on" : "off"}</span>. Only an owner
            can change this.
          </p>
        )}
        {mcpGateway === "off" && (
          <p className="text-dim text-sm">All MCP servers connect directly.</p>
        )}
        <LastSetNote lastSet={lastSet} describe={(v) => (v === "off" ? "off" : "on")} />
      </Card>
    </section>
  );
}

function McpGatewayControl({ current }: { current: McpGatewaySwitch }) {
  const fetcher = useFetcher<McpGatewayFetcherData>();
  const [staged, setStaged] = useState<McpGatewaySwitch>(current);
  const pending = fetcher.state !== "idle";
  const dirty = staged !== current;
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-mcp-gateway" />
      <fieldset className="space-y-2">
        <legend className="sr-only">MCP gateway</legend>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="mcp_gateway"
            value="on"
            checked={staged === "on"}
            disabled={pending}
            onChange={() => setStaged("on")}
            className="accent-accent"
          />
          On
        </label>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="mcp_gateway"
            value="off"
            checked={staged === "off"}
            disabled={pending}
            onChange={() => setStaged("off")}
            className="accent-accent"
          />
          Off
        </label>
      </fieldset>
      {dirty && (
        <SaveControls
          saveLabel={staged === "on" ? "Turn the gateway on" : "Turn the gateway off"}
          pending={pending}
          error={error}
          onCancel={() => setStaged(current)}
        />
      )}
    </fetcher.Form>
  );
}
