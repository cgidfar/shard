/**
 * Iconify: take a source shard icon SVG and produce two rendered variants.
 *
 *   alpha   — dark backing disc removed. Gaps between fragments are
 *             transparent; the icon reads as floating shards.
 *   black   — dark backing replaced with pure #000000. Classic icon look,
 *             solid circle with the tessellation on top.
 *
 * Both variants keep the SVG viewBox corners transparent (so the icon is
 * circular when rendered to PNG, not a square).
 *
 * Usage:
 *   bun iconify.ts <path-to-source-svg>
 *
 * Output → icon-drafts/final/
 *   shard-icon-alpha.svg             shard-icon-black.svg
 *   shard-icon-alpha-1024.png        shard-icon-black-1024.png
 *   shard-icon-alpha-512.png         shard-icon-black-512.png
 *   ...                              ...
 *   preview.html                     (shows both variants at every size)
 */

import { Resvg } from "@resvg/resvg-js";
import pngToIco from "png-to-ico";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { join, basename } from "node:path";

// Sizes we render as standalone PNGs (also used in the preview).
const SIZES = [1024, 512, 256, 128, 64, 48, 32, 24, 16];
// Sizes packed into the Windows .ico file (Microsoft standard set).
const ICO_SIZES = [256, 128, 64, 48, 32, 16];
// Sizes small enough that we show a pixelated zoom in the preview to make
// small-size legibility problems obvious.
const ZOOM_SIZES = [16, 24, 32, 48, 64];

// ───────────────────────── Variant builders ─────────────────────────

function makeAlphaVariant(svg: string): string {
  // Drop the dark background disc entirely. The tessellation still draws,
  // the outer stroke ring remains, and everything else becomes transparent.
  return svg.replace(
    /  <circle cx="256" cy="256" r="256" fill="#080604"\/>\n/,
    "",
  );
}

function makeBlackVariant(svg: string): string {
  // Swap the near-black dark fill for pure #000000. Classic icon look.
  return svg.replace(
    '<circle cx="256" cy="256" r="256" fill="#080604"/>',
    '<circle cx="256" cy="256" r="256" fill="#000000"/>',
  );
}

// ───────────────────────── Rendering ─────────────────────────

function renderPng(svg: string, size: number): Buffer {
  const resvg = new Resvg(svg, {
    fitTo: { mode: "width", value: size },
    background: "rgba(0, 0, 0, 0)",
  });
  return resvg.render().asPng();
}

// ───────────────────────── Preview page ─────────────────────────

function writePreview(outDir: string, sourceName: string) {
  // Pick a zoom factor for each small size so the zoomed width is ~192px.
  // This makes the pixel grid visible without taking up too much space.
  const zoomFactor = (s: number) => Math.max(2, Math.round(192 / s));

  // Large sizes: we want to see them at display-useful widths without making
  // the page absurdly long. Cap the displayed width at 256px for everything
  // 256 and up; smaller sizes render at their actual pixel dimensions.
  const displaySize = (s: number) => (s <= 256 ? s : 256);

  const nativeCell = (variant: string, s: number) => `
        <div class="size">
          <img src="shard-icon-${variant}-${s}.png" width="${displaySize(s)}" height="${displaySize(s)}">
          <div class="label">${s}px${s > 256 ? ` (shown @ ${displaySize(s)})` : ""}</div>
        </div>`;

  const zoomCell = (variant: string, s: number) => {
    const z = zoomFactor(s);
    return `
        <div class="size">
          <img class="pixelated" src="shard-icon-${variant}-${s}.png" width="${s * z}" height="${s * z}">
          <div class="label">${s}px @ ${z}x</div>
        </div>`;
  };

  const row = (variant: string, isAlpha: boolean, desc: string) => `
    <div class="row">
      <div class="row-header">
        <div class="name">${variant}</div>
        <div class="desc">${desc}</div>
      </div>

      <div class="section-label">Actual size — how the OS will render it</div>
      <div class="sizes ${isAlpha ? "checker" : "solid-black"}">
        ${SIZES.map((s) => nativeCell(variant, s)).join("")}
      </div>

      <div class="section-label">Zoomed — small sizes at scale, with pixel grid visible</div>
      <div class="sizes ${isAlpha ? "checker" : "solid-black"}">
        ${ZOOM_SIZES.map((s) => zoomCell(variant, s)).join("")}
      </div>
    </div>`;

  const html = `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Shard — Iconified Preview</title>
  <style>
    body {
      background: #0f0d0a;
      color: #e8ddd0;
      font-family: system-ui, -apple-system, sans-serif;
      margin: 0;
      padding: 40px;
    }
    h1 { color: #e8956a; margin: 0 0 4px; font-size: 22px; }
    .sub {
      color: #b0a08c;
      font-family: ui-monospace, "Cascadia Mono", monospace;
      font-size: 12px;
      margin-bottom: 32px;
    }
    .row { margin-bottom: 48px; }
    .row-header {
      display: flex;
      align-items: baseline;
      gap: 16px;
      margin-bottom: 16px;
    }
    .row-header .name {
      color: #f0a57e;
      font-family: ui-monospace, "Cascadia Mono", monospace;
      font-size: 20px;
    }
    .row-header .desc { color: #b0a08c; font-size: 13px; }
    .section-label {
      color: #8a7a68;
      font-family: ui-monospace, "Cascadia Mono", monospace;
      font-size: 11px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
      margin: 14px 0 8px;
    }
    .sizes {
      display: flex;
      gap: 24px;
      align-items: flex-end;
      padding: 20px;
      border-radius: 8px;
      border: 1px solid #302820;
      flex-wrap: wrap;
    }
    .size { text-align: center; }
    .size img { display: block; image-rendering: -webkit-optimize-contrast; }
    .size img.pixelated {
      image-rendering: pixelated;
      image-rendering: crisp-edges;
      border: 1px solid #302820;
    }
    .size .label {
      color: #8a7a68;
      font-family: ui-monospace, "Cascadia Mono", monospace;
      font-size: 11px;
      margin-top: 6px;
    }
    .solid-black { background: #000; }
    /* Checkerboard for alpha row so transparency is visible */
    .checker {
      background-image:
        linear-gradient(45deg, #1a1a1a 25%, transparent 25%),
        linear-gradient(-45deg, #1a1a1a 25%, transparent 25%),
        linear-gradient(45deg, transparent 75%, #1a1a1a 75%),
        linear-gradient(-45deg, transparent 75%, #1a1a1a 75%);
      background-size: 16px 16px;
      background-position: 0 0, 0 8px, 8px -8px, -8px 0;
      background-color: #2a2a2a;
    }
  </style>
</head>
<body>
  <h1>Shard — Iconified Preview</h1>
  <div class="sub">source: ${sourceName}</div>
  ${row("alpha", true, "dark backing removed — fragments float on transparent")}
  ${row("black", false, "dark backing replaced with pure #000000")}
</body>
</html>
`;
  writeFileSync(join(outDir, "preview.html"), html);
}

// ───────────────────────── Main ─────────────────────────

async function main() {
  const sourcePath = process.argv[2];
  if (!sourcePath) {
    console.error("usage: bun iconify.ts <path-to-source-svg>");
    process.exit(1);
  }

  const scriptDir = import.meta.dir ?? ".";
  const outDir = join(scriptDir, "final");
  mkdirSync(outDir, { recursive: true });

  const sourceSvg = readFileSync(sourcePath, "utf-8");

  const variants: Record<string, string> = {
    alpha: makeAlphaVariant(sourceSvg),
    black: makeBlackVariant(sourceSvg),
  };

  // Sanity check — make sure the replacements actually happened.
  if (variants.alpha === sourceSvg) {
    console.warn(
      "warning: alpha variant identical to source — the dark disc pattern didn't match. " +
      "The source SVG may have a different structure than expected.",
    );
  }
  if (variants.black === sourceSvg) {
    console.warn(
      "warning: black variant identical to source — the fill swap didn't match.",
    );
  }

  for (const [name, svg] of Object.entries(variants)) {
    const svgPath = join(outDir, `shard-icon-${name}.svg`);
    writeFileSync(svgPath, svg);
    console.log(`  ${basename(svgPath)}`);

    // Keep PNG buffers in memory so we can pack them into an ICO afterwards
    // without re-reading from disk.
    const pngBuffers: Record<number, Buffer> = {};

    for (const size of SIZES) {
      const pngPath = join(outDir, `shard-icon-${name}-${size}.png`);
      const png = renderPng(svg, size);
      writeFileSync(pngPath, png);
      pngBuffers[size] = png;
      console.log(`  ${basename(pngPath)}  (${png.length.toLocaleString()} bytes)`);
    }

    // Pack the standard ICO sizes into a single multi-image .ico file.
    const icoInput = ICO_SIZES.map((s) => pngBuffers[s]).filter(Boolean);
    const icoBuffer = await pngToIco(icoInput);
    const icoPath = join(outDir, `shard-icon-${name}.ico`);
    writeFileSync(icoPath, icoBuffer);
    console.log(`  ${basename(icoPath)}  (${icoBuffer.length.toLocaleString()} bytes, ${ICO_SIZES.length} sizes)`);
  }

  writePreview(outDir, basename(sourcePath));
  console.log(`\n  done → ${outDir}`);
  console.log(`  preview: ${join(outDir, "preview.html")}`);
}

main();
