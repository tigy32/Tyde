/// Lightweight markdown-to-HTML renderer for chat messages.
/// Handles the subset of markdown commonly produced by coding agents:
/// fenced code blocks, inline code, bold, italic, links, lists, blockquotes,
/// headings, horizontal rules, and paragraphs.
/// Output is intentionally safe: all user text is HTML-escaped before insertion.
pub fn render_markdown(input: &str) -> String {
    let mut html = String::with_capacity(input.len() * 2);
    let lines: Vec<&str> = input.lines().collect();
    let len = lines.len();
    let mut i = 0;

    while i < len {
        let line = lines[i];

        // Fenced code block
        if let Some(fence) = line.strip_prefix("```") {
            let lang = fence.trim();
            let lang_class = if lang.is_empty() {
                String::new()
            } else {
                format!(" data-lang=\"{}\"", escape_html(lang))
            };
            html.push_str(&format!(
                "<div class=\"md-code-block\"><div class=\"md-code-header\"><span class=\"md-code-lang\">{}</span><button class=\"md-copy-code\" onclick=\"(() => {{ const c = this.closest('.md-code-block').querySelector('code'); navigator.clipboard.writeText(c.textContent).then(() => {{ this.textContent = '✓'; setTimeout(() => this.textContent = 'Copy', 1200); }}); }})()\">Copy</button></div><pre><code{}>",
                escape_html(if lang.is_empty() { "text" } else { lang }),
                lang_class,
            ));
            i += 1;
            while i < len && !lines[i].starts_with("```") {
                if i > 0 && !html.ends_with("<code>") && !html.ends_with(">") {
                    html.push('\n');
                }
                // Check if we need a newline before this line
                let code_line = lines[i];
                html.push_str(&escape_html(code_line));
                html.push('\n');
                i += 1;
            }
            // Trim trailing newline inside code
            if html.ends_with('\n') {
                html.pop();
            }
            html.push_str("</code></pre></div>");
            i += 1; // skip closing ```
            continue;
        }

        // Heading
        if line.starts_with('#') {
            let level = line.chars().take_while(|c| *c == '#').count().min(6);
            let text = line[level..].trim_start_matches(' ');
            html.push_str(&format!(
                "<h{level} class=\"md-heading\">{}</h{level}>",
                render_inline(text)
            ));
            i += 1;
            continue;
        }

        // Horizontal rule
        if matches!(line.trim(), "---" | "***" | "___") {
            html.push_str("<hr class=\"md-hr\">");
            i += 1;
            continue;
        }

        // Blockquote
        if line.starts_with("> ") || line == ">" {
            html.push_str("<blockquote class=\"md-blockquote\">");
            while i < len && (lines[i].starts_with("> ") || lines[i] == ">") {
                let content = lines[i].strip_prefix("> ").unwrap_or("");
                html.push_str(&format!("<p>{}</p>", render_inline(content)));
                i += 1;
            }
            html.push_str("</blockquote>");
            continue;
        }

        // Unordered list
        if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
            html.push_str("<ul class=\"md-list\">");
            while i < len
                && (lines[i].starts_with("- ")
                    || lines[i].starts_with("* ")
                    || lines[i].starts_with("+ ")
                    || lines[i].starts_with("  "))
            {
                let content = if lines[i].starts_with("  ") {
                    lines[i].trim()
                } else {
                    &lines[i][2..]
                };
                html.push_str(&format!("<li>{}</li>", render_inline(content)));
                i += 1;
            }
            html.push_str("</ul>");
            continue;
        }

        // Ordered list
        if is_ordered_list_item(line) {
            html.push_str("<ol class=\"md-list\">");
            while i < len && (is_ordered_list_item(lines[i]) || lines[i].starts_with("   ")) {
                let content = if lines[i].starts_with("   ") {
                    lines[i].trim()
                } else {
                    let dot_pos = lines[i].find(". ").unwrap_or(0);
                    &lines[i][dot_pos + 2..]
                };
                html.push_str(&format!("<li>{}</li>", render_inline(content)));
                i += 1;
            }
            html.push_str("</ol>");
            continue;
        }

        // Empty line
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // Paragraph - collect contiguous non-empty lines
        html.push_str("<p class=\"md-paragraph\">");
        let mut first = true;
        while i < len
            && !lines[i].trim().is_empty()
            && !lines[i].starts_with('#')
            && !lines[i].starts_with("```")
            && !lines[i].starts_with("> ")
            && !lines[i].starts_with("- ")
            && !lines[i].starts_with("* ")
            && !lines[i].starts_with("+ ")
            && !is_ordered_list_item(lines[i])
            && !matches!(lines[i].trim(), "---" | "***" | "___")
        {
            if !first {
                html.push('\n');
            }
            html.push_str(&render_inline(lines[i]));
            first = false;
            i += 1;
        }
        html.push_str("</p>");
    }

    html
}

fn is_ordered_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    if let Some(pos) = trimmed.find(". ") {
        trimmed[..pos].chars().all(|c| c.is_ascii_digit()) && pos > 0
    } else {
        false
    }
}

/// Render inline markdown: bold, italic, strikethrough, inline code, links
fn render_inline(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Inline code (backtick)
        if chars[i] == '`'
            && let Some(end) = find_closing(&chars, i + 1, '`')
        {
            let code: String = chars[i + 1..end].iter().collect();
            result.push_str(&format!(
                "<code class=\"md-inline-code\">{}</code>",
                escape_html(&code)
            ));
            i = end + 1;
            continue;
        }

        // Bold + italic ***text***
        if i + 2 < len
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && chars[i + 2] == '*'
            && let Some(end) = find_closing_seq(&chars, i + 3, &['*', '*', '*'])
        {
            let inner: String = chars[i + 3..end].iter().collect();
            result.push_str(&format!(
                "<strong><em>{}</em></strong>",
                render_inline(&inner)
            ));
            i = end + 3;
            continue;
        }

        // Bold **text**
        if i + 1 < len
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(end) = find_closing_seq(&chars, i + 2, &['*', '*'])
        {
            let inner: String = chars[i + 2..end].iter().collect();
            result.push_str(&format!("<strong>{}</strong>", render_inline(&inner)));
            i = end + 2;
            continue;
        }

        // Italic *text*
        if chars[i] == '*'
            && let Some(end) = find_closing(&chars, i + 1, '*')
        {
            let inner: String = chars[i + 1..end].iter().collect();
            result.push_str(&format!("<em>{}</em>", render_inline(&inner)));
            i = end + 1;
            continue;
        }

        // Strikethrough ~~text~~
        if i + 1 < len
            && chars[i] == '~'
            && chars[i + 1] == '~'
            && let Some(end) = find_closing_seq(&chars, i + 2, &['~', '~'])
        {
            let inner: String = chars[i + 2..end].iter().collect();
            result.push_str(&format!("<del>{}</del>", render_inline(&inner)));
            i = end + 2;
            continue;
        }

        // Link [text](url)
        if chars[i] == '['
            && let Some(bracket_end) = find_closing(&chars, i + 1, ']')
            && bracket_end + 1 < len
            && chars[bracket_end + 1] == '('
            && let Some(paren_end) = find_closing(&chars, bracket_end + 2, ')')
        {
            let text: String = chars[i + 1..bracket_end].iter().collect();
            let url: String = chars[bracket_end + 2..paren_end].iter().collect();
            // Only allow safe protocols
            if url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("mailto:")
            {
                result.push_str(&format!(
                    "<a class=\"md-link\" href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>",
                    escape_attr(&url),
                    escape_html(&text)
                ));
            } else {
                result.push_str(&escape_html(&text));
            }
            i = paren_end + 1;
            continue;
        }

        // Plain character
        match chars[i] {
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '&' => result.push_str("&amp;"),
            '"' => result.push_str("&quot;"),
            c => result.push(c),
        }
        i += 1;
    }

    result
}

fn find_closing(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len()).find(|&j| chars[j] == marker)
}

fn find_closing_seq(chars: &[char], start: usize, markers: &[char]) -> Option<usize> {
    let mlen = markers.len();
    if chars.len() < start + mlen {
        return None;
    }
    (start..=chars.len() - mlen).find(|&j| chars[j..j + mlen] == *markers)
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
