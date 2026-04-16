/**
 * One-off: take a source icon SVG and emit icon.svg / icon.png / icon.ico
 * into crates/shard-app/icons/.
 *
 * Usage: bun apply-icon.ts <path-to-source-svg>
 */

import { Resvg } from "@resvg/resvg-js";
import pngToIco from "png-to-ico";
import { readFileSync, writeFileSync, copyFileSync } from "node:fs";
import { join, resolve } from "node:path";

const ICO_SIZES = [256, 128, 64, 48, 32, 16];
const PNG_SIZE = 1024;
const TRAY_SIZE = 64;

async function main() {
  const source = process.argv[2];
  if (!source) {
    console.error("usage: bun apply-icon.ts <path-to-source-svg>");
    process.exit(1);
  }

  const sourcePath = resolve(source);
  const svg = readFileSync(sourcePath, "utf-8");

  const outDir = resolve(
    import.meta.dir ?? ".",
    "../../crates/shard-app/icons",
  );

  // 1. icon.svg — copy verbatim.
  copyFileSync(sourcePath, join(outDir, "icon.svg"));
  console.log(`  icon.svg  ← ${sourcePath}`);

  // 2. icon.png — single-size 1024 px.
  const resvg = new Resvg(svg, {
    fitTo: { mode: "width", value: PNG_SIZE },
    background: "rgba(0, 0, 0, 0)",
  });
  const pngMain = resvg.render().asPng();
  writeFileSync(join(outDir, "icon.png"), pngMain);
  console.log(`  icon.png  (${PNG_SIZE}px, ${pngMain.length.toLocaleString()} bytes)`);

  // 3. icon.ico — multi-size Windows icon.
  const icoPngs: Buffer[] = [];
  for (const size of ICO_SIZES) {
    const r = new Resvg(svg, {
      fitTo: { mode: "width", value: size },
      background: "rgba(0, 0, 0, 0)",
    });
    icoPngs.push(r.render().asPng());
  }
  const ico = await pngToIco(icoPngs);
  writeFileSync(join(outDir, "icon.ico"), ico);
  console.log(`  icon.ico  (${ICO_SIZES.join(", ")}px, ${ico.length.toLocaleString()} bytes)`);

  // 4. tray-icon.png — 64×64 RGBA8, embedded in shard-cli/assets.
  const trayOut = resolve(
    import.meta.dir ?? ".",
    "../../crates/shard-cli/assets/tray-icon.png",
  );
  const trayResvg = new Resvg(svg, {
    fitTo: { mode: "width", value: TRAY_SIZE },
    background: "rgba(0, 0, 0, 0)",
  });
  const trayPng = trayResvg.render().asPng();
  writeFileSync(trayOut, trayPng);
  console.log(`  tray-icon.png  (${TRAY_SIZE}px, ${trayPng.length.toLocaleString()} bytes)`);
}

main();
