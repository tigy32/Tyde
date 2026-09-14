#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{sleep, timeout};

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
pub use browser::{Elapsed, sleep, timeout};
