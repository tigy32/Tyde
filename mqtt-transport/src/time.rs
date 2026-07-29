//! Timer/clock primitives, swapped by target so the shared
//! [`protocol_driver`](crate::protocol_driver) never names a runtime-specific
//! timer directly.
//!
//! Both backends expose the same `tokio::time`-shaped API (`Instant`,
//! `sleep`, `interval_at`), so the driver's `sleep(..)` / `interval_at(..)` /
//! `Instant::now()` calls are identical on both targets. `tokio::select!` itself
//! is just a macro that polls whatever futures it is given (no runtime needed),
//! so it is used directly on both targets.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::time::{Instant, interval_at, sleep};

// `wasmtimer::tokio` is a drop-in re-implementation of `tokio::time` backed by
// the browser's timer APIs, valid for wasm32-unknown-unknown where tokio's own
// time driver is unavailable.
// wasmtimer keeps `Instant` in its `std` module and the timer drivers in its
// `tokio` module.
#[cfg(target_arch = "wasm32")]
pub(crate) use wasmtimer::std::Instant;
#[cfg(target_arch = "wasm32")]
pub(crate) use wasmtimer::tokio::{Interval, interval_at, sleep};

pub(crate) fn unix_time_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now().max(0.0) as u64
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}
