import type { LoaderFunctionArgs } from "react-router";
import { PageHeader } from "@/components/ui";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";

export function meta() {
  return [{ title: "Your devices" }];
}

export async function loader({ request }: LoaderFunctionArgs) {
  const session = await requireSession(request);
  const actor = actorFromSession(session);
  if (!actor) {
    notFound();
  }
  return { email: actor.email };
}

export default function YourDevices() {
  return (
    <div className="space-y-8">
      <PageHeader title="Your devices" />
    </div>
  );
}
