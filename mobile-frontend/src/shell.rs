use std::{cell::RefCell, rc::Rc};

use leptos::prelude::*;
use wasm_bindgen::{JsCast, prelude::*};

#[wasm_bindgen(module = "/vendor/web-shell/shell.js")]
extern "C" {
    type ShellHandle;

    #[wasm_bindgen(catch, js_name = attachShell)]
    fn attach_shell(root: &web_sys::HtmlElement, options: &JsValue)
    -> Result<ShellHandle, JsValue>;

    #[wasm_bindgen(method)]
    fn destroy(this: &ShellHandle);

    #[wasm_bindgen(method)]
    fn refresh(this: &ShellHandle);

    #[wasm_bindgen(catch, js_name = attachTextarea)]
    fn attach_textarea(
        textarea: &web_sys::HtmlTextAreaElement,
    ) -> Result<js_sys::Function, JsValue>;
}

struct MountedShell {
    handle: ShellHandle,
    regions: [web_sys::Element; 3],
}

impl Drop for MountedShell {
    fn drop(&mut self) {
        self.handle.destroy();
    }
}

fn region(root: &web_sys::HtmlElement, selector: &str) -> Result<web_sys::Element, JsValue> {
    root.query_selector(selector)?
        .ok_or_else(|| JsValue::from_str(&format!("Mobile shell missing {selector}")))
}

fn bind_regions(
    root: &web_sys::HtmlElement,
    mounted: &RefCell<Option<MountedShell>>,
) -> Result<(), JsValue> {
    let active_root = if crate::native::is_native()
        && root.query_selector(".native-workspace")?.is_some()
    {
        Some(region(root, "[data-native-shell-active]")?.unchecked_into::<web_sys::HtmlElement>())
    } else {
        None
    };
    let region_root = active_root.as_ref().unwrap_or(root);
    let content = region(region_root, ".chat-messages, .view-body, .home-view")?;
    let header = region_root
        .query_selector(".shell-header, .chat-header, .view-header:not(.shell-header > *)")?
        .unwrap_or(region(root, ".shell-empty-header")?);
    let bottom = region_root
        .query_selector(".chat-bottom-dock, .shell-bottom")?
        .unwrap_or(region(root, ".shell-empty-bottom")?);
    let flow = content
        .query_selector(":scope > .shell-flow")?
        .ok_or_else(|| JsValue::from_str("Mobile shell missing content flow"))?;
    let regions = [header, content, bottom];
    if mounted
        .borrow()
        .as_ref()
        .is_some_and(|shell| shell.regions == regions)
    {
        return Ok(());
    }
    // Navigation replaces regions; keyboard changes never replace their DOM.
    mounted.borrow_mut().take();
    for (element, class) in regions
        .iter()
        .zip(["tws-header", "tws-content", "tws-bottom"])
    {
        element.class_list().add_1(class)?;
    }
    let options = js_sys::Object::new();
    let bindings = js_sys::Object::new();
    for (key, element) in [
        ("header", &regions[0]),
        ("content", &regions[1]),
        ("bottom", &regions[2]),
        ("flow", &flow),
    ] {
        js_sys::Reflect::set(&bindings, &key.into(), element)?;
    }
    js_sys::Reflect::set(&options, &"regions".into(), &bindings)?;
    js_sys::Reflect::set(&options, &"followEnd".into(), &false.into())?;
    // A focus retained across background lets iOS standalone WebKit return
    // with keyboard geometry for a keyboard that is no longer shown.
    js_sys::Reflect::set(&options, &"endEditingOnHidden".into(), &true.into())?;
    let handle = attach_shell(root, &options)?;
    log::debug!(
        "Shared mobile shell attached to mounted regions; followEnd=false endEditingOnHidden=true"
    );
    *mounted.borrow_mut() = Some(MountedShell { handle, regions });
    Ok(())
}

fn observe_native_surfaces(
    root: &web_sys::HtmlElement,
    observer: &web_sys::ResizeObserver,
) -> Result<(), JsValue> {
    observer.disconnect();
    observer.observe(root);
    let regions = root.query_selector_all(".native-pane .chat-header, .native-pane .chat-messages, .native-pane .chat-bottom-dock, .native-pane .shell-flow, .native-pane .view-header, .native-pane .view-body, .native-pane .home-view")?;
    for index in 0..regions.length() {
        let region = regions
            .item(index)
            .ok_or("Missing native region")?
            .dyn_into::<web_sys::Element>()?;
        observer.observe(&region);
    }
    Ok(())
}

pub fn synchronize_native_surfaces(root: &web_sys::HtmlElement) -> Result<(), JsValue> {
    let surfaces = root.query_selector_all(
        ".native-workspace > [data-native-shell-active], [data-native-back-destination]",
    )?;
    for index in 0..surfaces.length() {
        let surface = surfaces
            .item(index)
            .ok_or("Missing native surface")?
            .dyn_into::<web_sys::HtmlElement>()?;
        let content = region(&surface, ".chat-messages, .view-body, .home-view")?;
        let header = surface
            .query_selector(".shell-header, .chat-header, .view-header:not(.shell-header > *)")?;
        let bottom = if surface.has_attribute("data-native-parent-conversation") {
            let owner = surface
                .parent_element()
                .ok_or("Missing shared editor owner")?;
            Some(
                owner
                    .query_selector("[data-mobile-test='chat-bottom-dock']")?
                    .ok_or("Missing shared editor region")?,
            )
        } else {
            surface.query_selector(".chat-bottom-dock, .shell-bottom")?
        };
        let pane = content.get_bounding_client_rect();
        let mut title_hidden = false;
        let mut clear_top = 12.0;
        let mut clear_bottom = 12.0;
        if let Some(foot) = &bottom {
            let bounds = foot.get_bounding_client_rect();
            if bounds.height() > 0.0 {
                clear_bottom = (pane.bottom() - bounds.top()).max(0.0) + 12.0;
            }
            if let Some(head) = &header {
                let head_bounds = head.get_bounding_client_rect();
                if let Some(composer) = foot.query_selector(":scope > .tws-composer")? {
                    let height = composer.get_bounding_client_rect().height();
                    let window = root
                        .owner_document()
                        .and_then(|document| document.default_view())
                        .ok_or("Missing native layout window")?;
                    let style = window
                        .get_computed_style(foot)?
                        .ok_or("Missing native editor style")?;
                    let margin = style
                        .get_property_value("margin-bottom")?
                        .trim_end_matches("px")
                        .parse::<f64>()
                        .map_err(|_| JsValue::from_str("Invalid native editor margin"))?;
                    let root_bounds = root.get_bounding_client_rect();
                    title_hidden = head_bounds.height() > 0.0
                        && height > 0.0
                        && head_bounds.bottom() - root_bounds.top() + height + margin + 4.0
                            > root_bounds.height();
                }
            }
        }
        if let Some(head) = &header {
            let bounds = head.get_bounding_client_rect();
            if title_hidden {
                clear_top = 0.0;
            } else if bounds.height() > 0.0 {
                clear_top = (bounds.bottom() - pane.top()).max(0.0) + 12.0;
            }
            head.unchecked_ref::<web_sys::HtmlElement>()
                .style()
                .set_property(
                    "visibility",
                    if title_hidden { "hidden" } else { "visible" },
                )?;
        }
        surface
            .style()
            .set_property("--tws-clear-top", &format!("{clear_top}px"))?;
        surface
            .style()
            .set_property("--tws-clear-bottom", &format!("{clear_bottom}px"))?;
        if surface.has_attribute("data-native-parent-conversation") {
            let scroller = content.unchecked_ref::<web_sys::HtmlElement>();
            scroller.set_scroll_top(scroller.scroll_height());
        }
    }
    Ok(())
}

pub fn install(root: NodeRef<leptos::html::Div>) {
    Effect::new(move |_| {
        let Some(root) = root.get() else { return };
        let root: web_sys::HtmlElement = (*root).clone();
        // WebKit iOS (`RemoteLayerTreeViews.mm` `touchActionsForPoint`) returns
        // `TouchAction::Auto` inside `WKChildScrollView` (`overflow-y: auto`
        // regions like `.chat-messages` and `.view-body`), bypassing
        // `WKTouchActionGestureRecognizer`. WebKit's second double-tap zoom
        // gate (`WebPage::potentialTapAtPosition` ->
        // `DisableDoubleTapGesturesDuringTapIfNecessary`) checks
        // `Element::allowsDoubleTapGesture()` (`!touchAction().isAuto()`) only
        // when `nodeRespondingToClickEvents` finds a DOM ancestor with a direct
        // `click` listener (Leptos delegates `on:click` to `window`, not the
        // element). Installing a direct no-op `onclick` on `.mobile-app`
        // ensures every tap inside child scroll views resolves `.mobile-app`
        // (or a closer interactive descendant) with `touch-action: manipulation`
        // and suppresses double-tap page zoom.
        let click_noop = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(|_| {});
        root.set_onclick(Some(click_noop.as_ref().unchecked_ref()));
        let cleanup_root = root.clone();
        let mounted = Rc::new(RefCell::new(None));
        let native_geometry = if crate::native::is_native() {
            let target = root.clone();
            let state = expect_context::<crate::state::AppState>();
            let callback = Closure::<dyn FnMut(js_sys::Array, web_sys::ResizeObserver)>::new(
                move |_, _: web_sys::ResizeObserver| {
                    if synchronize_native_surfaces(&target).is_err() {
                        state
                            .mobile_shell_error
                            .set(Some(crate::state::MobileShellError {
                                code: protocol::MobileAccessErrorCode::Internal,
                                message: "Native surface geometry could not synchronize".into(),
                            }));
                    }
                },
            );
            match web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) {
                Ok(observer) => {
                    observer.observe(&root);
                    Some((observer, callback))
                }
                Err(error) => {
                    log::error!("Native surface observer could not start");
                    expect_context::<crate::state::AppState>()
                        .mobile_shell_error
                        .set(Some(crate::state::MobileShellError {
                            code: protocol::MobileAccessErrorCode::Internal,
                            message: "Native surface geometry is unavailable".into(),
                        }));
                    log::debug!("Native surface observer setup failed: {error:?}");
                    None
                }
            }
        } else {
            None
        };
        let mutation_state =
            crate::native::is_native().then(expect_context::<crate::state::AppState>);
        let surface_observer = native_geometry
            .as_ref()
            .map(|(observer, _)| observer.clone());
        let target = root.clone();
        let owner = mounted.clone();
        let callback = Closure::<dyn FnMut()>::new(move || {
            if let Some(observer) = &surface_observer
                && observe_native_surfaces(&target, observer).is_err()
                && let Some(state) = &mutation_state
            {
                state
                    .mobile_shell_error
                    .set(Some(crate::state::MobileShellError {
                        code: protocol::MobileAccessErrorCode::Internal,
                        message: "Native surface geometry could not synchronize".into(),
                    }));
            }
            if let Err(error) = bind_regions(&target, &owner) {
                log::error!("Shared mobile shell could not bind regions: {error:?}");
            }
        });
        let observer = web_sys::MutationObserver::new(callback.as_ref().unchecked_ref())
            .expect("mobile shell navigation observer");
        let options = web_sys::MutationObserverInit::new();
        options.set_child_list(true);
        options.set_subtree(true);
        options.set_attributes(true);
        options.set_attribute_filter(&js_sys::Array::of3(
            &"hidden".into(),
            &"data-native-shell-active".into(),
            &"data-native-back-destination".into(),
        ));
        observer
            .observe_with_options(&root, &options)
            .expect("observe mobile shell");
        let style_owner = mounted.clone();
        let style_callback =
            Closure::<dyn FnMut(js_sys::Array)>::new(move |records: js_sys::Array| {
                let stylesheet = |node: &web_sys::Node| {
                    node.node_name().eq_ignore_ascii_case("style")
                        || node.node_name().eq_ignore_ascii_case("link")
                };
                let changed = records.iter().any(|record| {
                    let record: web_sys::MutationRecord = record.unchecked_into();
                    if record.target().is_some_and(|target| {
                        stylesheet(&target)
                            || target
                                .parent_element()
                                .is_some_and(|parent| stylesheet(parent.unchecked_ref()))
                    }) {
                        return true;
                    }
                    [record.added_nodes(), record.removed_nodes()]
                        .iter()
                        .any(|nodes| {
                            (0..nodes.length())
                                .filter_map(|index| nodes.item(index))
                                .any(|node| stylesheet(&node))
                        })
                });
                if changed && let Some(shell) = style_owner.borrow().as_ref() {
                    log::debug!("Shared mobile shell refreshed after stylesheet mutation");
                    shell.handle.refresh();
                }
            });
        let style_observer =
            web_sys::MutationObserver::new(style_callback.as_ref().unchecked_ref())
                .expect("mobile shell stylesheet observer");
        if let Some(document) = root.owner_document() {
            // Insets can change without resizing any of the shell's bound regions.
            let style_options = web_sys::MutationObserverInit::new();
            style_options.set_child_list(true);
            style_options.set_subtree(true);
            style_options.set_character_data(true);
            style_options.set_attributes(true);
            style_options.set_attribute_filter(&js_sys::Array::of4(
                &"href".into(),
                &"disabled".into(),
                &"media".into(),
                &"rel".into(),
            ));
            style_observer
                .observe_with_options(&document, &style_options)
                .expect("observe mobile shell stylesheets");
        }
        if let Err(error) = bind_regions(&root, &mounted) {
            log::error!("Shared mobile shell could not mount: {error:?}");
        }
        let held = send_wrapper::SendWrapper::new((
            cleanup_root,
            click_noop,
            observer,
            callback,
            style_observer,
            style_callback,
            mounted,
            native_geometry,
        ));
        on_cleanup(move || {
            let (
                cleanup_root,
                click_noop,
                observer,
                callback,
                style_observer,
                style_callback,
                mounted,
                native_geometry,
            ) = held.take();
            if let Some((observer, callback)) = native_geometry {
                observer.disconnect();
                drop(callback);
            }
            cleanup_root.set_onclick(None);
            observer.disconnect();
            style_observer.disconnect();
            drop(click_noop);
            drop(callback);
            drop(style_callback);
            mounted.borrow_mut().take();
        });
    });
}

pub fn install_textarea(textarea: NodeRef<leptos::html::Textarea>) {
    Effect::new(move |_| {
        let Some(textarea) = textarea.get() else {
            return;
        };
        match attach_textarea(&textarea) {
            Ok(cleanup) => {
                let cleanup = send_wrapper::SendWrapper::new(cleanup);
                on_cleanup(move || {
                    let _ = cleanup.take().call0(&JsValue::UNDEFINED);
                });
            }
            Err(error) => log::error!("Shared textarea sizing could not mount: {error:?}"),
        }
    });
}
