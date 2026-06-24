//! Browser (PWA) QR scanning.
//!
//! Replaces the Tauri `plugin:barcode-scanner` path. This opens the rear camera
//! in a fullscreen overlay and resolves with the first QR payload it reads. It
//! prefers the browser's native `BarcodeDetector` when present, otherwise it
//! decodes frames with the bundled `jsQR` script loaded by `index.html`.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, HtmlVideoElement, MediaStream,
    MediaStreamConstraints,
};

use super::super::BarcodeScanResult;

/// Web has no separate permission-check command — `getUserMedia` itself prompts
/// and surfaces denial. Kept to satisfy the bridge contract.
pub async fn ensure_camera_permission() -> Result<(), String> {
    Ok(())
}

pub async fn scan_qr() -> Result<BarcodeScanResult, String> {
    let capability = scan_capability();
    if let Some(error) = scan_unavailable_error(capability) {
        return Err(error);
    }

    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let decoder = new_decoder(&document, capability)?;
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

    // Native detector and canvas/jsQR reads can reject transiently — most
    // commonly before the <video> has a decoded frame (`play()` resolving does
    // NOT guarantee `readyState >= HAVE_CURRENT_DATA`), e.g. an InvalidStateError
    // on the first poll on slower devices. So gate on `readyState` and treat
    // per-frame rejections as transient: log and keep polling, bailing only
    // after a run of consecutive failures so a genuinely broken decoder still
    // terminates.
    const MAX_CONSECUTIVE_FAILURES: u32 = 40;
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
        match detect_once(&decoder, &video).await {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanCapability {
    secure_context: bool,
    camera: bool,
    barcode_detector: bool,
    jsqr: bool,
}

impl ScanCapability {
    fn decoder_available(self) -> bool {
        self.barcode_detector || self.jsqr
    }

    fn scan_available(self) -> bool {
        self.secure_context && self.camera && self.decoder_available()
    }
}

fn scan_capability() -> ScanCapability {
    let Some(window) = web_sys::window() else {
        return ScanCapability {
            secure_context: false,
            camera: false,
            barcode_detector: false,
            jsqr: jsqr_supported(),
        };
    };
    let navigator = window.navigator();
    ScanCapability {
        secure_context: window.is_secure_context(),
        camera: get_user_media_supported(&navigator),
        barcode_detector: barcode_detector_supported(&window),
        jsqr: jsqr_supported(),
    }
}

fn scan_unavailable_error(capability: ScanCapability) -> Option<String> {
    if capability.scan_available() {
        return None;
    }
    let paste_hint =
        "Tap \"Paste pairing URI instead\" and paste the pairing code shown by your host.";
    if !capability.secure_context {
        return Some(format!(
            "Live camera scanning requires a secure browser context (HTTPS or localhost). {paste_hint}"
        ));
    }
    if !capability.camera {
        return Some(format!(
            "Camera access is unavailable in this browser. {paste_hint}"
        ));
    }
    if !capability.decoder_available() {
        return Some(format!(
            "No QR decoder is available in this browser. Reload Tyde Mobile so the bundled decoder can load, or {paste_hint}"
        ));
    }
    Some(format!("Live camera scanning is unavailable. {paste_hint}"))
}

fn get_user_media_supported(navigator: &web_sys::Navigator) -> bool {
    let Ok(media_devices) = navigator.media_devices() else {
        return false;
    };
    js_sys::Reflect::get(&media_devices, &JsValue::from_str("getUserMedia"))
        .ok()
        .and_then(|value| value.dyn_into::<js_sys::Function>().ok())
        .is_some()
}

fn barcode_detector_supported(window: &web_sys::Window) -> bool {
    js_sys::Reflect::get(window, &JsValue::from_str("BarcodeDetector"))
        .ok()
        .and_then(|ctor| ctor.dyn_into::<js_sys::Function>().ok())
        .is_some()
}

fn jsqr_supported() -> bool {
    jsqr_function().is_some()
}

fn jsqr_function() -> Option<js_sys::Function> {
    js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("jsQR"))
        .ok()
        .and_then(|decoder| decoder.dyn_into::<js_sys::Function>().ok())
}

enum QrDecoder {
    BarcodeDetector(JsValue),
    JsQr(JsQrDecoder),
}

fn new_decoder(
    document: &web_sys::Document,
    capability: ScanCapability,
) -> Result<QrDecoder, String> {
    if capability.barcode_detector {
        match new_barcode_detector() {
            Ok(detector) => return Ok(QrDecoder::BarcodeDetector(detector)),
            Err(error) if capability.jsqr => {
                log::warn!(
                    "BarcodeDetector unavailable after capability check: {error}; using jsQR"
                );
            }
            Err(error) => return Err(error),
        }
    }
    if capability.jsqr {
        return Ok(QrDecoder::JsQr(JsQrDecoder::new(document)?));
    }
    Err("no QR decoder is available".to_owned())
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
    decoder: &QrDecoder,
    video: &HtmlVideoElement,
) -> Result<Option<String>, String> {
    match decoder {
        QrDecoder::BarcodeDetector(detector) => {
            detect_once_with_barcode_detector(detector, video).await
        }
        QrDecoder::JsQr(jsqr) => jsqr.detect_once(video),
    }
}

async fn detect_once_with_barcode_detector(
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

struct JsQrDecoder {
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
}

impl JsQrDecoder {
    fn new(document: &web_sys::Document) -> Result<Self, String> {
        let canvas: HtmlCanvasElement = document
            .create_element("canvas")
            .map_err(|_| "failed to create QR decode canvas".to_owned())?
            .dyn_into()
            .map_err(|_| "QR decode canvas had the wrong type".to_owned())?;
        let context: CanvasRenderingContext2d = canvas
            .get_context("2d")
            .map_err(|error| {
                format!(
                    "failed to create QR decode canvas context: {}",
                    err_string(&error)
                )
            })?
            .ok_or_else(|| "2D canvas is unavailable in this browser".to_owned())?
            .dyn_into()
            .map_err(|_| "QR decode canvas context had the wrong type".to_owned())?;
        Ok(Self { canvas, context })
    }

    fn detect_once(&self, video: &HtmlVideoElement) -> Result<Option<String>, String> {
        let width = video.video_width();
        let height = video.video_height();
        if width == 0 || height == 0 {
            return Ok(None);
        }
        self.canvas.set_width(width);
        self.canvas.set_height(height);
        self.context
            .draw_image_with_html_video_element(video, 0.0, 0.0)
            .map_err(|error| format!("failed to draw camera frame: {}", err_string(&error)))?;
        let image_data = self
            .context
            .get_image_data(0.0, 0.0, f64::from(width), f64::from(height))
            .map_err(|error| format!("failed to read camera frame: {}", err_string(&error)))?;
        decode_qr_image_data(&image_data)
    }
}

fn decode_qr_image_data(image_data: &web_sys::ImageData) -> Result<Option<String>, String> {
    let decoder = jsqr_function().ok_or_else(|| "jsQR decoder is unavailable".to_owned())?;
    let data = js_sys::Reflect::get(image_data, &JsValue::from_str("data"))
        .map_err(|error| format!("failed to read ImageData pixels: {}", err_string(&error)))?;
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &options,
        &JsValue::from_str("inversionAttempts"),
        &JsValue::from_str("attemptBoth"),
    );
    let args = js_sys::Array::new();
    args.push(&data);
    args.push(&JsValue::from_f64(f64::from(image_data.width())));
    args.push(&JsValue::from_f64(f64::from(image_data.height())));
    args.push(&options);
    let result = js_sys::Reflect::apply(&decoder, &JsValue::UNDEFINED, &args)
        .map_err(|error| format!("jsQR decode failed: {}", err_string(&error)))?;
    if result.is_undefined() || result.is_null() {
        return Ok(None);
    }
    let data = js_sys::Reflect::get(&result, &JsValue::from_str("data"))
        .ok()
        .and_then(|data| data.as_string())
        .filter(|data| !data.is_empty());
    Ok(data)
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    #[test]
    fn mobile_jsqr_copy_matches_loader_source() {
        assert_eq!(
            include_str!("../../../vendor/jsqr.js"),
            include_str!("../../../../web/loader/vendor/jsqr.js"),
            "mobile-frontend/vendor/jsqr.js must stay identical to the loader's source-of-truth copy"
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn scan_capability_allows_jsqr_without_barcode_detector() {
        let capability = ScanCapability {
            secure_context: true,
            camera: true,
            barcode_detector: false,
            jsqr: true,
        };
        assert!(capability.scan_available());
        assert!(scan_unavailable_error(capability).is_none());
    }

    #[wasm_bindgen_test]
    fn scan_capability_requires_a_decoder() {
        let capability = ScanCapability {
            secure_context: true,
            camera: true,
            barcode_detector: false,
            jsqr: false,
        };
        let error = scan_unavailable_error(capability).expect("missing decoder must reject scan");
        assert!(error.contains("No QR decoder is available"), "{error}");
    }

    #[wasm_bindgen_test]
    fn decode_qr_image_data_uses_global_jsqr() {
        let global = js_sys::global();
        let key = JsValue::from_str("jsQR");
        let had_previous = js_sys::Reflect::has(&global, &key).unwrap_or(false);
        let previous = if had_previous {
            js_sys::Reflect::get(&global, &key).ok()
        } else {
            None
        };

        const DECODED: &str = "https://tycode.dev/tyde/#tyde-pair://fixture";
        let decoder = Closure::<dyn FnMut(JsValue, f64, f64, JsValue) -> JsValue>::new(
            |data: JsValue, width: f64, height: f64, options: JsValue| {
                assert!(!data.is_undefined());
                assert_eq!(width, 1.0);
                assert_eq!(height, 1.0);
                let inversion =
                    js_sys::Reflect::get(&options, &JsValue::from_str("inversionAttempts"))
                        .expect("inversionAttempts option")
                        .as_string();
                assert_eq!(inversion.as_deref(), Some("attemptBoth"));
                let result = js_sys::Object::new();
                js_sys::Reflect::set(
                    &result,
                    &JsValue::from_str("data"),
                    &JsValue::from_str(DECODED),
                )
                .expect("set decoded data");
                result.into()
            },
        );
        js_sys::Reflect::set(&global, &key, decoder.as_ref()).expect("install jsQR");

        let image_data = web_sys::ImageData::new_with_sw(1, 1).expect("image data");
        let decoded = decode_qr_image_data(&image_data)
            .expect("decode should not error")
            .expect("decode should return a value");
        assert_eq!(decoded, DECODED);

        if let Some(previous) = previous {
            js_sys::Reflect::set(&global, &key, &previous).expect("restore jsQR");
        } else {
            js_sys::Reflect::delete_property(&global, &key).expect("delete jsQR");
        }
    }
}
