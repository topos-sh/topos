import type { LoaderFunctionArgs } from "react-router";
import { getAuth } from "@/lib/auth/server";

/**
 * The Better Auth request handler, mounted at /api/auth/* — the login ceremony's whole surface
 * (sign-in, sign-up, sessions, and any composition-provided rungs). getAuth() constructs lazily
 * on first request, never at build time; the handler answers both GET and POST.
 */
const handler = ({ request }: LoaderFunctionArgs) => getAuth().handler(request);

export const loader = handler;
export const action = handler;
