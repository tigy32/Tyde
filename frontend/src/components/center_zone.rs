use leptos::prelude::*;

use crate::components::chat_view::ChatView;
use crate::components::diff_view::DiffView;
use crate::components::file_view::FileView;
use crate::components::home_view::HomeView;
use crate::state::{AppState, CenterView};

#[component]
pub fn CenterZone() -> impl IntoView {
    let state = expect_context::<AppState>();

    let set_home = move |_| state.center_view.set(CenterView::Home);
    let set_chat = move |_| state.center_view.set(CenterView::Chat);
    let set_editor = move |_| state.center_view.set(CenterView::Editor);

    let tab_class = move |target: CenterView| {
        move || {
            if state.center_view.get() == target {
                "tab active"
            } else {
                "tab"
            }
        }
    };

    view! {
        <div class="center-zone">
            <div class="tab-bar">
                <button class={tab_class(CenterView::Home)} on:click=set_home>"Home"</button>
                <button class={tab_class(CenterView::Chat)} on:click=set_chat>"Chat"</button>
                <button class={tab_class(CenterView::Editor)} on:click=set_editor>"Editor"</button>
            </div>
            <div class="center-content">
                {move || match state.center_view.get() {
                    CenterView::Home => view! {
                        <div class="center-content-scroll">
                            <HomeView />
                        </div>
                    }.into_any(),
                    CenterView::Chat => view! {
                        <ChatView />
                    }.into_any(),
                    CenterView::Editor => {
                        let has_file = state.open_file.get().is_some();
                        if has_file {
                            view! { <FileView /> }.into_any()
                        } else {
                            view! { <DiffView /> }.into_any()
                        }
                    }
                }}
            </div>
        </div>
    }
}
