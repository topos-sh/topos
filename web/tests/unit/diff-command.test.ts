import { describe, expect, it } from "vitest";
import {
  agentHandoffText,
  buildApproveCommand,
  buildDiffCommand,
  buildRejectCommand,
} from "@/lib/diff/command";

const VID = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

describe("command strings", () => {
  it("approve is the exact full-hash command", () => {
    expect(buildApproveCommand("deploy-runbook", VID)).toBe(
      `topos review deploy-runbook@${VID} --approve`,
    );
  });

  it("reject is the exact full-hash command", () => {
    expect(buildRejectCommand("deploy-runbook", VID)).toBe(
      `topos review deploy-runbook@${VID} --reject`,
    );
  });

  it("diff is the exact full-hash command", () => {
    expect(buildDiffCommand("deploy-runbook", VID)).toBe(`topos diff deploy-runbook@${VID}`);
  });

  it("the agent hand-off carries all three full-hash commands and the enrolled-device caveat", () => {
    const text = agentHandoffText("deploy-runbook", VID);
    expect(text).toContain(`topos diff deploy-runbook@${VID}`);
    expect(text).toContain(`topos review deploy-runbook@${VID} --approve`);
    expect(text).toContain(`topos review deploy-runbook@${VID} --reject`);
    expect(text).toContain("enrolled device");
    // one paragraph: no newlines
    expect(text).not.toContain("\n");
  });
});
