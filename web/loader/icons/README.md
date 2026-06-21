# Loader icons

`icon.svg` is the source-of-truth scalable icon (referenced by both the web
manifest and the favicon link). It works as-is in Chrome/Android PWAs.

**Still needed (raster PNGs) before shipping:** iOS Home-Screen and some
launchers require PNG icons at fixed sizes. Generate them from `icon.svg`:

```sh
# requires a rasterizer such as `rsvg-convert` (librsvg) or ImageMagick
rsvg-convert -w 180 -h 180 icon.svg -o icon-180.png   # apple-touch-icon
rsvg-convert -w 192 -h 192 icon.svg -o icon-192.png
rsvg-convert -w 512 -h 512 icon.svg -o icon-512.png
# maskable variant: re-export with ~10% safe-zone padding around the glyph
rsvg-convert -w 512 -h 512 icon.svg -o icon-512-maskable.png
```

These filenames match the `icons` entries in `../manifest.webmanifest` and the
`apple-touch-icon` link in `../index.html`. The Phase 6 deploy step should
produce and publish them alongside the loader.
