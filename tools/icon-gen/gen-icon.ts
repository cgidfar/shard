/**
 * Shard icon generator — procedural shattered-glass motif.
 *
 * Algorithm:
 *   1. Scatter seed points in concentric rings (fractal density: each ring has
 *      `density^i` times more points than the innermost ring).
 *   2. Compute Voronoi tessellation from those seeds — organic, tiling polygons.
 *   3. Shrink each cell toward its seed by an amount that grows with the seed's
 *      distance from the image center (weighted by `gapCurve`). This produces
 *      tight leading near the center, wider gaps near the edge.
 *   4. Color each cell by distance-from-center, interpolated through a palette.
 *   5. Emit SVG, clipped to a disc.
 *
 * Usage:
 *   bun gen-icon.ts                                  # 20 variants, seeds 1..20
 *   bun gen-icon.ts --seed 42 --count 1              # single variant
 *   bun gen-icon.ts --palette ember --count 30       # different palette
 *   bun gen-icon.ts --gap-max 12 --gap-curve cube    # wider, more dramatic gaps
 */

import { Delaunay } from "d3-delaunay";
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
// Each palette: [position 0..1, [r, g, b]]. Interpolated smoothly.

type ColorStop = [number, [number, number, number]];
const palettes: Record<string, ColorStop[]> = {
  sunset: [
    [0.00, [250, 236, 196]], // hot gold-white
    [0.12, [245, 223, 160]],
    [0.26, [240, 200, 96]],
    [0.42, [232, 149, 106]], // accent orange
    [0.58, [196, 112, 80]],
    [0.74, [138, 84, 64]],
    [0.88, [74, 56, 40]],
    [1.00, [42, 26, 20]],
  ],
  ember: [
    [0.00, [255, 245, 210]],
    [0.15, [250, 200, 110]],
    [0.30, [240, 130, 70]],
    [0.50, [210, 70, 50]],
    [0.70, [140, 40, 35]],
    [0.88, [70, 25, 20]],
    [1.00, [30, 14, 10]],
  ],
  desert: [
    [0.00, [248, 232, 190]],
    [0.20, [230, 190, 130]],
    [0.40, [205, 140, 85]],
    [0.60, [160, 95, 60]],
    [0.80, [100, 65, 45]],
    [1.00, [45, 30, 22]],
  ],
  volcano: [
    [0.00, [255, 252, 240]],
    [0.10, [255, 220, 110]],
    [0.25, [250, 140, 50]],
    [0.45, [220, 70, 40]],
    [0.65, [150, 35, 25]],
    [0.85, [60, 20, 15]],
    [1.00, [15, 8, 6]],
  ],
  // Vivid real-sky sunset: gold → coral → magenta → violet → near-black purple
  twilight: [
    [0.00, [255, 242, 196]],
    [0.13, [255, 213, 128]],
    [0.26, [255, 158, 94]],
    [0.40, [242, 107, 122]],
    [0.55, [192, 72, 150]],
    [0.70, [124, 58, 158]],
    [0.85, [61, 29, 92]],
    [1.00, [21, 10, 40]],
  ],
  // Muted dusty variant: soft peach → mauve → slate-purple
  dusk: [
    [0.00, [248, 232, 208]],
    [0.16, [240, 200, 144]],
    [0.32, [232, 154, 138]],
    [0.48, [200, 120, 152]],
    [0.64, [138, 88, 143]],
    [0.80, [80, 56, 112]],
    [1.00, [24, 18, 40]],
  ],
  // Direct gold → amethyst with less pink emphasis
  amethyst: [
    [0.00, [255, 240, 200]],
    [0.14, [248, 200, 104]],
    [0.28, [232, 144, 85]],
    [0.44, [184, 96, 144]],
    [0.60, [112, 80, 160]],
    [0.76, [60, 40, 104]],
    [0.90, [28, 20, 60]],
    [1.00, [12, 8, 28]],
  ],
  // Peach → coral → rose → wine. Anchored on user-supplied colors; bright
  // golden-cream core for warmth, extends to near-black burgundy at the edge.
  cinder: [
    [0.00, [252, 234, 190]], // bright gold-cream core
    [0.13, [248, 200, 130]], // warm golden peach
    [0.27, [243, 158, 96]],  // user: peach
    [0.43, [225, 106, 84]],  // user: coral
    [0.58, [188, 92, 84]],   // blend
    [0.72, [159, 82, 85]],   // user: rose
    [0.85, [124, 68, 79]],   // user: wine
    [0.95, [62, 30, 38]],    // dark burgundy
    [1.00, [20, 10, 14]],    // near-black wine
  ],
  // Gunmetal Rose: pink-cream center → dusty rose accent → cool gunmetal edge.
  // Passes through the UI accent color #c4758a at t≈0.44.
  rose: [
    [0.00, [252, 232, 228]], // warm cream with pink tint
    [0.13, [240, 200, 200]], // soft blush
    [0.28, [220, 160, 168]], // light rose
    [0.44, [196, 117, 138]], // dusty rose — accent #c4758a
    [0.60, [148, 82, 108]],  // deep mauve
    [0.76, [88, 52, 72]],    // dark plum
    [0.90, [36, 26, 34]],    // near-black with rose tint
    [1.00, [11, 12, 15]],    // gunmetal black — surface #0b0c0f
  ],
};

// Chrome colors for the SVG frame — backing disc, outer stroke, glow tint.
// Warm palettes use the original dark-brown chrome; cool palettes get matched.
interface ChromeColors { disc: string; stroke: string; glow: string }
const paletteChrome: Record<string, ChromeColors> = {
  rose:     { disc: "#08090c", stroke: "#292b30", glow: "#fff0f2" },
};
const defaultChrome: ChromeColors = { disc: "#080604", stroke: "#2a1a14", glow: "#fff8ee" };

function getChromeColors(paletteName: string): ChromeColors {
  return paletteChrome[paletteName] ?? defaultChrome;
}

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

// ───────────────────────── Gap curves ─────────────────────────

const gapCurves: Record<string, (t: number) => number> = {
  linear: (t) => t,
  quad: (t) => t * t,
  cube: (t) => t * t * t,
  exp: (t) => (Math.exp(t * 2.2) - 1) / (Math.exp(2.2) - 1),
  sqrt: (t) => Math.sqrt(t),
  smooth: (t) => t * t * (3 - 2 * t),
};

// ───────────────────────── Params ─────────────────────────

interface Params {
  seed: number;
  size: number;
  rings: number;
  basePoints: number;
  density: number;
  radiusMax: number;
  gapMin: number;
  gapMax: number;
  gapCurve: (t: number) => number;
  gapCurveName: string;
  palette: string;
  angleJitter: number;   // 0..1, fraction of slot width
  radiusJitter: number;  // px
  dropRings: number;     // drop the N outermost rings (jagged edge); 0 = smooth
  fade: number;          // 0..1, how far through the palette the edge reaches
}

function defaultParams(seed: number, overrides: Partial<Params> = {}): Params {
  return {
    seed,
    size: 512,
    rings: 5,
    basePoints: 6,
    density: 1.55,
    radiusMax: 232,
    gapMin: 1.5,
    gapMax: 9,
    gapCurve: gapCurves.quad,
    gapCurveName: "quad",
    palette: "sunset",
    angleJitter: 0.55,
    radiusJitter: 14,
    dropRings: 0,
    fade: 1.0,
    ...overrides,
  };
}

// ───────────────────────── Seed-point scatter ─────────────────────────

interface SeedScatter {
  points: Array<[number, number]>;
  // Per-seed ring index (0 = center, 1..rings = ring number).
  // Used by the renderer to drop the N outermost rings for a jagged edge.
  ringIndex: number[];
}

function generateSeeds(p: Params, rng: () => number): SeedScatter {
  const cx = p.size / 2;
  const cy = p.size / 2;
  const points: Array<[number, number]> = [];
  const ringIndex: number[] = [];

  // Single center point — it will be the brightest cell.
  points.push([cx, cy]);
  ringIndex.push(0);

  for (let ring = 1; ring <= p.rings; ring++) {
    const tRing = ring / p.rings;
    const r = tRing * p.radiusMax;
    const n = Math.max(3, Math.round(p.basePoints * Math.pow(p.density, ring - 1)));
    const slot = (2 * Math.PI) / n;

    for (let i = 0; i < n; i++) {
      const baseAngle = i * slot + (ring * 0.37); // rotate each ring so slots don't line up
      const aJit = (rng() - 0.5) * p.angleJitter * slot;
      const rJit = (rng() - 0.5) * 2 * p.radiusJitter;
      const angle = baseAngle + aJit;
      const radius = Math.max(8, r + rJit);
      points.push([cx + radius * Math.cos(angle), cy + radius * Math.sin(angle)]);
      ringIndex.push(ring);
    }
  }

  return { points, ringIndex };
}

// ───────────────────────── Polygon shrinkage ─────────────────────────
// Pull every vertex of `poly` toward `(px, py)` by `amount` px. For convex
// Voronoi cells (Voronoi cells are always convex), this is a reasonable
// approximation of a polygon inset and produces a uniform-looking gap.

function shrinkTowardPoint(
  poly: Array<[number, number]>,
  px: number,
  py: number,
  amount: number,
): Array<[number, number]> {
  return poly.map(([x, y]) => {
    const dx = px - x;
    const dy = py - y;
    const dist = Math.hypot(dx, dy);
    if (dist < 0.01) return [x, y];
    const move = Math.min(amount, dist * 0.48); // don't let the cell collapse
    const k = move / dist;
    return [x + dx * k, y + dy * k] as [number, number];
  });
}

// ───────────────────────── SVG generation ─────────────────────────

function generateIcon(p: Params): { svg: string; cellCount: number } {
  const rng = mulberry32(p.seed);
  const size = p.size;
  const cx = size / 2;
  const cy = size / 2;
  const discR = size / 2 - 4;

  const { points: seeds, ringIndex } = generateSeeds(p, rng);
  const delaunay = Delaunay.from(seeds);
  // Note: we always compute the Voronoi from ALL seeds (including any
  // dropped ones) so that the kept cells retain their natural shapes —
  // their outer edges are constrained by the now-invisible outer cells,
  // which is what produces a believably jagged boundary.
  const voronoi = delaunay.voronoi([-size, -size, size * 2, size * 2]);

  const palette = palettes[p.palette] ?? palettes.sunset;
  const chrome = getChromeColors(p.palette);

  // When dropRings > 0 we drop the smooth disc, clip, and outer stroke
  // entirely so the icon ends with the natural cell-edge silhouette.
  const jagged = p.dropRings > 0;
  // Cells in any ring strictly greater than this index are dropped.
  const maxKeptRing = p.rings - p.dropRings;

  const polyStrs: string[] = [];
  let cellCount = 0;

  for (let i = 0; i < seeds.length; i++) {
    if (ringIndex[i] > maxKeptRing) continue;

    const cell = voronoi.cellPolygon(i);
    if (!cell || cell.length < 4) continue;

    // cellPolygon returns a closed ring (first vertex repeated at end). Drop the duplicate.
    const verts: Array<[number, number]> = [];
    for (let j = 0; j < cell.length - 1; j++) {
      verts.push([cell[j][0], cell[j][1]]);
    }

    const [sx, sy] = seeds[i];
    const distFromCenter = Math.hypot(sx - cx, sy - cy);
    const tDist = Math.min(1, distFromCenter / p.radiusMax);

    const curveT = p.gapCurve(tDist);
    const shrink = p.gapMin + (p.gapMax - p.gapMin) * curveT;

    const shrunk = shrinkTowardPoint(verts, sx, sy, shrink / 2);

    const color = interpolatePalette(palette, tDist * p.fade);

    const pts = shrunk
      .map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`)
      .join(" ");

    polyStrs.push(`    <polygon points="${pts}" fill="${color}"/>`);
    cellCount++;
  }

  // Smooth (default) layout: backing disc + circular clip + outer stroke ring.
  // Jagged layout: no disc, no clip, no stroke — just the cells.
  const defs = jagged
    ? `  <defs>
    <radialGradient id="glow" cx="50%" cy="50%" r="18%">
      <stop offset="0%" stop-color="${chrome.glow}" stop-opacity="0.35"/>
      <stop offset="100%" stop-color="${chrome.glow}" stop-opacity="0"/>
    </radialGradient>
  </defs>`
    : `  <defs>
    <clipPath id="d"><circle cx="${cx}" cy="${cy}" r="${discR}"/></clipPath>
    <radialGradient id="glow" cx="50%" cy="50%" r="18%">
      <stop offset="0%" stop-color="${chrome.glow}" stop-opacity="0.35"/>
      <stop offset="100%" stop-color="${chrome.glow}" stop-opacity="0"/>
    </radialGradient>
  </defs>`;

  const backing = jagged
    ? ""
    : `  <circle cx="${cx}" cy="${cy}" r="${discR + 4}" fill="${chrome.disc}"/>\n`;

  const cellsGroup = jagged
    ? `  <g>\n${polyStrs.join("\n")}\n  </g>`
    : `  <g clip-path="url(#d)">\n${polyStrs.join("\n")}\n  </g>`;

  const outerRing = jagged
    ? ""
    : `  <circle cx="${cx}" cy="${cy}" r="${discR}" fill="none" stroke="${chrome.stroke}" stroke-width="2"/>\n`;

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${size} ${size}" width="${size}" height="${size}">
${defs}
${backing}${cellsGroup}
  <circle cx="${cx}" cy="${cy}" r="40" fill="url(#glow)"/>
${outerRing}</svg>
`;

  return { svg, cellCount };
}

// ───────────────────────── CLI ─────────────────────────

interface CliArgs {
  seed: number;
  count: number;
  out: string;
  palette: string;
  rings: number;
  density: number;
  basePoints: number;
  gapMin: number;
  gapMax: number;
  gapCurve: string;
  radiusJitter: number;
  angleJitter: number;
  dropRings: number;
  fade: number;
}

function parseArgs(argv: string[]): CliArgs {
  const out: CliArgs = {
    seed: 1,
    count: 20,
    out: "", // empty means auto-generate a timestamped run folder
    palette: "sunset",
    rings: 5,
    density: 1.55,
    basePoints: 6,
    gapMin: 1.5,
    gapMax: 9,
    gapCurve: "quad",
    radiusJitter: 14,
    angleJitter: 0.55,
    dropRings: 0,
    fade: 1.0,
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
      case "--density": out.density = parseFloat(next()); break;
      case "--base":
      case "--base-points": out.basePoints = parseInt(next(), 10); break;
      case "--gap-min": out.gapMin = parseFloat(next()); break;
      case "--gap-max": out.gapMax = parseFloat(next()); break;
      case "--gap-curve": out.gapCurve = next(); break;
      case "--radius-jitter": out.radiusJitter = parseFloat(next()); break;
      case "--angle-jitter": out.angleJitter = parseFloat(next()); break;
      case "--drop-rings": out.dropRings = parseInt(next(), 10); break;
      case "--fade": out.fade = parseFloat(next()); break;
      case "--help":
      case "-h":
        printHelp();
        process.exit(0);
      default:
        console.error(`error: unknown flag "${a}"`);
        console.error(`       run with --help for the list of supported flags`);
        process.exit(2);
    }
  }
  return out;
}

function printHelp() {
  console.log(`Shard icon generator

Usage: bun gen-icon.ts [options]

Options:
  --seed N            Starting seed (default 1)
  --count N           Number of variants to generate (default 20)
  --out DIR           Output directory (default: runs/<timestamp>_<params>/)
  --palette NAME      sunset | ember | desert | volcano | twilight | dusk
                      | amethyst | cinder | rose (default sunset)
  --rings N           Number of concentric rings of seeds (default 5)
  --base N            Points in innermost ring (default 6; alias: --base-points)
  --density X         Each ring has density^i times more points (default 1.55)
  --gap-min PX        Minimum leading between fragments (default 1.5)
  --gap-max PX        Maximum leading between fragments (default 9)
  --gap-curve NAME    linear | quad | cube | exp | sqrt | smooth (default quad)
  --radius-jitter PX  Random offset applied to ring radius (default 14)
  --angle-jitter F    0..1, fraction of slot width to randomize (default 0.55)
  --drop-rings N      Drop the N outermost rings of cells AND the smooth disc/
                      stroke, leaving a jagged silhouette (default 0 = smooth).
                      Pair with a higher --rings if you want to keep the icon's
                      visual size, e.g. --rings 6 --drop-rings 1.
  --fade F            0..1, how far through the palette the edge reaches
                      (default 1.0). Lower = lighter edges. 0.6 stops at
                      deep mauve, 0.4 stays near the accent color.
`);
}

// ───────────────────────── Gallery ─────────────────────────

function writeGallery(
  outDir: string,
  variants: Array<{ seed: number; file: string; cellCount: number }>,
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
      <div class="sub">${v.cellCount} cells</div>
    </div>`,
    )
    .join("\n");

  const paramStr = [
    `palette=${args.palette}`,
    `rings=${args.rings}`,
    `density=${args.density}`,
    `base=${args.basePoints}`,
    `gap=${args.gapMin}..${args.gapMax}`,
    `curve=${args.gapCurve}`,
    ...(args.dropRings > 0 ? [`drop=${args.dropRings}`] : []),
  ].join(" · ");

  // Gallery chrome matches the palette family — warm for warm, cool for cool.
  const isRose = args.palette === "rose";
  const g = {
    bg:       isRose ? "#0b0c0f" : "#0f0d0a",
    text:     isRose ? "#d3d7de" : "#e8ddd0",
    accent:   isRose ? "#c4758a" : "#e8956a",
    muted:    isRose ? "#9296a0" : "#b0a08c",
    cardBg:   isRose ? "#121316" : "#161310",
    border:   isRose ? "#292b30" : "#302820",
    dim:      isRose ? "#51555e" : "#6b5d4f",
    label:    isRose ? "#d4a0b0" : "#f0a57e",
  };

  const html = `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Shard — Generated Icon Variants</title>
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
  <h1>Shard — Generated Icon Variants</h1>
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
// Default output is runs/<timestamp>_<param-summary>/ so consecutive runs
// never overwrite each other and the folder name tells you what produced it.

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
  // Compact param summary — palette, rings, base, density, gap range, curve.
  // Format: 20260411-143022_sunset_r5b6_d1.55_g1.5-9_quad[_drop1]
  const dropPart = args.dropRings > 0 ? `_drop${args.dropRings}` : "";
  const fadePart = args.fade < 1.0 ? `_f${args.fade}` : "";
  return `${ts}_${args.palette}_r${args.rings}b${args.basePoints}_d${args.density}_g${args.gapMin}-${args.gapMax}_${args.gapCurve}${dropPart}${fadePart}`;
}

// ───────────────────────── Main ─────────────────────────

function main() {
  const args = parseArgs(process.argv);
  const scriptDir = import.meta.dir ?? ".";

  // If --out was provided, honor it verbatim. Otherwise auto-generate a
  // timestamped folder under runs/ so we never clobber a prior batch.
  const outDir = args.out
    ? join(scriptDir, args.out)
    : join(scriptDir, "runs", makeAutoDirName(args));
  mkdirSync(outDir, { recursive: true });

  const curveFn = gapCurves[args.gapCurve] ?? gapCurves.quad;
  if (!gapCurves[args.gapCurve]) {
    console.warn(`Unknown gap curve "${args.gapCurve}", using quad`);
  }
  if (!palettes[args.palette]) {
    console.warn(`Unknown palette "${args.palette}", using sunset`);
  }

  const variants: Array<{ seed: number; file: string; cellCount: number }> = [];

  for (let i = 0; i < args.count; i++) {
    const seed = args.seed + i;
    const params = defaultParams(seed, {
      rings: args.rings,
      basePoints: args.basePoints,
      density: args.density,
      gapMin: args.gapMin,
      gapMax: args.gapMax,
      gapCurve: curveFn,
      gapCurveName: args.gapCurve,
      palette: args.palette,
      radiusJitter: args.radiusJitter,
      angleJitter: args.angleJitter,
      dropRings: args.dropRings,
      fade: args.fade,
    });

    const { svg, cellCount } = generateIcon(params);
    const filename = `icon-${String(seed).padStart(4, "0")}.svg`;
    writeFileSync(join(outDir, filename), svg);
    variants.push({ seed, file: filename, cellCount });
    console.log(`  ${filename}  (${cellCount} cells)`);
  }

  // Snapshot the full params so the run is fully reproducible later.
  const paramsJson = {
    timestamp: new Date().toISOString(),
    command: "bun gen-icon.ts " + process.argv.slice(2).join(" "),
    seeds: {
      start: args.seed,
      end: args.seed + args.count - 1,
      count: args.count,
    },
    config: {
      palette: args.palette,
      rings: args.rings,
      basePoints: args.basePoints,
      density: args.density,
      gapMin: args.gapMin,
      gapMax: args.gapMax,
      gapCurve: args.gapCurve,
      radiusJitter: args.radiusJitter,
      angleJitter: args.angleJitter,
      dropRings: args.dropRings,
      fade: args.fade,
    },
  };
  writeFileSync(join(outDir, "params.json"), JSON.stringify(paramsJson, null, 2));

  writeGallery(outDir, variants, args);
  console.log(`\n  ${variants.length} variants → ${outDir}`);
  console.log(`  gallery: ${join(outDir, "index.html")}`);
}

main();
