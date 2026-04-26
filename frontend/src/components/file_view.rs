use leptos::prelude::*;

use crate::components::find_bar::{FindBar, FindState};
use crate::highlight::highlight_code_blocks;
use crate::state::{AppState, TabContent};

use protocol::ProjectPath;

#[component]
pub fn FileView(path: ProjectPath) -> impl IntoView {
    let state = expect_context::<AppState>();

    let file_path = path.clone();
    let file_info = move || {
        state
            .open_files
            .with(|files| files.get(&file_path).cloned())
    };

    let close_path = path.clone();

    view! {
        <div class="file-view">
            {move || {
                let close_path = close_path.clone();
                match file_info() {
                    Some(f) => {
                        let path_display = format!("{}/{}", f.path.root.0, f.path.relative_path);
                        let lang_class = lang_class_from_path(&f.path.relative_path);
                        let content = if f.is_binary {
                            "(binary file)".to_owned()
                        } else {
                            f.contents.unwrap_or_else(|| "(file not found)".to_owned())
                        };
                        let on_close = move |_| {
                            let state = expect_context::<AppState>();
                            let tab_id = state.center_zone.with_untracked(|cz| {
                                cz.find_tab(&TabContent::File {
                                    path: close_path.clone(),
                                })
                            });
                            if let Some(id) = tab_id {
                                state.close_tab(id);
                            }
                        };

                        let lines: Vec<String> = content.lines().map(|l| l.to_owned()).collect();
                        let find_state = FindState::new(lines.clone());
                        provide_context(find_state.clone());

                        let pre_ref: NodeRef<leptos::html::Pre> = NodeRef::new();
                        Effect::new(move |_| {
                            if let Some(el) = pre_ref.get() {
                                highlight_code_blocks(&el);
                            }
                        });

                        let find_bar_open = state.find_bar_open;

                        view! {
                            <div class="file-view-header">
                                <span class="file-view-path">{path_display}</span>
                                <button class="file-view-close" on:click=on_close title="Close">"×"</button>
                            </div>
                            {move || {
                                if find_bar_open.get() {
                                    Some(view! { <FindBar /> })
                                } else {
                                    None
                                }
                            }}
                            <pre class="file-view-content" node_ref=pre_ref>
                                {lines.iter().enumerate().map(|(i, line_text)| {
                                    let text = line_text.clone();
                                    let lang = lang_class.clone();
                                    let find = find_state.clone();
                                    view! {
                                        <div
                                            class=move || file_line_class(i, &find)
                                            attr:data-find-idx=i
                                        >
                                            <span class="file-line-num">{i + 1}</span>
                                            <code class=lang.clone()>{text}</code>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </pre>
                        }.into_any()
                    }
                    None => view! {
                        <div class="panel-empty">"No file open"</div>
                    }.into_any(),
                }
            }}
        </div>
    }
}

fn file_line_class(line_idx: usize, find: &FindState) -> &'static str {
    let results = find.results.get();
    if !results.match_set.contains(&line_idx) {
        return "file-line";
    }
    let active = find.active_index.get();
    if active >= 0 && results.match_lines.get(active as usize) == Some(&line_idx) {
        "file-line find-hit-active"
    } else {
        "file-line find-hit"
    }
}

fn lang_class_from_path(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("");
    let lang = match ext {
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "sh" | "bash" | "zsh" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sql" => "sql",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "r" => "r",
        "lua" => "lua",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "php" => "php",
        "pl" | "pm" => "perl",
        "dart" => "dart",
        "scala" => "scala",
        "clj" | "cljs" => "clojure",
        "vim" => "vim",
        "tf" => "hcl",
        "proto" => "protobuf",
        "graphql" | "gql" => "graphql",
        _ => "",
    };
    if lang.is_empty() {
        String::new()
    } else {
        format!("language-{lang}")
    }
}

/// Render-layer tests for `FileView`.
///
/// Asserts on what the user perceives: line count, visible text, and geometry
/// (the gap between consecutive rendered lines vs. the height of a single row).
/// Avoid asserting on internal class names or DOM structure so the tests
/// survive refactors of the component as long as the rendered output stays
/// correct.
///
/// Run with: `tools/run-wasm-tests.sh wasm_tests::` (the script handles
/// chromedriver and `wasm-bindgen-cli` setup automatically — see CLAUDE.md).
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, OpenFile};
    use leptos::mount::mount_to;
    use protocol::{ProjectPath, ProjectRootPath};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    /// Inject the production stylesheet once per test session so layout
    /// assertions reflect real styling.
    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document.get_element_by_id("test-prod-styles").is_some() {
            return;
        }
        let style = document.create_element("style").unwrap();
        style.set_id("test-prod-styles");
        style.set_text_content(Some(PROD_STYLES));
        document.head().unwrap().append_child(&style).unwrap();
    }

    /// Create a fresh, sized container appended to the document body so child
    /// elements have a real layout box. Returns the container as an
    /// `HtmlElement` ready to pass to `mount_to`.
    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        // Give the container a deterministic size so flex children lay out
        // predictably regardless of the headless browser viewport.
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 800px; height: 600px; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    fn line_rows(container: &HtmlElement) -> Vec<Element> {
        // Query for rendered rows by structural pattern: the direct children of
        // the file-content `<pre>`. Using a structural query (rather than a
        // specific class on each row) keeps the test resilient to row-level
        // class renames as long as one row per source line is rendered.
        let nodes = container
            .query_selector_all(".file-view-content > *")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<Element>().ok())
            .collect()
    }

    /// Yield to the browser event loop so reactive effects flush and the DOM
    /// reflects the rendered view before we assert on it.
    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    #[wasm_bindgen_test]
    async fn renders_lines_single_spaced() {
        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "hello.rs".to_owned(),
        };
        let content = "line one\nline two\nline three\nline four\nline five";

        let container = make_container();
        let mount_path = path.clone();
        // Create the AppState inside the mount closure so its signals belong to
        // the mount's reactive Owner and the provided-context lookup resolves
        // them correctly.
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! { <FileView path=mount_path.clone() /> }
        });

        next_tick().await;

        let rows = line_rows(&container);
        assert_eq!(
            rows.len(),
            5,
            "expected one rendered row per source line, got {}",
            rows.len()
        );

        // Geometry assertion — the heart of the test.
        let row0 = rows[0].get_bounding_client_rect();
        let row1 = rows[1].get_bounding_client_rect();
        let row4 = rows[4].get_bounding_client_rect();

        let row_height = row0.height();
        let gap = row1.top() - row0.top();
        let total = row4.top() - row0.top();

        assert!(row_height > 0.0, "row0 has no height — layout failed");

        // Geometry guard: the gap between consecutive rows should be ~one row
        // tall. Catches CSS regressions (line-height, padding) that would
        // visually space rows apart.
        let ratio = gap / row_height;
        assert!(
            (0.95..=1.10).contains(&ratio),
            "lines are not single-spaced: gap={gap:.2}px, row_height={row_height:.2}px, \
             ratio={ratio:.2}"
        );

        // Spacing should be uniform across all rows, not just the first pair.
        let expected_total = gap * 4.0;
        assert!(
            (total - expected_total).abs() < row_height * 0.5,
            "row spacing is not uniform: total={total:.2}px, expected≈{expected_total:.2}px"
        );

        // Text guard: each row's rendered text should equal the line number
        // followed by the source line, with no extra characters. Catches the
        // class of bug where stray characters (e.g. a trailing "\n") leak into
        // the rendered output — which is the original double-spacing
        // regression we hit. Text-equality is the right assertion shape here:
        // the user perceives "what text appears in the row," and any character
        // we didn't intend to add is a defect regardless of whether the
        // headless renderer expands the row's pixel height for it.
        let expected = [
            "line one",
            "line two",
            "line three",
            "line four",
            "line five",
        ];
        for (i, row) in rows.iter().enumerate() {
            let text = row.text_content().unwrap_or_default();
            let want = format!("{}{}", i + 1, expected[i]);
            assert_eq!(
                text, want,
                "row {i} rendered text does not match source line exactly"
            );
        }
    }
}
