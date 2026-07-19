import { useState } from "react";
import { CommandBlock } from "@/components/command-block";
import { AGENT_PUBLISH_PROMPT, PUBLISH_COMMAND } from "@/components/shell/publish-dialog";
import { SectionHeading } from "@/components/ui";

/**
 * The dashboard's onboarding checklist — three live steps whose checkmarks derive from real
 * rows (devices / published skills / seats), shown while the workspace is still getting going
 * (the loader decides visibility; this component only renders what it is given). AGENT-FIRST:
 * every step leads with the copyable "paste into your agent" prompt, with the manual terminal
 * commands folded behind a disclosure. Dismissal is a client-set cookie the loader reads back,
 * so the choice sticks without a hydration flicker.
 */

export interface OnboardingState {
  /** The cookie name the dismiss writes — workspace-scoped, read by the dashboard loader. */
  dismissCookie: string;
  /** This deployment's public origin — `<origin>/agent` and `<origin>/install` build from it. */
  origin: string;
  /** The full shareable workspace address. */
  shareAddress: string;
  deviceCount: number;
  publishedSkillCount: number;
  memberCount: number;
}

function StepMarker({ done, index }: { done: boolean; index: number }) {
  if (done) {
    return (
      <span
        aria-hidden="true"
        className="inline-flex h-[22px] w-[22px] shrink-0 items-center justify-center rounded-md bg-accent text-[12px] text-on-accent"
      >
        ✓
      </span>
    );
  }
  return (
    <span
      aria-hidden="true"
      className="inline-flex h-[22px] w-[22px] shrink-0 items-center justify-center rounded-md border border-line bg-panel font-mono text-[12px] text-faint"
    >
      {index}
    </span>
  );
}

function Step({
  done,
  index,
  title,
  doneNote,
  agentPrompt,
  manual,
}: {
  done: boolean;
  index: number;
  title: string;
  /** The one-line receipt shown once the step is complete. */
  doneNote: string;
  agentPrompt: string;
  /** The terminal alternative: intro line + commands. */
  manual: { intro: string; commands: string[] };
}) {
  return (
    <li className="flex gap-3 border-line-soft border-b px-4 py-4 last:border-b-0">
      <StepMarker done={done} index={index} />
      <div className="min-w-0 flex-auto space-y-2">
        <div className="flex flex-wrap items-baseline gap-x-2">
          <span className="font-medium text-ink text-sm">{title}</span>
          {done && <span className="text-faint text-xs">{doneNote}</span>}
        </div>
        {!done && (
          <>
            <p className="text-faint text-xs">Paste into the agent you already have:</p>
            <CommandBlock command={agentPrompt} copyLabel={`Copy the agent prompt: ${title}`} />
            <details className="text-sm">
              <summary className="cursor-pointer text-faint text-xs hover:text-dim">
                Or run it yourself
              </summary>
              <div className="mt-2 space-y-2">
                <p className="text-dim text-xs">{manual.intro}</p>
                {manual.commands.map((command) => (
                  <CommandBlock key={command} command={command} />
                ))}
              </div>
            </details>
          </>
        )}
      </div>
    </li>
  );
}

export function OnboardingChecklist({ state }: { state: OnboardingState }) {
  const [dismissed, setDismissed] = useState(false);
  if (dismissed) {
    return null;
  }
  const dismiss = () => {
    // biome-ignore lint/suspicious/noDocumentCookie: the deliberate lightweight dismiss — one first-party preference cookie the loader reads back (no Cookie Store dependency).
    document.cookie = `${state.dismissCookie}=1; path=/; max-age=31536000; samesite=lax`;
    setDismissed(true);
  };
  const { origin, shareAddress, deviceCount, publishedSkillCount, memberCount } = state;
  return (
    <section aria-labelledby="onboarding-heading" className="space-y-3">
      <div className="flex items-center justify-between gap-3">
        <SectionHeading>
          <span id="onboarding-heading">Get your team running</span>
        </SectionHeading>
        <button
          type="button"
          onClick={dismiss}
          className="text-faint text-xs transition-colors hover:text-dim focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
        >
          Dismiss
        </button>
      </div>
      <div className="overflow-hidden rounded-lg border border-line-soft bg-panel">
        <ol>
          <Step
            done={deviceCount >= 1}
            index={1}
            title="Enroll a device"
            doneNote={deviceCount === 1 ? "1 device enrolled" : `${deviceCount} devices enrolled`}
            agentPrompt={`Set up Topos for us: fetch ${origin}/agent and follow it. Our workspace: ${shareAddress}`}
            manual={{
              intro: "Install the CLI, then follow this workspace and approve in the browser:",
              commands: [`curl -fsSL ${origin}/install | sh`, `topos follow ${shareAddress}`],
            }}
          />
          <Step
            done={publishedSkillCount >= 1}
            index={2}
            title="Publish your first skill"
            doneNote={
              publishedSkillCount === 1
                ? "1 skill published"
                : `${publishedSkillCount} skills published`
            }
            agentPrompt={AGENT_PUBLISH_PROMPT}
            manual={{
              intro: "From an enrolled device, publish the skill's directory:",
              commands: [PUBLISH_COMMAND],
            }}
          />
          <Step
            done={memberCount >= 2}
            index={3}
            title="Invite a teammate"
            doneNote={`${memberCount} members`}
            agentPrompt={`Invite my teammate <email> to our Topos workspace, then send them our address: ${shareAddress}`}
            manual={{
              intro: "The invite mail carries the address; the roster lives on the Members page:",
              commands: [`topos invite <email>`],
            }}
          />
        </ol>
      </div>
    </section>
  );
}
