//! The code-intelligence hover popover (M2).
//!
//! A pure Leptos overlay — **no** `window.confirm`/`alert`/`prompt` and no
//! imperative DOM popover (root `CLAUDE.md`). It renders from the
//! `code_intel_hover` signal: when a hover result with markdown lands, the
//! `state.code_intel_hover` popover is positioned over the hovered span (using
//! the viewport-relative anchor rect captured when the request fired) and its
//! markdown is rendered with the shared `render_markdown` (pulldown-cmark).
//!
//! It renders nothing while a request is in flight (`contents: None`) so there
//! is no empty-box flash, and nothing once the hover is dismissed
//! (`code_intel_hover == None`).

use leptos::prelude::*;

use crate::markdown::render_markdown;
use crate::state::AppState;

/// Approximate popover height budget used to decide whether to place the
/// popover above or below the hovered span. Purely a placement heuristic; the
/// box itself is sized by its content + CSS `max-height`.
const ESTIMATED_POPOVER_HEIGHT: f64 = 220.0;

#[component]
pub fn HoverPopover() -> impl IntoView {
    let state = expect_context::<AppState>();
    let hover = state.code_intel_hover;

    move || {
        hover.with(|current| {
            let popover = current.as_ref()?;
            // Render only once real markdown has arrived (no empty flash while
            // the request is in flight).
            let contents = popover.contents.as_ref()?;
            let html = render_markdown(contents);

            // Prefer placing the popover below the span; flip above when there
            // isn't room beneath it in the viewport.
            let viewport_height = web_sys::window()
                .and_then(|w| w.inner_height().ok())
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let space_below = viewport_height - popover.anchor_bottom;
            let style =
                if space_below < ESTIMATED_POPOVER_HEIGHT && popover.anchor_top > space_below {
                    // Anchor the popover's bottom to the span's top (grows upward).
                    format!(
                        "position: fixed; left: {:.0}px; bottom: {:.0}px;",
                        popover.anchor_left.max(0.0),
                        (viewport_height - popover.anchor_top).max(0.0),
                    )
                } else {
                    format!(
                        "position: fixed; left: {:.0}px; top: {:.0}px;",
                        popover.anchor_left.max(0.0),
                        popover.anchor_bottom,
                    )
                };

            Some(view! {
                <div
                    class="code-intel-hover-popover"
                    data-code-intel-hover-popover="true"
                    style=style
                    inner_html=html
                ></div>
            })
        })
    }
}
