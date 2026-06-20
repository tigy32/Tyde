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
