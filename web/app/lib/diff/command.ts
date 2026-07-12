/**
 * The exact command strings the review page hands off to an enrolled device. `skill` is the
 * catalog NAME — commands address a skill by its user-facing name, and the version is always the
 * FULL 64-hex version id (a truncated id must never reach a signed action). Pure string building;
 * exact-string tests pin every one of these.
 */

export function buildApproveCommand(skill: string, versionId: string): string {
  return `topos review ${skill}@${versionId} --approve`;
}

export function buildRejectCommand(skill: string, versionId: string): string {
  return `topos review ${skill}@${versionId} --reject`;
}

export function buildDiffCommand(skill: string, versionId: string): string {
  return `topos diff ${skill}@${versionId}`;
}

/** The one-paragraph hand-off a reviewer pastes to the agent on an enrolled device. */
export function agentHandoffText(skill: string, versionId: string): string {
  return (
    `There is an open proposal for the skill "${skill}" on the Topos server. ` +
    `Run \`${buildDiffCommand(skill, versionId)}\` to inspect the change on this machine, ` +
    `then decide whether it should become the team's current version. ` +
    `To approve it, run \`${buildApproveCommand(skill, versionId)}\`; ` +
    `to reject it, run \`${buildRejectCommand(skill, versionId)}\`. ` +
    `Approving runs on an enrolled device, which authenticates the action.`
  );
}
