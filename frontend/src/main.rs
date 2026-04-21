mod actions;
mod app;
mod bridge;
mod components;
mod devtools;
mod dispatch;
mod highlight;
mod markdown;
mod send;
mod state;
mod term_bridge;

use leptos::prelude::*;

fn main() {
    console_log::init_with_level(log::Level::Debug).expect("failed to init logger");
    console_error_panic_hook::set_once();
    mount_to_body(app::App);
}
