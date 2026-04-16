use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// Render agent/assistant markdown to HTML.
///
/// - GFM extensions: tables, strikethrough, task lists, footnotes.
/// - Raw HTML in the source is downgraded to escaped text to prevent XSS.
/// - Fenced code blocks are wrapped in `.md-code-block` with a language label
///   and a copy button; the inner `<code>` gets `class="language-XXX"` so
///   highlight.js can pick it up.
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
    let lang_label = if lang.is_empty() {
        "text".to_owned()
    } else {
        escape_html(lang)
    };
    let lang_class = if lang.is_empty() {
        String::new()
    } else {
        format!(" class=\"language-{}\"", escape_attr(lang))
    };
    let code_escaped = escape_html(code_trimmed);

    format!(
        "<div class=\"md-code-block\">\
         <div class=\"md-code-header\">\
         <span class=\"md-code-lang\">{lang_label}</span>\
         <button class=\"md-copy-code\" onclick=\"(() => {{ const c = this.closest('.md-code-block').querySelector('code'); navigator.clipboard.writeText(c.textContent).then(() => {{ this.textContent='\u{2713}'; setTimeout(() => this.textContent='Copy', 1200); }}); }})()\">Copy</button>\
         </div>\
         <pre><code{lang_class}>{code_escaped}</code></pre>\
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

fn escape_attr(s: &str) -> String {
    escape_html(s)
}
