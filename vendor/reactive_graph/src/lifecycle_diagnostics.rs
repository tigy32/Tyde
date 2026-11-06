//! Content-free observation of the synchronous render-effect loop only.
use std::cell::Cell;

/// Branches observed directly in the synchronous render-effect implementation.
#[derive(Clone, Copy)]
pub enum Event {
    /// The initial synchronous callback is about to run.
    InitialCallbackEnter,
    /// The initial callback returned.
    InitialCallbackExit,
    /// The receiver yielded a notification.
    ReceiverReady,
    /// The receiver closed.
    ReceiverClosed,
    /// A notification is not processed because the owner is paused.
    OwnerPaused,
    /// The subscriber dependency check is about to run.
    DependencyEnter,
    /// The dependency check requested a callback.
    DependencyChanged,
    /// The dependency check did not request a callback.
    DependencyUnchanged,
    /// The deferred callback is about to run.
    CallbackEnter,
    /// The deferred callback returned.
    CallbackExit,
}

thread_local! {
    static OBSERVER: Cell<Option<fn(Event)>> = const { Cell::new(None) };
    static REGISTERING: Cell<bool> = const { Cell::new(false) };
}

/// Install an observer on this executor thread, without changing graph state.
pub fn set_observer(observer: fn(Event)) {
    OBSERVER.set(Some(observer));
}

pub(crate) fn emit(event: Event) {
    if let Some(observer) = OBSERVER.get() {
        observer(event);
    }
}

/// True only during immediate registration, never inherited by a polled task.
pub fn registering_render_effect() -> bool {
    REGISTERING.get()
}

pub(crate) struct Registration(bool);

impl Registration {
    pub(crate) fn enter() -> Self {
        Self(REGISTERING.replace(true))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        REGISTERING.set(self.0);
    }
}
