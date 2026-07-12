import type { LoaderFunctionArgs } from "react-router";
import { useLoaderData } from "react-router";
import { PageHeader } from "@/components/ui";
import { notFound, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { skillIndexRow } from "@/lib/db/queries.server";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `Settings · ${params.skill ?? "skill"}` }];
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  const skill = params.skill;
  if (!ws || !skill) {
    notFound();
  }
  const owner = await requireWorkspaceOwner(request, ws);
  const row = await skillIndexRow(owner, skill);
  if (!row) {
    notFound();
  }
  return { ws, skill: row.name };
}

export default function SkillSettings() {
  const { skill } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader title={`${skill} settings`} />
    </div>
  );
}
