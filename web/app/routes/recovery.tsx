import type { ReactNode } from "react";
import {
  type ActionFunctionArgs,
  data,
  Form,
  type LoaderFunctionArgs,
  type MetaFunction,
  redirect,
  useActionData,
} from "react-router";
import { buttonClasses } from "@/components/ui";
import { notFound } from "@/lib/auth/guards.server";
import { consumeRecoveryCode } from "@/lib/auth/recovery.server";
import { allowPublicRead, clientKeyFromXff } from "@/lib/rate-limit.server";

export const meta: MetaFunction = () => [{ title: "Account recovery · Topos" }];

/**
 * The mail-less recovery hatch's public half: a sessionless form that trades a box-minted
 * one-shot code (`node scripts/mint-recovery-code.mjs <email>`, run where the server runs) for
 * a new password. The code is the whole proof — the form never asks who you are, and every
 * failure answers ONE constant refusal, so the page confirms nothing about accounts or codes.
 */

/** A recovery code rides the POST body, but the page itself still stays out of caches/indexes. */
export function headers() {
  return { "Cache-Control": "no-store", "X-Robots-Tag": "noindex" };
}

/** The public-probe belt, shared by GET and POST: a denied client gets the uniform miss. */
function requireBelt(request: Request): void {
  if (!allowPublicRead(clientKeyFromXff(request.headers.get("x-forwarded-for")))) {
    notFound();
  }
}

export async function loader({ request }: LoaderFunctionArgs) {
  requireBelt(request);
  return null;
}

/** The ONE refusal — the same string for an unknown, expired, or already-consumed code. */
const RECOVERY_REFUSED = "That code didn’t work. Codes are single-use and expire after 15 minutes.";

export async function action({ request }: ActionFunctionArgs) {
  requireBelt(request);
  const form = await request.formData();
  const code = String(form.get("code") ?? "").trim();
  const password = String(form.get("password") ?? "");
  // Mirror Better Auth's minimum so the new password is one sign-in will accept.
  if (code === "" || password.length < 8) {
    return data(
      { error: "Enter the recovery code and a new password of at least 8 characters." },
      { status: 400 },
    );
  }
  const consumed = await consumeRecoveryCode(code, password);
  if (consumed === null) {
    return data({ error: RECOVERY_REFUSED }, { status: 400 });
  }
  throw redirect("/login");
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function RecoveryPage() {
  const actionData = useActionData<typeof action>();
  return (
    <Shell>
      <p className="font-medium text-faint text-xs uppercase tracking-wide">Account recovery</p>
      <h1 className="mt-2 font-display font-semibold text-ink text-lg tracking-[-0.02em]">
        Set a new password
      </h1>
      <p className="mt-1 text-faint text-sm">
        Enter the recovery code from the server operator and choose a new password.
      </p>
      <Form method="post" className="mt-6 space-y-3">
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Recovery code</span>
          <input
            type="text"
            name="code"
            required
            autoComplete="off"
            spellCheck={false}
            className={`${INPUT} font-mono`}
            placeholder="paste the code"
          />
        </label>
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">New password</span>
          <input
            type="password"
            name="password"
            required
            minLength={8}
            autoComplete="new-password"
            className={INPUT}
            placeholder="••••••••"
          />
        </label>
        <button type="submit" className={`${buttonClasses("primary")} min-h-11 w-full`}>
          Reset password
        </button>
      </Form>
      {actionData?.error !== undefined && (
        <p className="mt-3 text-red-600 text-sm" role="alert">
          {actionData.error}
        </p>
      )}
    </Shell>
  );
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <main className="flex min-h-dvh items-center justify-center bg-ground px-4">
      <div className="w-full max-w-sm">
        <div className="rounded-lg border border-line-soft bg-panel p-8 shadow-card">
          {children}
        </div>
      </div>
    </main>
  );
}
