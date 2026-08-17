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
