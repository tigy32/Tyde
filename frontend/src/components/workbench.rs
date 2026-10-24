use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::components::center_zone::CenterZone;
use crate::components::dock_zone::{DockZone, SidebarPanels};
use crate::sidebar_layout::SidebarSide;
use crate::state::AppState;

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
    let tab_drag = state.sidebar_drag;
    let left_visible = move || !layout_blocked.get() && layouts.with(|l| l.current().left.visible);
    let right_visible =
        move || !layout_blocked.get() && layouts.with(|l| l.current().right.visible);
    let bottom_visible =
        move || !layout_blocked.get() && layouts.with(|l| l.current().bottom.visible);

    let left_width = Signal::derive(move || layouts.with(|l| l.current().left.width));
    let right_width = Signal::derive(move || layouts.with(|l| l.current().right.width));
    let set_width = move |side, width: f64| {
        layouts.update(|l| {
            l.current_mut().pane_mut(side).width = width.clamp(
                if side == SidebarSide::Bottom {
                    MIN_BOTTOM
                } else {
                    MIN_DOCK
                },
                2400.0,
            )
        })
    };
    let bottom_height = Signal::derive(move || layouts.with(|l| l.current().bottom.width));

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
                            set_width(SidebarSide::Bottom, new_h.min(ch - MIN_CENTER));
                        } else {
                            set_width(SidebarSide::Bottom, new_h);
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
            "0px".into()
        };
        let right = if right_visible() {
            format!("minmax(160px, min({}px, 35%))", right_width.get())
        } else {
            "0px".into()
        };
        let left_handle = if left_visible() { 4 } else { 0 };
        let right_handle = if right_visible() { 4 } else { 0 };
        format!(
            "grid-template-columns:{left} {left_handle}px minmax(200px, 1fr) {right_handle}px {right};grid-template-rows:34px minmax(0,1fr) {}px {}px {}px",
            if bottom_visible() { 4 } else { 0 },
            if bottom_visible() { 34 } else { 0 },
            if bottom_visible() {
                (bottom_height.get() - 34.0).max(0.0) as i32
            } else {
                0
            }
        )
    };

    view! {
        <div class="workbench" node_ref=workbench_ref>
            <div class="workbench-main sidebar-workspace" style=grid_style>
                <div class="sidebar-bar sidebar-bar-left" node_ref=left_bar_ref
                    class:sidebar-empty-drop-target=move || !left_visible() && tab_drag.get().is_some()
                    style=move || if left_visible() || (!layout_blocked.get() && tab_drag.get().is_some()) { "" } else { "display:none" }><DockZone position=SidebarSide::Left /></div>
                <div class="resize-handle resize-handle-h sidebar-resize-left" style=move || if left_visible() { "" } else { "visibility:hidden" } on:mousedown=on_left_handle></div>
                <div class="sidebar-center"><CenterZone /></div>
                <div class="resize-handle resize-handle-h sidebar-resize-right" style=move || if right_visible() { "" } else { "visibility:hidden" } on:mousedown=on_right_handle></div>
                <div class="sidebar-bar sidebar-bar-right" node_ref=right_bar_ref
                    class:sidebar-empty-drop-target=move || !right_visible() && tab_drag.get().is_some()
                    style=move || if right_visible() || (!layout_blocked.get() && tab_drag.get().is_some()) { "" } else { "display:none" }><DockZone position=SidebarSide::Right /></div>
                <div class="resize-handle resize-handle-v sidebar-resize-bottom" style=move || if bottom_visible() { "" } else { "display:none" } on:mousedown=on_bottom_handle></div>
                <div class="sidebar-bar sidebar-bar-bottom"
                    class:sidebar-empty-drop-target=move || !bottom_visible() && tab_drag.get().is_some()
                    style=move || if bottom_visible() || (!layout_blocked.get() && tab_drag.get().is_some()) { "" } else { "display:none" }><DockZone position=SidebarSide::Bottom /></div>
                <SidebarPanels />
                <div class="sidebar-layout-error" role="alert" style=move || if layout_blocked.get() { "" } else { "display:none" }>"Sidebar layout unavailable. Open Settings → Appearance → Reset sidebar layout to continue."</div>
            </div>

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

    async fn mode(container: &web_sys::HtmlElement, value: &str) {
        element(container, "[aria-label='Interface mode']").click();
        tick().await;
        let label = match value {
            "vibe" => "Vibe",
            "dinosaur" => "Dinosaur",
            _ => panic!("Unknown test interface mode"),
        };
        element(
            container,
            &format!("[role='menuitemradio'][aria-label='{label}']"),
        )
        .click();
    }

    fn press(element: &web_sys::HtmlElement, name: &str) {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(name);
        init.set_bubbles(true);
        init.set_cancelable(true);
        element
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                    .unwrap(),
            )
            .unwrap();
    }

    fn shell(container: &web_sys::HtmlElement, state: AppState) -> impl Sized {
        mount_to(container.clone(), move || {
            state.install_sidebar_persistence();
            provide_context(state.clone());
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <div style="height:720px;width:1100px;display:flex;flex-direction:column">
                    <crate::components::header::Header /><div class="app-body"><crate::components::project_rail::ProjectRail /><Workbench /></div>
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
        use crate::sidebar_layout::{ClientLayouts, STORAGE_KEY, SidebarSide, SidebarTab};
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
        let trigger = element(&container, "[aria-label='Interface mode']");
        assert_eq!(
            trigger.tag_name(),
            "BUTTON",
            "Mode choice uses the app toolbar, not a native select"
        );
        trigger.focus().unwrap();
        press(&trigger, "ArrowDown");
        tick().await;
        let chosen = element(&container, "[role='menuitemradio'][aria-checked='true']");
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&chosen))
        );
        press(&chosen, "End");
        tick().await;
        assert_eq!(
            document
                .active_element()
                .unwrap()
                .get_attribute("aria-label")
                .as_deref(),
            Some("Dinosaur")
        );
        press(&chosen, "Escape");
        tick().await;
        assert!(container.query_selector("[role='menu']").unwrap().is_none());
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&trigger))
        );
        trigger.click();
        tick().await;
        element(&container, ".header-mode-backdrop").click();
        tick().await;
        assert_eq!(
            trigger.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert_eq!(
            element(&container, "nav[aria-label='Projects']")
                .get_bounding_client_rect()
                .width(),
            0.0
        );
        let left = element(&container, ".sidebar-bar-left");
        assert!(
            left.get_bounding_client_rect().width() > 0.0,
            "A fresh Vibe layout opens its sidebar on the left"
        );
        assert!(left.text_content().unwrap().contains("Agents"));
        assert!(left.text_content().unwrap().contains("History"));
        assert!(
            element(&container, "section[aria-label='Agents panel']")
                .get_bounding_client_rect()
                .width()
                > 0.0
        );
        assert_eq!(
            element(&container, ".sidebar-bar-right")
                .get_bounding_client_rect()
                .width(),
            0.0,
            "A fresh Vibe layout leaves the right dock closed"
        );

        // Preserve an explicit older right-docked Vibe layout instead of
        // treating the new default as a migration for saved client choices.
        let mut saved = ClientLayouts::default();
        saved
            .vibe
            .move_tab(SidebarTab::Agents, SidebarSide::Right, None);
        saved
            .vibe
            .move_tab(SidebarTab::Sessions, SidebarSide::Right, None);
        saved.vibe.left.visible = false;
        saved.vibe.right.active = Some(SidebarTab::Agents);
        drop(handle);
        storage
            .set_item(STORAGE_KEY, &serde_json::to_string(&saved).unwrap())
            .unwrap();
        let handle = shell(&container, state.clone());
        tick().await;
        assert_eq!(
            element(&container, ".sidebar-bar-left")
                .get_bounding_client_rect()
                .width(),
            0.0,
            "An explicit saved Vibe layout keeps its right dock"
        );
        assert!(
            element(&container, ".sidebar-bar-right button[aria-label='Agents']")
                .get_bounding_client_rect()
                .width()
                > 0.0
        );
        let center_left = element(&container, ".sidebar-center")
            .get_bounding_client_rect()
            .left();
        assert_eq!(
            center_left,
            element(&container, ".workbench")
                .get_bounding_client_rect()
                .left(),
            "A saved empty left dock reserves no gutter"
        );
        // A version 1 saved empty left pane must remain closed after upgrade.
        let mut legacy: serde_json::Value =
            serde_json::from_str(&storage.get_item(STORAGE_KEY).unwrap().unwrap()).unwrap();
        legacy["version"] = serde_json::json!(1);
        legacy["vibe"]["left"]["visible"] = serde_json::json!(true);
        for mode in ["vibe", "dinosaur"] {
            legacy[mode].as_object_mut().unwrap().remove("bottom");
        }
        drop(handle);
        storage
            .set_item(STORAGE_KEY, &serde_json::to_string(&legacy).unwrap())
            .unwrap();
        let handle = shell(&container, state.clone());
        tick().await;
        assert_eq!(
            element(&container, ".sidebar-bar-left")
                .get_bounding_client_rect()
                .width(),
            0.0,
            "Upgrade must not open a formerly hidden default empty dock"
        );
        assert!(
            element(&container, "section[aria-label='Agents panel']")
                .get_bounding_client_rect()
                .width()
                > 0.0,
            "Upgrade preserves the existing populated right dock"
        );
        let upgraded: serde_json::Value =
            serde_json::from_str(&storage.get_item(STORAGE_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(upgraded["version"], serde_json::json!(2));
        for side in ["Left", "Bottom"] {
            let toggle = element(&container, &format!("[aria-label='Toggle {side} Dock']"));
            assert_eq!(
                toggle.get_attribute("aria-pressed").as_deref(),
                Some("false")
            );
            assert!(toggle.query_selector("svg").unwrap().is_some());
            toggle.click();
            tick().await;
            assert_eq!(
                toggle.get_attribute("aria-pressed").as_deref(),
                Some("true")
            );
            let empty = element(&container, &format!("[aria-label='{side} dock empty']"));
            assert!(empty.get_bounding_client_rect().width() > 0.0);
            assert!(empty.get_bounding_client_rect().height() > 0.0);
            assert_eq!(
                empty.text_content().unwrap(),
                "Drag a tab hereRight-click to add tabs"
            );
            toggle.click();
            tick().await;
            assert_eq!(empty.get_bounding_client_rect().height(), 0.0);
        }
        // Add through the same menu, then drag into an explicitly opened empty
        // dock. Removing the last tab must leave a usable placeholder.
        element(&container, "[aria-label='Toggle Bottom Dock']").click();
        tick().await;
        element(&container, "[aria-label='Bottom dock tab menu']").click();
        tick().await;
        let choice = element(&container, "[role='menuitemcheckbox'][aria-label='Search']");
        let rect = choice.get_bounding_client_rect();
        let hit = document
            .element_from_point(
                (rect.left() + 8.0) as f32,
                (rect.top() + rect.height() / 2.0) as f32,
            )
            .unwrap();
        assert!(
            choice.contains(Some(&hit)),
            "The empty dock menu must receive pointer input"
        );
        element(&container, "[role='menuitemcheckbox'][aria-label='Search']").click();
        press(
            &element(&container, "[role='menuitemcheckbox'][aria-label='Search']"),
            "Escape",
        );
        tick().await;
        let search_tab = element(
            &container,
            ".sidebar-bar-bottom button[aria-label='Search']",
        );
        assert!(
            element(&container, "section[aria-label='Search panel']")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        element(&container, "[aria-label='Toggle Left Dock']").click();
        tick().await;
        search_tab
            .dispatch_event(&web_sys::DragEvent::new("dragstart").unwrap())
            .unwrap();
        tick().await;
        element(&container, "[aria-label='Left dock empty']")
            .dispatch_event(&web_sys::DragEvent::new("drop").unwrap())
            .unwrap();
        tick().await;
        assert!(
            element(&container, ".sidebar-bar-left button[aria-label='Search']")
                .get_bounding_client_rect()
                .width()
                > 0.0
        );
        assert!(
            element(&container, "[aria-label='Bottom dock empty']")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        press(
            &element(&container, ".sidebar-bar-left .dock-tab-bar"),
            "ContextMenu",
        );
        tick().await;
        element(&container, "[role='menuitemcheckbox'][aria-label='Search']").click();
        press(
            &element(&container, "[role='menuitemcheckbox'][aria-label='Search']"),
            "Escape",
        );
        tick().await;
        assert_eq!(
            element(&container, "section[aria-label='Search panel']")
                .get_bounding_client_rect()
                .height(),
            0.0
        );
        assert!(
            element(&container, "[aria-label='Left dock empty']")
                .get_bounding_client_rect()
                .width()
                > 0.0
        );
        let context = web_sys::MouseEventInit::new();
        context.set_bubbles(true);
        context.set_cancelable(true);
        element(&container, "[aria-label='Left dock empty']")
            .dispatch_event(
                &web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &context)
                    .unwrap(),
            )
            .unwrap();
        tick().await;
        assert!(
            element(&container, "[role='menuitemcheckbox'][aria-label='Search']")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        press(
            &element(&container, "[role='menuitemcheckbox'][aria-label='Search']"),
            "Escape",
        );
        tick().await;
        element(&container, "[aria-label='Toggle Left Dock']").click();
        element(&container, "[aria-label='Toggle Bottom Dock']").click();
        tick().await;
        let tab = element(&container, ".sidebar-bar-right button[aria-label='Agents']");
        tab.dispatch_event(&web_sys::DragEvent::new("dragstart").unwrap())
            .unwrap();
        tick().await;
        assert!(
            element(&container, ".sidebar-bar-left")
                .get_bounding_client_rect()
                .width()
                > 0.0,
            "An empty-side drop target appears during drag"
        );
        assert_eq!(
            element(&container, ".sidebar-center")
                .get_bounding_client_rect()
                .left(),
            center_left,
            "The transient drop target must not displace chat"
        );
        tab.dispatch_event(&web_sys::DragEvent::new("dragend").unwrap())
            .unwrap();
        tick().await;
        assert_eq!(
            element(&container, ".sidebar-bar-left")
                .get_bounding_client_rect()
                .width(),
            0.0
        );
        assert_eq!(tab.get_attribute("draggable").as_deref(), Some("true"));
        assert_eq!(
            tab.parent_element().unwrap().text_content().unwrap().trim(),
            "Agents",
            "Whole tabs have no extra dot gutter or standalone move handle"
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
        mode(&container, "dinosaur").await;
        tick().await;
        assert!(
            element(&container, "nav[aria-label='Projects']")
                .get_bounding_client_rect()
                .width()
                > 0.0,
            "The developer rail remains available in Dinosaur"
        );
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
        // Reorder and transfer using the existing explicit keyboard controls.
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
        key(
            &element(&container, ".sidebar-bar-right button[aria-label='Agents']"),
            "ArrowLeft",
        );
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
        mode(&container, "vibe").await;
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
        // One canonical checklist works from a tab or blank strip, in both modes.
        let selections = state.sidebar_layout.with_untracked(|layouts| {
            (
                layouts.current().left.active,
                layouts.current().right.active,
            )
        });
        let strip = element(&container, ".sidebar-bar-right .dock-tab-bar");
        let context = web_sys::MouseEventInit::new();
        context.set_bubbles(true);
        context.set_cancelable(true);
        element(
            &container,
            ".sidebar-bar-right button[aria-label='History']",
        )
        .dispatch_event(
            &web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &context).unwrap(),
        )
        .unwrap();
        tick().await;
        let checklist = element(&container, "[role='menu'][aria-label='Sidebar tabs']");
        assert_eq!(
            checklist
                .query_selector_all("[role='menuitemcheckbox']")
                .unwrap()
                .length(),
            SidebarTab::ALL.len() as u32
        );
        for tab in SidebarTab::ALL {
            let choice = element(
                &container,
                &format!("[role='menuitemcheckbox'][aria-label='{}']", tab.label()),
            );
            assert_eq!(
                choice.get_attribute("aria-checked").as_deref(),
                Some(
                    if matches!(tab, SidebarTab::Agents | SidebarTab::Sessions) {
                        "true"
                    } else {
                        "false"
                    }
                )
            );
        }
        element(&container, "[role='menuitemcheckbox'][aria-label='Files']").click();
        element(&container, "[role='menuitemcheckbox'][aria-label='Git']").click();
        tick().await;
        assert_eq!(
            state.sidebar_layout.with_untracked(|layouts| (
                layouts.current().left.active,
                layouts.current().right.active
            )),
            selections,
            "Enabling tabs must not select them"
        );
        assert_eq!(
            state.sidebar_layout.get_untracked().mode,
            crate::sidebar_layout::InterfaceMode::Vibe
        );
        assert_eq!(
            state
                .center_zone
                .with_untracked(|center| (center.all_tab_ids(), center.active_tab_id())),
            tabs_before
        );
        assert_eq!(textarea.value(), "Unsent draft\nsecond line");
        let choice = element(
            &container,
            "[role='menuitemcheckbox'][aria-label='History']",
        );
        let rect = choice.get_bounding_client_rect();
        let hit = document
            .element_from_point(
                (rect.left() + 8.0) as f32,
                (rect.top() + rect.height() / 2.0) as f32,
            )
            .unwrap();
        assert!(
            choice.contains(Some(&hit)),
            "A populated dock must not cover its tab visibility menu"
        );
        let history_width = element(&container, "section[aria-label='History panel']")
            .get_bounding_client_rect()
            .width();
        element(
            &container,
            "[role='menuitemcheckbox'][aria-label='History']",
        )
        .click();
        tick().await;
        assert_eq!(
            element(&container, "section[aria-label='History panel']")
                .get_bounding_client_rect()
                .width(),
            history_width,
            "Hiding a tab retains the active view until explicit navigation"
        );
        assert_eq!(
            element(
                &container,
                "[role='menuitemcheckbox'][aria-label='History']"
            )
            .get_attribute("aria-checked")
            .as_deref(),
            Some("false")
        );
        element(
            &container,
            "[role='menuitemcheckbox'][aria-label='History']",
        )
        .click();
        tick().await;
        assert_eq!(
            state
                .sidebar_layout
                .with_untracked(|layouts| layouts.current().right.active),
            selections.1
        );
        assert_eq!(
            element(&container, "[role='menuitemcheckbox'][aria-label='Files']")
                .get_attribute("aria-checked")
                .as_deref(),
            Some("true")
        );
        press(
            &document
                .active_element()
                .unwrap()
                .dyn_into::<web_sys::HtmlElement>()
                .unwrap(),
            "End",
        );
        tick().await;
        assert_eq!(
            document
                .active_element()
                .unwrap()
                .get_attribute("aria-label")
                .as_deref(),
            Some("Terminal")
        );
        press(
            &document
                .active_element()
                .unwrap()
                .dyn_into::<web_sys::HtmlElement>()
                .unwrap(),
            "Escape",
        );
        tick().await;
        strip
            .dispatch_event(
                &web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &context)
                    .unwrap(),
            )
            .unwrap();
        tick().await;
        press(
            &element(&container, "[role='menuitemcheckbox'][aria-label='Agents']"),
            "Escape",
        );
        tick().await;
        press(&strip, "ContextMenu");
        tick().await;
        for label in ["Files", "Git", "History"] {
            element(
                &container,
                &format!("[role='menuitemcheckbox'][aria-label='{label}']"),
            )
            .click();
        }
        tick().await;
        assert_eq!(
            element(&container, "section[aria-label='History panel']")
                .get_bounding_client_rect()
                .width(),
            0.0,
            "Removing the final tab shows the empty dock, not an unlisted panel"
        );
        assert!(
            strip.get_bounding_client_rect().width() > 0.0,
            "The empty dock keeps its usable checklist anchor"
        );
        press(
            &element(&container, "[role='menuitemcheckbox'][aria-label='Agents']"),
            "Escape",
        );
        tick().await;
        press(&strip, "ContextMenu");
        tick().await;
        assert!(
            container
                .query_selector("[role='menu'][aria-label='Sidebar tabs']")
                .unwrap()
                .is_some()
        );
        for label in ["Files", "Git", "History"] {
            let choice = element(
                &container,
                &format!("[role='menuitemcheckbox'][aria-label='{label}']"),
            );
            assert_eq!(
                choice.get_attribute("aria-checked").as_deref(),
                Some("false")
            );
            choice.click();
        }
        tick().await;
        press(
            &element(&container, "[role='menuitemcheckbox'][aria-label='Agents']"),
            "Escape",
        );
        tick().await;
        mode(&container, "dinosaur").await;
        tick().await;
        press(
            &element(&container, ".sidebar-bar-left .dock-tab-bar"),
            "ContextMenu",
        );
        tick().await;
        assert_eq!(
            element(&container, "[role='menu'][aria-label='Sidebar tabs']")
                .query_selector_all("[role='menuitemcheckbox']")
                .unwrap()
                .length(),
            SidebarTab::ALL.len() as u32
        );
        press(
            &element(&container, "[role='menuitemcheckbox'][aria-label='Agents']"),
            "Escape",
        );
        tick().await;
        mode(&container, "vibe").await;
        tick().await;
        for label in ["Files", "Git"] {
            assert!(
                container
                    .query_selector(&format!(
                        ".sidebar-bar-right button.dock-tab[aria-label='{label}']"
                    ))
                    .unwrap()
                    .is_some()
            );
        }
        // The persisted mode, order, active choices, visibility and widths
        // restore in a fresh component tree, not just in an in-memory mirror.
        state.sidebar_layout.update(|layouts| {
            layouts.current_mut().left.width = 280.0;
            layouts.current_mut().bottom.visible = true;
            layouts.current_mut().right.visible = false;
        });
        tick().await;
        drop(handle);
        let restored = AppState::new();
        let handle = shell(&container, restored.clone());
        tick().await;
        assert_eq!(
            element(&container, "[aria-label='Interface mode']")
                .text_content()
                .unwrap()
                .trim()
                .to_ascii_lowercase(),
            "vibe"
        );
        assert_eq!(
            element(&container, ".sidebar-bar-left button.dock-tab")
                .text_content()
                .unwrap(),
            "Agents"
        );
        for label in ["Files", "Git"] {
            assert!(
                container
                    .query_selector(&format!(
                        ".sidebar-bar-right button.dock-tab[aria-label='{label}']"
                    ))
                    .unwrap()
                    .is_some(),
                "Custom Vibe tabs survive remount/storage restore"
            );
        }
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
        mode(&container, "dinosaur").await;
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
        assert_eq!(
            element(&container, "[aria-label='Toggle Bottom Dock']")
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("false"),
            "Dinosaur has an independent bottom layout"
        );
        mode(&container, "vibe").await;
        tick().await;
        assert!(
            element(&container, "[aria-label='Bottom dock empty']")
                .get_bounding_client_rect()
                .height()
                > 0.0,
            "Explicitly open empty dock survives storage restore and mode switching"
        );
        mode(&container, "dinosaur").await;
        tick().await;
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
                .text_content()
                .unwrap()
                .trim()
                .to_ascii_lowercase(),
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
        mode(&container, "vibe").await;
        tick().await;
        drop(handle);
        let handle = shell(&container, AppState::new());
        tick().await;
        assert_eq!(
            element(&container, "[aria-label='Interface mode']")
                .text_content()
                .unwrap()
                .trim()
                .to_ascii_lowercase(),
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
        invalid.sidebar_layout.update(|layouts| {
            layouts
                .dinosaur
                .move_tab(SidebarTab::Agents, SidebarSide::Left, None);
        });
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
            element(&container, ".sidebar-bar-left button[aria-label='Agents']")
                .get_bounding_client_rect()
                .width()
                > 0.0,
            "Reset restores the Vibe sidebar on the left"
        );
        mode(&container, "dinosaur").await;
        tick().await;
        assert!(
            element(&container, ".sidebar-bar-left button[aria-label='Agents']")
                .get_bounding_client_rect()
                .width()
                > 0.0,
            "Resetting Vibe leaves the customized Dinosaur dock unchanged"
        );
        assert!(
            element(&container, ".sidebar-bar-right")
                .text_content()
                .unwrap()
                .contains("History")
        );
        mode(&container, "vibe").await;
        tick().await;
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
