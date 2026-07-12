---
version: "alpha"
name: "Topos Klein"
description: "The Topos visual identity: a warm-gray print ground, near-black ink, and International Klein Blue placed as objects — never as a wash."
colors:
  ground: "#f1f1ee"
  panel: "#f8f8f5"
  panel2: "#ebebe7"
  line: "#d8d8d2"
  line-soft: "#e3e3dd"
  hairline: "#b9b9b2"
  ink: "#161618"
  dim: "#3f3f3a"
  faint: "#5c5c55"
  primary: "#002fa7"
  accent: "#002fa7"
  accent-deep: "#00227a"
  accent-phos: "#6691ff"
  accent-wash: "#ebf0ff"
  on-accent: "#f4f4f0"
  glass: "#101013"
  glass-line: "#2c2c31"
  glass-ink: "#e9e9e5"
  glass-dim: "#b8b8b2"
  glass-faint: "#8c8c86"
typography:
  display:
    fontFamily: "Martian Mono"
    fontSize: 25px
    fontWeight: 600
    lineHeight: 1.45
    letterSpacing: -0.03em
  heading:
    fontFamily: "Martian Mono"
    fontSize: 23px
    fontWeight: 600
    lineHeight: 1.45
    letterSpacing: -0.02em
  title:
    fontFamily: "Martian Mono"
    fontSize: 15px
    fontWeight: 600
    lineHeight: 1.45
    letterSpacing: -0.02em
  label:
    fontFamily: "Martian Mono"
    fontSize: 10px
    fontWeight: 500
    lineHeight: 1.2
    letterSpacing: 0.12em
  body:
    fontFamily: "IBM Plex Sans"
    fontSize: 15px
    fontWeight: 400
    lineHeight: 1.6
  small:
    fontFamily: "IBM Plex Sans"
    fontSize: 13px
    fontWeight: 400
    lineHeight: 1.5
  mono:
    fontFamily: "IBM Plex Mono"
    fontSize: 13.5px
    fontWeight: 400
    lineHeight: 1.6
  terminal:
    fontFamily: "IBM Plex Mono"
    fontSize: 12.75px
    fontWeight: 400
    lineHeight: 1.75
rounded:
  sm: 4px
  md: 6px
  lg: 10px
  full: 9999px
spacing:
  xs: 4px
  sm: 8px
  md: 16px
  lg: 24px
  xl: 40px
  2xl: 64px
  section: 84px
components:
  page:
    backgroundColor: "{colors.ground}"
    textColor: "{colors.ink}"
    typography: "{typography.body}"
  micro-label:
    textColor: "{colors.faint}"
    typography: "{typography.label}"
  link-underline:
    backgroundColor: "{colors.hairline}"
    height: 1px
  border-default:
    backgroundColor: "{colors.line-soft}"
    height: 1px
  border-strong:
    backgroundColor: "{colors.line}"
    height: 1px
  selection:
    backgroundColor: "{colors.accent-wash}"
    textColor: "{colors.ink}"
  terminal-chrome:
    backgroundColor: "{colors.glass}"
    textColor: "{colors.glass-faint}"
    typography: "{typography.terminal}"
  terminal-line-success:
    backgroundColor: "{colors.glass}"
    textColor: "{colors.accent-phos}"
    typography: "{typography.terminal}"
  terminal-line-border:
    backgroundColor: "{colors.glass-line}"
    height: 1px
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.on-accent}"
    typography: "{typography.mono}"
    rounded: "{rounded.md}"
    height: 36px
    padding: 0 14px
  button-primary-hover:
    backgroundColor: "{colors.accent-deep}"
    textColor: "{colors.on-accent}"
  button-quiet:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.dim}"
    rounded: "{rounded.md}"
    height: 36px
    padding: 0 14px
  button-quiet-hover:
    backgroundColor: "{colors.panel2}"
    textColor: "{colors.dim}"
  card:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
    padding: 24px
  input:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    height: 44px
  tag-chip:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.on-accent}"
    typography: "{typography.label}"
    rounded: "{rounded.full}"
    padding: 3px 10px
  terminal-window:
    backgroundColor: "{colors.glass}"
    textColor: "{colors.glass-ink}"
    typography: "{typography.terminal}"
    rounded: "{rounded.lg}"
  install-command:
    backgroundColor: "{colors.glass}"
    textColor: "{colors.glass-ink}"
    typography: "{typography.mono}"
    rounded: "{rounded.md}"
    padding: 12px 16px
---

# Topos Klein

## Overview

Klein is a print-inspired system: a warm-gray paper ground, near-black ink set in
IBM Plex Sans, and **International Klein Blue (`#002fa7`) used as a placed object** —
a button, a chip, a label bar, a prompt marker — never as a page wash or a decorative
gradient. Headings and micro-labels are set in Martian Mono, giving the page its
technical, specified voice; running text stays humanist and quiet. Dark
"terminal glass" panels carry the product's own transcript output and are the only
dark surfaces in the system; inside them a single phosphor blue (`accent-phos`)
plays the role ink plays on paper.

The overall feel: a well-set spec sheet, not a SaaS gradient. Calm, flat, precise.

## Colors

**Neutrals are one warm-gray ramp** from `ground` (the page) through `panel` (raised
cards) and `panel2` (inset/alternating rows), with `line-soft` as the default border,
`line` for emphasized borders, and `hairline` for link underlines. Text steps down
`ink → dim → faint`; never use pure black or pure white.

**Blue is one ramp** — `hsl(223 100% L)` at lightness steps: `accent-deep` (24%,
hover/pressed), `accent` (33%, International Klein Blue — the base), `accent-phos`
(70%, the in-glass intensity), `accent-wash` (96%, tint backgrounds and selection).
`on-accent` is the paper-white text that sits on blue. **Never mint a new blue**: if a
new need appears, take another lightness step on this ramp.

**Reserved semantics (do not repurpose):** green (`green-50`/`green-800`) and amber
(`amber-50`/`amber-800`) belong exclusively to domain-verification states
(verified / pending), and red (`red-50`, `red-600`/`red-700`) to errors and
destructive actions. Keeping these off general UI is what keeps the trust signal
unambiguous. Muted gray is the third verification state (unverified).

**Glass tokens** (`glass`, `glass-line`, `glass-ink`, `glass-dim`, `glass-faint`)
exist only inside terminal windows and command blocks — never as a general dark theme.

## Typography

Three families, three jobs:

- **Martian Mono** (`display`, `heading`, `title`, `label`) — headlines, card titles,
  and uppercase micro-labels. Semibold, tight tracking (−0.02 to −0.03em) at display
  sizes; the `label` voice is 10px, weight 500, uppercase, +0.12em tracking, usually
  in `faint`. Display sizes fluidly scale (`clamp(19px, 2.4vw, 25px)` for the hero,
  `clamp(18px, 2.2vw, 23px)` for section headings).
- **IBM Plex Sans** (`body`, `small`) — all running text at 15px/1.6. Secondary copy
  in `dim`, tertiary/annotation copy at 12.5–13px in `faint`. Emphasis is weight 500
  in `ink`, not bold.
- **IBM Plex Mono** (`mono`, `terminal`) — commands, hashes, transcript output, and
  **button labels**. Terminal transcripts run at 12.75px/1.75.

Headline case is sentence case everywhere; only the `label` voice is uppercase.

## Layout

Content sits in a 1080px max-width wrap with 24px side padding (`mx-auto max-w-[1080px] px-6`).
Spacing follows a 4px base grid; the named steps are `xs` 4 / `sm` 8 / `md` 16 /
`lg` 24 / `xl` 40 / `2xl` 64, with `section` (84px, growing to ~116px on large
screens) as the vertical rhythm between landing sections. Cards pad 24px (28px for a
featured card); dense rows pad 20×11px. The signed-in app uses the same tokens on a
tighter wrap (max-w-5xl) with a 56–60px top bar.

## Elevation & Depth

The system is nearly flat — hierarchy comes from the neutral ramp (ground → panel →
panel2) and borders, not shadows. Two soft, large-radius shadows exist:
`0 8px 22px -20px rgba(22,22,28,0.35)` under feature cards, and
`0 18px 44px -24px rgba(22,22,28,0.40)` under terminal glass. Nothing else casts a
shadow; no inner shadows, no glows.

## Shapes

Corners are quiet: `md` (6px) for buttons, inputs, and command blocks; `lg` (10px)
for cards and terminal windows; `full` only for the small tag/status chips and the
traffic-light dots. No fully-round buttons, no sharp-cornered panels.

## Components

- **Primary button** — Klein blue block, `on-accent` text, **mono** label at 12.5–13px,
  6px radius, hover steps to `accent-deep`, `active:scale-[0.98]`. Reserve it for the
  one action that matters on the surface.
- **Quiet button** — `line` border on panel, `dim` text, hover fills `panel2`. The
  default row action.
- **Danger button** — quiet shape, red-700 text, red-50 hover fill. Red is semantic.
- **Links** — body links are ink-colored with a `hairline` bottom border that darkens
  to `ink` on hover; nav links are borderless `dim → ink`. Blue is not the link color.
- **Card** — `panel` on `ground`, `line-soft` border, 10px radius; inset zones use
  `panel2` or alternating `panel/panel2` rows (tables).
- **Tag chip** — the Klein-blue pill carrying an uppercase `label`-voice word
  (Share / Join / Follow). Verification chips use the reserved semantic colors.
- **Input** — 44px tall, `line` border on panel, focus ring `accent` at 25–30%
  opacity plus an `accent` border.
- **Terminal window** — `glass` surface, `glass-line` chrome with three traffic-light
  dots, mono transcript: prompts in `glass-ink` with an `accent-phos` `❯` marker,
  output in `glass-dim`, success lines in `accent-phos`, annotations in `glass-faint`.
- **Install command** — single-row glass block: `$` in `glass-faint`, command in
  `glass-ink`, trailing copy affordance.
- **Focus** — every interactive element: `outline-2 outline-accent outline-offset-2`
  on `:focus-visible`.
- **Wordmark** — `topos_` in Martian Mono semibold, the underscore in `accent`.

## Do's and Don'ts

- **Do** place blue as an object (button, chip, marker, label bar). **Don't** wash
  sections, backgrounds, or headings in blue.
- **Don't** mint a new blue, and don't reach for Tailwind stock accents — take a
  lightness step on the `hsl(223 100%)` ramp.
- **Don't** use green or amber outside domain-verification states, or red outside
  errors/destructive actions.
- **Do** keep dark surfaces exclusively for terminal glass. **Don't** build a dark
  section or theme from `glass` tokens.
- **Don't** introduce warm cream/amber neutrals; the warm-gray ramp above is the only
  neutral family.
- **Do** set button labels and commands in mono; **don't** set running text in mono.
- **Do** use sentence case for headings; uppercase belongs to the 10px `label` voice
  only.
- **Don't** add gradients, glows, or emoji-as-icons; the system's texture comes from
  type, hairlines, and the glass transcript.
