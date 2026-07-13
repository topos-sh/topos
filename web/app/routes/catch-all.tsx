import { notFound } from "@/lib/auth/guards.server";

/**
 * The fallback for any path no route claims. A non-browser DOCUMENT fetch gets the SAME
 * constant protocol card every resource address serves — from the server entry, so a client
 * that fetched a mistyped or deeper path still learns what to do and an unmatched path is
 * never an existence oracle; a browser gets the house 404.
 */
export async function loader() {
  notFound();
}

export default function CatchAll() {
  return null;
}
