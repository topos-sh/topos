import type { LoaderFunctionArgs } from "react-router";
import { useLoaderData } from "react-router";
import { PageHeader } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `History · #${params.channel ?? "channel"}` }];
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  const channel = params.channel;
  if (!ws || !channel) {
    notFound();
  }
  await requireMember(request, ws);
  return { ws, channel };
}

export default function ChannelHistory() {
  const { channel } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader title={`#${channel} history`} />
    </div>
  );
}
