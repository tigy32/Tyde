mod actions;
mod app;
mod bridge;
mod components;
mod dispatch;
mod send;
mod state;

use leptos::prelude::*;

fn main() {
    console_log::init_with_level(log::Level::Debug).expect("failed to init logger");
    mount_to_body(app::App);
}
