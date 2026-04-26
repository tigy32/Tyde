//! `ModifyFile` renderer.
//!
//! Pre-completion: shows the proposed diff (before → after) in unified form,
//! 2-line context, classified `+`/`-`/`@@`/context per line — the legacy
//! parity behavior. Post-completion: same diff, plus `+A -B` shown in the
//! header detail (handled by the shell).
//!
//! Diff text is computed with the `similar` crate from the `before`/`after`
//! protocol fields. Both are already typed protocol payloads; this is a pure
//! presentation transform.

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};
use similar::{ChangeTag, TextDiff};

use crate::state::ToolOutputMode;

use super::escape_html;

const COMPACT_LINE_CAP: usize = 200;

pub(crate) fn render(
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::ModifyFile {
        file_path,
        before,
        after,
    } = req
    else {
        unreachable!("modify_file::render dispatched on non-ModifyFile request");
    };

    let _ = result; // result for ModifyFile contributes only to the header summary.

    if mode == ToolOutputMode::Summary {
        return view! {
            <div class="tool-result-modify"></div>
        }
        .into_any();
    }

    let lines = build_diff_lines(before, after);
    let total_lines = lines.len();
    let over_cap = mode == ToolOutputMode::Compact && total_lines > COMPACT_LINE_CAP;
    let expanded = RwSignal::new(!over_cap);

    let path = file_path.clone();
    let html = build_diff_html(&lines, mode, expanded);

    let toggle_label = move || {
        if expanded.get() {
            "Show less".to_owned()
        } else {
            format!("Show more ({total_lines} lines)")
        }
    };

    view! {
        <div class="tool-result-modify">
            <div class="tool-file-path">{path}</div>
            <pre class="tool-inline-diff" inner_html=html></pre>
            <Show when=move || over_cap>
                <button
                    class="tool-show-more"
                    on:click=move |_| expanded.update(|v| *v = !*v)
                >{toggle_label}</button>
            </Show>
        </div>
    }
    .into_any()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DiffLineKind {
    Hunk,
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug)]
struct DiffLine {
    kind: DiffLineKind,
    text: String,
}

/// Compute a unified-diff line list from `before`/`after`, with 2-line
/// context. Hunk headers are emitted as `@@ ... @@` lines.
fn build_diff_lines(before: &str, after: &str) -> Vec<DiffLine> {
    let diff = TextDiff::from_lines(before, after);
    let mut out: Vec<DiffLine> = Vec::new();
    for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
        out.push(DiffLine {
            kind: DiffLineKind::Hunk,
            text: hunk.header().to_string(),
        });
        for change in hunk.iter_changes() {
            let kind = match change.tag() {
                ChangeTag::Equal => DiffLineKind::Context,
                ChangeTag::Insert => DiffLineKind::Added,
                ChangeTag::Delete => DiffLineKind::Removed,
            };
            // `change.value()` includes its trailing newline — strip the last
            // newline only so an empty line still renders as an empty row.
            let mut text = change.value().to_string();
            if text.ends_with('\n') {
                text.pop();
            }
            out.push(DiffLine { kind, text });
        }
    }
    out
}

fn build_diff_html(
    lines: &[DiffLine],
    mode: ToolOutputMode,
    expanded: RwSignal<bool>,
) -> Box<dyn Fn() -> String + Send + Sync> {
    let lines = lines.to_vec();
    Box::new(move || {
        let cap = if mode == ToolOutputMode::Compact && !expanded.get() {
            COMPACT_LINE_CAP
        } else {
            usize::MAX
        };

        let mut html = String::new();
        for line in lines.iter().take(cap) {
            let (cls, prefix) = match line.kind {
                DiffLineKind::Hunk => ("inline-diff-hunk", ""),
                DiffLineKind::Context => ("inline-diff-context", " "),
                DiffLineKind::Added => ("inline-diff-added", "+"),
                DiffLineKind::Removed => ("inline-diff-removed", "-"),
            };
            html.push_str(&format!(
                "<div class=\"inline-diff-line {cls}\"><span class=\"diff-prefix\">{}</span><span class=\"diff-text\">{}</span></div>",
                prefix,
                escape_html(&line.text),
            ));
        }
        if cap < lines.len() {
            html.push_str(
                "<div class=\"inline-diff-line inline-diff-context\"><span class=\"diff-prefix\"> </span><span class=\"diff-text\">\u{2026}</span></div>",
            );
        }
        html
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diff_has_no_lines() {
        let lines = build_diff_lines("a\nb\n", "a\nb\n");
        assert!(lines.is_empty());
    }

    #[test]
    fn single_added_line_classifies_correctly() {
        let lines = build_diff_lines("a\n", "a\nb\n");
        // Should have at least one hunk header and one added line.
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Hunk));
        assert!(
            lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Added && l.text == "b")
        );
    }

    #[test]
    fn replace_classifies_added_and_removed() {
        let lines = build_diff_lines("a\nb\nc\n", "a\nB\nc\n");
        assert!(
            lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Added && l.text == "B")
        );
        assert!(
            lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Removed && l.text == "b")
        );
        assert!(
            lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Context && l.text == "a")
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn small_req() -> ToolRequestType {
        ToolRequestType::ModifyFile {
            file_path: "src/main.rs".to_owned(),
            before: "fn main() {\n    println!(\"old\");\n}\n".to_owned(),
            after: "fn main() {\n    println!(\"new\");\n}\n".to_owned(),
        }
    }

    fn big_req() -> ToolRequestType {
        let before: String = (0..400)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after: String = (0..400)
            .map(|i| {
                if i % 2 == 0 {
                    format!("line{i}")
                } else {
                    format!("LINE{i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        ToolRequestType::ModifyFile {
            file_path: "src/main.rs".to_owned(),
            before,
            after,
        }
    }

    #[wasm_bindgen_test]
    async fn summary_renders_no_diff_lines() {
        let container = mount(move || render(&small_req(), None, ToolOutputMode::Summary));
        next_tick().await;
        assert_eq!(count(&container, ".inline-diff-line"), 0);
    }

    #[wasm_bindgen_test]
    async fn compact_under_cap_shows_added_and_removed() {
        let container = mount(move || render(&small_req(), None, ToolOutputMode::Compact));
        next_tick().await;
        let added = count(&container, ".inline-diff-added");
        let removed = count(&container, ".inline-diff-removed");
        assert!(added >= 1);
        assert!(removed >= 1);
        assert!(!has_show_more(&container), "small diff has no toggle");
    }

    #[wasm_bindgen_test]
    async fn compact_over_cap_truncates_with_toggle() {
        let container = mount(move || render(&big_req(), None, ToolOutputMode::Compact));
        next_tick().await;
        let lines = count(&container, ".inline-diff-line");
        // Cap + 1 ellipsis row.
        assert!(lines <= COMPACT_LINE_CAP + 1);
        assert!(has_show_more(&container));
    }

    #[wasm_bindgen_test]
    async fn full_renders_more_than_compact_cap() {
        let container = mount(move || render(&big_req(), None, ToolOutputMode::Full));
        next_tick().await;
        let lines = count(&container, ".inline-diff-line");
        assert!(lines > COMPACT_LINE_CAP);
        assert!(!has_show_more(&container));
    }
}
