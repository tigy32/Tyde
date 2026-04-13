use leptos::prelude::*;

use crate::components::center_zone::CenterZone;
use crate::components::dock_zone::{DockPosition, DockZone};
use crate::state::{AppState, DockVisibility};

#[component]
pub fn Workbench() -> impl IntoView {
    let state = expect_context::<AppState>();

    let left_visible = move || state.left_dock.get() == DockVisibility::Visible;
    let right_visible = move || state.right_dock.get() == DockVisibility::Visible;
    let bottom_visible = move || state.bottom_dock.get() == DockVisibility::Visible;

    view! {
        <div class="workbench">
            <div class="workbench-main">
                <Show when=left_visible>
                    <DockZone position=DockPosition::Left />
                </Show>
                <CenterZone />
                <Show when=right_visible>
                    <DockZone position=DockPosition::Right />
                </Show>
            </div>
            <Show when=bottom_visible>
                <DockZone position=DockPosition::Bottom />
            </Show>
        </div>
    }
}
