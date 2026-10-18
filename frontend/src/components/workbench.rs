use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::components::center_zone::CenterZone;
use crate::components::dock_zone::{DockPosition, DockZone, SidebarPanels};
use crate::sidebar_layout::SidebarSide;
use crate::state::{AppState, DockVisibility};

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

struct WindowMouseListenerHandle {
    window: web_sys::Window,
    event: &'static str,
    callback: Closure<dyn Fn(web_sys::MouseEvent)>,
}

impl WindowMouseListenerHandle {
    fn remove(self) {
        let _ = self.window.remove_event_listener_with_callback(
            self.event,
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

thread_local! {
    static MOUSEMOVE_LISTENER_HANDLE: RefCell<Option<WindowMouseListenerHandle>> = const { RefCell::new(None) };
    static MOUSEUP_LISTENER_HANDLE: RefCell<Option<WindowMouseListenerHandle>> = const { RefCell::new(None) };
}

fn clear_drag_window_listeners() {
    MOUSEMOVE_LISTENER_HANDLE.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            handle.remove();
        }
    });
    MOUSEUP_LISTENER_HANDLE.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            handle.remove();
        }
    });
}

#[component]
pub fn Workbench() -> impl IntoView {
    let state = expect_context::<AppState>();

    let layouts = state.sidebar_layout;
    let layout_blocked = state.sidebar_storage_blocked;
    let left_visible = move || {
        !layout_blocked.get()
            && layouts.with(|l| l.current().left.visible && !l.current().left.tabs.is_empty())
    };
    let right_visible = move || {
        !layout_blocked.get()
            && layouts.with(|l| l.current().right.visible && !l.current().right.tabs.is_empty())
    };
    let bottom_visible = move || state.bottom_dock.get() == DockVisibility::Visible;

    let left_width = Signal::derive(move || layouts.with(|l| l.current().left.width));
    let right_width = Signal::derive(move || layouts.with(|l| l.current().right.width));
    let set_width = move |side, width: f64| {
        layouts.update(|l| l.current_mut().pane_mut(side).width = width.clamp(MIN_DOCK, 2400.0))
    };
    let bottom_height = RwSignal::new(BOTTOM_DEFAULT);

    let dragging = RwSignal::new(None::<DragAxis>);
    let drag_start_pos = RwSignal::new(0.0f64);
    let drag_start_size = RwSignal::new(0.0f64);

    let left_bar_ref = NodeRef::<leptos::html::Div>::new();
    let right_bar_ref = NodeRef::<leptos::html::Div>::new();
    let workbench_ref = NodeRef::<leptos::html::Div>::new();
    on_cleanup(clear_drag_window_listeners);

    // Global mousemove + mouseup for drag
    Effect::new(move |_| {
        clear_drag_window_listeners();
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
                                let Some(bar) = right_bar_ref.get_untracked() else {
                                    return;
                                };
                                bar.get_bounding_client_rect().width()
                            } else {
                                0.0
                            };
                            set_width(
                                SidebarSide::Left,
                                new_w.min(cw - rw - MIN_CENTER - 8.0).min(cw * 0.35),
                            );
                        } else {
                            crate::components::header::report_user_error(
                                "Cannot resize sidebar without workspace geometry",
                            );
                        }
                    }
                    DragAxis::Right => {
                        let delta = start - ev.client_x() as f64;
                        let new_w = (start_size + delta).max(MIN_DOCK);
                        if let Some(el) = workbench_ref.get_untracked() {
                            let cw = el.client_width() as f64;
                            let lw = if left_visible() {
                                let Some(bar) = left_bar_ref.get_untracked() else {
                                    return;
                                };
                                bar.get_bounding_client_rect().width()
                            } else {
                                0.0
                            };
                            set_width(
                                SidebarSide::Right,
                                new_w.min(cw - lw - MIN_CENTER - 8.0).min(cw * 0.35),
                            );
                        } else {
                            crate::components::header::report_user_error(
                                "Cannot resize sidebar without workspace geometry",
                            );
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
        MOUSEMOVE_LISTENER_HANDLE.with(|slot| {
            slot.borrow_mut().replace(WindowMouseListenerHandle {
                window: window.clone(),
                event: "mousemove",
                callback: on_mousemove,
            });
        });
        MOUSEUP_LISTENER_HANDLE.with(|slot| {
            slot.borrow_mut().replace(WindowMouseListenerHandle {
                window,
                event: "mouseup",
                callback: on_mouseup,
            });
        });
    });

    let start_drag = move |axis: DragAxis, ev: web_sys::MouseEvent| {
        ev.prevent_default();
        let (pos, size) = match axis {
            DragAxis::Left | DragAxis::Right => {
                let bar = match axis {
                    DragAxis::Left => left_bar_ref.get_untracked(),
                    DragAxis::Right => right_bar_ref.get_untracked(),
                    DragAxis::Bottom => unreachable!(),
                };
                let Some(bar) = bar else {
                    crate::components::header::report_user_error(
                        "Cannot resize sidebar before its geometry is available",
                    );
                    return;
                };
                (ev.client_x() as f64, bar.get_bounding_client_rect().width())
            }
            DragAxis::Bottom => (ev.client_y() as f64, bottom_height.get_untracked()),
        };
        dragging.set(Some(axis));
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

    let grid_style = move || {
        let left = if left_visible() {
            format!("minmax(160px, min({}px, 35%))", left_width.get())
        } else {
            "28px".into()
        };
        let right = if right_visible() {
            format!("minmax(160px, min({}px, 35%))", right_width.get())
        } else {
            "28px".into()
        };
        format!("grid-template-columns:{left} 4px minmax(200px, 1fr) 4px {right}")
    };
    let bottom_style = move || format!("height:{}px", bottom_height.get() as i32);

    view! {
        <div class="workbench" node_ref=workbench_ref>
            <div class="workbench-main sidebar-workspace" style=grid_style>
                <div class="sidebar-bar sidebar-bar-left" node_ref=left_bar_ref><DockZone position=DockPosition::Left /></div>
                <div class="resize-handle resize-handle-h sidebar-resize-left" style=move || if left_visible() { "" } else { "visibility:hidden" } on:mousedown=on_left_handle></div>
                <div class="sidebar-center"><CenterZone /></div>
                <div class="resize-handle resize-handle-h sidebar-resize-right" style=move || if right_visible() { "" } else { "visibility:hidden" } on:mousedown=on_right_handle></div>
                <div class="sidebar-bar sidebar-bar-right" node_ref=right_bar_ref><DockZone position=DockPosition::Right /></div>
                <SidebarPanels />
                <div class="sidebar-layout-error" role="alert" style=move || if layout_blocked.get() { "" } else { "display:none" }>"Sidebar layout unavailable. Open Settings → Appearance → Reset sidebar layout to continue."</div>
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use wasm_bindgen_test::*;
    wasm_bindgen_test_configure!(run_in_browser);

    async fn tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            web_sys::window()
                .unwrap()
                .request_animation_frame(&resolve)
                .unwrap();
        });
        wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
    }

    fn element(container: &web_sys::HtmlElement, selector: &str) -> web_sys::HtmlElement {
        container
            .query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("Missing {selector}"))
            .dyn_into()
            .unwrap()
    }

    fn mode(container: &web_sys::HtmlElement, value: &str) {
        let select = element(container, "[aria-label='Interface mode']")
            .dyn_into::<web_sys::HtmlSelectElement>()
            .unwrap();
        select.set_value(value);
        select
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
    }

    fn shell(container: &web_sys::HtmlElement, state: AppState) -> impl Sized {
        mount_to(container.clone(), move || {
            state.install_sidebar_persistence();
            provide_context(state.clone());
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <div style="height:720px;width:1100px;display:flex;flex-direction:column">
                    <crate::components::header::Header /><Workbench />
                </div>
            }
        })
    }

    fn key(element: &web_sys::HtmlElement, name: &str) {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(name);
        init.set_ctrl_key(true);
        init.set_shift_key(true);
        init.set_bubbles(true);
        init.set_cancelable(true);
        element
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                    .unwrap(),
            )
            .unwrap();
    }

    #[wasm_bindgen_test]
    async fn sidebar_modes_keep_one_agents_mount_and_independent_layouts() {
        use crate::sidebar_layout::{STORAGE_KEY, SidebarSide, SidebarTab};
        use crate::state::TabContent;
        let window = web_sys::window().unwrap();
        let storage = window.local_storage().unwrap().unwrap();
        let previous = storage.get_item(STORAGE_KEY).unwrap();
        let legacy_key = crate::components::agents_panel::COLLAPSED_FOLDERS_STORAGE_KEY;
        let previous_legacy = storage.get_item(legacy_key).unwrap();
        storage.remove_item(STORAGE_KEY).unwrap();
        storage.remove_item(legacy_key).unwrap();
        let document = window.document().unwrap();
        let container = document
            .create_element("div")
            .unwrap()
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        let state = AppState::new();
        state.open_tab(
            TabContent::Chat {
                agent_ref: None,
                pending_team_member: None,
            },
            "Draft".into(),
            true,
        );
        let handle = shell(&container, state.clone());
        tick().await;
        assert!(
            container
                .query_selector("[aria-label='Interface mode']")
                .unwrap()
                .is_some(),
            "Mode choice must be visible in top chrome"
        );
        assert_eq!(
            container
                .query_selector_all(".agents-panel")
                .unwrap()
                .length(),
            1,
            "Both modes must reuse one Agents mount"
        );
        let agents = element(&container, ".agents-panel");
        let search = element(&container, ".agents-panel input")
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        search.set_value("keep this filter");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        let textarea = element(&container, ".chat-textarea")
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        textarea.set_value("Unsent draft\nsecond line");
        textarea
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        textarea.focus().unwrap();
        textarea.set_selection_range(3, 8).unwrap();
        let tabs_before = state
            .center_zone
            .with_untracked(|center| (center.all_tab_ids(), center.active_tab_id()));
        assert!(
            element(&container, ".sidebar-bar-right")
                .text_content()
                .unwrap()
                .contains("History")
        );
        assert!(
            !element(&container, ".sidebar-bar-left")
                .text_content()
                .unwrap()
                .contains("Files")
        );
        // Keyboard transfer leaves Agents and History visible on opposite sides.
        key(
            &element(&container, ".sidebar-bar-right button[aria-label='Agents']"),
            "ArrowLeft",
        );
        tick().await;
        let left_panel = element(&container, "section[aria-label='Agents panel']");
        let right_panel = element(&container, "section[aria-label='History panel']");
        assert!(left_panel.get_bounding_client_rect().width() > 0.0);
        assert!(right_panel.get_bounding_client_rect().width() > 0.0);
        assert!(
            left_panel.get_bounding_client_rect().right()
                < right_panel.get_bounding_client_rect().left()
        );
        assert!(agents.is_same_node(Some(&element(&container, ".agents-panel"))));
        assert_eq!(search.value(), "keep this filter");
        assert_eq!(textarea.value(), "Unsent draft\nsecond line");
        assert_eq!(textarea.selection_start().unwrap(), Some(3));
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&textarea))
        );
        mode(&container, "dinosaur");
        tick().await;
        assert!(
            element(&container, ".sidebar-bar-left")
                .text_content()
                .unwrap()
                .contains("Files")
        );
        assert!(
            element(&container, ".sidebar-bar-right")
                .text_content()
                .unwrap()
                .contains("Agents")
        );
        // DOM drag fixtures test the event plumbing, not native platform drag.
        let start = element(
            &container,
            ".sidebar-bar-right button[aria-label='History']",
        );
        start
            .dispatch_event(&web_sys::DragEvent::new("dragstart").unwrap())
            .unwrap();
        let before = element(&container, ".sidebar-bar-right button[aria-label='Agents']")
            .parent_element()
            .unwrap();
        before
            .dispatch_event(&web_sys::DragEvent::new("dragover").unwrap())
            .unwrap();
        element(&container, ".sidebar-bar-right .dock-tab-bar")
            .dispatch_event(&web_sys::DragEvent::new("drop").unwrap())
            .unwrap();
        tick().await;
        let order = element(&container, ".sidebar-bar-right")
            .query_selector_all("button.dock-tab")
            .unwrap();
        assert_eq!(order.item(0).unwrap().text_content().unwrap(), "History");
        // Reorder with the keyboard, then transfer with the visible tab menu.
        key(
            &element(&container, ".sidebar-bar-right button[aria-label='Agents']"),
            "ArrowUp",
        );
        tick().await;
        assert_eq!(
            element(&container, ".sidebar-bar-right button.dock-tab")
                .text_content()
                .unwrap(),
            "Agents"
        );
        let summary = element(
            &container,
            ".sidebar-bar-right summary[aria-label='Agents tab actions']",
        );
        summary.click();
        tick().await;
        let actions = summary.parent_element().unwrap();
        actions
            .query_selector("button")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap()
            .click();
        tick().await;
        assert!(
            element(&container, ".sidebar-bar-left")
                .text_content()
                .unwrap()
                .contains("Agents")
        );
        let start = element(&container, ".sidebar-bar-left button[aria-label='Agents']");
        start
            .dispatch_event(&web_sys::DragEvent::new("dragstart").unwrap())
            .unwrap();
        key(&start, "Escape");
        element(&container, ".sidebar-bar-right .dock-tab-bar")
            .dispatch_event(&web_sys::DragEvent::new("drop").unwrap())
            .unwrap();
        tick().await;
        assert_eq!(
            container
                .query_selector_all("button.dock-tab[aria-label='Agents']")
                .unwrap()
                .length(),
            1
        );
        assert_eq!(
            state
                .center_zone
                .with_untracked(|center| (center.all_tab_ids(), center.active_tab_id())),
            tabs_before
        );
        assert!(textarea.is_same_node(Some(&element(&container, ".chat-textarea"))));
        mode(&container, "vibe");
        tick().await;
        assert_eq!(
            element(&container, ".sidebar-bar-left button.dock-tab")
                .text_content()
                .unwrap(),
            "Agents"
        );
        assert_eq!(
            element(&container, ".sidebar-bar-right button.dock-tab")
                .text_content()
                .unwrap(),
            "History"
        );
        // The persisted mode, order, active choices, visibility and widths
        // restore in a fresh component tree, not just in an in-memory mirror.
        state.sidebar_layout.update(|layouts| {
            layouts.current_mut().left.width = 280.0;
            layouts.current_mut().right.visible = false;
        });
        tick().await;
        drop(handle);
        let restored = AppState::new();
        let handle = shell(&container, restored.clone());
        tick().await;
        assert_eq!(
            element(&container, "[aria-label='Interface mode']")
                .dyn_into::<web_sys::HtmlSelectElement>()
                .unwrap()
                .value(),
            "vibe"
        );
        assert_eq!(
            element(&container, ".sidebar-bar-left button.dock-tab")
                .text_content()
                .unwrap(),
            "Agents"
        );
        assert_eq!(
            element(&container, "section[aria-label='History panel']")
                .get_bounding_client_rect()
                .height(),
            0.0
        );
        assert_eq!(
            element(&container, "section[aria-label='Agents panel']")
                .get_bounding_client_rect()
                .width(),
            280.0
        );
        mode(&container, "dinosaur");
        tick().await;
        assert!(
            element(&container, ".sidebar-bar-left")
                .text_content()
                .unwrap()
                .contains("Agents")
        );
        assert_eq!(
            element(&container, ".sidebar-bar-right button.dock-tab")
                .text_content()
                .unwrap(),
            "History"
        );
        // Canonical activation follows a moved tab, including developer actions.
        restored.activate_sidebar_tab(SidebarTab::Agents);
        tick().await;
        assert!(
            element(&container, "section[aria-label='Agents panel']")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        assert_eq!(
            restored
                .sidebar_layout
                .with_untracked(|l| l.current().side(SidebarTab::Agents)),
            Some(SidebarSide::Left)
        );
        restored.sidebar_layout.update(|layouts| {
            layouts.current_mut().left.width = 1000.0;
            layouts.current_mut().right.width = 360.0;
        });
        let shell_element = element(&container, ".workbench")
            .parent_element()
            .unwrap()
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap();
        shell_element
            .style()
            .set_property("width", "900px")
            .unwrap();
        tick().await;
        let right = element(&container, ".sidebar-bar-right");
        let old_width = right.get_bounding_client_rect().width();
        let divider = element(&container, ".sidebar-resize-right");
        let start = divider.get_bounding_client_rect().left() as i32;
        let down = web_sys::MouseEventInit::new();
        down.set_client_x(start);
        down.set_bubbles(true);
        divider
            .dispatch_event(
                &web_sys::MouseEvent::new_with_mouse_event_init_dict("mousedown", &down).unwrap(),
            )
            .unwrap();
        let moved = web_sys::MouseEventInit::new();
        moved.set_client_x(start + 20);
        window
            .dispatch_event(
                &web_sys::MouseEvent::new_with_mouse_event_init_dict("mousemove", &moved).unwrap(),
            )
            .unwrap();
        window
            .dispatch_event(&web_sys::MouseEvent::new("mouseup").unwrap())
            .unwrap();
        tick().await;
        assert!(
            (right.get_bounding_client_rect().width() - (old_width - 20.0)).abs() < 1.0,
            "A clamped pane resizes from its rendered divider, not an offscreen preferred width"
        );
        assert!(
            right.get_bounding_client_rect().width() > 160.0,
            "The opposite pane's large preference must not force a jump to minimum width"
        );
        shell_element
            .style()
            .set_property("width", "1100px")
            .unwrap();
        drop(handle);
        storage.remove_item(STORAGE_KEY).unwrap();
        storage
            .set_item(legacy_key, "[\"project:local:legacy\"]")
            .unwrap();
        let legacy = AppState::new();
        let handle = shell(&container, legacy);
        tick().await;
        assert_eq!(
            element(&container, "[aria-label='Interface mode']")
                .dyn_into::<web_sys::HtmlSelectElement>()
                .unwrap()
                .value(),
            "dinosaur",
            "Recognized upgraded clients retain developer panes"
        );
        assert_eq!(
            element(&container, ".sidebar-bar-left button.dock-tab")
                .text_content()
                .unwrap(),
            "Files"
        );
        assert_eq!(
            element(&container, ".sidebar-bar-right button.dock-tab")
                .text_content()
                .unwrap(),
            "Agents"
        );
        assert_eq!(
            element(&container, "section[aria-label='Agents panel']")
                .get_bounding_client_rect()
                .width(),
            320.0
        );
        assert_eq!(
            storage.get_item(legacy_key).unwrap().as_deref(),
            Some("[\"project:local:legacy\"]"),
            "Migration preserves canonical folder choices"
        );
        mode(&container, "vibe");
        tick().await;
        drop(handle);
        let handle = shell(&container, AppState::new());
        tick().await;
        assert_eq!(
            element(&container, "[aria-label='Interface mode']")
                .dyn_into::<web_sys::HtmlSelectElement>()
                .unwrap()
                .value(),
            "vibe",
            "Migration runs once; the explicit last mode wins thereafter"
        );
        drop(handle);
        storage.set_item(STORAGE_KEY, "{}").unwrap();
        let invalid = AppState::new();
        let handle = shell(&container, invalid.clone());
        tick().await;
        assert!(invalid.sidebar_storage_blocked.get_untracked());
        assert!(
            element(&container, ".sidebar-layout-error")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        assert!(element(&container, "[aria-label='Interface mode']").has_attribute("disabled"));
        assert_eq!(
            element(&container, ".sidebar-bar-right .dock-tab-bar")
                .get_bounding_client_rect()
                .height(),
            0.0
        );
        assert_eq!(
            element(&container, "section[aria-label='Agents panel']")
                .get_bounding_client_rect()
                .height(),
            0.0
        );
        assert_eq!(
            storage.get_item(STORAGE_KEY).unwrap().as_deref(),
            Some("{}"),
            "Invalid layouts are not replaced by defaults"
        );
        element(&container, "button[title='Settings']").click();
        tick().await;
        let nav = container.query_selector_all(".settings-nav-item").unwrap();
        let appearance = (0..nav.length())
            .filter_map(|index| nav.item(index))
            .find(|node| node.text_content().unwrap().trim() == "Appearance")
            .unwrap()
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap();
        appearance.click();
        tick().await;
        let buttons = container.query_selector_all("button").unwrap();
        let reset = (0..buttons.length())
            .filter_map(|index| buttons.item(index))
            .find(|node| node.text_content().unwrap() == "Reset sidebar layout")
            .unwrap()
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap();
        reset.click();
        tick().await;
        assert!(!invalid.sidebar_storage_blocked.get_untracked());
        assert!(
            element(&container, ".sidebar-bar-right .dock-tab-bar")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        assert_ne!(
            storage.get_item(STORAGE_KEY).unwrap().as_deref(),
            Some("{}")
        );
        drop(handle);
        crate::components::header::reset_user_notice_for_tests();
        container.remove();
        match previous_legacy {
            Some(value) => storage.set_item(legacy_key, &value).unwrap(),
            None => storage.remove_item(legacy_key).unwrap(),
        }
        match previous {
            Some(value) => storage.set_item(STORAGE_KEY, &value).unwrap(),
            None => storage.remove_item(STORAGE_KEY).unwrap(),
        }
        crate::components::center_zone::CenterWorkspaceWidth::forget_measurement();
    }
}
