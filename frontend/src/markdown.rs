use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, CowStr, Event, LinkType, Options, Parser, Tag, TagEnd, html};
use pulldown_latex::{
    Parser as LatexParser, RenderConfig, Storage, config::DisplayMode, mathml::push_mathml,
};

use crate::syntax_highlight::{highlight_to_html, syntax_for_lang_token};

/// Render agent/assistant markdown to HTML.
///
/// - GFM extensions: tables, strikethrough, task lists, footnotes.
/// - Raw HTML in the source is downgraded to escaped text to prevent XSS.
/// - **Link/Image URLs are scheme-filtered**: only `http`, `https`, `mailto`,
///   and relative/anchor targets survive. A link with a disallowed scheme
///   (`javascript:`, `data:`, …) is unwrapped to its plain text; a disallowed
///   image is dropped to its alt text. This runs in the shared render path so
///   every consumer (chat, code-intel hover) is protected — RA hover docs can
///   carry `[x](javascript:…)` or `data:`/remote image beacons.
/// - **Bare `http(s)://` URLs in plain text are autolinked** (pulldown-cmark
///   has no GFM autolink extension). This runs at the event level, so text
///   inside code blocks, inline code, existing links, and image alt text is
///   never rewritten. Trailing punctuation is excluded per the GFM autolink
///   rules (a closing `)`/`]`/`}` is kept only while balanced in the URL).
/// - Fenced code blocks are wrapped in `.md-code-block` with a language label
///   and a copy button; the inner `<code>` body is pre-tokenized by syntect
///   into colored `<span>`s emitted directly into the HTML, so no client-side
///   DOM mutation is needed.
/// - **LaTeX math is rendered to MathML**: `\(…\)` inline, `\[…\]` and `$$…$$`
///   as display. See [`rewrite_math_delimiters`] for which delimiters are
///   honored and why a lone `$` is not.
pub fn render_markdown(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    // Math delimiters are normalized against the plain-CommonMark reading of
    // the input, then the result is parsed again with the math extension on.
    let source = rewrite_math_delimiters(input, options);
    let mut math_options = options;
    math_options.insert(Options::ENABLE_MATH);

    let parser = Parser::new_ext(&source, math_options);

    let mut in_code: Option<(String, String)> = None;
    // Stacks tracking whether the enclosing link / image was suppressed (unsafe
    // URL), so the matching `End` is dropped too. CommonMark allows an image
    // inside a link, so these must be depth stacks, not single flags.
    let mut link_suppressed: Vec<bool> = Vec::new();
    let mut image_suppressed: Vec<bool> = Vec::new();
    let events = parser.filter_map(|ev| match ev {
        Event::Start(Tag::CodeBlock(kind)) => {
            let lang = match kind {
                CodeBlockKind::Fenced(l) => l.to_string(),
                CodeBlockKind::Indented => String::new(),
            };
            in_code = Some((lang, String::new()));
            None
        }
        Event::End(TagEnd::CodeBlock) => in_code.take().map(|(lang, code)| {
            Event::Html(CowStr::Boxed(
                build_code_block(&lang, &code).into_boxed_str(),
            ))
        }),
        Event::Text(t) if in_code.is_some() => {
            if let Some((_, code)) = in_code.as_mut() {
                code.push_str(&t);
            }
            None
        }
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            if is_safe_url(&dest_url) {
                link_suppressed.push(false);
                Some(Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }))
            } else {
                // Drop the <a> wrapper; the inner text still flows through as
                // plain text.
                link_suppressed.push(true);
                None
            }
        }
        Event::End(TagEnd::Link) => {
            if link_suppressed.pop().unwrap_or(false) {
                None
            } else {
                Some(Event::End(TagEnd::Link))
            }
        }
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            if is_safe_url(&dest_url) {
                image_suppressed.push(false);
                Some(Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                }))
            } else {
                // Drop the <img>; its alt text (inner events) renders as plain
                // text once the Image wrapper is gone.
                image_suppressed.push(true);
                None
            }
        }
        Event::End(TagEnd::Image) => {
            if image_suppressed.pop().unwrap_or(false) {
                None
            } else {
                Some(Event::End(TagEnd::Image))
            }
        }
        // MathML is laid out natively by every engine Tyde ships on (WKWebView
        // on desktop and mobile, Chromium under test), so a formula costs no
        // client-side pass and no math font — unlike a JS typesetter.
        Event::InlineMath(latex) => Some(Event::InlineHtml(CowStr::Boxed(
            render_math(&latex, DisplayMode::Inline).into_boxed_str(),
        ))),
        Event::DisplayMath(latex) => Some(Event::InlineHtml(CowStr::Boxed(
            render_math(&latex, DisplayMode::Block).into_boxed_str(),
        ))),
        Event::Html(s) => Some(Event::Text(s)),
        Event::InlineHtml(s) => Some(Event::Text(s)),
        other => Some(other),
    });

    // Autolink bare URLs. Code-block text was consumed above and inline code
    // is a separate `Event::Code`, so only genuine prose `Text` events reach
    // this stage; `wrap_depth` additionally skips text nested in a surviving
    // link (nested <a> is invalid) or image (it becomes the alt attribute).
    //
    // pulldown-cmark splits `Text` at emphasis-delimiter candidates (`_`,
    // `*`, `~`) even when they end up staying literal, so consecutive `Text`
    // events must be merged before scanning — otherwise a URL such as
    // `…/wiki/Foo_(bar)` arrives in pieces and only `…/Foo` gets linked.
    let mut linked: Vec<Event> = Vec::new();
    let mut text_buf = String::new();
    let mut wrap_depth = 0usize;
    fn flush<'a>(buf: &mut String, out: &mut Vec<Event<'a>>) {
        if !buf.is_empty() {
            let merged = CowStr::Boxed(std::mem::take(buf).into_boxed_str());
            out.extend(linkify_text(merged));
        }
    }
    for ev in events {
        match &ev {
            Event::Text(t) if wrap_depth == 0 => {
                text_buf.push_str(t);
                continue;
            }
            Event::Start(Tag::Link { .. }) | Event::Start(Tag::Image { .. }) => {
                flush(&mut text_buf, &mut linked);
                wrap_depth += 1;
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                wrap_depth = wrap_depth.saturating_sub(1);
            }
            _ => flush(&mut text_buf, &mut linked),
        }
        linked.push(ev);
    }
    flush(&mut text_buf, &mut linked);

    let mut out = String::with_capacity(input.len() * 2);
    html::push_html(&mut out, linked.into_iter());
    out
}

/// Rewrite LaTeX's `\(…\)` and `\[…\]` delimiters into the `$…$` / `$$…$$`
/// forms pulldown-cmark's math extension recognizes, and neutralize every
/// other `$` so it can never open a math span.
///
/// **Why a source rewrite and not an event filter.** CommonMark treats `\(` as
/// a backslash escape of ASCII punctuation, so the parser eats the delimiter
/// and hands back a bare `(` that is indistinguishable from one the author
/// typed. A model writing `\(h_y\)` reached the reader as `(h_y)`. By the time
/// events exist the information is gone, so the rewrite has to precede the
/// parse.
///
/// **Why a lone `$` is disarmed rather than honored.** This is a coding tool:
/// `$HOME … $PATH` and `it costs $5 … $10` are ordinary prose here, and inline
/// `$…$` math would silently swallow the span between them, names and all.
/// `\(…\)` covers inline math, and a doubled `$$` is rare enough by accident to
/// keep. Rewriting the lone `$` to `\$` renders the same literal character.
///
/// Code is exempt. The regions to skip come from a real parse rather than
/// hand-rolled fence tracking, so indented blocks, tilde fences, and nested
/// backtick runs are all classified by the same parser that reads the result.
fn rewrite_math_delimiters(input: &str, options: Options) -> String {
    if !input.contains('\\') && !input.contains('$') {
        return input.to_owned();
    }

    let mut out = String::with_capacity(input.len() + 16);
    let mut cursor = 0;
    for code in code_spans(input, options) {
        if code.start > cursor {
            rewrite_prose(&input[cursor..code.start], &mut out);
        }
        if code.end > cursor {
            out.push_str(&input[cursor.max(code.start)..code.end]);
            cursor = code.end;
        }
    }
    if cursor < input.len() {
        rewrite_prose(&input[cursor..], &mut out);
    }
    out
}

/// Byte ranges of the input that are code — inline spans and whole code blocks,
/// fence lines included. A code block's `Start` range already covers its body,
/// and code cannot nest, so the ranges come out disjoint and in document order.
fn code_spans(input: &str, options: Options) -> Vec<Range<usize>> {
    Parser::new_ext(input, options)
        .into_offset_iter()
        .filter_map(|(ev, range)| match ev {
            Event::Start(Tag::CodeBlock(_)) | Event::Code(_) => Some(range),
            _ => None,
        })
        .collect()
}

/// Normalize math delimiters in a run of the input already known to be prose.
fn rewrite_prose(chunk: &str, out: &mut String) {
    let mut chars = chunk.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('(' | ')') => out.push('$'),
                Some('[' | ']') => out.push_str("$$"),
                // A backslash escape consumes whatever follows it, so `\\(`
                // stays a literal backslash plus `(` instead of the second
                // backslash pairing with the paren to open math.
                Some(escaped) => {
                    out.push('\\');
                    out.push(escaped);
                }
                None => out.push('\\'),
            },
            '$' if chars.peek() == Some(&'$') => {
                out.push_str("$$");
                chars.next();
            }
            '$' => out.push_str("\\$"),
            c => out.push(c),
        }
    }
}

/// Render one LaTeX formula to MathML.
///
/// Invalid LaTeX is not an error path: pulldown-latex emits an inline
/// `<merror>` node carrying the offending source, so a typo shows up on screen
/// instead of blanking the formula.
fn render_math(latex: &str, display_mode: DisplayMode) -> String {
    let storage = Storage::new();
    let config = RenderConfig {
        display_mode,
        ..RenderConfig::default()
    };

    let mut mathml = String::new();
    match push_mathml(&mut mathml, LatexParser::new(latex, &storage), config) {
        Ok(()) => mathml,
        // Only reachable if writing into a String fails, which it cannot. A
        // partial write would be unbalanced markup, so show the source rather
        // than emit it — the formula stays readable either way.
        Err(_) => format!("<code>{}</code>", escape_html(latex)),
    }
}

/// Whether a link/image destination is safe to emit into `inner_html`. Allows
/// relative/anchor targets (no scheme) and the `http`, `https`, `mailto`
/// schemes; rejects everything else (`javascript:`, `data:`, `vbscript:`,
/// `file:`, …). Mirrors browser scheme parsing leniency: leading/embedded ASCII
/// whitespace and control characters are ignored when reading the scheme, and a
/// `/`, `?`, or `#` before any `:` means there is no scheme (relative).
fn is_safe_url(url: &str) -> bool {
    let mut scheme = String::new();
    for c in url.chars() {
        match c {
            ':' => return matches!(scheme.as_str(), "http" | "https" | "mailto"),
            '/' | '?' | '#' => return true, // path/query/anchor before any scheme
            c if c.is_ascii_whitespace() || c.is_control() => continue,
            c => scheme.push(c.to_ascii_lowercase()),
        }
    }
    // No ':' at all → relative reference.
    true
}

/// Split a prose text event into text/link/text… events, wrapping each bare
/// `http://`/`https://` URL in a real link so it renders clickable. Only these
/// two schemes are recognized, so every produced destination is safe by
/// construction under [`is_safe_url`].
fn linkify_text(text: CowStr<'_>) -> Vec<Event<'_>> {
    if !text.contains("http") {
        return vec![Event::Text(text)];
    }

    let s: &str = &text;
    let mut events: Vec<Event> = Vec::new();
    let mut cursor = 0; // start of not-yet-emitted plain text
    let mut search = 0;
    while let Some(rel) = s[search..].find("http") {
        let start = search + rel;
        let rest = &s[start..];
        let scheme_len = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            search = start + 4;
            continue;
        };
        // Must begin a word: an alphanumeric right before means this is the
        // tail of some other token, not a URL.
        let mid_word = s[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric());
        if mid_word {
            search = start + scheme_len;
            continue;
        }
        // The URL runs to the first whitespace/control char (or angle
        // bracket, which cannot appear raw in a URL), then sheds trailing
        // punctuation that is almost certainly sentence text.
        let end_rel = rest
            .find(|c: char| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
            .unwrap_or(rest.len());
        let url = trim_url_tail(&rest[..end_rel]);
        if url.len() <= scheme_len {
            // Just a scheme with nothing after it — leave as text.
            search = start + scheme_len;
            continue;
        }
        if cursor < start {
            events.push(owned_text(&s[cursor..start]));
        }
        events.push(Event::Start(Tag::Link {
            link_type: LinkType::Autolink,
            dest_url: CowStr::Boxed(url.to_owned().into_boxed_str()),
            title: CowStr::Borrowed(""),
            id: CowStr::Borrowed(""),
        }));
        events.push(owned_text(url));
        events.push(Event::End(TagEnd::Link));
        cursor = start + url.len();
        search = cursor;
    }

    if events.is_empty() {
        return vec![Event::Text(text)];
    }
    if cursor < s.len() {
        events.push(owned_text(&s[cursor..]));
    }
    events
}

fn owned_text(s: &str) -> Event<'static> {
    Event::Text(CowStr::Boxed(s.to_owned().into_boxed_str()))
}

/// Trim trailing sentence punctuation off a candidate bare URL, following the
/// GFM autolink extension: `.,:;!?'"*_~` never end a URL, and a closing
/// bracket is kept only while the URL contains at least as many opening ones
/// (so `…/Foo_(bar)` keeps its `)` but `(see https://x.com/y)` does not).
fn trim_url_tail(mut url: &str) -> &str {
    loop {
        let Some(last) = url.chars().next_back() else {
            return url;
        };
        match last {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '"' | '*' | '_' | '~' => {
                url = &url[..url.len() - last.len_utf8()];
            }
            ')' | ']' | '}' => {
                let open = match last {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if url.matches(last).count() > url.matches(open).count() {
                    url = &url[..url.len() - last.len_utf8()];
                } else {
                    return url;
                }
            }
            _ => return url,
        }
    }
}

fn build_code_block(lang: &str, code: &str) -> String {
    let code_trimmed = code.strip_suffix('\n').unwrap_or(code);
    // Fenced info strings can carry trailing modifiers (e.g. ```rust ignore`,
    // ```rust no_run`), so the actual language is just the first whitespace-
    // separated token. pulldown-cmark's own HTML renderer splits the same way.
    let lang_token = lang.split_whitespace().next().unwrap_or("");
    let lang_label = if lang_token.is_empty() {
        "text".to_owned()
    } else {
        escape_html(lang_token)
    };
    // Pre-render the code body. If syntect knows the language, emit colored
    // spans directly; otherwise just escape the plain text. Either way the
    // produced HTML is safe to inline — `escape_html` runs over every chunk
    // we don't control.
    let code_html = match syntax_for_lang_token(lang_token) {
        Some(syn) => highlight_to_html(code_trimmed, syn),
        None => escape_html(code_trimmed),
    };

    format!(
        "<div class=\"md-code-block\">\
         <div class=\"md-code-header\">\
         <span class=\"md-code-lang\">{lang_label}</span>\
         <button class=\"md-copy-code\" onclick=\"(() => {{ const c = this.closest('.md-code-block').querySelector('code'); navigator.clipboard.writeText(c.textContent).then(() => {{ this.textContent='\u{2713}'; setTimeout(() => this.textContent='Copy', 1200); }}); }})()\">Copy</button>\
         </div>\
         <pre><code class=\"md-code\">{code_html}</code></pre>\
         </div>"
    )
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    /// Render `input` into a real element so assertions read what a reader
    /// actually sees. `inner_html` is the production sink for this function, so
    /// parsing through it is what the browser does anyway — and it is the only
    /// way to observe MathML, which the HTML parser builds into real
    /// MathML-namespace nodes rather than plain markup.
    fn rendered(input: &str) -> HtmlElement {
        let document = web_sys::window()
            .expect("window")
            .document()
            .expect("document");
        let host = document.create_element("div").expect("create host");
        host.set_inner_html(&render_markdown(input));
        host.dyn_into::<HtmlElement>().expect("host is an element")
    }

    fn text_of(host: &HtmlElement) -> String {
        host.text_content().unwrap_or_default()
    }

    /// Models write math with LaTeX's `\(…\)` and `\[…\]` delimiters. Those are
    /// backslash escapes of ASCII punctuation under CommonMark, so the parser
    /// ate the delimiters and left the body as prose: the reader saw `(h_y)`,
    /// a literal `\Lambda`, and `[ L_y=(1-h_y)D(\Lambda) ]` instead of a
    /// formula.
    #[wasm_bindgen_test]
    fn latex_delimiters_render_as_math_not_mangled_prose() {
        let host = rendered(
            "Let \\(h_y\\) be the cache-hit rate and \\(\\Lambda\\) the miss rate:\n\n\
             \\[ L_y = (1 - h_y) D(\\Lambda) \\]\n",
        );
        let text = text_of(&host);

        assert!(
            !text.contains('\\'),
            "raw LaTeX leaked into the rendered text: {text:?}"
        );
        assert!(
            text.contains('Λ'),
            "\\Lambda did not become a symbol: {text:?}"
        );
        assert!(
            !text.contains("(h_y)") && !text.contains("[ L_y"),
            "delimiters were stripped to literal brackets: {text:?}"
        );

        let math = host.query_selector_all("math").expect("query math");
        assert_eq!(
            math.length(),
            3,
            "expected two inline formulas and one display formula, got {}: {text:?}",
            math.length()
        );

        // The `\[…\]` form is display math: it renders as its own centered
        // block, not inline with the sentence.
        let display = host
            .query_selector_all("math[display=\"block\"]")
            .expect("query display math");
        assert_eq!(
            display.length(),
            1,
            "`\\[…\\]` did not render as display math"
        );
    }

    /// `$$…$$` is the other delimiter models reach for, and it is unambiguous
    /// enough to support. Single `$` deliberately is not — see
    /// [`prose_dollars_are_never_math`].
    #[wasm_bindgen_test]
    fn double_dollar_renders_as_display_math() {
        let host = rendered("$$ E = mc^2 $$\n");

        let display = host
            .query_selector_all("math[display=\"block\"]")
            .expect("query display math");
        assert_eq!(
            display.length(),
            1,
            "`$$…$$` did not render as display math"
        );
        assert!(
            !text_of(&host).contains('$'),
            "delimiters survived into the text: {:?}",
            text_of(&host)
        );
    }

    /// A single `$` is money or a shell variable far more often than it is
    /// math, and this is a coding tool. Enabling inline `$…$` would turn
    /// `$HOME … $PATH` into a formula and eat both names.
    #[wasm_bindgen_test]
    fn prose_dollars_are_never_math() {
        let host = rendered("Set $HOME before $PATH, and it costs $5 not $10.\n");
        let text = text_of(&host);

        assert_eq!(
            text.trim(),
            "Set $HOME before $PATH, and it costs $5 not $10.",
            "prose dollars were rewritten"
        );
        assert_eq!(
            host.query_selector_all("math")
                .expect("query math")
                .length(),
            0,
            "prose dollars produced a formula"
        );
    }

    /// Math delimiters are prose syntax. Inside a fence or an inline code span
    /// the same bytes are the code the user asked to see, verbatim.
    #[wasm_bindgen_test]
    fn code_keeps_its_backslashes_and_dollars() {
        let host = rendered(
            "Inline `\\(not math\\)` and `$PATH`:\n\n\
             ```sh\n\
             echo \"\\[literal\\]\" $HOME $$\n\
             ```\n",
        );
        let text = text_of(&host);

        assert!(
            text.contains("\\(not math\\)") && text.contains("$PATH"),
            "inline code was rewritten: {text:?}"
        );
        assert!(
            text.contains("echo \"\\[literal\\]\" $HOME $$"),
            "fenced code was rewritten: {text:?}"
        );
        assert_eq!(
            host.query_selector_all("math")
                .expect("query math")
                .length(),
            0,
            "code produced a formula"
        );
    }

    /// Invalid LaTeX must stay visible. Dropping it would lose content the
    /// model meant to communicate, with nothing on screen to say so.
    #[wasm_bindgen_test]
    fn invalid_latex_stays_visible() {
        let host = rendered("Broken: \\(\\frobnicate{x}\\)\n");
        let text = text_of(&host);

        assert!(
            text.contains("frobnicate"),
            "invalid LaTeX rendered as nothing: {text:?}"
        );
    }
}
