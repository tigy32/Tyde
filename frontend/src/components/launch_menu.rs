use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::actions::begin_new_chat_with_profile;
use crate::state::{AppState, DEFAULT_CUSTOM_AGENT_ID};

use protocol::{BackendKind, CustomAgent, LaunchProfileEntry, LaunchProfileId, LaunchProfileKind};

fn backend_label_for(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
    }
}

/// The subtle backend tag shown on the right of a launch-profile row, or `None`
/// when it would be redundant. Backend-default rows never show it — their label
/// already *is* the backend name — so we don't render e.g. "Claude   CLAUDE".
/// Custom rows show it only when their label differs from the backend identity,
/// so a custom profile that happens to be named after its backend also stays
/// clean. Driven entirely by the server-provided `kind`, never inferred from
/// the label string.
fn backend_tag_for(
    kind: LaunchProfileKind,
    backend: BackendKind,
    label: &str,
) -> Option<&'static str> {
    match kind {
        LaunchProfileKind::BackendDefault => None,
        LaunchProfileKind::Custom => {
            let name = backend_label_for(backend);
            if label.trim().eq_ignore_ascii_case(name) {
                None
            } else {
                Some(name)
            }
        }
    }
}

const ITEM_STYLE: &str = "display:flex;align-items:center;gap:0.5rem;width:100%;text-align:left;padding:0.4rem 0.6rem;background:transparent;border:none;color:inherit;cursor:pointer;border-radius:4px;white-space:nowrap;";
const SUBITEM_STYLE: &str = "display:block;width:100%;text-align:left;padding:0.4rem 0.8rem;background:transparent;border:none;color:inherit;cursor:pointer;border-radius:4px;white-space:nowrap;";
const BADGE_STYLE: &str =
    "font-size:0.65rem;opacity:0.6;text-transform:uppercase;letter-spacing:0.03em;";
const CHEVRON_STYLE: &str = "opacity:0.5;font-size:0.7rem;";
// Top-aligned with the hovered row (`top:0`) so the flyout reads as attached to
// the row rather than floating above it. The 2px horizontal overlap tucks the
// flyout under the parent's border so there is no visible gap on either side.
const SUBMENU_STYLE_RIGHT: &str = "position:absolute;left:100%;top:0;margin-left:-2px;background:var(--bg-surface,#1e1e1e);border:1px solid var(--border-subtle,#333);border-radius:6px;padding:0.5rem;z-index:101;box-shadow:0 4px 16px rgba(0,0,0,0.4);white-space:nowrap;";
const SUBMENU_STYLE_LEFT: &str = "position:absolute;right:100%;top:0;margin-right:-2px;background:var(--bg-surface,#1e1e1e);border:1px solid var(--border-subtle,#333);border-radius:6px;padding:0.5rem;z-index:101;box-shadow:0 4px 16px rgba(0,0,0,0.4);white-space:nowrap;";

/// Which side a row's custom-agent submenu expands toward. Positioning is a
/// pure presentation concern owned by the caller, not derived from any server
/// state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubmenuAlign {
    /// Always open to the right of the row (`left:100%`).
    Right,
    /// Always open to the left of the row (`right:100%`).
    Left,
    /// Prefer the natural right-side flyout, but flip to the left when the row
    /// sits too close to the viewport's right edge for the flyout to fit. Used
    /// by the chat-top New Chat dropdown, which is right-anchored: with room it
    /// reads as a normal attached flyout, and near the edge it stays on-screen
    /// and cleanly attached to the row's left instead of floating off-screen.
    Auto,
}

/// Width we assume a custom-agent flyout needs before it would clip the right
/// edge. Only used to decide the flip side in [`SubmenuAlign::Auto`]; the flyout
/// itself is content-sized. A generous estimate biases toward flipping left one
/// row early rather than letting the flyout spill off-screen.
const SUBMENU_ESTIMATED_WIDTH: f64 = 240.0;

fn submenu_style_for(align: SubmenuAlign) -> &'static str {
    match align {
        SubmenuAlign::Left => SUBMENU_STYLE_LEFT,
        // `Auto` is resolved to Left/Right before this is read; default the
        // unresolved case to the natural right-side flyout.
        SubmenuAlign::Right | SubmenuAlign::Auto => SUBMENU_STYLE_RIGHT,
    }
}

fn chevron_for(align: SubmenuAlign) -> &'static str {
    match align {
        SubmenuAlign::Left => "◀",
        SubmenuAlign::Right | SubmenuAlign::Auto => "▶",
    }
}

/// Resolve an [`SubmenuAlign::Auto`] row to a concrete side by measuring where
/// the hovered row sits in the viewport. Opens right when the flyout fits, else
/// flips left so it never spills off the right edge.
fn resolve_auto_align(ev: &web_sys::MouseEvent) -> SubmenuAlign {
    let Some(target) = ev.current_target() else {
        return SubmenuAlign::Left;
    };
    let Ok(row) = target.dyn_into::<web_sys::Element>() else {
        return SubmenuAlign::Left;
    };
    let Some(viewport_width) = web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
    else {
        return SubmenuAlign::Left;
    };
    let room_to_right = viewport_width - row.get_bounding_client_rect().right();
    if room_to_right >= SUBMENU_ESTIMATED_WIDTH {
        SubmenuAlign::Right
    } else {
        SubmenuAlign::Left
    }
}

/// Shared new-chat launch menu body used by both the Home tab dropdown and the
/// chat-top New Chat split button. Renders the server-owned launch profile
/// catalog for the current chat context host: ready profiles are clickable and
/// expand a side submenu of custom agents; unavailable profiles render disabled
/// with the server-provided reason. The caller owns the surrounding popover
/// container; this component only renders the rows and closes `open_sig` on a
/// successful selection. `submenu_align` lets the caller open the side submenu
/// toward the viewport interior.
#[component]
pub fn LaunchMenuBody(open_sig: RwSignal<bool>, submenu_align: SubmenuAlign) -> impl IntoView {
    let state = expect_context::<AppState>();
    let open_profile = RwSignal::new(None::<LaunchProfileId>);
    // The concrete side the flyout currently opens toward. Fixed for
    // `Right`/`Left`; for `Auto` it is re-measured each time a row is hovered so
    // it flips only when the row is too close to the right edge. Chevron and
    // flyout style both read this, so the arrow can never disagree with the side
    // the flyout actually opens.
    let resolved_align = RwSignal::new(match submenu_align {
        SubmenuAlign::Left => SubmenuAlign::Left,
        SubmenuAlign::Right | SubmenuAlign::Auto => SubmenuAlign::Right,
    });

    let entries_state = state.clone();
    let entries = Memo::new(move |_| {
        let Some(host_id) = entries_state.chat_context_host_id() else {
            return Vec::new();
        };
        entries_state
            .launch_profile_catalog
            .get()
            .get(&host_id)
            .map(|catalog| catalog.entries.clone())
            .unwrap_or_default()
    });

    let agents_state = state.clone();
    let custom_agents_for_host = Memo::new(move |_| {
        let Some(host_id) = agents_state.chat_context_host_id() else {
            return Vec::<CustomAgent>::new();
        };
        let mut agents: Vec<CustomAgent> = agents_state
            .custom_agents
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        agents.retain(|a| a.id.0 != DEFAULT_CUSTOM_AGENT_ID);
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    });

    move || {
        let entries = entries.get();
        if entries.is_empty() {
            return view! {
                <div class="new-chat-menu-empty panel-empty">
                    "No launch profiles available. Enable a backend in Settings → Backends."
                </div>
            }
            .into_any();
        }

        let agents = custom_agents_for_host.get();
        let rows = entries
            .into_iter()
            .map(|entry| match entry {
                LaunchProfileEntry::Ready { profile } => {
                    let profile_id = profile.id.clone();
                    let backend = profile.backend_kind;

                    let on_row_click = {
                        let profile = profile.clone();
                        move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation();
                            open_sig.set(false);
                            let s = expect_context::<AppState>();
                            begin_new_chat_with_profile(&s, profile.clone(), None);
                        }
                    };

                    let agents_for_sub = agents.clone();
                    let submenu_profile = profile.clone();
                    let submenu_id = profile_id.clone();
                    let submenu = move || {
                        if open_profile.get() != Some(submenu_id.clone()) {
                            return None;
                        }
                        let on_default = {
                            let profile = submenu_profile.clone();
                            move |ev: web_sys::MouseEvent| {
                                ev.stop_propagation();
                                open_sig.set(false);
                                let s = expect_context::<AppState>();
                                begin_new_chat_with_profile(&s, profile.clone(), None);
                            }
                        };
                        let agent_items = agents_for_sub
                            .clone()
                            .into_iter()
                            .map(|agent| {
                                let profile = submenu_profile.clone();
                                let id = agent.id.clone();
                                let name = agent.name.clone();
                                let desc = agent.description.clone();
                                let on_click = move |ev: web_sys::MouseEvent| {
                                    ev.stop_propagation();
                                    open_sig.set(false);
                                    let s = expect_context::<AppState>();
                                    begin_new_chat_with_profile(
                                        &s,
                                        profile.clone(),
                                        Some(id.clone()),
                                    );
                                };
                                view! {
                                    <button
                                        class="new-chat-flyout-item"
                                        role="menuitem"
                                        style=SUBITEM_STYLE
                                        title=desc
                                        on:click=on_click
                                    >
                                        {name}
                                    </button>
                                }
                            })
                            .collect_view();
                        Some(
                            view! {
                                <div
                                    class="new-chat-submenu"
                                    role="menu"
                                    style=move || submenu_style_for(resolved_align.get())
                                >
                                    <button
                                        class="new-chat-flyout-item"
                                        role="menuitem"
                                        style=SUBITEM_STYLE
                                        on:click=on_default
                                    >
                                        "Default agent"
                                    </button>
                                    {agent_items}
                                </div>
                            }
                            .into_any(),
                        )
                    };

                    let hover_id = profile_id.clone();
                    let expanded_id = profile_id.clone();
                    let label = profile.label.clone();
                    let tag = backend_tag_for(profile.kind, backend, &label)
                        .map(|t| view! { <span style=BADGE_STYLE>{t}</span> });
                    view! {
                        <div
                            class="new-chat-backend-row-wrap"
                            style="position:relative;"
                            on:mouseenter=move |ev: web_sys::MouseEvent| {
                                open_profile.set(Some(hover_id.clone()));
                                if submenu_align == SubmenuAlign::Auto {
                                    resolved_align.set(resolve_auto_align(&ev));
                                }
                            }
                            on:mouseleave=move |_| open_profile.set(None)
                        >
                            <button
                                class="new-chat-flyout-item new-chat-menu-item"
                                role="menuitem"
                                aria-haspopup="menu"
                                aria-expanded=move || {
                                    if open_profile.get() == Some(expanded_id.clone()) {
                                        "true"
                                    } else {
                                        "false"
                                    }
                                }
                                style=ITEM_STYLE
                                on:click=on_row_click
                            >
                                <span>{label}</span>
                                <span style="flex:1;"></span>
                                {tag}
                                // Trailing arrow that points toward the side the
                                // flyout actually opens; reactive so `Auto` flips
                                // it in lock-step with the flyout.
                                <span style=CHEVRON_STYLE>
                                    {move || chevron_for(resolved_align.get())}
                                </span>
                            </button>
                            {submenu}
                        </div>
                    }
                    .into_any()
                }
                LaunchProfileEntry::Unavailable {
                    id: _,
                    kind,
                    backend_kind,
                    label,
                    message,
                } => {
                    let title = message.clone();
                    // Same padding/flex as the ready row so ready and
                    // unavailable entries align down the menu.
                    let tag = backend_tag_for(kind, backend_kind, &label)
                        .map(|t| view! { <span style=BADGE_STYLE>{t}</span> });
                    view! {
                        <div
                            class="new-chat-backend-row-wrap new-chat-menu-item-disabled"
                            role="menuitem"
                            aria-disabled="true"
                            style="position:relative;opacity:0.55;cursor:not-allowed;padding:0.4rem 0.6rem;"
                            title=title
                        >
                            <div style="display:flex;align-items:center;gap:0.5rem;">
                                <span>{label}</span>
                                <span style="flex:1;"></span>
                                {tag}
                            </div>
                            <div style="font-size:0.7rem;opacity:0.8;white-space:normal;">
                                {message}
                            </div>
                        </div>
                    }
                    .into_any()
                }
            })
            .collect_view();

        view! { <>{rows}</> }.into_any()
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use protocol::{
        CustomAgentId, LaunchProfile, LaunchProfileCatalog, SessionSettingValue,
        SessionSettingsValues, ToolPolicy,
    };
    use std::collections::HashMap;
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

    fn visible_text(container: &HtmlElement) -> String {
        container.text_content().unwrap_or_default()
    }

    fn find_button_by_text(container: &HtmlElement, text: &str) -> Option<HtmlElement> {
        let nodes = container.query_selector_all("button").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|btn| btn.text_content().unwrap_or_default().contains(text))
    }

    fn ready(id: &str, label: &str, backend: BackendKind) -> LaunchProfileEntry {
        ready_kind(id, label, backend, LaunchProfileKind::BackendDefault)
    }

    fn ready_kind(
        id: &str,
        label: &str,
        backend: BackendKind,
        kind: LaunchProfileKind,
    ) -> LaunchProfileEntry {
        LaunchProfileEntry::Ready {
            profile: LaunchProfile {
                id: LaunchProfileId(id.to_owned()),
                kind,
                label: label.to_owned(),
                description: None,
                backend_kind: backend,
                session_settings: SessionSettingsValues::default(),
            },
        }
    }

    fn set_catalog(state: &AppState, host_id: &str, entries: Vec<LaunchProfileEntry>) {
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.launch_profile_catalog.update(|map| {
            map.insert(
                host_id.to_owned(),
                LaunchProfileCatalog {
                    entries,
                    default_profile_id: None,
                },
            );
        });
    }

    fn mount_menu(
        container: &HtmlElement,
        state: &AppState,
        open: RwSignal<bool>,
        align: SubmenuAlign,
    ) {
        let state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <LaunchMenuBody open_sig=open submenu_align=align /> }
        });
        std::mem::forget(_handle);
    }

    /// Ready profiles render as selectable rows with a backend badge, and
    /// unavailable entries stay visible with their server-provided reason —
    /// they are never silently dropped from the menu.
    #[wasm_bindgen_test]
    async fn renders_ready_profiles_and_unavailable_reason() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![
                ready("claude:default", "Claude", BackendKind::Claude),
                LaunchProfileEntry::Unavailable {
                    id: LaunchProfileId("hermes:codex".to_owned()),
                    kind: LaunchProfileKind::Custom,
                    backend_kind: BackendKind::Codex,
                    label: "Hermes · Codex".to_owned(),
                    message: "Codex CLI not installed".to_owned(),
                },
            ],
        );
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.contains("Claude"),
            "ready profile label missing: {text}"
        );
        assert!(
            text.contains("Hermes · Codex"),
            "unavailable profile label missing: {text}"
        );
        assert!(
            text.contains("Codex CLI not installed"),
            "unavailable reason must be shown, not silently dropped: {text}"
        );
        assert!(
            find_button_by_text(&container, "Codex CLI not installed").is_none(),
            "unavailable entry must not be a clickable button"
        );
        assert!(
            find_button_by_text(&container, "Claude").is_some(),
            "ready profile must be a clickable button"
        );
    }

    /// Selecting a ready profile sets the draft launch profile id, backend, and
    /// session settings straight from the server-provided profile — no id
    /// parsing — and closes the menu.
    #[wasm_bindgen_test]
    async fn selecting_ready_profile_sets_draft_from_profile() {
        let container = make_container();
        let state = AppState::new();
        let mut settings = HashMap::new();
        settings.insert(
            "model".to_owned(),
            SessionSettingValue::String("opus".to_owned()),
        );
        let entry = LaunchProfileEntry::Ready {
            profile: LaunchProfile {
                id: LaunchProfileId("claude:default".to_owned()),
                kind: LaunchProfileKind::BackendDefault,
                label: "Claude".to_owned(),
                description: None,
                backend_kind: BackendKind::Claude,
                session_settings: SessionSettingsValues(settings.clone()),
            },
        };
        set_catalog(&state, "local", vec![entry]);
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        find_button_by_text(&container, "Claude")
            .expect("ready profile row")
            .click();
        next_tick().await;

        assert_eq!(
            state.draft_launch_profile_id.get_untracked(),
            Some(LaunchProfileId("claude:default".to_owned())),
            "draft launch profile id must be set from the selected profile"
        );
        assert_eq!(
            state.draft_backend_override.get_untracked(),
            Some(BackendKind::Claude),
            "draft backend must come from the profile"
        );
        assert_eq!(
            state.draft_session_settings.get_untracked(),
            SessionSettingsValues(settings),
            "draft session settings must come from the profile"
        );
        assert!(
            !open.get_untracked(),
            "selecting a profile must close the menu"
        );
    }

    /// Hovering a ready profile reveals a side submenu of custom agents;
    /// choosing one composes the custom agent with the selected profile.
    #[wasm_bindgen_test]
    async fn custom_agent_submenu_composes_with_profile() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        state.custom_agents.update(|map| {
            let host = map.entry("local".to_owned()).or_default();
            host.insert(
                CustomAgentId("reviewer".to_owned()),
                CustomAgent {
                    id: CustomAgentId("reviewer".to_owned()),
                    name: "Reviewer".to_owned(),
                    description: "Reviews code".to_owned(),
                    instructions: None,
                    skill_ids: Vec::new(),
                    mcp_server_ids: Vec::new(),
                    tool_policy: ToolPolicy::Unrestricted,
                },
            );
        });
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        // Reveal the side submenu by hovering the ready profile row.
        let row = container
            .query_selector(".new-chat-backend-row-wrap")
            .unwrap()
            .expect("ready profile row wrapper")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let enter = web_sys::MouseEvent::new("mouseenter").unwrap();
        row.dispatch_event(&enter).unwrap();
        next_tick().await;

        let agent_btn = find_button_by_text(&container, "Reviewer")
            .expect("custom agent must appear in submenu");
        agent_btn.click();
        next_tick().await;

        assert_eq!(
            state.draft_custom_agent_id.get_untracked(),
            Some(CustomAgentId("reviewer".to_owned())),
            "selecting a custom agent must set the draft custom agent id"
        );
        assert_eq!(
            state.draft_launch_profile_id.get_untracked(),
            Some(LaunchProfileId("claude:default".to_owned())),
            "custom agent selection must keep the composed launch profile"
        );
        assert_eq!(
            state.draft_backend_override.get_untracked(),
            Some(BackendKind::Claude),
            "custom agent selection must keep the profile backend"
        );
    }

    /// A ready profile row advertises its submenu to assistive tech via
    /// `aria-haspopup="menu"` and reflects the open state in `aria-expanded`,
    /// which flips to `true` while the row is hovered.
    #[wasm_bindgen_test]
    async fn ready_row_exposes_haspopup_and_expanded() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        let row_button = container
            .query_selector(".new-chat-backend-row-wrap [aria-haspopup=\"menu\"]")
            .unwrap()
            .expect("ready row must advertise a popup submenu")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            row_button.get_attribute("aria-expanded").as_deref(),
            Some("false"),
            "submenu starts collapsed"
        );

        let row = container
            .query_selector(".new-chat-backend-row-wrap")
            .unwrap()
            .expect("ready profile row wrapper")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let enter = web_sys::MouseEvent::new("mouseenter").unwrap();
        row.dispatch_event(&enter).unwrap();
        next_tick().await;

        assert_eq!(
            row_button.get_attribute("aria-expanded").as_deref(),
            Some("true"),
            "hovering the row must expand its submenu in the a11y tree"
        );
    }

    /// The submenu opens toward the side the caller requested: `Left` so the
    /// chat-top (right-anchored) dropdown stays on-screen, `Right` for the Home
    /// dropdown. Asserted via geometry, which is what the user perceives.
    #[wasm_bindgen_test]
    async fn submenu_alignment_follows_caller_choice() {
        async fn submenu_and_row_left(align: SubmenuAlign) -> (f64, f64) {
            let container = make_container();
            let state = AppState::new();
            set_catalog(
                &state,
                "local",
                vec![ready("claude:default", "Claude", BackendKind::Claude)],
            );
            state.custom_agents.update(|map| {
                map.entry("local".to_owned()).or_default().insert(
                    CustomAgentId("reviewer".to_owned()),
                    CustomAgent {
                        id: CustomAgentId("reviewer".to_owned()),
                        name: "Reviewer".to_owned(),
                        description: String::new(),
                        instructions: None,
                        skill_ids: Vec::new(),
                        mcp_server_ids: Vec::new(),
                        tool_policy: ToolPolicy::Unrestricted,
                    },
                );
            });
            let open = RwSignal::new(true);
            mount_menu(&container, &state, open, align);
            next_tick().await;

            let row = container
                .query_selector(".new-chat-backend-row-wrap")
                .unwrap()
                .expect("ready profile row wrapper")
                .dyn_into::<HtmlElement>()
                .unwrap();
            let enter = web_sys::MouseEvent::new("mouseenter").unwrap();
            row.dispatch_event(&enter).unwrap();
            next_tick().await;

            let submenu = container
                .query_selector(".new-chat-submenu")
                .unwrap()
                .expect("submenu must be visible after hover")
                .dyn_into::<HtmlElement>()
                .unwrap();
            (
                row.get_bounding_client_rect().left(),
                submenu.get_bounding_client_rect().left(),
            )
        }

        let (row_left, submenu_left) = submenu_and_row_left(SubmenuAlign::Left).await;
        assert!(
            submenu_left < row_left,
            "left-aligned submenu must open to the left of its row \
             (submenu_left={submenu_left}, row_left={row_left})"
        );

        let (row_left, submenu_left) = submenu_and_row_left(SubmenuAlign::Right).await;
        assert!(
            submenu_left >= row_left,
            "right-aligned submenu must open at/to the right of its row \
             (submenu_left={submenu_left}, row_left={row_left})"
        );
    }

    /// When the host has no launch profiles yet, the empty state guides the
    /// user to enable a backend rather than showing a dead end.
    #[wasm_bindgen_test]
    async fn empty_catalog_shows_actionable_copy() {
        let container = make_container();
        let state = AppState::new();
        // Host selected but no catalog entries → empty state.
        state.selected_host_id.set(Some("local".to_owned()));
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.contains("Settings → Backends"),
            "empty menu must point the user at enabling a backend: {text}"
        );
    }

    /// Unavailable profile rows expose disabled menu semantics for assistive
    /// technology instead of looking like a plain clickable row.
    #[wasm_bindgen_test]
    async fn unavailable_row_exposes_aria_disabled() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![LaunchProfileEntry::Unavailable {
                id: LaunchProfileId("hermes:codex".to_owned()),
                kind: LaunchProfileKind::Custom,
                backend_kind: BackendKind::Codex,
                label: "Hermes · Codex".to_owned(),
                message: "Codex CLI not installed".to_owned(),
            }],
        );
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        let disabled = container
            .query_selector("[aria-disabled=\"true\"][role=\"menuitem\"]")
            .unwrap();
        assert!(
            disabled.is_some(),
            "unavailable profile row must expose disabled menu-item semantics"
        );
    }

    /// The primary "New Chat" button starts from the catalog's
    /// `default_profile_id` when it names an exact ready entry — backend and
    /// profile id come straight from the server-owned catalog.
    #[wasm_bindgen_test]
    fn primary_new_chat_honors_ready_default_profile() {
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        state.launch_profile_catalog.update(|map| {
            if let Some(catalog) = map.get_mut("local") {
                catalog.default_profile_id = Some(LaunchProfileId("claude:default".to_owned()));
            }
        });

        crate::actions::begin_new_chat_default(&state);

        assert_eq!(
            state.draft_launch_profile_id.get_untracked(),
            Some(LaunchProfileId("claude:default".to_owned())),
            "primary button must preselect the server default profile"
        );
        assert_eq!(
            state.draft_backend_override.get_untracked(),
            Some(BackendKind::Claude),
            "primary button must use the default profile's backend"
        );
    }

    /// With no server `default_profile_id`, the primary button opens an
    /// ordinary draft: no profile, no backend override (the server resolves its
    /// own default at spawn time). No inference from the catalog contents.
    #[wasm_bindgen_test]
    fn primary_new_chat_without_default_leaves_profile_unset() {
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );

        crate::actions::begin_new_chat_default(&state);

        assert_eq!(
            state.draft_launch_profile_id.get_untracked(),
            None,
            "no server default → no profile preselected"
        );
        assert_eq!(
            state.draft_backend_override.get_untracked(),
            None,
            "no server default → no backend override; server resolves its default"
        );
    }

    fn row_button_text(container: &HtmlElement) -> String {
        container
            .query_selector(".new-chat-menu-item")
            .unwrap()
            .expect("ready profile row button")
            .dyn_into::<HtmlElement>()
            .unwrap()
            .text_content()
            .unwrap_or_default()
    }

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    /// A backend-default profile's label already *is* the backend name, so the
    /// row must not also render a backend badge — the name appears exactly once,
    /// never doubled up as "Claude … Claude". Suppression is driven by the typed
    /// `LaunchProfileKind::BackendDefault`, not by comparing label strings.
    #[wasm_bindgen_test]
    async fn backend_default_row_shows_backend_name_once() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready_kind(
                "claude:default",
                "Claude",
                BackendKind::Claude,
                LaunchProfileKind::BackendDefault,
            )],
        );
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        let text = row_button_text(&container);
        assert_eq!(
            count_occurrences(&text, "Claude"),
            1,
            "backend-default row must show the backend name exactly once: {text:?}"
        );
    }

    /// A custom profile whose label differs from its backend keeps a subtle
    /// backend tag, so the user can still see which backend it runs on even
    /// though the label doesn't say so.
    #[wasm_bindgen_test]
    async fn custom_profile_shows_label_and_backend_tag() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready_kind(
                "team:reviewer",
                "My Reviewer",
                BackendKind::Claude,
                LaunchProfileKind::Custom,
            )],
        );
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Right);
        next_tick().await;

        let text = row_button_text(&container);
        assert!(
            text.contains("My Reviewer"),
            "custom profile label must render: {text:?}"
        );
        assert!(
            text.contains("Claude"),
            "custom profile keeps a subtle backend tag when its label differs \
             from the backend: {text:?}"
        );
    }

    /// The row chevron points toward the side the flyout actually opens: `▶`
    /// when the caller opens the submenu to the right, `◀` when it opens left.
    /// The glyph and the submenu side can never disagree.
    #[wasm_bindgen_test]
    async fn chevron_direction_matches_submenu_alignment() {
        async fn chevron_text(align: SubmenuAlign) -> String {
            let container = make_container();
            let state = AppState::new();
            set_catalog(
                &state,
                "local",
                vec![ready("claude:default", "Claude", BackendKind::Claude)],
            );
            let open = RwSignal::new(true);
            mount_menu(&container, &state, open, align);
            next_tick().await;
            row_button_text(&container)
        }

        let right = chevron_text(SubmenuAlign::Right).await;
        assert!(
            right.contains('▶') && !right.contains('◀'),
            "right-opening row must show a right chevron only: {right:?}"
        );

        let left = chevron_text(SubmenuAlign::Left).await;
        assert!(
            left.contains('◀') && !left.contains('▶'),
            "left-opening row must show a left chevron only: {left:?}"
        );
    }

    fn make_container_styled(style: &str) -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container.set_attribute("style", style).unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    /// Mount an `Auto`-aligned menu (one ready profile + a custom agent so the
    /// flyout has content) inside a container placed by `container_style`, hover
    /// the row, and report its geometry plus the row button text.
    async fn auto_hover_geometry(container_style: &str) -> (f64, f64, f64, f64, String) {
        let container = make_container_styled(container_style);
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        state.custom_agents.update(|map| {
            map.entry("local".to_owned()).or_default().insert(
                CustomAgentId("reviewer".to_owned()),
                CustomAgent {
                    id: CustomAgentId("reviewer".to_owned()),
                    name: "Reviewer".to_owned(),
                    description: String::new(),
                    instructions: None,
                    skill_ids: Vec::new(),
                    mcp_server_ids: Vec::new(),
                    tool_policy: ToolPolicy::Unrestricted,
                },
            );
        });
        let open = RwSignal::new(true);
        mount_menu(&container, &state, open, SubmenuAlign::Auto);
        next_tick().await;

        let row = container
            .query_selector(".new-chat-backend-row-wrap")
            .unwrap()
            .expect("ready profile row wrapper")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let enter = web_sys::MouseEvent::new("mouseenter").unwrap();
        row.dispatch_event(&enter).unwrap();
        next_tick().await;

        let submenu = container
            .query_selector(".new-chat-submenu")
            .unwrap()
            .expect("submenu must be visible after hover")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let row_rect = row.get_bounding_client_rect();
        let result = (
            row_rect.left(),
            row_rect.right(),
            submenu.get_bounding_client_rect().left(),
            web_sys::window()
                .unwrap()
                .inner_width()
                .unwrap()
                .as_f64()
                .unwrap(),
            row_button_text(&container),
        );
        container.remove();
        result
    }

    /// `Auto` opens the natural right-side flyout when the row has room to its
    /// right, with the chevron pointing right — the normal, attached flyout.
    #[wasm_bindgen_test]
    async fn auto_flyout_prefers_right_when_room() {
        // Narrow menu pinned to the left edge → lots of room on the right.
        let (row_left, row_right, submenu_left, viewport_width, text) =
            auto_hover_geometry("position:fixed;top:0;left:0;width:160px;").await;
        assert!(
            row_left.abs() < 1.0 && viewport_width - row_right >= SUBMENU_ESTIMATED_WIDTH,
            "fixture row must hug the viewport's left edge with enough room for the flyout \
             (row_left={row_left}, row_right={row_right}, viewport_width={viewport_width})"
        );
        assert!(
            submenu_left >= row_left,
            "with room, Auto must open the flyout to the right \
             (submenu_left={submenu_left}, row_left={row_left})"
        );
        assert!(
            text.contains('▶') && !text.contains('◀'),
            "right-opening Auto row must show a right chevron: {text:?}"
        );
    }

    /// `Auto` flips to a clean left-attached flyout when the row hugs the right
    /// edge, so the custom-agent list stays on-screen instead of clipping — and
    /// the chevron flips with it.
    #[wasm_bindgen_test]
    async fn auto_flyout_flips_left_near_right_edge() {
        // Narrow menu pinned to the right edge → no room on the right.
        let (row_left, row_right, submenu_left, viewport_width, text) =
            auto_hover_geometry("position:fixed;top:0;right:0;width:160px;").await;
        assert!(
            (viewport_width - row_right).abs() < 1.0,
            "fixture row must hug the viewport's right edge before testing Auto \
             (viewport_width={viewport_width}, row_right={row_right})"
        );
        assert!(
            submenu_left < row_left,
            "near the right edge, Auto must flip the flyout to the left \
             (submenu_left={submenu_left}, row_left={row_left})"
        );
        assert!(
            text.contains('◀') && !text.contains('▶'),
            "left-flipped Auto row must show a left chevron: {text:?}"
        );
    }
}
