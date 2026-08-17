//! DOM → byte-offset primitives shared by the code-intel surfaces.
//!
//! Code intelligence addresses everything by absolute file byte offset
//! (`dev-docs/24-code-intelligence.md` §2.2), but the browser hands us a caret
//! as a (text node, UTF-16 offset) pair. Turning one into the other is the same
//! work in the file viewer and the diff viewer, so it lives here rather than
//! being duplicated (or exported) out of `components/file_view.rs`.
//!
//! What differs per surface is only *row resolution* — which element is a row,
//! which attribute carries its line identity, and which element wraps the
//! rendered code. Each surface keeps its own resolver and calls into these
//! primitives for the conversion.

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

/// Keeps a pending `setTimeout` handle and its callback alive until it fires,
/// is replaced, or the owning view remounts. Both code-intel surfaces debounce
/// hover the same way.
pub type TimeoutClosureSlot = StoredValue<Option<(i32, Closure<dyn FnMut()>)>, LocalStorage>;

pub fn clear_timeout_timer(timer: TimeoutClosureSlot) {
    timer.update_value(|slot| {
        if let Some((id, _cb)) = slot.take()
            && let Some(window) = web_sys::window()
        {
            window.clear_timeout_with_handle(id);
        }
    });
}

/// A resolved caret: the text node + UTF-16 offset under a point, plus an
/// optional client rect for anchoring a popover. Unifies the two browser APIs
/// (`caretPositionFromPoint` and WebKit's `caretRangeFromPoint`).
pub struct CaretHit {
    pub node: web_sys::Node,
    pub offset: u32,
    pub rect: Option<web_sys::DomRect>,
}

/// Byte offset within `line` of UTF-16 column `utf16_col`. This is the inverse
/// of walking a line's chars accumulating UTF-16 widths — the same conversion
/// the server's `LineIndex` does, kept here so a click's DOM caret column maps
/// to a Tyde byte offset. Multibyte-safe: a column past the line end clamps to
/// the line's byte length, and a column landing between surrogate halves is not
/// representable from a real caret so it never slices mid-char.
pub fn line_byte_for_utf16_col(line: &str, utf16_col: u32) -> u32 {
    let mut seen = 0u32;
    for (byte, ch) in line.char_indices() {
        if seen >= utf16_col {
            return byte as u32;
        }
        seen += ch.len_utf16() as u32;
    }
    line.len() as u32
}

/// All descendant text nodes of `root`, in document order.
pub fn descendant_text_nodes(root: &web_sys::Node) -> Vec<web_sys::Node> {
    let mut out = Vec::new();
    let children = root.child_nodes();
    for i in 0..children.length() {
        let Some(child) = children.item(i) else {
            continue;
        };
        if child.node_type() == web_sys::Node::TEXT_NODE {
            out.push(child);
        } else {
            out.extend(descendant_text_nodes(&child));
        }
    }
    out
}

/// The UTF-16 column within a line's rendered code element for a caret at
/// (`target` text node, `target_offset`). Sums the UTF-16 lengths of the text
/// nodes preceding `target` (the line may be split into many colored / squiggle
/// spans) and adds the in-node offset. `None` if the caret isn't on one of the
/// code element's text nodes.
///
/// `code` must wrap *only* the line's code text. Both surfaces render sibling
/// chrome next to the code — the file viewer's line-number gutter, the diff
/// viewer's `+`/`-` prefix — and counting those would shift every column.
pub fn utf16_col_in_code(
    code: &web_sys::Node,
    target: &web_sys::Node,
    target_offset: u32,
) -> Option<u32> {
    let mut acc = 0u32;
    for text_node in descendant_text_nodes(code) {
        if text_node.is_same_node(Some(target)) {
            return Some(acc + target_offset);
        }
        let len = text_node
            .text_content()
            .unwrap_or_default()
            .encode_utf16()
            .count() as u32;
        acc += len;
    }
    None
}

/// Map an `Element`-or-text `Node` to its containing `Element`.
pub fn node_to_element(node: &web_sys::Node) -> Option<web_sys::Element> {
    if let Some(element) = node.dyn_ref::<web_sys::Element>() {
        return Some(element.clone());
    }
    node.parent_element()
}

/// Call WebKit's non-standard `caretRangeFromPoint`, which `web-sys` does not
/// bind. Tyde runs in WKWebView, which historically ships `caretRangeFromPoint`
/// but **not** the standard `caretPositionFromPoint`. Invoked via `Reflect`
/// (a `#[wasm_bindgen] method` on the foreign `Document` type isn't allowed),
/// only after `document_has_method` confirms it exists.
fn caret_range_from_point(document: &web_sys::Document, x: f64, y: f64) -> Option<web_sys::Range> {
    let func = js_sys::Reflect::get(
        document,
        &wasm_bindgen::JsValue::from_str("caretRangeFromPoint"),
    )
    .ok()?
    .dyn_into::<js_sys::Function>()
    .ok()?;
    func.call2(
        document.as_ref(),
        &wasm_bindgen::JsValue::from_f64(x),
        &wasm_bindgen::JsValue::from_f64(y),
    )
    .ok()?
    .dyn_into::<web_sys::Range>()
    .ok()
}

/// Whether `document` exposes a callable method `name` (walks the prototype
/// chain). Used to feature-detect the caret API so we never call a method that
/// doesn't exist (which would throw in WKWebView).
fn document_has_method(document: &web_sys::Document, name: &str) -> bool {
    js_sys::Reflect::get(document, &wasm_bindgen::JsValue::from_str(name))
        .map(|value| value.is_function())
        .unwrap_or(false)
}

/// The caret under a viewport point. Prefers the standard
/// `caretPositionFromPoint` (Chromium/Firefox); falls back to WebKit's
/// `caretRangeFromPoint` (WKWebView). `None` if neither API exists or the point
/// isn't over text — both degrade gracefully to "click does nothing".
pub fn caret_at_point(client_x: f64, client_y: f64) -> Option<CaretHit> {
    let document = web_sys::window()?.document()?;
    if document_has_method(&document, "caretPositionFromPoint") {
        let caret = document.caret_position_from_point(client_x as f32, client_y as f32)?;
        return Some(CaretHit {
            node: caret.offset_node()?,
            offset: caret.offset(),
            rect: caret.get_client_rect(),
        });
    }
    if document_has_method(&document, "caretRangeFromPoint") {
        let range = caret_range_from_point(&document, client_x, client_y)?;
        return Some(CaretHit {
            node: range.start_container().ok()?,
            offset: range.start_offset().ok()?,
            rect: Some(range.get_bounding_client_rect()),
        });
    }
    None
}

/// Whether the char at `line_byte` in `line` begins an identifier-ish token
/// (alphanumeric or `_`). Used to gate hover requests so we don't pop a hover
/// over whitespace / punctuation.
pub fn is_identifier_byte(line: &str, line_byte: u32) -> bool {
    line.get(line_byte as usize..)
        .and_then(|rest| rest.chars().next())
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false)
}
