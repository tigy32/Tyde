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
                                            <code class=lang.clone()>{text}{"\n"}</code>
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
