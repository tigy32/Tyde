use std::sync::RwLock;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use protocol::{ProjectGitDiffHunk, ProjectGitDiffLineKind};

/// Maximum number of lines a single highlight call will accept. Above this we
/// fall back to plain text. Wasm is single-threaded, so very large diffs would
/// freeze the UI.
const MAX_LINES_TO_HIGHLIGHT: usize = 5000;

/// Default theme name when no preference is stored. Catppuccin Mocha is the
/// most popular dark theme in 2025 surveys; ships in `two-face`.
pub const DEFAULT_THEME_NAME: &str = "Catppuccin Mocha";

static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);

/// All bundled themes. Built from `two-face`'s extra theme set (32 themes
/// including Catppuccin's 4 flavors, Dracula, GitHub, Monokai variants,
/// Nord, Gruvbox, OneHalf, Solarized, Sublime Snazzy, TwoDark, Zenburn),
/// plus the syntect defaults so InspiredGitHub / base16-ocean.* are still
/// available. Loaded once at startup.
static THEME_SET: Lazy<ThemeSet> = Lazy::new(|| {
    let mut set = ThemeSet::load_defaults();
    let extra = two_face::theme::extra();
    for name in two_face::theme::EmbeddedLazyThemeSet::theme_names() {
        let theme = extra.get(*name);
        set.themes.insert(name.as_name().to_owned(), theme.clone());
    }
    // Hide ANSI/Base16-256 — they target terminal palettes, not real
    // syntax-highlighting colors.
    set.themes.remove("ansi");
    set.themes.remove("base16");
    set.themes.remove("base16-256");
    // Drop "1337" (`Leet`); it's a meme theme and the contrast is bad.
    set.themes.remove("1337");
    set
});

/// Currently-selected theme name (matches a key in `THEME_SET.themes`).
/// Empty string falls back to `DEFAULT_THEME_NAME`. Writes are infrequent
/// (only when the user picks a theme from settings); reads are on every
/// highlight call.
static SELECTED_THEME_NAME: RwLock<String> = RwLock::new(String::new());

/// List the bundled theme names available for selection, sorted.
pub fn available_themes() -> Vec<String> {
    let mut names: Vec<String> = THEME_SET.themes.keys().cloned().collect();
    names.sort();
    names
}

/// Update the active theme. Future `LineHighlighter::new` calls pick up the
/// new theme; already-running highlight tasks continue with their previously
/// bound theme until they finish. Returns false (and is a no-op) if `name`
/// isn't a bundled theme.
pub fn set_selected_theme(name: &str) -> bool {
    if !THEME_SET.themes.contains_key(name) {
        return false;
    }
    if let Ok(mut slot) = SELECTED_THEME_NAME.write() {
        *slot = name.to_owned();
    }
    true
}

fn current_theme() -> &'static Theme {
    // THEME_SET is held in a `'static Lazy<ThemeSet>`, so any `&Theme`
    // returned by indexing into its `themes` map has effective `'static`
    // lifetime — Rust just can't infer it through `Lazy`'s deref. The
    // closure-returning-reference pattern below makes the lifetime
    // implicit at the call site without unsafe.
    let theme_set: &'static ThemeSet = &THEME_SET;
    let name_owned = SELECTED_THEME_NAME
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    let lookup = if name_owned.is_empty() {
        DEFAULT_THEME_NAME
    } else {
        name_owned.as_str()
    };
    theme_set
        .themes
        .get(lookup)
        .or_else(|| theme_set.themes.get(DEFAULT_THEME_NAME))
        .expect("default theme present in ThemeSet")
}

/// One styled run inside a single line. `fg` carries syntect's RGBA
/// foreground; we (de)serialize it as a flat `{r,g,b,a}` so the type
/// can cross a Web Worker `postMessage` boundary without depending on
/// syntect's optional serde feature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    #[serde(with = "color_serde")]
    pub fg: Color,
    pub text: String,
}

mod color_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use syntect::highlighting::Color;

    #[derive(Serialize, Deserialize)]
    struct Wire {
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    }

    pub fn serialize<S: Serializer>(c: &Color, s: S) -> Result<S::Ok, S::Error> {
        Wire {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Color, D::Error> {
        let w = Wire::deserialize(d)?;
        Ok(Color {
            r: w.r,
            g: w.g,
            b: w.b,
            a: w.a,
        })
    }
}

pub type LineTokens = Vec<Token>;

/// Resolve a syntax for the given path. Tries (in order) the file's full
/// basename (catches `Makefile`, `Dockerfile`, etc.), then the extension, then
/// the syntect first-line heuristic by name. Returns `None` for unknown
/// languages — callers should fall back to plain text.
pub fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let ss: &'static SyntaxSet = &SYNTAX_SET;
    let p = std::path::Path::new(path);
    if let Some(name) = p.file_name().and_then(|n| n.to_str())
        && let Some(s) = ss.find_syntax_by_token(name)
    {
        return Some(s);
    }
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        if let Some(s) = ss.find_syntax_by_extension(ext) {
            return Some(s);
        }
        if let Some(s) = ss.find_syntax_by_token(ext) {
            return Some(s);
        }
    }
    None
}

/// Highlight a sequence of plain text lines (without trailing newlines).
/// Returns one `LineTokens` per input line, or `None` if the input is too
/// large.
fn highlight_lines(lines: &[&str], syntax: &SyntaxReference) -> Option<Vec<LineTokens>> {
    if lines.len() > MAX_LINES_TO_HIGHLIGHT {
        return None;
    }
    let ss: &'static SyntaxSet = &SYNTAX_SET;
    let mut h = HighlightLines::new(syntax, current_theme());
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let with_nl = format!("{line}\n");
        let ranges = match h.highlight_line(&with_nl, ss) {
            Ok(r) => r,
            Err(_) => return None,
        };
        let mut tokens: LineTokens = Vec::with_capacity(ranges.len());
        for (style, text) in ranges {
            let trimmed = text.strip_suffix('\n').unwrap_or(text);
            if trimmed.is_empty() {
                continue;
            }
            tokens.push(Token {
                fg: style.foreground,
                text: trimmed.to_string(),
            });
        }
        out.push(tokens);
    }
    Some(out)
}

/// Stateful per-line highlighter that maintains syntect parser state across
/// calls. Use when you need to chunk highlighting work (e.g. yielding to the
/// browser between chunks of a large file) — successive `highlight_one` calls
/// give the same result as feeding the lines into `highlight_lines` in one go.
pub struct LineHighlighter {
    inner: HighlightLines<'static>,
}

impl LineHighlighter {
    pub fn new(syntax: &'static SyntaxReference) -> Self {
        Self {
            inner: HighlightLines::new(syntax, current_theme()),
        }
    }

    pub fn highlight_one(&mut self, line: &str) -> LineTokens {
        let with_nl = format!("{line}\n");
        let ranges = match self.inner.highlight_line(&with_nl, &SYNTAX_SET) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut tokens: LineTokens = Vec::with_capacity(ranges.len());
        for (style, text) in ranges {
            let trimmed = text.strip_suffix('\n').unwrap_or(text);
            if trimmed.is_empty() {
                continue;
            }
            tokens.push(Token {
                fg: style.foreground,
                text: trimmed.to_string(),
            });
        }
        tokens
    }
}

/// Force-load the bundled syntax set and theme so the next call doesn't have
/// to deserialize ~341 KB of grammars on the wasm main thread. Cheap to call
/// repeatedly; only the first call does real work.
///
/// Intended to be invoked once after the app's first paint (via a
/// `setTimeout(0)`-style yield in `app::App`) so the cost lands during idle
/// time rather than on the first file open or first markdown render.
pub fn warm_up() {
    Lazy::force(&SYNTAX_SET);
    // Force lazy syntect/onig regex compilation. Without this the first
    // real highlight call pays the regex-compile cost (~300ms in debug
    // for a Rust file) on top of its tokenize work, which the user
    // perceives as a slow first file-open. A throwaway one-liner is
    // enough to populate onig's compiled-regex cache.
    if let Some(rust) = SYNTAX_SET.find_syntax_by_extension("rs") {
        let mut hl = LineHighlighter::new(rust);
        let _ = hl.highlight_one("fn main() {}");
    }
    Lazy::force(&THEME_SET);
}

/// For a single hunk, compute per-diff-line `LineTokens` for unified
/// rendering. Each line gets tokens from its own side (Removed → old stream,
/// Added → new stream); context lines use the new-side state, since that's
/// the post-edit version readers usually want.
///
/// In `Hunks` context mode each hunk is highlighted in isolation, so
/// multi-line constructs (block comments, multi-line strings) that cross hunk
/// boundaries can mis-color. `FullFile` is the user-visible escape hatch —
/// there's one giant hunk in that mode, so highlighting is exact.
pub fn compute_hunk_tokens(
    hunk: &ProjectGitDiffHunk,
    syntax: &SyntaxReference,
) -> Vec<Option<LineTokens>> {
    let (old_per_line, new_per_line) = compute_hunk_tokens_dual(hunk, syntax);
    hunk.lines
        .iter()
        .enumerate()
        .map(|(i, line)| match line.kind {
            ProjectGitDiffLineKind::Removed => old_per_line[i].clone(),
            ProjectGitDiffLineKind::Added | ProjectGitDiffLineKind::Context => {
                new_per_line[i].clone()
            }
        })
        .collect()
}

/// Like [`compute_hunk_tokens`] but returns both old-side and new-side tokens
/// per hunk line. Side-by-side rendering needs both: the left pane uses
/// old-side tokens for context+removed; the right pane uses new-side tokens
/// for context+added. Context lines may differ between sides because parser
/// state diverges around edits.
///
/// Result tuple: `(old_per_line, new_per_line)`, each indexed by hunk line.
/// Old entries are `Some` only for `Context` and `Removed` lines; new entries
/// are `Some` only for `Context` and `Added` lines.
pub fn compute_hunk_tokens_dual(
    hunk: &ProjectGitDiffHunk,
    syntax: &SyntaxReference,
) -> (Vec<Option<LineTokens>>, Vec<Option<LineTokens>>) {
    let mut old_lines: Vec<&str> = Vec::new();
    let mut new_lines: Vec<&str> = Vec::new();
    let mut idx_for_line: Vec<(Option<usize>, Option<usize>)> =
        Vec::with_capacity(hunk.lines.len());

    for line in &hunk.lines {
        match line.kind {
            ProjectGitDiffLineKind::Context => {
                let oi = old_lines.len();
                let ni = new_lines.len();
                old_lines.push(&line.text);
                new_lines.push(&line.text);
                idx_for_line.push((Some(oi), Some(ni)));
            }
            ProjectGitDiffLineKind::Removed => {
                let oi = old_lines.len();
                old_lines.push(&line.text);
                idx_for_line.push((Some(oi), None));
            }
            ProjectGitDiffLineKind::Added => {
                let ni = new_lines.len();
                new_lines.push(&line.text);
                idx_for_line.push((None, Some(ni)));
            }
        }
    }

    let old_hl = highlight_lines(&old_lines, syntax);
    let new_hl = highlight_lines(&new_lines, syntax);

    let old_per_line = idx_for_line
        .iter()
        .map(|(oi, _)| oi.and_then(|i| old_hl.as_ref()?.get(i).cloned()))
        .collect();
    let new_per_line = idx_for_line
        .iter()
        .map(|(_, ni)| ni.and_then(|i| new_hl.as_ref()?.get(i).cloned()))
        .collect();
    (old_per_line, new_per_line)
}

pub fn color_to_css(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// Resolve a syntax by markdown code-fence token (e.g. `"rust"`, `"ts"`,
/// `"python"`). Tries syntect's name and extension lookups in order.
pub fn syntax_for_lang_token(token: &str) -> Option<&'static SyntaxReference> {
    let ss: &'static SyntaxSet = &SYNTAX_SET;
    if token.is_empty() {
        return None;
    }
    if let Some(s) = ss.find_syntax_by_token(token) {
        return Some(s);
    }
    ss.find_syntax_by_extension(token)
}

/// Highlight `text` with the given syntax and emit HTML containing one
/// `<span style="color:#…">…</span>` per token. Used by markdown rendering
/// where the result is concatenated into a server-emitted HTML string and
/// injected via `inner_html`.
///
/// Returns escaped plain text (no span wrapping) when the input is over the
/// highlight cap; callers can still inject the result safely as inner HTML.
pub fn highlight_to_html(text: &str, syntax: &SyntaxReference) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let highlighted = match highlight_lines(&lines, syntax) {
        Some(v) => v,
        None => return escape_html(text),
    };
    let mut out = String::with_capacity(text.len() * 4);
    for (i, line_tokens) in highlighted.iter().enumerate() {
        for tok in line_tokens {
            let style = format!("color:#{:02x}{:02x}{:02x}", tok.fg.r, tok.fg.g, tok.fg.b);
            out.push_str("<span style=\"");
            out.push_str(&style);
            out.push_str("\">");
            out.push_str(&escape_html(&tok.text));
            out.push_str("</span>");
        }
        if i + 1 < highlighted.len() {
            out.push('\n');
        }
    }
    out
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}
