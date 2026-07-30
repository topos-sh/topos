import type { ReactElement } from "react";

/**
 * The agent apps' own logomarks, for the places the landing page names which apps Topos works
 * with. NOMINATIVE use only — each mark sits beside the app's name and claims nothing else: no
 * taglines, no endorsement.
 *
 * Every mark is VENDORED from artwork its own owner publishes — an official repository or an
 * official site — and is shown in the form that owner publishes it in: full colour where the
 * owner's mark is colour, the page's own ink where the owner publishes it in one, and an embedded
 * raster where a raster is all there is. Nothing is fetched at runtime, and nothing here is
 * redrawn, traced, re-tinted, or taken from a logo aggregator. An app whose owner publishes no
 * mark is simply absent from the map below: `HarnessMark` renders nothing and the caller falls
 * back to the app's name as a text mark — a missing mark is honest, an invented one is not.
 *
 * Where each mark comes from:
 *
 *  - Claude Code — simple-icons 16.27.1, slug `claudecode`; artwork CC0-1.0, upstream source
 *    https://code.claude.com, drawn in the brand colour that set records for it (#d97757).
 *  - OpenClaw — the project's own pixel-art lobster (openclaw/openclaw `docs/assets/
 *    pixel-lobster.svg`, MIT): its fill groups and all of their rects, in source order, on the
 *    source's own 16-unit grid, drawn without antialiasing so the pixels stay pixels. Only the
 *    source's no-op transparent backing rect is left out.
 *  - Hermes — the Hermes desktop app's own brand asset (NousResearch/hermes-agent
 *    `apps/desktop/public/nous-girl.jpg`, MIT), which is the only form its owner publishes: a
 *    raster. Scaled to 48px and re-encoded in the source's own codec, embedded below. The artwork
 *    is line work on white paper, so it is composited by multiplying: the paper drops out to
 *    whatever it sits on and the line work is untouched. That only works on a light ground, which
 *    is the only kind this page has.
 *  - Codex — OpenAI publishes no Codex-specific mark, so the OpenAI blossom stands beside the
 *    name: `OpenAI-black-monoblossom.svg` as served by OpenAI's own developer site, its path
 *    untouched (the two clip paths it carries clip nothing and are dropped), inked by the page —
 *    which is the black the file is published in.
 *  - Cursor — simple-icons 16.27.1, slug `cursor`; artwork CC0-1.0, upstream source
 *    https://cursor.com/brand, whose own brand kit draws this mark in a single ink.
 */

/**
 * 16px, not 14: OpenClaw's lobster is pixel art on a 16-unit grid, and only a whole-pixel-per-unit
 * size keeps its pixels square on both a 1x and a 2x screen.
 */
const DEFAULT_SIZE = "h-4 w-4 shrink-0";

/**
 * OpenClaw's lobster, group by group: the group's fill, then its rects as `x y width height`, in
 * the order the source file draws them.
 */
const LOBSTER: readonly (readonly [string, string])[] = [
  // outline
  [
    "#3a0a0d",
    "1 5 1 3, 2 4 1 1, 2 8 1 1, 3 3 1 1, 3 9 1 1, 4 2 1 1, 4 10 1 1, 5 2 6 1, 11 2 1 1, 12 3 1 1, 12 9 1 1, 13 4 1 1, 13 8 1 1, 14 5 1 3, 5 11 6 1, 4 12 1 1, 11 12 1 1, 3 13 1 1, 12 13 1 1, 5 14 6 1",
  ],
  // body
  [
    "#ff4f40",
    "5 3 6 1, 4 4 8 1, 3 5 10 1, 3 6 10 1, 3 7 10 1, 4 8 8 1, 5 9 6 1, 5 12 6 1, 6 13 4 1",
  ],
  // claws
  ["#ff775f", "1 6 2 1, 2 5 1 1, 2 7 1 1, 13 6 2 1, 13 5 1 1, 13 7 1 1"],
  // eyes
  ["#081016", "6 5 1 1, 9 5 1 1"],
  ["#f5fbff", "6 4 1 1, 9 4 1 1"],
];

/** The Hermes mark, embedded — see the provenance note above. */
const HERMES_MARK =
  "data:image/jpeg;base64,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";

/** OpenClaw's lobster, at whatever size the caller asks for. */
function LobsterMark({ className }: { className: string }) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" shapeRendering="crispEdges" className={className}>
      {LOBSTER.map(([fill, rects]) => (
        <g key={fill} fill={fill}>
          {rects.split(", ").map((rect) => {
            const [x, y, width, height] = rect.split(" ");
            return <rect key={rect} x={x} y={y} width={width} height={height} />;
          })}
        </g>
      ))}
    </svg>
  );
}

/**
 * Keyed by the app name exactly as the page prints it, so a caller passes the label it already
 * has. Each entry renders its own artwork at the caller's size — a mark is an inline vector or an
 * embedded raster depending only on what its owner publishes.
 */
const MARKS: Record<string, (className: string) => ReactElement> = {
  "Claude Code": (className) => (
    <svg viewBox="0 0 24 24" aria-hidden="true" fill="#d97757" className={className}>
      <path d="M21 10.5h3v3h-3v3h-1.5v3H18v-3h-1.5v3H15v-3H9v3H7.5v-3H6v3H4.5v-3H3v-3H0v-3h3v-6h18Zm-15 0h1.5v-3H6Zm10.5 0H18v-3h-1.5z" />
    </svg>
  ),
  OpenClaw: (className) => <LobsterMark className={className} />,
  Hermes: (className) => (
    <img
      src={HERMES_MARK}
      alt=""
      aria-hidden="true"
      className={`${className} mix-blend-multiply`}
    />
  ),
  Codex: (className) => (
    <svg viewBox="0 0 721 721" aria-hidden="true" fill="currentColor" className={className}>
      <path d="M304.246 294.611V249.028C304.246 245.189 305.687 242.309 309.044 240.392L400.692 187.612C413.167 180.415 428.042 177.058 443.394 177.058C500.971 177.058 537.44 221.682 537.44 269.182C537.44 272.54 537.44 276.379 536.959 280.218L441.954 224.558C436.197 221.201 430.437 221.201 424.68 224.558L304.246 294.611ZM518.245 472.145V363.224C518.245 356.505 515.364 351.707 509.608 348.349L389.174 278.296L428.519 255.743C431.877 253.826 434.757 253.826 438.115 255.743L529.762 308.523C556.154 323.879 573.905 356.505 573.905 388.171C573.905 424.636 552.315 458.225 518.245 472.141V472.145ZM275.937 376.182L236.592 353.152C233.235 351.235 231.794 348.354 231.794 344.515V238.956C231.794 187.617 271.139 148.749 324.4 148.749C344.555 148.749 363.264 155.468 379.102 167.463L284.578 222.164C278.822 225.521 275.942 230.319 275.942 237.039V376.186L275.937 376.182ZM360.626 425.122L304.246 393.455V326.283L360.626 294.616L417.002 326.283V393.455L360.626 425.122ZM396.852 570.989C376.698 570.989 357.989 564.27 342.151 552.276L436.674 497.574C442.431 494.217 445.311 489.419 445.311 482.699V343.552L485.138 366.582C488.495 368.499 489.936 371.379 489.936 375.219V480.778C489.936 532.117 450.109 570.985 396.852 570.985V570.989ZM283.134 463.99L191.486 411.211C165.094 395.854 147.343 363.229 147.343 331.562C147.343 294.616 169.415 261.509 203.48 247.593V356.991C203.48 363.71 206.361 368.508 212.117 371.866L332.074 441.437L292.729 463.99C289.372 465.907 286.491 465.907 283.134 463.99ZM277.859 542.68C223.639 542.68 183.813 501.895 183.813 451.514C183.813 447.675 184.294 443.836 184.771 439.997L279.295 494.698C285.051 498.056 290.812 498.056 296.568 494.698L417.002 425.127V470.71C417.002 474.549 415.562 477.429 412.204 479.346L320.557 532.126C308.081 539.323 293.206 542.68 277.854 542.68H277.859ZM396.852 599.776C454.911 599.776 503.37 558.513 514.41 503.812C568.149 489.896 602.696 439.515 602.696 388.176C602.696 354.587 588.303 321.962 562.392 298.45C564.791 288.373 566.231 278.296 566.231 268.224C566.231 199.611 510.571 148.267 446.274 148.267C433.322 148.267 420.846 150.184 408.37 154.505C386.775 133.392 357.026 119.958 324.4 119.958C266.342 119.958 217.883 161.22 206.843 215.921C153.104 229.837 118.557 280.218 118.557 331.557C118.557 365.146 132.95 397.771 158.861 421.283C156.462 431.36 155.022 441.437 155.022 451.51C155.022 520.123 210.682 571.466 274.978 571.466C287.931 571.466 300.407 569.549 312.883 565.228C334.473 586.341 364.222 599.776 396.852 599.776Z" />
    </svg>
  ),
  Cursor: (className) => (
    <svg viewBox="0 0 24 24" aria-hidden="true" fill="currentColor" className={className}>
      <path d="M11.503.131 1.891 5.678a.84.84 0 0 0-.42.726v11.188c0 .3.162.575.42.724l9.609 5.55a1 1 0 0 0 .998 0l9.61-5.55a.84.84 0 0 0 .42-.724V6.404a.84.84 0 0 0-.42-.726L12.497.131a1.01 1.01 0 0 0-.996 0M2.657 6.338h18.55c.263 0 .43.287.297.515L12.23 22.918c-.062.107-.229.064-.229-.06V12.335a.59.59 0 0 0-.295-.51l-9.11-5.257c-.109-.063-.064-.23.061-.23" />
    </svg>
  ),
};

/**
 * Whether this app publishes a mark at all — so a caller can lay out the tile that would hold it
 * without rendering an empty one for an app the map above deliberately omits.
 */
export function hasHarnessMark(name: string): boolean {
  return name in MARKS;
}

/**
 * The app's mark, or null when its owner publishes none — the badge around it decides what to do
 * with nothing (it keeps the name, which was always the real label).
 */
export function HarnessMark({ name, className }: { name: string; className?: string }) {
  return MARKS[name]?.(className ?? DEFAULT_SIZE) ?? null;
}
