import { useEffect, useRef } from "react";

/**
 * The routing diagram: a skill chip leaves its publisher, is verified at the hub (gains a
 * check), then splits and travels onward to every follower — across harnesses. The five nodes
 * and the hub are static SVG rendered by React; the animated chips live in one ref-owned <g>
 * that React never reconciles, driven by requestAnimationFrame on straight ease-in-out paths.
 * Reduced motion shows the static diagram only.
 */

type NodeDef = {
  id: string;
  x: number;
  y: number;
  name: string;
  role: string;
  harness: string;
};

const NODES: NodeDef[] = [
  { id: "maya", x: 75, y: 52, name: "maya", role: "@backend", harness: "Claude Code" },
  { id: "dev", x: 345, y: 52, name: "dev", role: "@backend", harness: "OpenClaw" },
  { id: "ana", x: 352, y: 214, name: "ana", role: "@frontend", harness: "Hermes" },
  { id: "sam", x: 210, y: 326, name: "sam", role: "@support", harness: "Claude app" },
  { id: "kim", x: 62, y: 214, name: "kim", role: "@marketing", harness: "Claude app" },
];
const HUB = { x: 210, y: 184 };
const SCENES = [
  { skill: "deploy", from: "maya", to: ["dev", "ana"] },
  { skill: "brand-voice", from: "kim", to: ["sam", "ana"] },
];

const CHIP_NS = "http://www.w3.org/2000/svg";

function nodeAt(id: string): { x: number; y: number } {
  const node = NODES.find((n) => n.id === id);
  return node ? { x: node.x, y: node.y } : HUB;
}

export function RoutingStar() {
  const svgRef = useRef<SVGSVGElement>(null);
  const chipLayer = useRef<SVGGElement>(null);

  useEffect(() => {
    const svg = svgRef.current;
    const layer = chipLayer.current;
    if (!svg || !layer) {
      return;
    }
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      return;
    }

    let alive = true;
    const timers = new Set<ReturnType<typeof setTimeout>>();
    const frames = new Set<number>();

    function sleep(ms: number): Promise<void> {
      return new Promise((resolve) => {
        const t = setTimeout(() => {
          timers.delete(t);
          resolve();
        }, ms);
        timers.add(t);
      });
    }

    function makeChip(text: string, at?: { x: number; y: number }): SVGGElement {
      const width = text.length * 6.2 + 16;
      const g = document.createElementNS(CHIP_NS, "g");
      if (at) {
        g.setAttribute("transform", `translate(${at.x},${at.y})`);
      }
      const rect = document.createElementNS(CHIP_NS, "rect");
      rect.setAttribute("x", String(-width / 2));
      rect.setAttribute("y", "-10");
      rect.setAttribute("width", String(width));
      rect.setAttribute("height", "20");
      rect.setAttribute("rx", "9");
      rect.setAttribute("fill", "var(--color-accent)");
      const label = document.createElementNS(CHIP_NS, "text");
      label.setAttribute("text-anchor", "middle");
      label.setAttribute("y", "3.5");
      label.setAttribute("fill", "var(--color-on-accent)");
      label.setAttribute(
        "style",
        "font-family: var(--font-mono); font-size: 10px; font-weight: 500;",
      );
      label.textContent = text;
      g.appendChild(rect);
      g.appendChild(label);
      layer?.appendChild(g);
      return g;
    }

    /** Straight-line flight with ease-in-out, translate only. */
    function fly(
      chip: SVGGElement,
      from: { x: number; y: number },
      to: { x: number; y: number },
      duration: number,
    ): Promise<void> {
      return new Promise((resolve) => {
        let t0: number | null = null;
        // One pending frame id per flight: each tick retires its own id before scheduling the
        // next, so `frames` stays bounded at the number of in-flight chips (it only holds what
        // cleanup genuinely needs to cancel).
        let f = 0;
        function tick(ts: number) {
          frames.delete(f);
          if (!alive) {
            return resolve();
          }
          if (t0 === null) {
            t0 = ts;
          }
          const k = Math.min(1, (ts - t0) / duration);
          const e = k < 0.5 ? 2 * k * k : 1 - (-2 * k + 2) ** 2 / 2;
          const x = from.x + (to.x - from.x) * e;
          const y = from.y + (to.y - from.y) * e;
          chip.setAttribute("transform", `translate(${x},${y})`);
          if (k < 1) {
            f = requestAnimationFrame(tick);
            frames.add(f);
          } else {
            resolve();
          }
        }
        f = requestAnimationFrame(tick);
        frames.add(f);
      });
    }

    function light(id: string, ms: number) {
      const el = svg?.querySelector(`[data-lit="${id}"]`);
      if (!el) {
        return;
      }
      el.setAttribute("data-on", "1");
      const t = setTimeout(() => {
        timers.delete(t);
        el.removeAttribute("data-on");
      }, ms);
      timers.add(t);
    }

    async function run() {
      let i = 0;
      while (alive) {
        // Scene-boundary pause: when the diagram scrolls off-screen the loop parks here
        // instead of animating an invisible SVG for the page's lifetime.
        await untilVisible();
        if (!alive) {
          return;
        }
        const scene = SCENES[i % SCENES.length];
        i += 1;
        if (!scene) {
          return;
        }
        light(scene.from, 1600);
        const chip = makeChip(scene.skill);
        await fly(chip, nodeAt(scene.from), HUB, 850);
        chip.remove();
        if (!alive) {
          return;
        }
        // Verified at the hub: the chip gains a check, then it splits and travels onward.
        const checked = makeChip(`${scene.skill} ✓`, HUB);
        light("hub", 900);
        await sleep(650);
        const flights = scene.to.map((id) => {
          const copy = makeChip(`${scene.skill} ✓`, HUB);
          return fly(copy, HUB, nodeAt(id), 850).then(() => {
            copy.remove();
            light(id, 1400);
          });
        });
        checked.remove();
        await Promise.all(flights);
        await sleep(2400);
      }
    }

    let started = false;
    let visible = false;
    let onVisible: (() => void) | null = null;

    function untilVisible(): Promise<void> {
      if (visible) {
        return Promise.resolve();
      }
      return new Promise((resolve) => {
        onVisible = resolve;
      });
    }

    const observer = new IntersectionObserver(
      (entries) => {
        visible = entries[0]?.isIntersecting ?? false;
        if (!visible) {
          return;
        }
        if (onVisible) {
          onVisible();
          onVisible = null;
        }
        if (!started) {
          started = true;
          void run();
        }
      },
      { threshold: 0.3 },
    );
    observer.observe(svg);

    return () => {
      alive = false;
      observer.disconnect();
      // Release a run() parked off-screen so its (now dead) loop can exit and be collected.
      if (onVisible) {
        onVisible();
        onVisible = null;
      }
      for (const t of timers) {
        clearTimeout(t);
      }
      for (const f of frames) {
        cancelAnimationFrame(f);
      }
      layer.replaceChildren();
    };
  }, []);

  return (
    <div>
      <svg
        ref={svgRef}
        viewBox="0 0 420 372"
        role="img"
        aria-label="A skill published by one person is verified by Topos, then delivered onward to every teammate who follows it, across different agent apps"
        className="block h-auto w-full"
      >
        <title>Skill routing through Topos</title>
        <g>
          {NODES.map((n) => (
            <line
              key={n.id}
              x1={n.x}
              y1={n.y}
              x2={HUB.x}
              y2={HUB.y}
              className="stroke-[1.2] stroke-line"
            />
          ))}
        </g>
        {NODES.map((n) => (
          <g key={n.id} transform={`translate(${n.x},${n.y})`} data-lit={n.id} className="group">
            <rect
              x="-55"
              y="-27"
              width="110"
              height="54"
              rx="8"
              className="fill-panel stroke-[1.2] stroke-line transition-[stroke] duration-300 group-data-[on]:stroke-accent"
            />
            <text x="-43" y="-9" className="fill-ink font-mono text-[11.5px]">
              {n.name}
            </text>
            <text x="-43" y="6" className="fill-dim font-mono text-[9.5px]">
              {n.role}
            </text>
            <text x="-43" y="20" className="fill-faint font-mono text-[9.5px]">
              {n.harness}
            </text>
          </g>
        ))}
        <g transform={`translate(${HUB.x},${HUB.y})`} data-lit="hub" className="group">
          <circle
            r="30"
            className="fill-panel stroke-[1.2] stroke-ink transition-[stroke,stroke-width] duration-300 group-data-[on]:stroke-[2] group-data-[on]:stroke-accent"
          />
          <text
            textAnchor="middle"
            y="4"
            className="fill-ink font-display font-semibold text-[11px]"
          >
            topos
          </text>
        </g>
        <g ref={chipLayer} />
      </svg>
      <p className="mt-2 text-center text-[12.5px] text-faint">
        Verified by Topos, then delivered to everyone who follows it.
      </p>
    </div>
  );
}
