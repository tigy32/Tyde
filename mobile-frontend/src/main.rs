pub mod actions;
mod app;
mod bridge;
mod components;
mod dispatch;
#[cfg(all(feature = "ui-fixtures", debug_assertions))]
mod fixtures;
mod markdown;
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

/// Home-screen (standalone) launches can lay out `100dvh` shorter than the
/// real visible viewport, stranding the bottom nav above a dead band. The
/// shell's CSS takes `max(100dvh, var(--app-height))`, so publishing the
/// measured visual-viewport height here only ever recovers missing height —
/// keyboard-driven visual-viewport shrinkage can never squeeze the shell
/// below `100dvh`. This must run from the app (not the bundle's index.html):
/// the production loader injects only stylesheet links and the entry script,
/// so inline markup never reaches the phone.
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
}

fn apply_app_height(window: &web_sys::Window) {
    let measured = window
        .visual_viewport()
        .map(|viewport| viewport.height())
        .filter(|height| height.is_finite() && *height > 0.0)
        .or_else(|| {
            window
                .inner_height()
                .ok()
                .and_then(|value| value.as_f64())
                .filter(|height| height.is_finite() && *height > 0.0)
        });
    let Some(height) = measured else {
        return;
    };
    if let Some(style) = window
        .document()
        .and_then(|document| document.document_element())
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
        .map(|element| element.style())
    {
        let _ = style.set_property("--app-height", &format!("{height}px"));
    }
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
