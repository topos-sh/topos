// Browser-side auth client — used ONLY by the login UI. Same-origin base URL: it talks to
// /api/auth/* on this app's own server tier and never to the vault. The magic-link plugin is
// always registered here (it only adds `signIn.magicLink` to the surface); the login form shows
// that rung only when the composition enables delivery, so a password-only build simply never
// calls it.
import { magicLinkClient } from "better-auth/client/plugins";
import { createAuthClient } from "better-auth/react";

export const authClient = createAuthClient({
  plugins: [magicLinkClient()],
});
