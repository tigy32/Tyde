# Loader icons

`icon.svg` is the source-of-truth scalable icon (referenced by both the web
manifest and the favicon link).

The raster PNGs referenced by `../manifest.webmanifest` and the
`apple-touch-icon` link in `../index.html` are committed and rendered from
`icon.svg`:

- `icon-180.png` (180×180) — iOS `apple-touch-icon`
- `icon-192.png` (192×192) — web manifest
- `icon-512.png` (512×512) — web manifest
- `icon-512-maskable.png` (512×512) — maskable

To regenerate after editing `icon.svg` (any rasterizer works):

```sh
rsvg-convert -w 512 -h 512 icon.svg -o icon-512.png
rsvg-convert -w 192 -h 192 icon.svg -o icon-192.png
rsvg-convert -w 180 -h 180 icon.svg -o icon-180.png
# or, on macOS without librsvg, render once via Quick Look then resize with sips:
#   qlmanage -t -s 512 -o . icon.svg && mv icon.svg.png icon-512.png
#   sips -z 192 192 icon-512.png --out icon-192.png
```

Polish TODO: `icon-512-maskable.png` is currently the same render as
`icon-512.png`; re-export it with ~10% safe-zone padding around the glyph so
the maskable crop on Android doesn't clip the mark.
