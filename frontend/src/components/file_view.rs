use leptos::prelude::*;

use crate::state::AppState;

#[component]
pub fn FileView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let file_info = move || state.open_file.get();

    let close = move |_| {
        state.open_file.set(None);
    };

    view! {
        <div class="file-view">
            {move || match file_info() {
                Some(f) => {
                    let path_display = format!("{}/{}", f.path.root.0, f.path.relative_path);
                    let content = if f.is_binary {
                        "(binary file)".to_owned()
                    } else {
                        f.contents.unwrap_or_else(|| "(file not found)".to_owned())
                    };
                    view! {
                        <div class="file-view-header">
                            <span class="file-view-path">{path_display}</span>
                            <button class="file-view-close" on:click=close title="Close">"×"</button>
                        </div>
                        <pre class="file-view-content"><code>{content}</code></pre>
                    }.into_any()
                }
                None => view! {
                    <div class="panel-empty">"No file open"</div>
                }.into_any(),
            }}
        </div>
    }
}
