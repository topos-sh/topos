import type { MiddlewareFunction } from "react-router";
import { notFound } from "@/lib/auth/guards.server";
import { cardResponse } from "@/lib/card.server";

/**
 * The fallback for any path no route claims. A non-browser fetcher gets the SAME constant
 * protocol card every resource address serves (a client that fetched a mistyped or deeper
 * path still learns what to do, and an unmatched path is never an existence oracle); a
 * browser gets the house 404.
 */
export const middleware: MiddlewareFunction[] = [
  async ({ request }, next) => {
    const card = cardResponse(request);
    if (card) {
      return card;
    }
    return next();
  },
];

export async function loader() {
  notFound();
}

export default function CatchAll() {
  return null;
}
