use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};

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
/// - Fenced code blocks are wrapped in `.md-code-block` with a language label
///   and a copy button; the inner `<code>` body is pre-tokenized by syntect
///   into colored `<span>`s emitted directly into the HTML, so no client-side
///   DOM mutation is needed.
pub fn render_markdown(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(input, options);

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
        Event::Html(s) => Some(Event::Text(s)),
        Event::InlineHtml(s) => Some(Event::Text(s)),
        other => Some(other),
    });

    let mut out = String::with_capacity(input.len() * 2);
    html::push_html(&mut out, events);
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_info_string_with_modifier_still_highlights_as_rust() {
        // ```rust ignore``` — pulldown-cmark passes the whole "rust ignore"
        // string. We must split on whitespace and take the first token,
        // otherwise common docs/test fences fall back to plain text.
        let html = render_markdown("```rust ignore\nfn main() {}\n```\n");
        assert!(
            html.contains("<span style=\"color:"),
            "expected syntect-emitted span; got: {html}"
        );
        // The visible language label should be just "rust", not "rust ignore".
        assert!(
            html.contains(">rust</span>"),
            "expected language label to be just 'rust'; got: {html}"
        );
    }

    #[test]
    fn unknown_language_falls_back_to_escaped_text() {
        let html = render_markdown("```nosuchlang\n<x> & y\n```\n");
        // No styled spans emitted.
        assert!(!html.contains("color:#"), "did not expect colored spans");
        // HTML special chars escaped inside the code body.
        assert!(html.contains("&lt;x&gt; &amp; y"));
    }

    #[test]
    fn no_language_renders_as_plain_text() {
        let html = render_markdown("```\nplain code\n```\n");
        assert!(!html.contains("color:#"));
        assert!(html.contains("plain code"));
    }

    #[test]
    fn javascript_link_is_unwrapped_to_plain_text() {
        let html = render_markdown("[click me](javascript:alert(1))");
        assert!(
            !html.contains("javascript:"),
            "javascript: scheme must not survive into the HTML: {html}"
        );
        assert!(
            !html.contains("<a "),
            "the link wrapper must be dropped: {html}"
        );
        // The link text is preserved as plain text.
        assert!(html.contains("click me"), "link text should remain: {html}");
    }

    #[test]
    fn data_image_is_dropped_to_alt_text() {
        let html = render_markdown(
            "![beacon](data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==)",
        );
        assert!(
            !html.contains("<img"),
            "data: image must not be emitted: {html}"
        );
        assert!(
            !html.contains("data:image"),
            "data: URL must not survive: {html}"
        );
        // Alt text renders as plain text.
        assert!(html.contains("beacon"), "alt text should remain: {html}");
    }

    #[test]
    fn http_link_is_preserved() {
        let html = render_markdown("[example](http://example.com/path)");
        assert!(
            html.contains("href=\"http://example.com/path\""),
            "http link should be preserved: {html}"
        );
        assert!(html.contains("example"));
    }

    #[test]
    fn https_and_mailto_and_relative_and_anchor_are_safe() {
        assert!(is_safe_url("https://example.com"));
        assert!(is_safe_url("http://example.com"));
        assert!(is_safe_url("mailto:user@example.com"));
        assert!(is_safe_url("/relative/path"));
        assert!(is_safe_url("./rel"));
        assert!(is_safe_url("#anchor"));
        assert!(is_safe_url("relative-no-scheme"));
        assert!(is_safe_url(""));
    }

    #[test]
    fn dangerous_schemes_are_rejected_even_when_obfuscated() {
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("data:text/html,<script>"));
        assert!(!is_safe_url("vbscript:msgbox"));
        assert!(!is_safe_url("file:///etc/passwd"));
        // Browser-tolerant obfuscation: leading/embedded whitespace + control.
        assert!(!is_safe_url("  javascript:alert(1)"));
        assert!(!is_safe_url("java\tscript:alert(1)"));
        assert!(!is_safe_url("JavaScript:alert(1)"));
    }

    #[test]
    fn http_image_is_preserved() {
        let html = render_markdown("![logo](https://example.com/logo.png)");
        assert!(
            html.contains("src=\"https://example.com/logo.png\""),
            "https image should be preserved: {html}"
        );
    }
}
