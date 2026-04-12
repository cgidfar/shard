# shard icon-gen

Procedural generator for the Shard app icon. Two scripts:

- **`gen-icon.ts`** — produces SVG variants from a Voronoi tessellation. Each
  run scatters seed points in concentric rings, computes the Voronoi cells,
  shrinks them by a distance-weighted amount to create the leading between
  fragments, and colors each cell by its distance from the center.
- **`iconify.ts`** — takes one chosen SVG and emits the final assets:
  modified SVG variants (alpha and black background), PNGs at every common
  icon size, a multi-size Windows `.ico`, and a `preview.html` showing how
  each variant looks at every size including a pixelated zoom on the small
  ones for legibility checks.

## Setup

```powershell
cd tools\icon-gen
bun install
```

## Generating variants

`gen-icon.ts` writes a timestamped folder under `runs/` so consecutive runs
never overwrite each other. The folder name encodes every parameter so a
batch is reproducible from its name alone.

```powershell
bun gen-icon.ts                                  # 20 variants, defaults
bun gen-icon.ts --count 30                       # 30 variants
bun gen-icon.ts --palette ember                  # different palette
bun gen-icon.ts --rings 4 --base 5 --gap-max 14  # tune the look
bun gen-icon.ts --seed 42 --count 1              # re-render a specific seed
ii runs\<latest>\index.html                      # open the gallery
```

The gallery shows each variant at full size with a 32px thumbnail pinned to
the corner so you can spot legibility-friendly designs at a glance.

### Knobs

| Flag | Default | What it does |
|---|---|---|
| `--seed N` | 1 | Starting seed (RNG, reproducible) |
| `--count N` | 20 | How many variants to generate |
| `--out DIR` | `runs/<timestamp>_<params>` | Override output directory |
| `--palette NAME` | `sunset` | `sunset \| ember \| desert \| volcano \| twilight \| dusk \| amethyst \| cinder` |
| `--rings N` | 5 | Concentric rings of seed points |
| `--base N` | 6 | Points in the innermost ring |
| `--density X` | 1.55 | Each ring has `density^i` times more points |
| `--gap-min PX` | 1.5 | Minimum leading between fragments (near center) |
| `--gap-max PX` | 9 | Maximum leading between fragments (near edge) |
| `--gap-curve NAME` | `quad` | `linear \| quad \| cube \| exp \| sqrt \| smooth` — how gaps grow |
| `--radius-jitter PX` | 14 | Random offset on ring radii |
| `--angle-jitter F` | 0.55 | 0..1, jitter as a fraction of slot width |
| `--drop-rings N` | 0 | Drop the N outermost rings + the smooth disc, leaving a jagged edge |

Each run writes a `params.json` next to the SVGs containing the full config
plus the exact CLI command, so any run can be reproduced or tweaked later.

## Producing the final assets

Once you've found a seed you like, feed it to `iconify.ts`:

```powershell
bun iconify.ts runs\<folder>\icon-0042.svg
ii final\preview.html
```

Output lands in `final/`:

```
shard-icon-alpha.svg          shard-icon-black.svg
shard-icon-alpha-1024.png     shard-icon-black-1024.png
shard-icon-alpha-512.png      shard-icon-black-512.png
shard-icon-alpha-256.png      shard-icon-black-256.png
shard-icon-alpha-128.png      shard-icon-black-128.png
shard-icon-alpha-64.png       shard-icon-black-64.png
shard-icon-alpha-48.png       shard-icon-black-48.png
shard-icon-alpha-32.png       shard-icon-black-32.png
shard-icon-alpha-24.png       shard-icon-black-24.png
shard-icon-alpha-16.png       shard-icon-black-16.png
shard-icon-alpha.ico          shard-icon-black.ico         # 6-size multi-image .ico
preview.html                  # both variants at every size + pixelated zoom
```

The two variants are:

- **alpha** — backing disc removed; gaps between fragments are transparent.
  Reads as floating shards on whatever the icon sits on.
- **black** — backing replaced with pure `#000000`. Classic solid app-icon
  look. This is the variant currently shipped.

## Installing into the app

After picking a variant, copy the assets into the app crate(s) by hand:

```powershell
# Tauri app icon (window, taskbar, Alt-Tab, bundled .exe resource)
cp final\shard-icon-black.ico       ..\..\crates\shard-app\icons\icon.ico
cp final\shard-icon-black-1024.png  ..\..\crates\shard-app\icons\icon.png
cp final\shard-icon-black.svg       ..\..\crates\shard-app\icons\icon.svg

# Daemon tray icon (embedded into shardctl.exe via include_bytes!)
cp final\shard-icon-black-64.png    ..\..\crates\shard-cli\assets\tray-icon.png
```

Then rebuild:

```powershell
cargo build -p shard-app    # picks up icon.ico via tauri_build
cargo build -p shard-cli    # embeds tray-icon.png via include_bytes!
```

## Adding a new palette

Palettes are defined at the top of `gen-icon.ts` as arrays of `[position, [r, g, b]]`
stops. Position is 0..1 where 0 is the brightest center cell and 1 is the
outermost edge cell. Add an entry to the `palettes` object and append the
name to the `--palette` line in `printHelp()`.
