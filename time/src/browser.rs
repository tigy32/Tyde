use std::{future::Future, pin::pin, time::Duration};

use futures_channel::oneshot;
use futures_util::future::{Either, select};
use wasm_bindgen::{JsCast, prelude::*};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(callback: &js_sys::Function, milliseconds: i32) -> i32;
    #[wasm_bindgen(js_name = clearTimeout)]
    fn clear_timeout(handle: i32);
}

struct BrowserTimer {
    handle: i32,
    callback: Option<Closure<dyn FnMut()>>,
}

impl Drop for BrowserTimer {
    fn drop(&mut self) {
        clear_timeout(self.handle);
        drop(self.callback.take());
    }
}

// Each sleep owns one browser timer. Cancelling a long deadline clears its
// callback immediately, so completed requests cannot delay later short timers.
pub async fn sleep(mut duration: Duration) {
    loop {
        let segment = duration.min(Duration::from_millis(i32::MAX as u64));
        let (send, receive) = oneshot::channel();
        let callback = Closure::once(move || {
            let _ = send.send(());
        });
        let timer = BrowserTimer {
            handle: set_timeout(
                callback.as_ref().unchecked_ref(),
                segment.as_millis() as i32,
            ),
            callback: Some(callback),
        };
        receive.await.expect("browser timer callback was dropped");
        drop(timer);
        duration = duration.saturating_sub(segment);
        if duration.is_zero() {
            return;
        }
    }
}

#[derive(Debug)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    match select(pin!(future), pin!(sleep(duration))).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(_) => Err(Elapsed),
    }
}
