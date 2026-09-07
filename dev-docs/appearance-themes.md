# Appearance themes

Settings → Appearance → Appearance theme selects interface colors on this
device. Changes apply immediately and persist in `tyde-theme` localStorage.
Existing `dark` and `light` preferences remain valid; missing or unrecognized
values fall back to Tyde Dark. This is a frontend preference, not a host setting.

Bundled choices: Tyde Dark, Tyde Light, GitHub Light, GitHub Dark,
Catppuccin Latte, and Catppuccin Mocha. These are Tyde adaptations of the
palettes, not complete VS Code theme imports. They do not alter layout,
spacing, typefaces, font sizes, icons, or content. The existing color setting
uses the standard settings dropdown to accommodate the larger list.

Syntax highlighting remains a separate choice in Code & Output Display;
terminal ANSI colors retain the terminal's existing palette.

## Add a theme

1. Add a stable ID, display label, and `light` or `dark` color scheme to
   `frontend/src/appearance.rs`.
2. Add a matching `[data-appearance-theme="your-id"]` color-token block beside
   the bundled palettes in `frontend/styles.css`. Supply the same tokens as the
   other named palettes, including `--text-on-accent` for readable text on
   accent-filled buttons. Derived hover, selection, and compatibility colors
   follow those tokens automatically. Do not add geometry or typography rules.
3. Add source attribution and applicable license notices. Extend the real-DOM
   appearance flow in `settings_panel.rs` to cover the new palette's visible
   colors, persistence, and unchanged geometry. Run `./dev.sh check` through
   the normal workbench and landing workflow.

`data-theme` retains the light/dark color scheme for native controls and
existing selectors. `data-appearance-theme` identifies the named palette.
Keeping those separate lets additional palettes reuse the existing component
styles without introducing per-theme markup or behavior.

## Palette sources

- GitHub: https://github.com/primer/github-vscode-theme, particularly
  `src/colors.js` and `src/theme.js`, using the GitHub/Primer neutral and blue
  palette. The adaptations keep Tyde's blue primary action.
- Catppuccin palette 1.8.0: https://github.com/catppuccin/palette and
  https://catppuccin.com/palette/. Latte success and warning foregrounds and
  its blue hover are darkened for small interface text and controls.

MIT notices are preserved in `frontend/appearance-theme-licenses.txt`.
