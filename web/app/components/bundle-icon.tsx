import { Package, Plug } from "lucide-react";
import type { ElementType } from "react";
import { type BundleBase, baseEntry, kindEntry } from "@/lib/bundle-base";

/**
 * THE MARK ONE KIND OF BUNDLE WEARS — the one place a kind record's icon NAME becomes a
 * component, so the rail, a channel's curation list and its picker cannot drift to different
 * marks for the same thing. Names rather than components live in the records themselves: the
 * route table reads those records too, and it has no business dragging an icon set in with them.
 */
const ICONS: Record<string, ElementType> = {
  package: Package,
  plug: Plug,
};

/** The icon component for a catalog `kind` (anything undefined reads as a skill). */
export function bundleIconForKind(kind: string | null | undefined): ElementType {
  return ICONS[kindEntry(kind).railIcon] ?? Package;
}

/** The icon component for a URL base — for the surfaces that group by base rather than by kind. */
export function bundleIconForBase(base: BundleBase): ElementType {
  return ICONS[baseEntry(base).railIcon] ?? Package;
}
