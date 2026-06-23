//! In-process event hub for the browser (PWA) backend.
//!
//! The Tauri shell delivers `tyde://host-line`, `tyde://host-disconnected`,
//! etc. over Tauri's JS event bus because the connection manager lives in a
//! separate native process from the webview. In the browser the manager and the
//! Leptos app are the *same* wasm context, so there is no bus to cross — the web
//! connection manager calls the registered Rust callbacks directly through this
//! hub. The hub mirrors the Tauri event names/payloads exactly so the Leptos
//! app's listener wiring (`app.rs`) is unchanged.

use std::cell::RefCell;
use std::rc::Rc;

use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent};
use mobile_shell_types::{
    MobileShellError, PairedHostConnectionStatusEvent, PairedHostsChangedEvent,
};

type Callback<T> = Rc<dyn Fn(T)>;

struct Listeners<T> {
    next_id: u64,
    callbacks: Vec<(u64, Callback<T>)>,
}

impl<T> Default for Listeners<T> {
    fn default() -> Self {
        Self {
            next_id: 0,
            callbacks: Vec::new(),
        }
    }
}

impl<T: Clone> Listeners<T> {
    fn register(&mut self, callback: Callback<T>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.callbacks.push((id, callback));
        id
    }

    fn unregister(&mut self, id: u64) {
        self.callbacks.retain(|(existing, _)| *existing != id);
    }

    fn snapshot(&self) -> Vec<Callback<T>> {
        self.callbacks
            .iter()
            .map(|(_, callback)| callback.clone())
            .collect()
    }
}

#[derive(Default)]
struct EventHub {
    host_line: Listeners<HostLineEvent>,
    host_disconnected: Listeners<HostDisconnectedEvent>,
    host_error: Listeners<HostErrorEvent>,
    paired_hosts_changed: Listeners<PairedHostsChangedEvent>,
    connection_status: Listeners<PairedHostConnectionStatusEvent>,
    shell_error: Listeners<MobileShellError>,
}

thread_local! {
    static HUB: Rc<RefCell<EventHub>> = Rc::new(RefCell::new(EventHub::default()));
}

/// Generates a register fn (returns an unregister `FnOnce`) plus an emit fn for
/// one event channel. Emitting snapshots the callbacks *before* invoking them so
/// a listener that (un)registers during dispatch cannot re-enter the `RefCell`.
macro_rules! event_channel {
    ($field:ident, $register:ident, $emit:ident, $payload:ty) => {
        pub fn $register(callback: impl Fn($payload) + 'static) -> impl FnOnce() {
            let id = HUB.with(|hub| hub.borrow_mut().$field.register(Rc::new(callback)));
            move || HUB.with(|hub| hub.borrow_mut().$field.unregister(id))
        }

        pub fn $emit(event: $payload) {
            let callbacks = HUB.with(|hub| hub.borrow().$field.snapshot());
            for callback in callbacks {
                callback(event.clone());
            }
        }
    };
}

event_channel!(host_line, on_host_line, emit_host_line, HostLineEvent);
event_channel!(
    host_disconnected,
    on_host_disconnected,
    emit_host_disconnected,
    HostDisconnectedEvent
);
event_channel!(host_error, on_host_error, emit_host_error, HostErrorEvent);
event_channel!(
    paired_hosts_changed,
    on_paired_hosts_changed,
    emit_paired_hosts_changed,
    PairedHostsChangedEvent
);
event_channel!(
    connection_status,
    on_connection_status,
    emit_connection_status,
    PairedHostConnectionStatusEvent
);

// Register-only: the browser backend never *originates* a mobile-shell error
// (the Leptos app sets `mobile_shell_error` directly). The listener exists only
// for parity with the Tauri event bus, so there is no matching emit fn.
pub fn on_shell_error(callback: impl Fn(MobileShellError) + 'static) -> impl FnOnce() {
    let id = HUB.with(|hub| hub.borrow_mut().shell_error.register(Rc::new(callback)));
    move || HUB.with(|hub| hub.borrow_mut().shell_error.unregister(id))
}
