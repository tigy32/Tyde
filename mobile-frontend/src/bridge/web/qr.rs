//! Browser (PWA) QR scanning.
//!
//! Replaces the Tauri `plugin:barcode-scanner` path. Where the browser supports
//! the (experimental) `BarcodeDetector` API plus `getUserMedia`, this opens the
//! rear camera in a fullscreen overlay and resolves with the first QR payload it
//! reads. Browsers without `BarcodeDetector` (notably desktop Firefox and, at
//! time of writing, iOS Safari) return a clear error so the caller falls back to
//! the always-available paste-the-`tyde-pair://`-URI screen (`pairing_flow.rs`
//! `ManualPasteScreen`). The authoritative parse stays
//! `MobilePairingQrPayload::from_uri`, run later by `preview_pairing_uri`.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{HtmlVideoElement, MediaStream, MediaStreamConstraints};

use super::super::BarcodeScanResult;

/// Web has no separate permission-check command — `getUserMedia` itself prompts
/// and surfaces denial. Kept to satisfy the bridge contract.
pub async fn ensure_camera_permission() -> Result<(), String> {
    Ok(())
}

pub async fn scan_qr() -> Result<BarcodeScanResult, String> {
    if !barcode_detector_supported() {
        return Err(
            "Live camera scanning isn't supported in this browser. Tap \"Paste pairing URI instead\" and paste the tyde-pair:// URI shown by your host."
                .to_owned(),
        );
    }

    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let navigator = window.navigator();
    let media_devices = navigator
        .media_devices()
        .map_err(|_| "camera access is unavailable in this browser".to_owned())?;

    let constraints = MediaStreamConstraints::new();
    let video_constraints = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &video_constraints,
        &JsValue::from_str("facingMode"),
        &JsValue::from_str("environment"),
    );
    constraints.set_video(&video_constraints);

    let stream_promise = media_devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|error| format!("failed to request camera: {}", err_string(&error)))?;
    let stream: MediaStream = JsFuture::from(stream_promise)
        .await
        .map_err(|error| {
            format!(
                "camera permission was denied or unavailable: {}",
                err_string(&error)
            )
        })?
        .dyn_into()
        .map_err(|_| "getUserMedia did not return a MediaStream".to_owned())?;

    let cancelled = Rc::new(Cell::new(false));
    let video: HtmlVideoElement = document
        .create_element("video")
        .map_err(|_| "failed to create video element".to_owned())?
        .dyn_into()
        .map_err(|_| "video element had the wrong type".to_owned())?;

    let (overlay, cancel_closure) = match build_overlay(&document, &video, &cancelled) {
        Ok(parts) => parts,
        Err(error) => {
            stop_stream(&stream);
            return Err(error);
        }
    };
    // Guard tears the camera + overlay down (and drops the cancel-button
    // listener) on every exit path below — including the future being dropped.
    let guard = ScanGuard {
        stream: stream.clone(),
        overlay,
        _cancel_closure: cancel_closure,
    };

    video.set_autoplay(true);
    video.set_muted(true);
    video.set_attribute("playsinline", "true").ok();
    video.set_src_object(Some(&stream));
    if let Ok(play_promise) = video.play() {
        let _ = JsFuture::from(play_promise).await;
    }

    // `BarcodeDetector.detect()` can reject transiently — most commonly before
    // the <video> has a decoded frame (`play()` resolving does NOT guarantee
    // `readyState >= HAVE_CURRENT_DATA`), e.g. an InvalidStateError on the first
    // poll on slower devices. So gate on `readyState` and treat per-frame
    // rejections as transient: log and keep polling, bailing only after a run of
    // consecutive failures so a genuinely broken detector still terminates.
    const MAX_CONSECUTIVE_FAILURES: u32 = 40;
    let detector = new_barcode_detector()?;
    let mut consecutive_failures: u32 = 0;
    let result = loop {
        if cancelled.get() {
            break Err("QR scan cancelled".to_owned());
        }
        if video.ready_state() < web_sys::HtmlMediaElement::HAVE_CURRENT_DATA {
            // No decoded frame yet — wait without counting it as a failure.
            sleep_ms(120).await;
            continue;
        }
        match detect_once(&detector, &video).await {
            Ok(Some(content)) => break Ok(content),
            Ok(None) => consecutive_failures = 0,
            Err(error) => {
                consecutive_failures += 1;
                log::warn!("QR detect transient error ({consecutive_failures}): {error}");
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    break Err(format!("QR scanning failed repeatedly: {error}"));
                }
            }
        }
        sleep_ms(250).await;
    };

    drop(guard);
    result.map(|content| BarcodeScanResult {
        content,
        format: Some("qr_code".to_owned()),
    })
}

fn barcode_detector_supported() -> bool {
    web_sys::window()
        .and_then(|window| {
            js_sys::Reflect::get(&window, &JsValue::from_str("BarcodeDetector")).ok()
        })
        .map(|ctor| !ctor.is_undefined() && !ctor.is_null())
        .unwrap_or(false)
}

fn new_barcode_detector() -> Result<JsValue, String> {
    let window = web_sys::window().ok_or("no window")?;
    let ctor = js_sys::Reflect::get(&window, &JsValue::from_str("BarcodeDetector"))
        .map_err(|_| "BarcodeDetector is unavailable".to_owned())?;
    let ctor: js_sys::Function = ctor
        .dyn_into()
        .map_err(|_| "BarcodeDetector is not constructible".to_owned())?;
    let options = js_sys::Object::new();
    let formats = js_sys::Array::of1(&JsValue::from_str("qr_code"));
    let _ = js_sys::Reflect::set(&options, &JsValue::from_str("formats"), &formats);
    let args = js_sys::Array::of1(&options);
    js_sys::Reflect::construct(&ctor, &args).map_err(|error| {
        format!(
            "failed to construct BarcodeDetector: {}",
            err_string(&error)
        )
    })
}

async fn detect_once(
    detector: &JsValue,
    video: &HtmlVideoElement,
) -> Result<Option<String>, String> {
    let detect_fn = js_sys::Reflect::get(detector, &JsValue::from_str("detect"))
        .map_err(|_| "BarcodeDetector.detect missing".to_owned())?;
    let detect_fn: js_sys::Function = detect_fn
        .dyn_into()
        .map_err(|_| "BarcodeDetector.detect is not callable".to_owned())?;
    let promise = detect_fn
        .call1(detector, video)
        .map_err(|error| format!("BarcodeDetector.detect failed: {}", err_string(&error)))?;
    let promise: js_sys::Promise = promise
        .dyn_into()
        .map_err(|_| "BarcodeDetector.detect did not return a promise".to_owned())?;
    let detected = JsFuture::from(promise)
        .await
        .map_err(|error| format!("QR detection error: {}", err_string(&error)))?;
    let array: js_sys::Array = detected
        .dyn_into()
        .map_err(|_| "BarcodeDetector.detect did not return a list".to_owned())?;
    for value in array.iter() {
        if let Ok(raw) = js_sys::Reflect::get(&value, &JsValue::from_str("rawValue"))
            && let Some(raw) = raw.as_string()
            && !raw.is_empty()
        {
            return Ok(Some(raw));
        }
    }
    Ok(None)
}

type CancelClosure = Closure<dyn FnMut()>;

fn build_overlay(
    document: &web_sys::Document,
    video: &HtmlVideoElement,
    cancelled: &Rc<Cell<bool>>,
) -> Result<(web_sys::Element, CancelClosure), String> {
    let overlay = document
        .create_element("div")
        .map_err(|_| "failed to create scanner overlay".to_owned())?;
    overlay
        .set_attribute(
            "style",
            "position:fixed;inset:0;z-index:9999;background:#000;display:flex;flex-direction:column;align-items:center;justify-content:center;",
        )
        .ok();
    video
        .set_attribute(
            "style",
            "width:100%;height:100%;object-fit:cover;position:absolute;inset:0;",
        )
        .ok();
    overlay.append_child(video).ok();

    let cancel = document
        .create_element("button")
        .map_err(|_| "failed to create cancel button".to_owned())?;
    cancel.set_text_content(Some("Cancel"));
    cancel
        .set_attribute(
            "style",
            "position:relative;z-index:1;margin-top:auto;margin-bottom:2rem;padding:0.75rem 2rem;font-size:1rem;border-radius:0.5rem;border:none;background:#fff;color:#000;",
        )
        .ok();
    let cancelled_for_click = cancelled.clone();
    let on_click = Closure::<dyn FnMut()>::new(move || cancelled_for_click.set(true));
    cancel
        .add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())
        .ok();
    overlay.append_child(&cancel).ok();

    if let Some(body) = document.body() {
        body.append_child(&overlay).ok();
    }
    // The closure is returned (not `forget`-ed) so `ScanGuard` owns it and drops
    // it with the overlay — no per-scan closure leak.
    Ok((overlay, on_click))
}

/// Stops the camera tracks and removes the overlay when scanning ends (normal
/// completion, cancel, error, or the future being dropped). Owns the cancel
/// button's click closure so it is freed with the rest of the overlay.
struct ScanGuard {
    stream: MediaStream,
    overlay: web_sys::Element,
    _cancel_closure: CancelClosure,
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        stop_stream(&self.stream);
        self.overlay.remove();
    }
}

fn stop_stream(stream: &MediaStream) {
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
    }
}

async fn sleep_ms(ms: i32) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = JsFuture::from(promise).await;
}

fn err_string(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"))
}
