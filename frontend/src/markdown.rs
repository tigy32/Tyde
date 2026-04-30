use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};

use crate::syntax_highlight::{highlight_to_html, syntax_for_lang_token};

/// Render agent/assistant markdown to HTML.
///
/// - GFM extensions: tables, strikethrough, task lists, footnotes.
/// - Raw HTML in the source is downgraded to escaped text to prevent XSS.
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
    let events = parser.filter_map(|ev| match ev {
        Event::Start(Tag::CodeBlock(kind)) => {
            let lang = match kind {
                CodeBlockKind::Fenced(l) => l.to_string(),
                CodeBlockKind::Indented => String::new(),
            };
            in_code = Some((lang, String::new()));
            None
        }
        Event::End(TagEnd::CodeBlock) => {
            let (lang, code) = in_code.take().expect("unbalanced code block");
            Some(Event::Html(CowStr::Boxed(
                build_code_block(&lang, &code).into_boxed_str(),
            )))
        }
        Event::Text(t) if in_code.is_some() => {
            in_code.as_mut().unwrap().1.push_str(&t);
            None
        }
        Event::Html(s) => Some(Event::Text(s)),
        Event::InlineHtml(s) => Some(Event::Text(s)),
        other => Some(other),
    });

    let mut out = String::with_capacity(input.len() * 2);
    html::push_html(&mut out, events);
    out
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
}
