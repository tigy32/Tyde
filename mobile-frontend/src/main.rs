pub mod actions;
mod app;
mod bridge;
mod components;
mod dispatch;
mod markdown;
mod send;
pub mod state;

use wasm_bindgen::JsCast;

fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Debug);

    let Some(root) = app_root() else {
        show_boot_error("Tyde Mobile could not mount: #app-root is missing");
        return;
    };

    leptos::mount::mount_to(root, app::App).forget();
    remove_boot_screen();

    wasm_bindgen_futures::spawn_local(async {
        crate::bridge::wasm_log("info", "Tyde mobile WASM mounted visible shell").await;
    });
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
