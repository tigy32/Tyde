pub mod actions;
mod app;
mod bridge;
mod components;
mod dispatch;
#[cfg(all(feature = "ui-fixtures", debug_assertions))]
mod fixtures;
mod markdown;
mod push;
mod send;
pub mod state;
mod voice;

use wasm_bindgen::JsCast;

fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Debug);

    let Some(root) = app_root() else {
        show_boot_error("Tyde Mobile could not mount: #app-root is missing");
        return;
    };

    install_app_height_probe();

    #[cfg(all(feature = "ui-fixtures", debug_assertions))]
    if fixtures::is_requested() {
        leptos::mount::mount_to(root, app::FixtureApp).forget();
        remove_boot_screen();
        fixtures::mark_ready();
        return;
    }

    leptos::mount::mount_to(root, app::App).forget();
    remove_boot_screen();

    wasm_bindgen_futures::spawn_local(async {
        crate::bridge::wasm_log(
            "info",
            &format!(
                "Tyde mobile WASM mounted visible shell; {}",
                viewport_metrics()
            ),
        )
        .await;
    });
}

/// Sizes the app shell to the region the user can actually see. Two unrelated
/// viewport defects meet here, and they pull the same measurement in opposite
/// directions:
///
/// 1. Home-screen (standalone) launches can lay out `100dvh` SHORTER than the
///    real visible viewport, stranding the bottom nav above a dead band from
///    the moment the app opens. The measurement has to *raise* the shell.
///
/// 2. When the software keyboard opens, iOS WebKit does NOT shrink the layout
///    viewport. `interactive-widget=resizes-content` in our viewport meta is a
///    Chromium-only mitigation; WebKit implements the default `resizes-visual`,
///    so `100dvh` stays at its full-screen value and only the visual viewport
///    shrinks. A shell floored at `100dvh` therefore keeps the composer *under*
///    the keyboard, and WebKit pans the whole page up to reveal the caret —
///    the app scrolls off screen. The measurement has to *lower* the shell.
///
/// A plain `max()` serves 1 and breaks 2; a plain `min()` does the reverse. So
/// we do not classify by the height itself but by its DELTA: track the tallest
/// visual viewport seen at the current width (the baseline) and treat a drop of
/// more than `KEYBOARD_MIN_INSET_PX` as the keyboard. A short but *stable*
/// reading — the bogus standalone-launch measurement — establishes the baseline
/// instead of looking like a keyboard, so case 1 still resolves through the
/// `max()` floor in CSS and only case 2 shrinks the shell.
///
/// This must run from the app (not the bundle's index.html): the production
/// loader injects only stylesheet links and the entry script, so inline markup
/// never reaches the phone.
fn install_app_height_probe() {
    let Some(window) = web_sys::window() else {
        return;
    };
    std::mem::forget(listen_for_app_height(&window));
}

fn schedule_app_height_frame(
    window: &web_sys::Window,
    pending: &std::cell::Cell<Option<i32>>,
    callback: &wasm_bindgen::closure::Closure<dyn FnMut()>,
) {
    if pending.get().is_none() {
        pending.set(
            window
                .request_animation_frame(callback.as_ref().unchecked_ref())
                .ok(),
        );
    }
}

fn listen_for_app_height(window: &web_sys::Window) -> impl FnOnce() {
    apply_app_height(window);
    let pending_frame = std::rc::Rc::new(std::cell::Cell::new(None));
    let frame_pending = pending_frame.clone();
    let frame_target = window.clone();
    let on_frame = std::rc::Rc::new(wasm_bindgen::closure::Closure::<dyn FnMut()>::new(
        move || {
            frame_pending.set(None);
            apply_app_height(&frame_target);
            log::debug!("Mobile viewport animation frame; {}", viewport_metrics());
        },
    ));
    // Match Tychat's bounded settling window: Home Screen keyboard geometry
    // can finish changing after both its resize event and the first frame.
    let timers = std::rc::Rc::new([50, 150, 300, 600].map(|delay| {
        let handle = std::rc::Rc::new(std::cell::Cell::new(None::<i32>));
        let timer_handle = handle.clone();
        let timer_target = window.clone();
        let timer_pending = pending_frame.clone();
        let timer_frame = on_frame.clone();
        let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            timer_handle.set(None);
            apply_app_height(&timer_target);
            log::debug!("Mobile viewport settle {delay}ms; {}", viewport_metrics());
            schedule_app_height_frame(&timer_target, &timer_pending, &timer_frame);
        });
        (delay, handle, callback)
    }));
    let resize_target = window.clone();
    let resize_pending = pending_frame.clone();
    let resize_timers = timers.clone();
    let on_resize = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(
        move |event: web_sys::Event| {
            apply_app_height(&resize_target);
            log::debug!("Mobile viewport {}; {}", event.type_(), viewport_metrics());
            schedule_app_height_frame(&resize_target, &resize_pending, &on_frame);
            for (delay, handle, callback) in resize_timers.iter() {
                if let Some(previous) = handle.take() {
                    resize_target.clear_timeout_with_handle(previous);
                }
                handle.set(
                    resize_target
                        .set_timeout_with_callback_and_timeout_and_arguments_0(
                            callback.as_ref().unchecked_ref(),
                            *delay,
                        )
                        .ok(),
                );
            }
        },
    );
    let mut targets: Vec<(web_sys::EventTarget, &str)> = vec![
        (window.clone().into(), "orientationchange"),
        (window.clone().into(), "resize"),
    ];
    if let Some(viewport) = window.visual_viewport() {
        targets.push((viewport.clone().into(), "resize"));
        targets.push((viewport.into(), "scroll"));
    }
    if let Some(document) = window.document() {
        for event in ["focusin", "focusout", "visibilitychange"] {
            targets.push((document.clone().into(), event));
        }
    }
    for (target, event) in &targets {
        let _ = target.add_event_listener_with_callback(event, on_resize.as_ref().unchecked_ref());
    }
    let cleanup_window = window.clone();
    move || {
        if let Some(frame) = pending_frame.take() {
            let _ = cleanup_window.cancel_animation_frame(frame);
        }
        for (_, handle, _) in timers.iter() {
            if let Some(timer) = handle.take() {
                cleanup_window.clear_timeout_with_handle(timer);
            }
        }
        for (target, event) in targets {
            let _ = target
                .remove_event_listener_with_callback(event, on_resize.as_ref().unchecked_ref());
        }
    }
}

/// A visual-viewport drop smaller than this is browser chrome — the iOS URL bar
/// is roughly 50-90px — not a keyboard. Every software keyboard is far taller
/// (~300px+), so this cleanly separates the two without measuring the keyboard.
const KEYBOARD_MIN_INSET_PX: f64 = 100.0;

thread_local! {
    /// `(width, tallest height seen at that width)`.
    static VIEWPORT_BASELINE: std::cell::Cell<(f64, f64)> =
        const { std::cell::Cell::new((0.0, 0.0)) };
}

/// The tallest visual viewport seen at this width. A width change (rotation) or
/// the absence of any prior baseline restarts it: heights are not comparable
/// across orientations, and carrying a landscape baseline into portrait would
/// read as a permanently open keyboard.
fn next_baseline(previous: (f64, f64), width: f64, height: f64) -> f64 {
    let (previous_width, previous_baseline) = previous;
    if previous_baseline <= 0.0 || (previous_width - width).abs() > 1.0 {
        height
    } else {
        previous_baseline.max(height)
    }
}

fn keyboard_is_open(baseline: f64, height: f64) -> bool {
    baseline - height > KEYBOARD_MIN_INSET_PX
}

/// The largest viewport shortfall we are willing to blame on the standalone
/// launch defect. The insets iOS can lose this way are a status bar (~59px)
/// and a home indicator (~34px); a bigger gap is a real one — browser chrome,
/// a split screen, an in-app banner — and has to be believed, or the shell
/// would run its bottom chrome underneath whatever owns that space.
const MAX_STANDALONE_SHORTFALL_PX: f64 = 120.0;

/// The screen edge the viewport should reach. iOS has reported `screen.width`
/// and `screen.height` orientation-independently across versions, so the axis
/// is chosen by the viewport's own orientation rather than by which field is
/// which.
fn screen_height_for(portrait: bool, screen_width: f64, screen_height: f64) -> f64 {
    if portrait {
        screen_width.max(screen_height)
    } else {
        screen_width.min(screen_height)
    }
}

/// A standalone (home-screen) launch can lay out BOTH `100dvh` and the visual
/// viewport short of the screen the app actually fills — `env(safe-area-inset-*)`
/// still reports the true insets, so the shell pads for a status bar it was
/// never given room for and every bottom-anchored capsule floats that far above
/// the screen edge, over a dead band of page background.
///
/// The screen is the only measurement left that is not short, so a shortfall
/// small enough to be an inset is recovered from it. Everything else — a
/// browser with chrome, a keyboard, a genuinely small window — keeps the
/// measurement, which is why the caller applies this only in standalone mode
/// with the keyboard closed.
fn recovered_standalone_height(
    measured: f64,
    viewport_width: f64,
    screen_width: f64,
    screen_height: f64,
) -> f64 {
    let expected = screen_height_for(viewport_width <= measured, screen_width, screen_height);
    let shortfall = expected - measured;
    if shortfall > 0.5 && shortfall <= MAX_STANDALONE_SHORTFALL_PX {
        expected
    } else {
        measured
    }
}

/// Whether the app is running as an installed app rather than in a browser tab.
/// iOS exposes the legacy `navigator.standalone`; everyone else answers the
/// `display-mode` media query, which iOS 26 also honours.
fn is_standalone_display(window: &web_sys::Window) -> bool {
    let ios_standalone = js_sys::Reflect::get(window.navigator().as_ref(), &"standalone".into())
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    ios_standalone
        || window
            .match_media("(display-mode: standalone)")
            .ok()
            .flatten()
            .is_some_and(|query| query.matches())
}

/// The screen size in CSS pixels, when the browser reports a usable one.
fn screen_size(window: &web_sys::Window) -> Option<(f64, f64)> {
    let screen = window.screen().ok()?;
    let width = f64::from(screen.width().ok()?);
    let height = f64::from(screen.height().ok()?);
    (usable(&width) && usable(&height)).then_some((width, height))
}

fn usable(value: &f64) -> bool {
    value.is_finite() && *value > 0.0
}

/// The tallest shell the root element can actually paint. `html` carries
/// `overflow: hidden`, and CSS propagates a root overflow to the viewport, so
/// the clip lands at the layout viewport no matter how tall the boxes beneath
/// it grow — `body { height: auto }` makes the shell paintable *within* that
/// box, it does not extend it. `clientHeight` is that box.
fn paintable_height(root: &web_sys::HtmlElement) -> Option<f64> {
    let height = f64::from(root.client_height());
    usable(&height).then_some(height)
}

/// A published height past the root's clip box does not push the shell down to
/// the screen edge; it pushes the shell's bottom chrome *out of the app*. The
/// tab dock is the first casualty: it floats a fixed inset above the shell's
/// bottom edge, so a shell that overshoots by more than that inset leaves the
/// dock cut in half, with the root background showing through below it. The
/// visual viewport reads taller than the layout viewport after a software
/// keyboard closes, which is when the overshoot happens.
///
/// Clamping costs the standalone recovery nothing: a launch that lays out short
/// only in `100dvh` still reports the full screen here, so the recovered height
/// survives. Where the root box is short too, the recovered height was never
/// paintable in the first place.
fn clamp_to_paintable(published: f64, paintable: Option<f64>) -> f64 {
    match paintable {
        Some(limit) => published.min(limit),
        None => published,
    }
}

fn apply_app_height(window: &web_sys::Window) {
    // The transcript owns scrolling, never the document. Restore a leftover
    // reveal pan before sampling, even when WebKit omitted a scroll event.
    if let Ok(offset) = window.scroll_y()
        && offset.abs() > 0.5
    {
        log::debug!("Mobile viewport resetting document scroll_y={offset}");
        window.scroll_to_with_x_and_y(0.0, 0.0);
    }
    let viewport = window.visual_viewport();
    let measured = viewport
        .as_ref()
        .map(web_sys::VisualViewport::height)
        .filter(usable)
        .or_else(|| {
            window
                .inner_height()
                .ok()
                .and_then(|value| value.as_f64())
                .filter(usable)
        });
    let Some(height) = measured else {
        return;
    };
    // Width only ever selects which baseline is in play, so a browser that
    // reports no width simply keeps one baseline for every orientation.
    let width = viewport
        .as_ref()
        .map(web_sys::VisualViewport::width)
        .filter(usable)
        .or_else(|| {
            window
                .inner_width()
                .ok()
                .and_then(|value| value.as_f64())
                .filter(usable)
        })
        .unwrap_or(0.0);

    let baseline = VIEWPORT_BASELINE.with(|cell| {
        let baseline = next_baseline(cell.get(), width, height);
        cell.set((width, baseline));
        baseline
    });

    let Some(root) = window
        .document()
        .and_then(|document| document.document_element())
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    let keyboard = keyboard_is_open(baseline, height);
    // Only an installed app with the keyboard closed can be short for the
    // standalone reason; in a browser the missing height belongs to chrome the
    // app must stay clear of, and with the keyboard up the measurement is the
    // whole point of this probe.
    let measured_or_recovered = match screen_size(window) {
        Some((screen_width, screen_height)) if !keyboard && is_standalone_display(window) => {
            recovered_standalone_height(height, width, screen_width, screen_height)
        }
        _ => height,
    };
    let published = clamp_to_paintable(measured_or_recovered, paintable_height(&root));
    let _ = root
        .style()
        .set_property("--app-height", &format!("{published}px"));
    if keyboard {
        let _ = root.set_attribute("data-keyboard-open", "");
    } else {
        let _ = root.remove_attribute("data-keyboard-open");
    }
}

/// Viewport and chrome geometry for the browser console, without chat content.
fn viewport_metrics() -> String {
    let Some(window) = web_sys::window() else {
        return "viewport: no window".to_owned();
    };
    let inner_height = window
        .inner_height()
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(-1.0);
    let visual_height = window
        .visual_viewport()
        .map(|viewport| viewport.height())
        .unwrap_or(-1.0);
    let screen_height = window
        .screen()
        .ok()
        .and_then(|screen| screen.height().ok())
        .unwrap_or(-1);
    // The root's clip box. The one number that says which of the two viewport
    // divergences a phone is actually hitting: equal to the screen means only
    // `100dvh` measured short (the standalone defect, recovery survives the
    // clamp), short of it means the layout viewport itself is short and the
    // clamp is what keeps the tab dock on screen.
    let client_height = window
        .document()
        .and_then(|document| document.document_element())
        .map(|root| root.client_height())
        .unwrap_or(-1);
    let standalone = is_standalone_display(&window);
    // What the shell was actually sized to, so a report from a phone says
    // whether the standalone recovery above fired and by how much.
    let app_height = window
        .document()
        .and_then(|document| document.document_element())
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
        .and_then(|root| root.style().get_property_value("--app-height").ok())
        .unwrap_or_default();
    let scroll_y = window.scroll_y().unwrap_or(-1.0);
    let baseline_height = VIEWPORT_BASELINE.with(|cell| cell.get().1);
    let document = window.document();
    let root = document
        .as_ref()
        .and_then(|document| document.document_element());
    let keyboard = root
        .as_ref()
        .is_some_and(|root| root.has_attribute("data-keyboard-open"));
    let visual_top = window
        .visual_viewport()
        .map(|viewport| viewport.offset_top())
        .unwrap_or(-1.0);
    let bottom = |selector| {
        document
            .as_ref()
            .and_then(|document| document.query_selector(selector).ok().flatten())
            .map(|element| element.get_bounding_client_rect().bottom())
            .unwrap_or(-1.0)
    };
    let shell_bottom = bottom(".mobile-app");
    let dock_bottom = bottom(".bottom-nav");
    let dock_inset = document
        .as_ref()
        .and_then(|document| document.query_selector(".bottom-nav").ok().flatten())
        .and_then(|dock| window.get_computed_style(&dock).ok().flatten())
        .and_then(|style| style.get_property_value("margin-bottom").ok())
        .unwrap_or_default();
    format!(
        "viewport: inner_h={inner_height} visual_h={visual_height} screen_h={screen_height} \
         client_h={client_height} standalone={standalone} app_height={app_height} \
         keyboard={keyboard} scroll_y={scroll_y} baseline_h={baseline_height} visual_top={visual_top} shell_bottom={shell_bottom} \
         dock_bottom={dock_bottom} dock_inset={dock_inset}"
    )
}

fn app_root() -> Option<web_sys::HtmlElement> {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("app-root"))
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
}

fn remove_boot_screen() {
    if let Some(boot) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("boot-screen"))
    {
        boot.remove();
    }
}

fn show_boot_error(message: &str) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let Some(root) = document
        .get_element_by_id("boot-screen")
        .or_else(|| document.get_element_by_id("app-root"))
        .or_else(|| document.body().map(Into::into))
    else {
        return;
    };
    let Ok(error) = document.create_element("div") else {
        return;
    };
    error.set_id("boot-error");
    error.set_class_name("boot-error");
    error.set_text_content(Some(message));
    let _ = root.append_child(&error);
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    async fn next_frame() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .request_animation_frame(&resolve)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    #[wasm_bindgen_test]
    async fn keyboard_dismissal_restores_reachable_bottom_tabs_after_late_measurement() {
        use leptos::prelude::*;
        use std::{cell::Cell, rc::Rc};

        let window = web_sys::window().unwrap();
        let document = window.document().unwrap();
        let root: web_sys::HtmlElement = document.document_element().unwrap().dyn_into().unwrap();
        let attributes: Vec<_> = ["style", "data-theme", "data-keyboard-open"]
            .into_iter()
            .map(|name| (name, root.get_attribute(name)))
            .collect();
        let baseline = VIEWPORT_BASELINE.with(|cell| cell.replace((0.0, 0.0)));
        root.remove_attribute("data-keyboard-open").unwrap();
        root.set_attribute("data-theme", "dark").unwrap();
        root.style()
            .set_property("--safe-area-bottom", "34px")
            .unwrap();
        let style = document.create_element("style").unwrap();
        style.set_text_content(Some(include_str!("../styles.css")));
        document.head().unwrap().append_child(&style).unwrap();
        let container: web_sys::HtmlElement =
            document.create_element("div").unwrap().dyn_into().unwrap();
        // Other wasm fixtures remain in the document (the first run placed
        // this shell at y=17438). Isolate its viewport, not its layout rules.
        container
            .set_attribute("style", "position:fixed;inset:0;z-index:2147483647")
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        let mount = leptos::mount::mount_to(container.clone(), || {
            provide_context(crate::state::AppState::new());
            view! {
                <div class="mobile-app">
                    <div class="mobile-content"><textarea aria-label="Message" /></div>
                    <crate::components::BottomNav />
                </div>
            }
        });
        next_tick().await;

        let viewport = window.visual_viewport().unwrap();
        let original =
            js_sys::Object::get_own_property_descriptor(viewport.as_ref(), &"height".into());
        let full_height = viewport.height();
        let visible_height = Rc::new(Cell::new(full_height));
        let height_for_getter = visible_height.clone();
        let getter = wasm_bindgen::closure::Closure::<dyn FnMut() -> f64>::new(move || {
            height_for_getter.get()
        });
        let descriptor = js_sys::Object::new();
        js_sys::Reflect::set(&descriptor, &"configurable".into(), &true.into()).unwrap();
        js_sys::Reflect::set(&descriptor, &"get".into(), getter.as_ref()).unwrap();
        js_sys::Object::define_property(viewport.as_ref(), &"height".into(), &descriptor);
        let cleanup = listen_for_app_height(&window);
        let dock = container
            .query_selector("[data-mobile-test='bottom-nav']")
            .unwrap()
            .unwrap();
        let initial_bottom = dock.get_bounding_client_rect().bottom();
        let input: web_sys::HtmlElement = container
            .query_selector("textarea")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.focus().unwrap();
        let keyboard_height = full_height / 2.0;
        visible_height.set(keyboard_height);
        viewport
            .dispatch_event(&web_sys::Event::new("resize").unwrap())
            .unwrap();
        next_frame().await;
        let open_bottom = dock.get_bounding_client_rect().bottom();

        // WebKit can notify before its height getter changes (bug 254861).
        // No second resize arrives to clear the keyboard-sized shell.
        input.blur().unwrap();
        viewport
            .dispatch_event(&web_sys::Event::new("resize").unwrap())
            .unwrap();
        visible_height.set(full_height);
        next_frame().await;
        next_frame().await;
        let restored_bottom = dock.get_bounding_client_rect().bottom();
        wasm_bindgen_test::console_log!(
            "Late keyboard dismissal: initial={initial_bottom} open={open_bottom} restored={restored_bottom}; {}",
            viewport_metrics()
        );
        let rect = dock.get_bounding_client_rect();
        let reachable = document
            .element_from_point(
                (rect.x() + rect.width() / 2.0) as f32,
                (rect.bottom() - 6.0) as f32,
            )
            .is_some_and(|hit| dock.contains(Some(&hit)));

        // A layout-viewport resize must also refresh the keyboard state,
        // even when visualViewport sends no matching event.
        visible_height.set(keyboard_height);
        viewport
            .dispatch_event(&web_sys::Event::new("resize").unwrap())
            .unwrap();
        next_frame().await;
        visible_height.set(full_height);
        window
            .dispatch_event(&web_sys::Event::new("resize").unwrap())
            .unwrap();
        next_frame().await;
        next_frame().await;
        let window_restored_bottom = dock.get_bounding_client_rect().bottom();
        wasm_bindgen_test::console_log!(
            "Window resize dismissal: restored={window_restored_bottom}; {}",
            viewport_metrics()
        );

        // The installed app can finish dismissal after the resize event and
        // its first animation frame. Tychat keeps sampling across this window.
        input.focus().unwrap();
        visible_height.set(keyboard_height);
        viewport
            .dispatch_event(&web_sys::Event::new("resize").unwrap())
            .unwrap();
        tyde_time::sleep(std::time::Duration::from_millis(700)).await;
        input.blur().unwrap();
        next_frame().await;
        tyde_time::sleep(std::time::Duration::from_millis(100)).await;
        visible_height.set(full_height);
        tyde_time::sleep(std::time::Duration::from_millis(650)).await;
        next_frame().await;
        let settled_bottom = dock.get_bounding_client_rect().bottom();
        wasm_bindgen_test::console_log!(
            "Home Screen late dismissal: before={initial_bottom} restored={settled_bottom}; {}",
            viewport_metrics()
        );

        let spacer = document.create_element("div").unwrap();
        spacer
            .set_attribute("style", "height:calc(100vh + 200px)")
            .unwrap();
        document.body().unwrap().append_child(&spacer).unwrap();
        window.scroll_to_with_x_and_y(0.0, 40.0);
        let panned = window.scroll_y().unwrap();
        window
            .dispatch_event(&web_sys::Event::new("resize").unwrap())
            .unwrap();
        next_frame().await;
        let recovered_scroll = window.scroll_y().unwrap();
        wasm_bindgen_test::console_log!(
            "Document reveal pan: before={panned} restored={recovered_scroll}"
        );
        // Tear down with settle timers still pending to catch leaked listeners
        // or callbacks changing the next mounted screen.
        cleanup();
        spacer.remove();
        window.scroll_to_with_x_and_y(0.0, 0.0);
        if original.is_undefined() {
            js_sys::Reflect::delete_property(viewport.as_ref(), &"height".into()).unwrap();
        } else {
            js_sys::Object::define_property(
                viewport.as_ref(),
                &"height".into(),
                original.unchecked_ref(),
            );
        }
        drop(getter);
        drop(mount);
        container.remove();
        style.remove();
        for (name, value) in attributes {
            if let Some(value) = value {
                root.set_attribute(name, &value).unwrap();
            } else {
                root.remove_attribute(name).unwrap();
            }
        }
        VIEWPORT_BASELINE.with(|cell| cell.set(baseline));

        assert!(
            (initial_bottom - (full_height - 26.0)).abs() <= 1.0,
            "the tabs must start at the safe-area inset: {initial_bottom}"
        );
        assert!(
            open_bottom <= keyboard_height && open_bottom > 52.0,
            "the entire tab bar must stay above the open keyboard: {open_bottom}"
        );
        assert!(
            (initial_bottom - restored_bottom).abs() <= 1.0,
            "dismissal must restore the tabs to their original bottom edge: before={initial_bottom} after={restored_bottom}"
        );
        assert!(
            reachable,
            "restoring height must not clip the bottom of the tabs"
        );
        assert!(
            (initial_bottom - window_restored_bottom).abs() <= 1.0,
            "a window resize must restore the tabs without a visual resize: before={initial_bottom} after={window_restored_bottom}"
        );
        assert!(
            (initial_bottom - settled_bottom).abs() <= 1.0,
            "focus-only dismissal must restore tabs when dimensions settle after the first frame: before={initial_bottom} after={settled_bottom}"
        );
        assert!(
            panned > 1.0,
            "the document must really scroll before recovery"
        );
        assert!(
            recovered_scroll.abs() <= 0.5,
            "a viewport sample must clear the document reveal pan: {recovered_scroll}"
        );
    }

    /// iPhone-ish numbers: a 393x852 portrait viewport and a ~336px keyboard.
    const WIDTH: f64 = 393.0;
    const TALL: f64 = 852.0;
    const WITH_KEYBOARD: f64 = 516.0;

    #[wasm_bindgen_test]
    fn keyboard_shrinkage_is_recognized() {
        let baseline = next_baseline((0.0, 0.0), WIDTH, TALL);
        assert_eq!(
            baseline, TALL,
            "the first measurement establishes the baseline"
        );
        assert!(
            !keyboard_is_open(baseline, TALL),
            "a full-height viewport is not a keyboard"
        );

        let baseline = next_baseline((WIDTH, baseline), WIDTH, WITH_KEYBOARD);
        assert_eq!(baseline, TALL, "the keyboard must not lower the baseline");
        assert!(
            keyboard_is_open(baseline, WITH_KEYBOARD),
            "a {}px drop must read as the keyboard",
            TALL - WITH_KEYBOARD
        );
    }

    /// The regression that made the floor-only design necessary in the first
    /// place: a standalone launch can report a bogus SHORT height. It is short
    /// but stable, so it becomes the baseline rather than looking like a
    /// keyboard — otherwise the shell would collapse at launch.
    #[wasm_bindgen_test]
    fn a_short_launch_measurement_is_not_a_keyboard() {
        let baseline = next_baseline((0.0, 0.0), WIDTH, 180.0);
        assert!(
            !keyboard_is_open(baseline, 180.0),
            "a short first measurement must establish the baseline, not a keyboard"
        );
    }

    /// iOS browser chrome (the URL bar, ~50-90px) shows and hides constantly.
    /// Treating that as a keyboard would resize the shell while the user is
    /// merely scrolling.
    #[wasm_bindgen_test]
    fn browser_chrome_is_not_a_keyboard() {
        let baseline = next_baseline((0.0, 0.0), WIDTH, TALL);
        let with_url_bar = TALL - 90.0;
        let baseline = next_baseline((WIDTH, baseline), WIDTH, with_url_bar);
        assert!(
            !keyboard_is_open(baseline, with_url_bar),
            "a 90px chrome change must not read as the keyboard"
        );
    }

    /// Rotation changes the width, and a landscape baseline carried into
    /// portrait would look like a permanently open keyboard.
    #[wasm_bindgen_test]
    fn rotation_restarts_the_baseline() {
        let landscape = next_baseline((0.0, 0.0), TALL, WIDTH);
        assert_eq!(landscape, WIDTH);

        let portrait = next_baseline((TALL, landscape), WIDTH, TALL);
        assert_eq!(portrait, TALL, "a width change must restart the baseline");
        assert!(
            !keyboard_is_open(portrait, TALL),
            "rotating must not leave the shell believing a keyboard is open"
        );
    }

    /// The defect this recovery exists for: a standalone launch lays out short
    /// by the insets it still reports through `env()`, so the shell pads for a
    /// status bar it was not given and every bottom capsule floats that far
    /// above the screen edge. The screen is the measurement that is not short.
    #[wasm_bindgen_test]
    fn a_standalone_shortfall_is_recovered_from_the_screen() {
        let short = TALL - 59.0;
        assert_eq!(
            recovered_standalone_height(short, WIDTH, WIDTH, TALL),
            TALL,
            "a status-bar-sized shortfall must be recovered to the screen height"
        );
        assert_eq!(
            recovered_standalone_height(TALL, WIDTH, WIDTH, TALL),
            TALL,
            "a viewport that already reaches the screen must be left alone"
        );
    }

    /// Anything bigger than the insets is a real reason for a short viewport,
    /// and expanding into it would run the composer under whatever owns that
    /// space (browser chrome, a split screen).
    #[wasm_bindgen_test]
    fn a_large_shortfall_is_believed() {
        let with_chrome = TALL - 140.0;
        assert_eq!(
            recovered_standalone_height(with_chrome, WIDTH, WIDTH, TALL),
            with_chrome,
            "a 140px gap is chrome, not a lost inset"
        );
    }

    /// iOS has reported the screen orientation-independently, so the axis has
    /// to be chosen by the viewport's orientation — otherwise a landscape app
    /// would "recover" to the portrait height and run off the screen.
    #[wasm_bindgen_test]
    fn the_screen_axis_follows_the_viewport_orientation() {
        let landscape_short = WIDTH - 20.0;
        assert_eq!(
            recovered_standalone_height(landscape_short, TALL, WIDTH, TALL),
            WIDTH,
            "a landscape viewport recovers to the screen's short axis"
        );
        assert_eq!(
            recovered_standalone_height(landscape_short, TALL, TALL, WIDTH),
            WIDTH,
            "which field holds which dimension must not matter"
        );
    }

    /// Closing the keyboard must restore the shell: the baseline is retained,
    /// so the full-height measurement reads as closed again.
    #[wasm_bindgen_test]
    fn closing_the_keyboard_restores_full_height() {
        let baseline = next_baseline((0.0, 0.0), WIDTH, TALL);
        let baseline = next_baseline((WIDTH, baseline), WIDTH, WITH_KEYBOARD);
        let baseline = next_baseline((WIDTH, baseline), WIDTH, TALL);
        assert!(
            !keyboard_is_open(baseline, TALL),
            "the shell must return to full height once the keyboard closes"
        );
    }

    /// A measurement the root cannot paint is worse than useless: the shell
    /// lays out past the clip and the bottom chrome leaves the app. Whatever
    /// the standalone recovery asked for, the clip is the ceiling.
    #[wasm_bindgen_test]
    fn an_unpaintable_height_is_clamped_to_the_root() {
        const CLIP: f64 = 793.0;
        assert_eq!(
            clamp_to_paintable(TALL, Some(CLIP)),
            CLIP,
            "a height past the root's clip must come back to the clip"
        );
        assert_eq!(
            clamp_to_paintable(WITH_KEYBOARD, Some(CLIP)),
            WITH_KEYBOARD,
            "a height the root can paint must pass through untouched"
        );
        assert_eq!(
            clamp_to_paintable(TALL, None),
            TALL,
            "a root that reports no usable box must not shrink the shell to nothing"
        );
    }

    /// **A keyboard cycle must not leave the tab dock cut in half.**
    ///
    /// Closing the software keyboard can leave the visual viewport reading
    /// taller than the box the root element can paint. The probe published that
    /// measurement verbatim, the shell laid out past the root's clip, and the
    /// dock — which floats a fixed inset above the shell's bottom edge — was
    /// sliced through the middle, with root background showing below it.
    ///
    /// This proves the geometry, against the production stylesheet: overshoot
    /// the root's clip and the dock cannot be reached; publish a height the
    /// root can paint and all of it comes back. The arithmetic that keeps
    /// `apply_app_height` on the paintable side is covered separately by
    /// `an_unpaintable_height_is_clamped_to_the_root`.
    ///
    /// It deliberately stops short of driving `apply_app_height` itself. That
    /// needs a document whose measured viewport exceeds its clip, and no
    /// browser here can produce one: in standards mode `documentElement`'s
    /// `clientHeight` *is* the viewport, so the two are equal by construction,
    /// and the iframe that could be posed otherwise cannot be passed to the
    /// probe at all — `dyn_into::<HtmlElement>` is an `instanceof` check
    /// against the calling realm's constructor, and an iframe's
    /// `documentElement` belongs to another realm. Only a device shows the
    /// divergence this guards.
    #[wasm_bindgen_test]
    async fn an_overshooting_height_never_reaches_the_shell() {
        // ── The defect, against the real stylesheet ──────────────────────
        const CLIP: f64 = 700.0;
        let document = web_sys::window().unwrap().document().unwrap();
        let frame = document
            .create_element("iframe")
            .unwrap()
            .dyn_into::<web_sys::HtmlIFrameElement>()
            .unwrap();
        frame
            .set_attribute("style", "width:393px;height:793px;border:0")
            .unwrap();
        document.body().unwrap().append_child(&frame).unwrap();
        let frame_document = frame.content_document().unwrap();
        let style = frame_document.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "{}\nhtml {{ height: {CLIP}px; }}",
            include_str!("../styles.css")
        )));
        frame_document.head().unwrap().append_child(&style).unwrap();
        let frame_root: web_sys::HtmlElement =
            frame_document.document_element().unwrap().unchecked_into();
        frame_root.set_attribute("data-theme", "dark").unwrap();
        frame_document.body().unwrap().set_inner_html(
            "<div class=\"mobile-app\">\
               <div class=\"mobile-content\"><div class=\"view\"></div></div>\
               <nav class=\"bottom-nav\" data-mobile-test=\"bottom-nav\">\
                 <button class=\"nav-tab\">\
                   <span class=\"nav-icon\">H</span><span class=\"nav-label\">Home</span>\
                 </button>\
               </nav>\
             </div>",
        );
        next_tick().await;

        let dock = frame_document
            .query_selector("[data-mobile-test='bottom-nav']")
            .unwrap()
            .unwrap();
        // Geometry alone still claims the dock is on screen; hit testing is
        // what proves the lower half was clipped away.
        let dock_is_reachable = || {
            let rect = dock.get_bounding_client_rect();
            frame_document
                .element_from_point(
                    (rect.x() + rect.width() / 2.0) as f32,
                    (rect.bottom() - 6.0) as f32,
                )
                .is_some_and(|hit| dock.contains(Some(&hit)))
        };

        let frame_viewport = f64::from(frame_root.client_height());
        frame_root
            .style()
            .set_property("--app-height", "852px")
            .unwrap();
        next_tick().await;
        assert!(
            !dock_is_reachable(),
            "an overshooting height must reproduce the cut-off tab dock"
        );

        frame_root
            .style()
            .set_property("--app-height", &format!("{frame_viewport}px"))
            .unwrap();
        next_tick().await;
        assert!(
            dock_is_reachable(),
            "a height the root can paint must put the whole dock back"
        );
        frame.remove();
    }
}
