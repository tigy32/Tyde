use leptos::prelude::*;

use crate::state::AppState;

/// One stop on the guided tour. `selector` locates the live UI element to
/// spotlight; when it isn't on screen (panel hidden, different view) the
/// callout falls back to the center of the screen with a hint about how to
/// reveal the panel.
struct TourStep {
    title: &'static str,
    body: &'static str,
    selector: &'static str,
    hidden_hint: &'static str,
}

static STEPS: [TourStep; 5] = [
    TourStep {
        title: "Your projects",
        body: "Each icon in the left rail is a project — a folder (usually a \
               codebase) that agents can read and edit. Click one to switch \
               to it, and use the + at the bottom to add a folder as a new \
               project.",
        selector: ".project-rail",
        hidden_hint: "",
    },
    TourStep {
        title: "Start a chat",
        body: "New Chat opens a conversation with an agent inside the current \
               project. Use the ▾ to pick a backend (Claude, Codex, Gemini…) \
               or a custom agent, then describe a task in plain language.",
        selector: ".home-actions",
        hidden_hint: "The New Chat button lives on the home screen and at the \
                      top of every project.",
    },
    TourStep {
        title: "Files and changes",
        body: "The left panel shows the open project's files and its git \
               status — browse the code and review every change an agent \
               makes, file by file.",
        selector: ".dock-left",
        hidden_hint: "This panel is hidden right now — the Left button in the \
                      header toggles it.",
    },
    TourStep {
        title: "Agents, history, and teams",
        body: "The right panel lists every agent in the open project — \
               running or finished — plus past sessions and agent teams. Run \
               several agents at once and jump between them here.",
        selector: ".dock-right",
        hidden_hint: "This panel is hidden right now — the Right button in \
                      the header toggles it.",
    },
    TourStep {
        title: "Terminals",
        body: "The bottom dock holds terminals: backend installs and sign-ins \
               run here, and agents can open shells you can watch. Tip: ⌘K \
               opens the command palette — type what you want to do.",
        selector: ".dock-bottom",
        hidden_hint: "This dock is hidden right now — the Bottom button in \
                      the header toggles it.",
    },
];

/// Viewport-relative box of the spotlighted element, if it is meaningfully
/// visible.
fn target_rect(selector: &str) -> Option<(f64, f64, f64, f64)> {
    let document = web_sys::window()?.document()?;
    let element = document.query_selector(selector).ok()??;
    let rect = element.get_bounding_client_rect();
    if rect.width() < 8.0 || rect.height() < 8.0 {
        return None;
    }
    Some((rect.x(), rect.y(), rect.width(), rect.height()))
}

fn viewport_size() -> (f64, f64) {
    let Some(window) = web_sys::window() else {
        return (1024.0, 768.0);
    };
    let width = window
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0);
    let height = window
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(768.0);
    (width, height)
}

const CARD_WIDTH: f64 = 340.0;
const CARD_EST_HEIGHT: f64 = 220.0;
const GAP: f64 = 16.0;

/// Place the callout card beside the spotlight: to the side with the most
/// room for tall targets, above/below for wide ones, clamped to the viewport.
fn card_position(rect: (f64, f64, f64, f64)) -> (f64, f64) {
    let (x, y, w, h) = rect;
    let (vw, vh) = viewport_size();
    let clamp = |value: f64, max: f64| value.max(GAP).min((max).max(GAP));

    let tall = h > w;
    if tall {
        let left = if x + w / 2.0 < vw / 2.0 {
            x + w + GAP
        } else {
            x - CARD_WIDTH - GAP
        };
        let top = y + h / 2.0 - CARD_EST_HEIGHT / 2.0;
        (
            clamp(left, vw - CARD_WIDTH - GAP),
            clamp(top, vh - CARD_EST_HEIGHT - GAP),
        )
    } else {
        let top = if y + h / 2.0 < vh / 2.0 {
            y + h + GAP
        } else {
            y - CARD_EST_HEIGHT - GAP
        };
        let left = x + w / 2.0 - CARD_WIDTH / 2.0;
        (
            clamp(left, vw - CARD_WIDTH - GAP),
            clamp(top, vh - CARD_EST_HEIGHT - GAP),
        )
    }
}

/// Full-screen guided tour overlay. Mounted once at the app root; renders
/// nothing until `state.help_tour_step` is set (the Help button on the home
/// screen starts it at step 0).
#[component]
pub fn HelpTour() -> impl IntoView {
    let state = expect_context::<AppState>();

    move || {
        let step_index = state.help_tour_step.get()?;
        let step = STEPS.get(step_index)?;
        let rect = target_rect(step.selector);

        let spotlight = rect.map(|(x, y, w, h)| {
            let style = format!(
                "left:{}px;top:{}px;width:{}px;height:{}px;",
                x - 4.0,
                y - 4.0,
                w + 8.0,
                h + 8.0
            );
            view! { <div class="help-tour-spotlight" style=style></div> }
        });

        let card_style = match rect {
            Some(r) => {
                let (left, top) = card_position(r);
                format!("left:{left}px;top:{top}px;width:{CARD_WIDTH}px;")
            }
            None => {
                format!("left:50%;top:50%;transform:translate(-50%,-50%);width:{CARD_WIDTH}px;")
            }
        };

        let hidden_hint =
            (rect.is_none() && !step.hidden_hint.is_empty()).then_some(step.hidden_hint);

        let is_first = step_index == 0;
        let is_last = step_index + 1 == STEPS.len();

        let close_state = state.clone();
        let on_close = move |_| close_state.help_tour_step.set(None);
        let back_state = state.clone();
        let on_back = move |_| {
            back_state
                .help_tour_step
                .set(Some(step_index.saturating_sub(1)));
        };
        let next_state = state.clone();
        let on_next = move |_| {
            if is_last {
                next_state.help_tour_step.set(None);
            } else {
                next_state.help_tour_step.set(Some(step_index + 1));
            }
        };

        Some(view! {
            <div class="help-tour">
                <Show when=move || rect.is_none()>
                    <div class="help-tour-backdrop"></div>
                </Show>
                {spotlight}
                <div class="help-tour-card" style=card_style>
                    <div class="help-tour-card-top">
                        <span class="help-tour-progress">
                            {format!("{} of {}", step_index + 1, STEPS.len())}
                        </span>
                        <button class="help-tour-close" title="Close tour" on:click=on_close>
                            "×"
                        </button>
                    </div>
                    <h3 class="help-tour-title">{step.title}</h3>
                    <p class="help-tour-body">{step.body}</p>
                    {hidden_hint.map(|hint| view! { <p class="help-tour-hidden-hint">{hint}</p> })}
                    <div class="help-tour-card-actions">
                        <Show when=move || !is_first>
                            <button class="action-btn help-tour-back" on:click=on_back>
                                "Back"
                            </button>
                        </Show>
                        <button class="action-btn primary" on:click=on_next>
                            {if is_last { "Done" } else { "Next" }}
                        </button>
                    </div>
                </div>
            </div>
        })
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
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

    /// Yield to the browser event loop so reactive effects flush and the DOM
    /// reflects the rendered view before we assert on it.
    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn visible_text(container: &HtmlElement) -> String {
        container.text_content().unwrap_or_default()
    }

    fn click_button(container: &HtmlElement, label: &str) {
        let nodes = container.query_selector_all("button").unwrap();
        let button = (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|b| b.text_content().unwrap_or_default().trim() == label)
            .unwrap_or_else(|| {
                panic!("button {label:?} not found in: {}", visible_text(container))
            });
        button.click();
    }

    /// The tour walks through every step with Next/Back, shows progress, and
    /// closes via Done, clearing the trigger signal.
    #[wasm_bindgen_test]
    async fn tour_steps_forward_back_and_completes() {
        let container = make_container();
        let state = AppState::new();
        state.help_tour_step.set(Some(0));
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <HelpTour /> }
        });
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.contains("Your projects") && text.contains("1 of 5"),
            "tour must open on step 1: {text}"
        );

        click_button(&container, "Next");
        next_tick().await;
        let text = visible_text(&container);
        assert!(
            text.contains("Start a chat") && text.contains("2 of 5"),
            "Next must advance to step 2: {text}"
        );

        click_button(&container, "Back");
        next_tick().await;
        assert!(
            visible_text(&container).contains("1 of 5"),
            "Back must return to step 1"
        );

        for _ in 0..4 {
            click_button(&container, "Next");
            next_tick().await;
        }
        let text = visible_text(&container);
        assert!(
            text.contains("5 of 5") && text.contains("Terminals"),
            "tour must reach the last step: {text}"
        );

        click_button(&container, "Done");
        next_tick().await;
        assert_eq!(
            state.help_tour_step.get_untracked(),
            None,
            "Done must close the tour"
        );
        assert!(
            visible_text(&container).trim().is_empty(),
            "closed tour must render nothing"
        );
    }

    /// The × control closes the tour from any step.
    #[wasm_bindgen_test]
    async fn tour_close_button_dismisses() {
        let container = make_container();
        let state = AppState::new();
        state.help_tour_step.set(Some(2));
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <HelpTour /> }
        });
        next_tick().await;

        assert!(visible_text(&container).contains("3 of 5"));
        click_button(&container, "×");
        next_tick().await;
        assert_eq!(state.help_tour_step.get_untracked(), None);
    }
}
