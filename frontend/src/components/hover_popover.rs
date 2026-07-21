//! The code-intelligence hover popover (M2).
//!
//! A pure Leptos overlay — **no** `window.confirm`/`alert`/`prompt` and no
//! imperative DOM popover (root `CLAUDE.md`). It renders from the
//! `code_intel_hover` signal: when a hover result with markdown lands, the
//! `state.code_intel_hover` popover is positioned over the hovered span (using
//! the viewport-relative anchor rect captured when the request fired) and its
//! markdown is rendered with the shared `render_markdown` (pulldown-cmark).
//!
//! Diagnostics under the hovered offset are merged in *above* the hover
//! markdown — the squiggle itself carries no readable message, so this popover
//! is where the user reads what is wrong. They render as soon as the popover is
//! seeded (no round-trip needed: the diagnostics are already client-side), and
//! they keep the popover alive even when the LSP hover comes back empty.
//!
//! With neither diagnostics nor markdown the popover renders nothing (no empty
//! box while a hover request is in flight), and nothing once dismissed
//! (`code_intel_hover == None`).

use leptos::prelude::*;
use protocol::CodeIntelSeverity;

use crate::markdown::render_markdown;
use crate::state::{AppState, CodeIntelKey};

/// Approximate popover height budget used to decide whether to place the
/// popover above or below the hovered span. Purely a placement heuristic; the
/// box itself is sized by its content + CSS `max-height`.
const ESTIMATED_POPOVER_HEIGHT: f64 = 220.0;

fn severity_label(severity: CodeIntelSeverity) -> &'static str {
    match severity {
        CodeIntelSeverity::Error => "Error",
        CodeIntelSeverity::Warning => "Warning",
        CodeIntelSeverity::Information => "Info",
        CodeIntelSeverity::Hint => "Hint",
    }
}

fn severity_class(severity: CodeIntelSeverity) -> &'static str {
    match severity {
        CodeIntelSeverity::Error => "error",
        CodeIntelSeverity::Warning => "warning",
        CodeIntelSeverity::Information => "information",
        CodeIntelSeverity::Hint => "hint",
    }
}

#[component]
pub fn HoverPopover() -> impl IntoView {
    let state = expect_context::<AppState>();
    let hover = state.code_intel_hover;
    let code_intel = state.code_intel;

    move || {
        hover.with(|current| {
            let popover = current.as_ref()?;

            // Diagnostics under the hovered offset, at the rendered version.
            // Already client-side, so they render without waiting for the
            // hover round-trip.
            let code_intel_key = CodeIntelKey {
                host_id: popover.key.host_id.clone(),
                project_id: popover.key.project_id.clone(),
                path: popover.key.path.clone(),
            };
            let diagnostics = code_intel.with(|map| {
                map.get(&code_intel_key)
                    .map(|file| file.diagnostics_at(popover.version, popover.offset))
                    .unwrap_or_default()
            });
            let hover_html = popover.contents.as_ref().map(|c| render_markdown(c));
            if diagnostics.is_empty() && hover_html.is_none() {
                // Request in flight and nothing to show yet: no empty flash.
                return None;
            }

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

            let diagnostic_views = diagnostics
                .iter()
                .map(|diagnostic| {
                    let row_class = format!(
                        "code-intel-hover-diagnostic code-intel-hover-diagnostic--{}",
                        severity_class(diagnostic.severity)
                    );
                    let label = severity_label(diagnostic.severity);
                    let source = diagnostic
                        .source
                        .as_ref()
                        .map(|source| format!(" · {source}"));
                    let message = diagnostic.message.clone();
                    view! {
                        <div class=row_class data-code-intel-hover-diagnostic="true">
                            <span class="code-intel-hover-diagnostic-severity">
                                {label}
                                {source}
                            </span>
                            <div class="code-intel-hover-diagnostic-message">{message}</div>
                        </div>
                    }
                })
                .collect::<Vec<_>>();

            Some(view! {
                <div
                    class="code-intel-hover-popover"
                    data-code-intel-hover-popover="true"
                    style=style
                >
                    {diagnostic_views}
                    {hover_html.map(|html| {
                        view! {
                            <div class="code-intel-hover-markdown" inner_html=html></div>
                        }
                    })}
                </div>
            })
        })
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{
        FileResourceKey, HoverPopover as HoverPopoverState, OpenTarget, TabContent, TabId,
    };
    use leptos::mount::mount_to;
    use protocol::{
        ByteRange, CodeIntelDiagnostic, ProjectFileVersion, ProjectId, ProjectPath, ProjectRootPath,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn fixture_path() -> ProjectPath {
        ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "main.rs".to_owned(),
        }
    }

    fn resource_key() -> FileResourceKey {
        FileResourceKey {
            host_id: "h".to_owned(),
            project_id: ProjectId("p".to_owned()),
            path: fixture_path(),
        }
    }

    /// State with one open tab, a rendered-version-1 diagnostic covering bytes
    /// 4..10 ("mismatched types"), and a seeded popover at offset 5 with the
    /// given hover contents. The popover is seeded *after* the tab opens (tab
    /// activation dismisses popovers by design).
    fn seeded_state(contents: Option<String>) -> (AppState, TabId) {
        let state = AppState::new();
        let key = resource_key();
        let tab = state
            .open_tab_at(
                OpenTarget::Focused,
                TabContent::File { key: key.clone() },
                "main.rs".to_owned(),
                true,
            )
            .expect("fixture tab");
        state.code_intel.update(|map| {
            let file = map
                .entry(CodeIntelKey {
                    host_id: key.host_id.clone(),
                    project_id: key.project_id.clone(),
                    path: key.path.clone(),
                })
                .or_default();
            file.set_rendered_version(ProjectFileVersion(1));
            file.merge_versioned(ProjectFileVersion(1), |data| {
                data.diagnostics = vec![CodeIntelDiagnostic {
                    range: ByteRange { start: 4, end: 10 },
                    severity: CodeIntelSeverity::Error,
                    message: "mismatched types".to_owned(),
                    source: Some("rustc".to_owned()),
                }];
            });
        });
        state.code_intel_hover.set(Some(HoverPopoverState {
            hover_id: 1,
            tab,
            key,
            version: ProjectFileVersion(1),
            offset: 5,
            anchor_left: 20.0,
            anchor_top: 30.0,
            anchor_bottom: 46.0,
            contents,
        }));
        (state, tab)
    }

    fn popover_text(container: &HtmlElement) -> Option<String> {
        container
            .query_selector("[data-code-intel-hover-popover=\"true\"]")
            .unwrap()
            .and_then(|el| el.text_content())
    }

    /// B1: hovering a flagged token whose LSP hover is empty must still show
    /// the diagnostic message — the squiggle itself is unreadable, and this
    /// popover is the only place the user can read what is wrong.
    #[wasm_bindgen_test]
    async fn popover_shows_diagnostic_message_without_hover_contents() {
        let container = make_container();
        let (state, _tab) = seeded_state(None);
        let _handle = mount_to(container.clone(), move || {
            provide_context(state);
            view! { <HoverPopover /> }
        });
        next_tick().await;

        let text = popover_text(&container).expect("diagnostic-only popover renders");
        assert!(
            text.contains("mismatched types"),
            "the diagnostic message must be readable; got {text:?}"
        );
        assert!(
            text.contains("Error"),
            "the severity must be visible; got {text:?}"
        );
        assert!(
            text.contains("rustc"),
            "the diagnostic source must be visible; got {text:?}"
        );
    }

    /// B1: when the LSP hover has markdown, the diagnostic message renders
    /// alongside it (not instead of it).
    #[wasm_bindgen_test]
    async fn popover_merges_diagnostics_with_hover_markdown() {
        let container = make_container();
        let (state, _tab) = seeded_state(Some("**Type**: `i32`".to_owned()));
        let _handle = mount_to(container.clone(), move || {
            provide_context(state);
            view! { <HoverPopover /> }
        });
        next_tick().await;

        let text = popover_text(&container).expect("popover renders");
        assert!(
            text.contains("mismatched types") && text.contains("i32"),
            "both the diagnostic and the hover contents must render; got {text:?}"
        );
    }

    /// C1a: a press anywhere in the app dismisses the popover — including
    /// presses on elements that stop propagation (the listener is capture-
    /// phase), so a stale popover can never float over unrelated content.
    #[wasm_bindgen_test]
    async fn any_mousedown_dismisses_the_popover() {
        let container = make_container();
        let (state, _tab) = seeded_state(None);
        let state_for_listener = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state);
            view! { <HoverPopover /> }
        });
        next_tick().await;
        assert!(popover_text(&container).is_some(), "popover starts visible");

        crate::app::install_hover_dismiss_listeners(state_for_listener.clone());
        let document = web_sys::window().unwrap().document().unwrap();
        let event = web_sys::MouseEvent::new("mousedown").unwrap();
        document
            .body()
            .unwrap()
            .dispatch_event(&event)
            .expect("dispatch mousedown");
        next_tick().await;

        assert!(
            state_for_listener
                .code_intel_hover
                .get_untracked()
                .is_none(),
            "a press anywhere must dismiss the popover state"
        );
        assert!(
            popover_text(&container).is_none(),
            "the popover element must be gone after the press"
        );
        crate::app::clear_app_listeners();
    }
}
