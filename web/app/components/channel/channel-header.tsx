import { Chip, PageHeader } from "@/components/ui";

/**
 * The channel page header shared by every channel section (Skills · Members · History · Settings) —
 * the `#name` title with the mode chip and, for the default channel, the "every member, minus
 * opt-outs" note. Each section renders it above its own ChannelTabs, exactly as the skill sections
 * share SkillHeader. The `#` is a decorative prefix (aria-hidden), so the heading's accessible name
 * is the bare channel name.
 */
export function ChannelHeader({
  name,
  mode,
  isDefault,
}: {
  name: string;
  mode: "open" | "curated";
  isDefault: boolean;
}) {
  return (
    <PageHeader
      title={
        <>
          <span className="text-faint" aria-hidden="true">
            #
          </span>
          {name}
        </>
      }
      meta={
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <Chip tone={mode === "curated" ? "pending" : "neutral"}>{mode}</Chip>
          {isDefault && <span>every member, minus opt-outs</span>}
        </div>
      }
    />
  );
}
