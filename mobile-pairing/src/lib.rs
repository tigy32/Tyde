mod reconnect;
mod types;
pub use reconnect::{RECONNECT_INITIAL, RECONNECT_MAX, ReconnectBackoff, ReconnectBackoffError};
pub use types::*;

/// Desktop pairing QR producer, shared with the real scanner boundary flow.
pub fn render_pairing_qr_svg(qr_uri: &str) -> Option<String> {
    let qr = qrcodegen::QrCode::encode_text(qr_uri, qrcodegen::QrCodeEcc::Medium).ok()?;
    // `qrcodegen` ships the module bitmap but no SVG writer (their
    // demo crate provides one). Implement a tiny SVG emitter inline.
    // border=2 keeps the quiet zone the spec requires. Fill/stroke
    // are CSS-overridable via `.settings-mobile-pairing-qr rect`.
    let border: i32 = 2;
    let size = qr.size();
    let dim = size + border * 2;
    use std::fmt::Write;
    let mut out = String::with_capacity((size * size) as usize * 32);
    let _ = write!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {dim} {dim}" stroke="none" shape-rendering="crispEdges"><rect width="100%" height="100%" fill="#ffffff"/>"##,
    );
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                let _ = write!(
                    out,
                    r##"<rect x="{}" y="{}" width="1" height="1" fill="#000000"/>"##,
                    x + border,
                    y + border,
                );
            }
        }
    }
    out.push_str("</svg>");
    Some(out)
}
