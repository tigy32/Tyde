# Test fonts

Subsets used only by the Git panel wasm tests, so their font-metric checks
measure the same faces on every host. They are not shipped in the app.

| File | Source | Version | License |
|---|---|---|---|
| dejavu-sans-mono.ttf | DejaVuSansMono.ttf (fonts-dejavu-mono) | 2.37 | DejaVu-LICENSE.txt |
| noto-sans-mono.ttf | NotoSansMono-Regular.ttf (fonts-noto-mono) | 2.006 | OFL.txt |
| noto-sans-cjk-jp.otf | NotoSansCJK-Regular.ttc, face 0 (fonts-noto-cjk) | 2.004 | OFL.txt |
| noto-sans-arabic.ttf | NotoSansArabic-Regular.ttf (fonts-noto-core) | 2.005 | OFL.txt |
| noto-sans-devanagari.ttf | NotoSansDevanagari-Regular.ttf (fonts-noto-core) | 2.001 | OFL.txt |
| noto-sans-thai.ttf | NotoSansThai-Regular.ttf (fonts-noto-core) | 2.000 | OFL.txt |
| noto-color-emoji.ttf | NotoColorEmoji.ttf (fonts-noto-color-emoji) | 2.051 | OFL.txt |

Each file was made with fontTools 4.55.3:

```
python -m fontTools.subset SOURCE --text=CHARS --layout-features='*' \
  --name-IDs='*' --name-languages='*' --output-file=FILE [--font-number=0]
```

- The two monospace faces keep printable ASCII, U+2026 and every character of
  the test paths. DejaVu covers Arabic itself, so dropping the test text would
  change which face draws it.
- Each script face keeps only its own characters from the test paths.

When a test path gains a character, regenerate these files. The test fails
while a bundled stack has no glyph for a character a row shows.
