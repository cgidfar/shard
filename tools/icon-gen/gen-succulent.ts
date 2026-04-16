/**
 * Shard succulent icon generator — stained-glass rosette motif.
 *
 * Generates a top-down view of a succulent rosette rendered in stained-glass
 * style. Concentric rings of leaf-shaped polygons with dark leading between
 * them, clipped to a disc. Inner rings draw on top of outer rings for natural
 * overlap, just like a real succulent.
 *
 * Usage:
 *   bun gen-succulent.ts                                  # 20 variants
 *   bun gen-succulent.ts --seed 42 --count 1              # single variant
 *   bun gen-succulent.ts --palette rose --fade 0.5        # lighter edges
 *   bun gen-succulent.ts --rings 6 --gap 5                # more rings, wider gaps
 *   bun gen-succulent.ts --veins                          # split each petal down center
 */

import { writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";

// ───────────────────────── Seeded RNG (mulberry32) ─────────────────────────

function mulberry32(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// ───────────────────────── Palettes ─────────────────────────

type ColorStop = [number, [number, number, number]];
const palettes: Record<string, ColorStop[]> = {
  rose: [
    [0.00, [252, 232, 228]],
    [0.13, [240, 200, 200]],
    [0.28, [220, 160, 168]],
    [0.44, [196, 117, 138]],
    [0.60, [148, 82, 108]],
    [0.76, [88, 52, 72]],
    [0.90, [36, 26, 34]],
    [1.00, [11, 12, 15]],
  ],
  sunset: [
    [0.00, [250, 236, 196]],
    [0.12, [245, 223, 160]],
    [0.26, [240, 200, 96]],
    [0.42, [232, 149, 106]],
    [0.58, [196, 112, 80]],
    [0.74, [138, 84, 64]],
    [0.88, [74, 56, 40]],
    [1.00, [42, 26, 20]],
  ],
  // Cool jade-green stained glass
  jade: [
    [0.00, [230, 248, 235]],
    [0.15, [180, 220, 195]],
    [0.30, [120, 190, 155]],
    [0.48, [70, 155, 120]],
    [0.65, [40, 110, 85]],
    [0.80, [22, 65, 52]],
    [1.00, [10, 14, 12]],
  ],
  // Soft lavender → deep violet
  wisteria: [
    [0.00, [240, 230, 250]],
    [0.15, [215, 195, 235]],
    [0.30, [185, 155, 215]],
    [0.48, [150, 110, 190]],
    [0.65, [105, 70, 150]],
    [0.80, [55, 35, 90]],
    [1.00, [12, 10, 18]],
  ],
};

function interpolatePalette(palette: ColorStop[], t: number): string {
  const c = Math.max(0, Math.min(1, t));
  for (let i = 0; i < palette.length - 1; i++) {
    const [p1, c1] = palette[i];
    const [p2, c2] = palette[i + 1];
    if (c >= p1 && c <= p2) {
      const frac = p2 === p1 ? 0 : (c - p1) / (p2 - p1);
      const r = c1[0] + (c2[0] - c1[0]) * frac;
      const g = c1[1] + (c2[1] - c1[1]) * frac;
      const b = c1[2] + (c2[2] - c1[2]) * frac;
      return rgbToHex(r, g, b);
    }
  }
  const last = palette[palette.length - 1][1];
  return rgbToHex(last[0], last[1], last[2]);
}

function rgbToHex(r: number, g: number, b: number): string {
  const h = (v: number) =>
    Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, "0");
  return "#" + h(r) + h(g) + h(b);
}

// ───────────────────────── Chrome colors ─────────────────────────

interface ChromeColors { disc: string; stroke: string; glow: string }
const paletteChrome: Record<string, ChromeColors> = {
  rose:     { disc: "#08090c", stroke: "#292b30", glow: "#fff0f2" },
  jade:     { disc: "#060a08", stroke: "#1a2b24", glow: "#f0fff6" },
  wisteria: { disc: "#08060c", stroke: "#28203a", glow: "#f4f0ff" },
};
const defaultChrome: ChromeColors = { disc: "#080604", stroke: "#2a1a14", glow: "#fff8ee" };

function getChromeColors(name: string): ChromeColors {
  return paletteChrome[name] ?? defaultChrome;
}

// ───────────────────────── Petal shape ─────────────────────────
// Width profile for a succulent leaf seen from above.
// t: 0 = inner (stem end), 1 = outer (leaf tip).
// Returns 0..1 width fraction. Widest around t=0.6, succulent-plump shape.

const petalProfile: [number, number][] = [
  [0.00, 0.00],   // inner tip — pointed
  [0.06, 0.22],   // rapid widening
  [0.16, 0.52],
  [0.30, 0.80],
  [0.45, 0.94],
  [0.60, 1.00],   // maximum width
  [0.75, 0.92],
  [0.87, 0.58],
  [0.95, 0.25],
  [1.00, 0.00],   // outer tip — pointed
];

function sampleProfile(t: number): number {
  const c = Math.max(0, Math.min(1, t));
  for (let i = 0; i < petalProfile.length - 1; i++) {
    const [t1, w1] = petalProfile[i];
    const [t2, w2] = petalProfile[i + 1];
    if (c >= t1 && c <= t2) {
      const frac = t2 === t1 ? 0 : (c - t1) / (t2 - t1);
      return w1 + (w2 - w1) * frac;
    }
  }
  return petalProfile[petalProfile.length - 1][1];
}

function generatePetal(
  cx: number, cy: number,
  angle: number,
  innerR: number, outerR: number,
  maxAngularWidth: number,
  rng: () => number,
  jitter: number,
  facets: number = 7,
): [number, number][] {
  const points: [number, number][] = [];
  const sidePoints = facets;

  // Inner tip
  const tipJitR = (rng() - 0.5) * jitter * 0.3;
  points.push([
    cx + (innerR + tipJitR) * Math.cos(angle),
    cy + (innerR + tipJitR) * Math.sin(angle),
  ]);

  // Left side (inner → outer)
  for (let i = 1; i <= sidePoints; i++) {
    const t = i / (sidePoints + 1);
    const r = innerR + (outerR - innerR) * t;
    const w = sampleProfile(t) * maxAngularWidth * 0.5;
    const rJit = (rng() - 0.5) * jitter;
    const aJit = (rng() - 0.5) * jitter * 0.003;
    points.push([
      cx + (r + rJit) * Math.cos(angle - w + aJit),
      cy + (r + rJit) * Math.sin(angle - w + aJit),
    ]);
  }

  // Outer tip
  const outerJitR = (rng() - 0.5) * jitter * 0.5;
  points.push([
    cx + (outerR + outerJitR) * Math.cos(angle),
    cy + (outerR + outerJitR) * Math.sin(angle),
  ]);

  // Right side (outer → inner)
  for (let i = sidePoints; i >= 1; i--) {
    const t = i / (sidePoints + 1);
    const r = innerR + (outerR - innerR) * t;
    const w = sampleProfile(t) * maxAngularWidth * 0.5;
    const rJit = (rng() - 0.5) * jitter;
    const aJit = (rng() - 0.5) * jitter * 0.003;
    points.push([
      cx + (r + rJit) * Math.cos(angle + w + aJit),
      cy + (r + rJit) * Math.sin(angle + w + aJit),
    ]);
  }

  return points;
}

// ───────────────────────── Polygon shrinkage ─────────────────────────

function shrinkTowardCentroid(
  poly: [number, number][],
  amount: number,
): [number, number][] {
  let cx = 0, cy = 0;
  for (const [x, y] of poly) { cx += x; cy += y; }
  cx /= poly.length;
  cy /= poly.length;

  return poly.map(([x, y]) => {
    const dx = cx - x;
    const dy = cy - y;
    const dist = Math.hypot(dx, dy);
    if (dist < 0.01) return [x, y] as [number, number];
    const move = Math.min(amount, dist * 0.45);
    const k = move / dist;
    return [x + dx * k, y + dy * k] as [number, number];
  });
}

// ───────────────────────── Params ─────────────────────────

interface Params {
  seed: number;
  size: number;
  rings: number;
  basePetals: number;
  petalGrowth: number;
  ringRotation: number;     // degrees between rings
  overlap: number;          // 0..1, how much rings overlap
  petalLenMin: number;      // px, innermost petal length
  petalLenMax: number;      // px, outermost petal length
  petalWidthFactor: number; // fraction of angular slot used
  gap: number;              // stained-glass leading width (px)
  jitter: number;           // shape randomness (px)
  colorJitter: number;      // per-petal color variation
  palette: string;
  fade: number;
  veins: boolean;           // split each petal along center axis
  facets: number;           // vertices per petal side (2=diamond, 3=kite, 7=smooth)
}

function defaultParams(seed: number, overrides: Partial<Params> = {}): Params {
  return {
    seed,
    size: 512,
    rings: 5,
    basePetals: 5,
    petalGrowth: 3,
    ringRotation: 34,
    overlap: 0.35,
    petalLenMin: 35,
    petalLenMax: 95,
    petalWidthFactor: 0.82,
    gap: 3,
    jitter: 4,
    colorJitter: 0.03,
    palette: "rose",
    fade: 0.65,
    veins: false,
    facets: 7,
    ...overrides,
  };
}

// ───────────────────────── Ring layout ─────────────────────────

interface RingLayout {
  index: number;
  petalCount: number;
  innerR: number;
  outerR: number;
  maxAngularWidth: number;
  rotation: number;
  colorT: number;
}

function computeRings(p: Params): RingLayout[] {
  const rings: RingLayout[] = [];

  for (let i = 0; i < p.rings; i++) {
    const t = p.rings <= 1 ? 0 : i / (p.rings - 1);
    const petalCount = Math.round(p.basePetals + i * p.petalGrowth);
    const petalLen = p.petalLenMin + (p.petalLenMax - p.petalLenMin) * t;

    let innerR: number;
    if (i === 0) {
      innerR = 6;
    } else {
      const prevLen = p.petalLenMin + (p.petalLenMax - p.petalLenMin) * ((i - 1) / (p.rings - 1));
      innerR = rings[i - 1].outerR - prevLen * p.overlap;
    }
    const outerR = innerR + petalLen;

    const angularSlot = (2 * Math.PI) / petalCount;
    const maxAngularWidth = angularSlot * p.petalWidthFactor;
    const rotation = i * p.ringRotation * (Math.PI / 180);
    const colorT = t * p.fade;

    rings.push({ index: i, petalCount, innerR, outerR, maxAngularWidth, rotation, colorT });
  }

  return rings;
}

// ───────────────────────── Vein splitting ─────────────────────────
// Split a petal polygon into left and right halves along the radial axis.

function splitPetalAlongVein(
  petal: [number, number][],
  facets: number,
): { left: [number, number][]; right: [number, number][] } {
  // The petal is: [innerTip, left1..N, outerTip, rightN..1]
  // Total: 1 + facets + 1 + facets = 2 + 2*facets points
  const innerTip = petal[0];
  const leftSide = petal.slice(1, 1 + facets);
  const outerTip = petal[1 + facets];
  const rightSide = petal.slice(2 + facets);

  return {
    left: [innerTip, ...leftSide, outerTip],
    right: [innerTip, outerTip, ...rightSide],
  };
}

// ───────────────────────── SVG generation ─────────────────────────

function generateSucculent(p: Params): { svg: string; petalCount: number } {
  const rng = mulberry32(p.seed);
  const size = p.size;
  const cx = size / 2;
  const cy = size / 2;
  const discR = size / 2 - 4;

  const palette = palettes[p.palette] ?? palettes.rose;
  const chrome = getChromeColors(p.palette);
  const rings = computeRings(p);

  const polyStrs: string[] = [];
  let petalCount = 0;

  // Draw outer rings first, inner rings on top (painter's algorithm).
  for (let r = rings.length - 1; r >= 0; r--) {
    const ring = rings[r];

    for (let i = 0; i < ring.petalCount; i++) {
      const angle = (i / ring.petalCount) * 2 * Math.PI + ring.rotation;

      // Per-petal color variation
      const jit = (rng() - 0.5) * 2 * p.colorJitter;
      const colorT = Math.max(0, Math.min(1, ring.colorT + jit));
      const color = interpolatePalette(palette, colorT);

      const petal = generatePetal(
        cx, cy, angle,
        ring.innerR, ring.outerR,
        ring.maxAngularWidth,
        rng, p.jitter, p.facets,
      );

      if (p.veins) {
        const { left, right } = splitPetalAlongVein(petal, p.facets);
        const leftShrunk = shrinkTowardCentroid(left, p.gap / 2);
        const rightShrunk = shrinkTowardCentroid(right, p.gap / 2);

        // Slight shade difference between halves
        const leftT = Math.max(0, Math.min(1, colorT - 0.015));
        const rightT = Math.max(0, Math.min(1, colorT + 0.015));
        const leftColor = interpolatePalette(palette, leftT);
        const rightColor = interpolatePalette(palette, rightT);

        const leftPts = leftShrunk.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
        const rightPts = rightShrunk.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
        polyStrs.push(`    <polygon points="${leftPts}" fill="${leftColor}"/>`);
        polyStrs.push(`    <polygon points="${rightPts}" fill="${rightColor}"/>`);
        petalCount += 2;
      } else {
        const shrunk = shrinkTowardCentroid(petal, p.gap / 2);
        const pts = shrunk.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
        polyStrs.push(`    <polygon points="${pts}" fill="${color}"/>`);
        petalCount++;
      }
    }
  }

  // Build SVG
  const defs = `  <defs>
    <clipPath id="d"><circle cx="${cx}" cy="${cy}" r="${discR}"/></clipPath>
    <radialGradient id="glow" cx="50%" cy="50%" r="22%">
      <stop offset="0%" stop-color="${chrome.glow}" stop-opacity="0.4"/>
      <stop offset="100%" stop-color="${chrome.glow}" stop-opacity="0"/>
    </radialGradient>
  </defs>`;

  const backing = `  <circle cx="${cx}" cy="${cy}" r="${discR + 4}" fill="${chrome.disc}"/>`;
  const cells = `  <g clip-path="url(#d)">\n${polyStrs.join("\n")}\n  </g>`;
  const glow = `  <circle cx="${cx}" cy="${cy}" r="50" fill="url(#glow)"/>`;
  const stroke = `  <circle cx="${cx}" cy="${cy}" r="${discR}" fill="none" stroke="${chrome.stroke}" stroke-width="2"/>`;

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${size} ${size}" width="${size}" height="${size}">
${defs}
${backing}
${cells}
${glow}
${stroke}
</svg>
`;

  return { svg, petalCount };
}

// ───────────────────────── CLI ─────────────────────────

interface CliArgs {
  seed: number;
  count: number;
  out: string;
  palette: string;
  rings: number;
  basePetals: number;
  petalGrowth: number;
  ringRotation: number;
  overlap: number;
  petalLenMin: number;
  petalLenMax: number;
  petalWidthFactor: number;
  gap: number;
  jitter: number;
  colorJitter: number;
  fade: number;
  veins: boolean;
  facets: number;
}

function parseArgs(argv: string[]): CliArgs {
  const out: CliArgs = {
    seed: 1,
    count: 20,
    out: "",
    palette: "rose",
    rings: 5,
    basePetals: 5,
    petalGrowth: 3,
    ringRotation: 34,
    overlap: 0.35,
    petalLenMin: 35,
    petalLenMax: 95,
    petalWidthFactor: 0.82,
    gap: 3,
    jitter: 4,
    colorJitter: 0.03,
    fade: 0.65,
    veins: false,
    facets: 7,
  };

  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    const next = () => argv[++i];
    switch (a) {
      case "--seed": out.seed = parseInt(next(), 10); break;
      case "--count": out.count = parseInt(next(), 10); break;
      case "--out": out.out = next(); break;
      case "--palette": out.palette = next(); break;
      case "--rings": out.rings = parseInt(next(), 10); break;
      case "--base-petals": out.basePetals = parseInt(next(), 10); break;
      case "--petal-growth": out.petalGrowth = parseFloat(next()); break;
      case "--rotation": out.ringRotation = parseFloat(next()); break;
      case "--overlap": out.overlap = parseFloat(next()); break;
      case "--petal-len-min": out.petalLenMin = parseFloat(next()); break;
      case "--petal-len-max": out.petalLenMax = parseFloat(next()); break;
      case "--petal-width": out.petalWidthFactor = parseFloat(next()); break;
      case "--gap": out.gap = parseFloat(next()); break;
      case "--jitter": out.jitter = parseFloat(next()); break;
      case "--color-jitter": out.colorJitter = parseFloat(next()); break;
      case "--fade": out.fade = parseFloat(next()); break;
      case "--veins": out.veins = true; break;
      case "--facets": out.facets = parseInt(next(), 10); break;
      case "--help":
      case "-h":
        printHelp();
        process.exit(0);
      default:
        console.error(`error: unknown flag "${a}"`);
        process.exit(2);
    }
  }
  return out;
}

function printHelp() {
  console.log(`Shard succulent icon generator — stained-glass rosette

Usage: bun gen-succulent.ts [options]

Options:
  --seed N             Starting seed (default 1)
  --count N            Number of variants (default 20)
  --out DIR            Output directory (default: runs/<timestamp>_...)
  --palette NAME       rose | sunset | jade | wisteria (default rose)
  --rings N            Concentric petal rings (default 5)
  --base-petals N      Petals in innermost ring (default 5)
  --petal-growth N     Additional petals per ring (default 3)
  --rotation DEG       Rotation between rings in degrees (default 34)
  --overlap F          0..1, how much rings overlap (default 0.35)
  --petal-len-min PX   Innermost petal length (default 35)
  --petal-len-max PX   Outermost petal length (default 95)
  --petal-width F      0..1, petal width as fraction of slot (default 0.82)
  --gap PX             Stained-glass leading width (default 3)
  --jitter PX          Shape randomness (default 4)
  --color-jitter F     Per-petal color variation (default 0.03)
  --fade F             0..1, how far through palette edges reach (default 0.65)
  --veins              Split each petal down center axis
  --facets N           Vertices per petal side (default 7). Lower = sharper.
                       2=diamond, 3=kite, 4=angular, 7=smooth organic.
`);
}

// ───────────────────────── Gallery ─────────────────────────

function writeGallery(
  outDir: string,
  variants: Array<{ seed: number; file: string; petalCount: number }>,
  args: CliArgs,
) {
  const rows = variants
    .map(
      (v) => `    <div class="card">
      <div class="previews">
        <img class="big" src="${v.file}" alt="seed ${v.seed}">
        <img class="mini" src="${v.file}" alt="" aria-hidden="true">
      </div>
      <div class="label">seed ${v.seed}</div>
      <div class="sub">${v.petalCount} pieces</div>
    </div>`,
    )
    .join("\n");

  const paramStr = [
    `palette=${args.palette}`,
    `rings=${args.rings}`,
    `petals=${args.basePetals}+${args.petalGrowth}/ring`,
    `gap=${args.gap}`,
    `fade=${args.fade}`,
    ...(args.veins ? ["veins"] : []),
  ].join(" · ");

  const chrome = getChromeColors(args.palette);
  const isWarm = !["rose", "jade", "wisteria"].includes(args.palette);
  const g = {
    bg:     isWarm ? "#0f0d0a" : chrome.disc,
    text:   isWarm ? "#e8ddd0" : "#d3d7de",
    accent: args.palette === "jade" ? "#4aac8c" : args.palette === "wisteria" ? "#a78bfa" : "#c4758a",
    muted:  isWarm ? "#b0a08c" : "#9296a0",
    cardBg: isWarm ? "#161310" : "#121316",
    border: isWarm ? "#302820" : chrome.stroke,
    dim:    isWarm ? "#6b5d4f" : "#51555e",
    label:  args.palette === "jade" ? "#80c8a8" : args.palette === "wisteria" ? "#c4b0e8" : "#d4a0b0",
  };

  const html = `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Shard — Succulent Icon Variants</title>
  <style>
    body {
      background: ${g.bg};
      color: ${g.text};
      font-family: system-ui, -apple-system, sans-serif;
      margin: 0;
      padding: 32px 40px;
    }
    h1 { color: ${g.accent}; margin: 0 0 8px; font-size: 22px; }
    .params {
      color: ${g.muted};
      font-family: ui-monospace, "Cascadia Mono", monospace;
      font-size: 12px;
      margin-bottom: 24px;
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
      gap: 20px;
    }
    .card {
      background: ${g.cardBg};
      border: 1px solid ${g.border};
      border-radius: 8px;
      padding: 12px;
      text-align: center;
      transition: transform 0.1s;
    }
    .card:hover { transform: scale(1.03); border-color: ${g.accent}; }
    .card .previews {
      display: flex;
      align-items: flex-end;
      gap: 10px;
    }
    .card img.big {
      flex: 1;
      aspect-ratio: 1;
      display: block;
      min-width: 0;
    }
    .card img.mini {
      flex: none;
      width: 32px;
      height: 32px;
      display: block;
      border: 1px solid ${g.border};
      background: #000;
    }
    .card .label {
      color: ${g.label};
      font-family: ui-monospace, "Cascadia Mono", monospace;
      font-size: 13px;
      margin-top: 8px;
    }
    .card .sub {
      color: ${g.dim};
      font-family: ui-monospace, "Cascadia Mono", monospace;
      font-size: 11px;
    }
  </style>
</head>
<body>
  <h1>Shard — Succulent Icon Variants</h1>
  <div class="params">${paramStr}</div>
  <div class="grid">
${rows}
  </div>
</body>
</html>
`;
  writeFileSync(join(outDir, "index.html"), html);
}

// ───────────────────────── Run folder naming ─────────────────────────

function formatTimestamp(d: Date = new Date()): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return (
    d.getFullYear() +
    p(d.getMonth() + 1) +
    p(d.getDate()) +
    "-" +
    p(d.getHours()) +
    p(d.getMinutes()) +
    p(d.getSeconds())
  );
}

function makeAutoDirName(args: CliArgs): string {
  const ts = formatTimestamp();
  const veinPart = args.veins ? "_veins" : "";
  const facetPart = args.facets !== 7 ? `_facets${args.facets}` : "";
  return `${ts}_succulent_${args.palette}_r${args.rings}_f${args.fade}_g${args.gap}${veinPart}${facetPart}`;
}

// ───────────────────────── Main ─────────────────────────

function main() {
  const args = parseArgs(process.argv);
  const scriptDir = import.meta.dir ?? ".";

  const outDir = args.out
    ? join(scriptDir, args.out)
    : join(scriptDir, "runs", makeAutoDirName(args));
  mkdirSync(outDir, { recursive: true });

  if (!palettes[args.palette]) {
    console.warn(`Unknown palette "${args.palette}", using rose`);
  }

  const variants: Array<{ seed: number; file: string; petalCount: number }> = [];

  for (let i = 0; i < args.count; i++) {
    const seed = args.seed + i;
    const params = defaultParams(seed, {
      rings: args.rings,
      basePetals: args.basePetals,
      petalGrowth: args.petalGrowth,
      ringRotation: args.ringRotation,
      overlap: args.overlap,
      petalLenMin: args.petalLenMin,
      petalLenMax: args.petalLenMax,
      petalWidthFactor: args.petalWidthFactor,
      gap: args.gap,
      jitter: args.jitter,
      colorJitter: args.colorJitter,
      palette: args.palette,
      fade: args.fade,
      veins: args.veins,
      facets: args.facets,
    });

    const { svg, petalCount } = generateSucculent(params);
    const filename = `succulent-${String(seed).padStart(4, "0")}.svg`;
    writeFileSync(join(outDir, filename), svg);
    variants.push({ seed, file: filename, petalCount });
    console.log(`  ${filename}  (${petalCount} pieces)`);
  }

  const paramsJson = {
    timestamp: new Date().toISOString(),
    command: "bun gen-succulent.ts " + process.argv.slice(2).join(" "),
    seeds: { start: args.seed, end: args.seed + args.count - 1, count: args.count },
    config: {
      palette: args.palette, rings: args.rings, basePetals: args.basePetals,
      petalGrowth: args.petalGrowth, ringRotation: args.ringRotation, overlap: args.overlap,
      petalLenMin: args.petalLenMin, petalLenMax: args.petalLenMax,
      petalWidthFactor: args.petalWidthFactor, gap: args.gap, jitter: args.jitter,
      colorJitter: args.colorJitter, fade: args.fade, veins: args.veins, facets: args.facets,
    },
  };
  writeFileSync(join(outDir, "params.json"), JSON.stringify(paramsJson, null, 2));

  writeGallery(outDir, variants, args);
  console.log(`\n  ${variants.length} variants → ${outDir}`);
  console.log(`  gallery: ${join(outDir, "index.html")}`);
}

main();
