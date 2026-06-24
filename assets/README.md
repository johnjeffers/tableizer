# Icons

`icon.svg` is the **source of truth** — an editable vector usable on any OS. The rasters are derived
from it and committed so the build needs no image tooling:

| File        | Use                                                              |
| ----------- | --------------------------------------------------------------- |
| `icon.svg`  | Vector source (edit this).                                      |
| `icon.png`  | 1024×1024 master + the app's runtime window icon (`src/main.rs`).|
| `icon.icns` | macOS app bundle icon (`scripts/package-macos.sh`).             |
| `icon.ico`  | Windows icon (for future Windows packaging).                    |

## Regenerating the rasters from `icon.svg`

After editing the SVG, re-render with any SVG rasterizer + the macOS icon tools, e.g.:

```sh
# 1024 master (rsvg-convert, resvg, or Inkscape all work)
rsvg-convert -w 1024 -h 1024 icon.svg -o icon.png

# .icns: render each iconset size, then iconutil
mkdir -p Tableizer.iconset
for s in 16 32 128 256 512; do
  rsvg-convert -w $s        -h $s        icon.svg -o Tableizer.iconset/icon_${s}x${s}.png
  rsvg-convert -w $((s*2))  -h $((s*2))  icon.svg -o Tableizer.iconset/icon_${s}x${s}@2x.png
done
iconutil -c icns Tableizer.iconset -o icon.icns && rm -r Tableizer.iconset

# .ico (ImageMagick): a few sizes packed into one
magick icon.svg -define icon:auto-resize=16,32,48,64,128,256 icon.ico
```
