pub struct AppearanceTheme {
    pub id: &'static str,
    pub label: &'static str,
    pub color_scheme: &'static str,
}

pub const DEFAULT_THEME_ID: &str = "dark";

pub const THEMES: &[AppearanceTheme] = &[
    AppearanceTheme {
        id: "dark",
        label: "Tyde Dark",
        color_scheme: "dark",
    },
    AppearanceTheme {
        id: "light",
        label: "Tyde Light",
        color_scheme: "light",
    },
    AppearanceTheme {
        id: "github-light",
        label: "GitHub Light",
        color_scheme: "light",
    },
    AppearanceTheme {
        id: "github-dark",
        label: "GitHub Dark",
        color_scheme: "dark",
    },
    AppearanceTheme {
        id: "catppuccin-latte",
        label: "Catppuccin Latte",
        color_scheme: "light",
    },
    AppearanceTheme {
        id: "catppuccin-mocha",
        label: "Catppuccin Mocha",
        color_scheme: "dark",
    },
];

pub fn resolve(id: &str) -> &'static AppearanceTheme {
    THEMES
        .iter()
        .find(|theme| theme.id == id)
        .unwrap_or(&THEMES[0])
}
