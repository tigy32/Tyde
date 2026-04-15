use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::components::center_zone::CenterZone;
use crate::components::dock_zone::{DockPosition, DockZone};
use crate::state::{AppState, DockVisibility};

const LEFT_DEFAULT: f64 = 320.0;
const RIGHT_DEFAULT: f64 = 320.0;
const BOTTOM_DEFAULT: f64 = 200.0;
const MIN_DOCK: f64 = 160.0;
const MIN_CENTER: f64 = 200.0;
const MIN_BOTTOM: f64 = 80.0;

#[derive(Clone, Copy, PartialEq)]
enum DragAxis {
    Left,
    Right,
    Bottom,
}

#[component]
pub fn Workbench() -> impl IntoView {
    let state = expect_context::<AppState>();

    let left_visible = move || state.left_dock.get() == DockVisibility::Visible;
    let right_visible = move || state.right_dock.get() == DockVisibility::Visible;
    let bottom_visible = move || state.bottom_dock.get() == DockVisibility::Visible;

    let left_width = RwSignal::new(LEFT_DEFAULT);
    let right_width = RwSignal::new(RIGHT_DEFAULT);
    let bottom_height = RwSignal::new(BOTTOM_DEFAULT);

    let dragging = RwSignal::new(None::<DragAxis>);
    let drag_start_pos = RwSignal::new(0.0f64);
    let drag_start_size = RwSignal::new(0.0f64);

    let workbench_ref = NodeRef::<leptos::html::Div>::new();

    // Global mousemove + mouseup for drag
    Effect::new(move |_| {
        let window = web_sys::window().unwrap();

        let on_mousemove =
            Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
                let Some(axis) = dragging.get_untracked() else {
                    return;
                };
                let start = drag_start_pos.get_untracked();
                let start_size = drag_start_size.get_untracked();

                match axis {
                    DragAxis::Left => {
                        let delta = ev.client_x() as f64 - start;
                        let new_w = (start_size + delta).max(MIN_DOCK);
                        if let Some(el) = workbench_ref.get_untracked() {
                            let cw = el.client_width() as f64;
                            let rw = if right_visible() {
                                right_width.get_untracked()
                            } else {
                                0.0
                            };
                            left_width.set(new_w.min(cw - rw - MIN_CENTER - 8.0));
                        } else {
                            left_width.set(new_w);
                        }
                    }
                    DragAxis::Right => {
                        let delta = start - ev.client_x() as f64;
                        let new_w = (start_size + delta).max(MIN_DOCK);
                        if let Some(el) = workbench_ref.get_untracked() {
                            let cw = el.client_width() as f64;
                            let lw = if left_visible() {
                                left_width.get_untracked()
                            } else {
                                0.0
                            };
                            right_width.set(new_w.min(cw - lw - MIN_CENTER - 8.0));
                        } else {
                            right_width.set(new_w);
                        }
                    }
                    DragAxis::Bottom => {
                        let delta = start - ev.client_y() as f64;
                        let new_h = (start_size + delta).max(MIN_BOTTOM);
                        if let Some(el) = workbench_ref.get_untracked() {
                            let ch = el.client_height() as f64;
                            bottom_height.set(new_h.min(ch - MIN_CENTER));
                        } else {
                            bottom_height.set(new_h);
                        }
                    }
                }
            });

        let on_mouseup =
            Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |_: web_sys::MouseEvent| {
                if dragging.get_untracked().is_some() {
                    dragging.set(None);
                    if let Some(body) = web_sys::window()
                        .and_then(|w| w.document())
                        .and_then(|d| d.body())
                    {
                        let _ = body.style().remove_property("cursor");
                        let _ = body.style().remove_property("user-select");
                    }
                }
            });

        let _ = window
            .add_event_listener_with_callback("mousemove", on_mousemove.as_ref().unchecked_ref());
        let _ =
            window.add_event_listener_with_callback("mouseup", on_mouseup.as_ref().unchecked_ref());
        on_mousemove.forget();
        on_mouseup.forget();
    });

    let start_drag = move |axis: DragAxis, ev: web_sys::MouseEvent| {
        ev.prevent_default();
        dragging.set(Some(axis));
        let (pos, size) = match axis {
            DragAxis::Left => (ev.client_x() as f64, left_width.get_untracked()),
            DragAxis::Right => (ev.client_x() as f64, right_width.get_untracked()),
            DragAxis::Bottom => (ev.client_y() as f64, bottom_height.get_untracked()),
        };
        drag_start_pos.set(pos);
        drag_start_size.set(size);
        let cursor = match axis {
            DragAxis::Left | DragAxis::Right => "col-resize",
            DragAxis::Bottom => "row-resize",
        };
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            let _ = body.style().set_property("cursor", cursor);
            let _ = body.style().set_property("user-select", "none");
        }
    };

    let on_left_handle = move |ev: web_sys::MouseEvent| start_drag(DragAxis::Left, ev);
    let on_right_handle = move |ev: web_sys::MouseEvent| start_drag(DragAxis::Right, ev);
    let on_bottom_handle = move |ev: web_sys::MouseEvent| start_drag(DragAxis::Bottom, ev);

    let left_style = move || format!("width:{}px", left_width.get() as i32);
    let right_style = move || format!("width:{}px", right_width.get() as i32);
    let bottom_style = move || format!("height:{}px", bottom_height.get() as i32);

    view! {
        <div class="workbench" node_ref=workbench_ref>
            <div class="workbench-main">
                <Show when=left_visible>
                    <div class="dock-zone dock-left" style=left_style>
                        <DockZone position=DockPosition::Left />
                    </div>
                    <div class="resize-handle resize-handle-h" on:mousedown=on_left_handle></div>
                </Show>
                <CenterZone />
                <Show when=right_visible>
                    <div class="resize-handle resize-handle-h" on:mousedown=on_right_handle></div>
                    <div class="dock-zone dock-right" style=right_style>
                        <DockZone position=DockPosition::Right />
                    </div>
                </Show>
            </div>
            <Show when=bottom_visible>
                <div class="resize-handle resize-handle-v" on:mousedown=on_bottom_handle></div>
                <div class="dock-zone dock-bottom" style=bottom_style>
                    <DockZone position=DockPosition::Bottom />
                </div>
            </Show>
        </div>
    }
}
