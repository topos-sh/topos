import { useState } from "react";
import { authClient } from "@/lib/auth/client";

/** The smallest honest sign-out: the Better Auth client call, then a hard move to /login. */
export function SignOutButton() {
  const [pending, setPending] = useState(false);
  return (
    <button
      type="button"
      disabled={pending}
      onClick={async () => {
        setPending(true);
        try {
          await authClient.signOut();
        } finally {
          window.location.href = "/login";
        }
      }}
      className="inline-flex min-h-9 items-center rounded-md border border-line px-3 font-mono text-[13px] text-dim hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 disabled:opacity-50"
    >
      {pending ? "Signing out…" : "Sign out"}
    </button>
  );
}
