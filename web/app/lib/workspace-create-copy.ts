/**
 * The workspace-create form's typed refusal strings — ONE spelling shared by every surface
 * that runs the create ceremony (`/new`, and the /verify approval's create arm), so a taken
 * address or a tripped floor reads byte-identically wherever the form lives. Client-safe: no
 * server import, strings only.
 */

export const ADDRESS_TAKEN = "That address is taken — try another.";
export const CREATE_RATE_LIMITED =
  "You’ve created several workspaces recently — wait a while before creating another.";
export const WORKSPACE_LIMIT = "You have reached the workspace limit for your account.";
export const NAME_REQUIRED = "Enter a name for your workspace (1–100 characters).";
export const SLUG_SHAPE =
  "The address uses lowercase letters, numbers, and hyphens (up to 100 characters).";
