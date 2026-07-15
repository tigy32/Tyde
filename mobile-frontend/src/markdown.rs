//! Markdown rendering for the mobile web client.
//!
//! The output of [`render_markdown`] is fed straight into `inner_html` (chat
//! messages, and the message a `tyde_send_agent_message` call delivered), so it
//! is an HTML injection sink and must be safe by construction. It carries the
//! same hardening contract as the desktop renderer (`frontend/src/markdown.rs`):
//!
//! - Raw HTML in the source is downgraded to escaped text, so a message
//!   containing `<img src=x onerror=…>` or `<svg onload=…>` renders as visible
//!   text rather than as live markup with live handlers.
//! - **Link/image URLs are scheme-filtered**: only `http`, `https`, `mailto`,
//!   and relative/anchor targets survive. A link with a disallowed scheme
//!   (`javascript:`, `data:`, …) is unwrapped to its plain text; a disallowed
//!   image is dropped to its alt text.
//!
//! This matters because the content is not necessarily authored by the agent you
//! are talking to: agents routinely relay text they did not write — a fetched
//! page, a file's contents, another agent's output, a pasted brief.
//!
//! The renderer deliberately does *not* mirror desktop's syntect highlighting or
//! copy-button chrome; mobile keeps plain `<pre><code>` fences. The safety
//! contract is shared; the presentation is not.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

pub fn render_markdown(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(input, options);

    // Stacks tracking whether the enclosing link / image was suppressed (unsafe
    // URL), so the matching `End` is dropped too. CommonMark allows an image
    // inside a link, so these must be depth stacks, not single flags.
    let mut link_suppressed: Vec<bool> = Vec::new();
    let mut image_suppressed: Vec<bool> = Vec::new();

    let events = parser.filter_map(|event| match event {
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
        Event::Html(s) | Event::InlineHtml(s) => Some(Event::Html(CowStr::Boxed(
            escape_raw_html_text(&s).into_boxed_str(),
        ))),
        other => Some(other),
    });

    let mut html_output = String::with_capacity(input.len() * 2);
    html::push_html(&mut html_output, events);
    html_output
}

fn escape_raw_html_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            '=' => escaped.push_str("&#61;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Whether a link/image destination is safe to emit into `inner_html`. Allows
/// relative/anchor targets (no scheme) and the `http`, `https`, `mailto`
/// schemes; rejects everything else (`javascript:`, `data:`, `vbscript:`,
/// `file:`, …). Mirrors browser scheme-parsing leniency: leading/embedded ASCII
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_block_html_is_downgraded_to_text() {
        let html = render_markdown("<img src=x onerror=\"alert(1)\">\n");
        assert!(
            !html.contains("<img"),
            "raw HTML must not survive as markup: {html}"
        );
        assert!(
            !html.contains("onerror=\"alert(1)\""),
            "an event handler must not survive as a live attribute: {html}"
        );
        // It renders as visible, escaped text instead.
        assert!(html.contains("&lt;img"), "raw HTML shows as text: {html}");
    }

    #[test]
    fn raw_inline_html_is_downgraded_to_text() {
        let html = render_markdown("hello <svg onload=\"alert(1)\"></svg> world");
        assert!(!html.contains("<svg"), "inline raw HTML escaped: {html}");
        assert!(!html.contains("onload="), "no live handler: {html}");
        assert!(html.contains("&lt;svg"), "shows as text: {html}");
    }

    #[test]
    fn javascript_link_is_unwrapped_to_plain_text() {
        let html = render_markdown("[click me](javascript:alert(1))");
        assert!(
            !html.contains("javascript:"),
            "javascript: scheme must not survive: {html}"
        );
        assert!(!html.contains("<a "), "the link wrapper is dropped: {html}");
        assert!(html.contains("click me"), "link text remains: {html}");
    }

    #[test]
    fn data_image_is_dropped_to_alt_text() {
        let html = render_markdown("![beacon](data:image/gif;base64,R0lGODlhAQABAAA=)");
        assert!(!html.contains("<img"), "data: image not emitted: {html}");
        assert!(!html.contains("data:image"), "data: URL dropped: {html}");
        assert!(html.contains("beacon"), "alt text remains: {html}");
    }

    #[test]
    fn safe_links_and_images_are_preserved() {
        let link = render_markdown("[example](https://example.com/path)");
        assert!(
            link.contains("href=\"https://example.com/path\""),
            "https link preserved: {link}"
        );
        let image = render_markdown("![logo](https://example.com/logo.png)");
        assert!(
            image.contains("src=\"https://example.com/logo.png\""),
            "https image preserved: {image}"
        );
    }

    #[test]
    fn scheme_filter_matches_the_desktop_contract() {
        assert!(is_safe_url("https://example.com"));
        assert!(is_safe_url("http://example.com"));
        assert!(is_safe_url("mailto:user@example.com"));
        assert!(is_safe_url("/relative/path"));
        assert!(is_safe_url("#anchor"));
        assert!(is_safe_url(""));

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
    fn ordinary_markdown_still_renders() {
        let html = render_markdown("## Title\n\n- one\n- two\n\n`code`\n");
        assert!(html.contains("<h2>"), "headings: {html}");
        assert!(html.contains("<li>"), "list items: {html}");
        assert!(html.contains("<code>"), "inline code: {html}");
    }
}
