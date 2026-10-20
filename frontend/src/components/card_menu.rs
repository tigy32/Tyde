//! The right-click menu shared by the sidebar's agent and saved-session rows.
//!
//! The rows are one line each and carry no hover text: everything they used to
//! reveal on hover is now either in the row's expanded details or in this menu.
//! Both surfaces offer the same actions, because right-click is not a
//! discoverable affordance on its own.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// A point in the viewport.
type Point = (f64, f64);

/// Where a card menu is open, in viewport coordinates. `None` is closed.
pub(crate) type CardMenuPosition = RwSignal<Option<Point>>;

/// A menu position clamped into the viewport, tagged with the raw point it was
/// computed from.
type PlacedMenu = Option<(Point, Point)>;

/// Open the menu where the pointer is, and keep the browser's own menu out of
/// the way.
pub(crate) fn open_card_menu(menu: CardMenuPosition, ev: &web_sys::MouseEvent) {
    ev.prevent_default();
    ev.stop_propagation();
    menu.set(Some((ev.client_x() as f64, ev.client_y() as f64)));
}

/// Renders `children` as menu items at the pointer, with a backdrop that
/// dismisses on any press outside. Nothing renders while `menu` is `None`.
///
/// `anchor` is the row the menu belongs to: closing the menu puts focus back
/// there, so a keyboard user who opens it is not dropped at the top of the
/// document.
#[component]
pub(crate) fn CardContextMenu(
    menu: CardMenuPosition,
    #[prop(into)] label: String,
    anchor: NodeRef<leptos::html::Div>,
    children: ChildrenFn,
) -> impl IntoView {
    let menu_ref = NodeRef::<leptos::html::Div>::new();
    // The clamped position, tagged with the raw position it was computed from.
    // Without the tag, reopening the menu somewhere else would paint one frame
    // at the previous spot.
    let placed: RwSignal<PlacedMenu> = RwSignal::new(None);

    // Keep the menu inside the viewport: a row near the bottom of a tall
    // sidebar would otherwise open a menu whose last items are unreachable.
    Effect::new(move |_| {
        let Some(requested) = menu.get() else {
            placed.set(None);
            return;
        };
        let Some(element) = menu_ref.get() else {
            return;
        };
        let Some(window) = web_sys::window() else {
            return;
        };
        let (Some(view_width), Some(view_height)) = (
            window.inner_width().ok().and_then(|value| value.as_f64()),
            window.inner_height().ok().and_then(|value| value.as_f64()),
        ) else {
            return;
        };
        const MARGIN: f64 = 8.0;
        let rect = element.get_bounding_client_rect();
        let (x, y) = requested;
        let left = if x + rect.width() + MARGIN > view_width {
            (view_width - rect.width() - MARGIN).max(MARGIN)
        } else {
            x
        };
        let top = if y + rect.height() + MARGIN > view_height {
            (view_height - rect.height() - MARGIN).max(MARGIN)
        } else {
            y
        };
        placed.set(Some((requested, (left, top))));
    });

    // The menu takes focus so Escape reaches it and so a keyboard-opened menu
    // is where the keyboard already is.
    Effect::new(move |_| {
        if menu.get().is_some()
            && let Some(element) = menu_ref.get()
        {
            match element.query_selector("button[role^='menuitem']:not(:disabled)") {
                Ok(Some(item)) => {
                    if let Some(item) = item.dyn_ref::<web_sys::HtmlElement>() {
                        let _ = item.focus();
                    }
                }
                _ => {
                    let _ = element.focus();
                }
            }
        }
    });

    let close = move || {
        menu.set(None);
        if let Some(card) = anchor.get_untracked() {
            let _ = card.focus();
        }
    };

    let style = move || {
        let Some(requested) = menu.get() else {
            return String::new();
        };
        let (left, top) = match placed.get() {
            Some((from, placed)) if from == requested => placed,
            _ => requested,
        };
        format!("left: {left}px; top: {top}px;")
    };

    view! {
        <Show when=move || menu.get().is_some()>
            <div
                class="card-menu-backdrop"
                on:mousedown=move |_| close()
                on:contextmenu=move |ev: web_sys::MouseEvent| {
                    ev.prevent_default();
                    close();
                }
            />
            <div
                class="context-menu sidebar-card-menu"
                role="menu"
                aria-label=label.clone()
                tabindex="-1"
                node_ref=menu_ref
                style=style
                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    ev.stop_propagation();
                    match ev.key().as_str() {
                        "Escape" => { ev.prevent_default(); close(); },
                        "Tab" => close(),
                        "ArrowDown" | "ArrowUp" | "Home" | "End" => {
                            ev.prevent_default();
                            let Some(element) = menu_ref.get_untracked() else { return; };
                            let Ok(items) = element.query_selector_all("button[role^='menuitem']:not(:disabled)") else { return; };
                            if items.length() == 0 { return; }
                            let focused = web_sys::window().and_then(|window| window.document()).and_then(|document| document.active_element());
                            let current = (0..items.length()).find(|index| items.item(*index).as_ref().is_some_and(|item| focused.as_ref().is_some_and(|focused| item.is_same_node(Some(focused)))));
                            let next = match ev.key().as_str() {
                                "Home" => 0,
                                "End" => items.length() - 1,
                                "ArrowUp" => current.map_or(items.length() - 1, |index| (index + items.length() - 1) % items.length()),
                                _ => current.map_or(0, |index| (index + 1) % items.length()),
                            };
                            if let Some(item) = items.item(next).and_then(|item| item.dyn_into::<web_sys::HtmlElement>().ok()) { let _ = item.focus(); }
                        },
                        _ => {},
                    }
                }
            >
                {children()}
            </div>
        </Show>
    }
}
