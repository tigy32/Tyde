mod actions;
mod app;
mod bridge;
mod components;
mod devtools;
mod dispatch;
mod highlight_worker;
mod line_source;
mod markdown;
mod perf;
mod send;
mod state;
mod syntax_highlight;
mod term_bridge;

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_test_support {
    use std::{any::Any, ops::Deref};

    pub struct Mounted<T> {
        value: T,
        _handle: Box<dyn Any>,
    }

    impl<T> Mounted<T> {
        pub fn new(handle: impl Any, value: T) -> Self {
            Self {
                value,
                _handle: Box::new(handle),
            }
        }
    }

    impl<T> Deref for Mounted<T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.value
        }
    }
}

use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();

    // The same wasm bundle runs in two contexts: the main page (with a
    // `window`) and the syntax-highlighting Web Worker (`window` is
    // absent there — only `self` exists). Branch on that instead of
    // using two separate wasm targets, which would double the bundle
    // download.
    if web_sys::window().is_none() {
        // Logging in the worker goes to its own console output; the
        // browser dev-tools merge it into the main thread's console
        // by default.
        let _ = console_log::init_with_level(log::Level::Info);
        highlight_worker::worker_main();
        return;
    }

    console_log::init_with_level(log::Level::Debug).expect("failed to init logger");
    mount_to_body(app::App);
}
