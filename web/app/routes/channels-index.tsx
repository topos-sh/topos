import type { LoaderFunctionArgs } from "react-router";
import { useLoaderData } from "react-router";
import { PageHeader } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Channels · ${params.ws ?? "Workspace"}` }];
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  await requireMember(request, ws);
  return { ws };
}

export default function ChannelsIndex() {
  const { ws } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader title="Channels" meta={<code className="font-mono">{ws}</code>} />
      <p className="text-dim text-sm">No channels to show yet.</p>
    </div>
  );
}
