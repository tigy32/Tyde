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
    apply_app_height(&window);

    let resize_target = window.clone();
    let on_resize = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        apply_app_height(&resize_target);
    });
    if let Some(viewport) = window.visual_viewport() {
        let _ =
            viewport.add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
    }
    let _ = window
        .add_event_listener_with_callback("orientationchange", on_resize.as_ref().unchecked_ref());
    on_resize.forget();

    install_document_scroll_guard(&window);
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

fn usable(value: &f64) -> bool {
    value.is_finite() && *value > 0.0
}

fn apply_app_height(window: &web_sys::Window) {
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
    let _ = root
        .style()
        .set_property("--app-height", &format!("{height}px"));
    if keyboard_is_open(baseline, height) {
        let _ = root.set_attribute("data-keyboard-open", "");
    } else {
        let _ = root.remove_attribute("data-keyboard-open");
    }
}

/// The shell never scrolls as a whole — the transcript is the only scroller —
/// so a non-zero document scroll offset is always the browser panning the page
/// to reveal the focused composer. Sizing the shell to the visible viewport
/// removes the reason to pan; this undoes any pan that still happens (WebKit
/// can scroll before it reports the matching viewport resize) so the header is
/// never left stranded off screen.
fn install_document_scroll_guard(window: &web_sys::Window) {
    let Some(viewport) = window.visual_viewport() else {
        return;
    };
    let guard_target = window.clone();
    let on_scroll = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        if guard_target
            .scroll_y()
            .is_ok_and(|offset| offset.abs() > 0.5)
        {
            guard_target.scroll_to_with_x_and_y(0.0, 0.0);
        }
    });
    let _ = viewport.add_event_listener_with_callback("scroll", on_scroll.as_ref().unchecked_ref());
    on_scroll.forget();
}

/// One-shot launch diagnostics so a phone with a mis-sized viewport reports
/// the actual numbers through the host log.
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
    let standalone = js_sys::Reflect::get(window.navigator().as_ref(), &"standalone".into())
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    format!(
        "viewport: inner_h={inner_height} visual_h={visual_height} screen_h={screen_height} standalone={standalone}"
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
}
