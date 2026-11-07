use leptos::prelude::*;
use mobile_shell_types::native::{
    NATIVE_BACK_SURFACE_VERSION, NATIVE_BRIDGE_VERSION, NativeBackDestination, NativeBackPhase,
    NativeBackUpdate, NativeEndpoint, NativeIntent, NativeLayout, NativePresentation, NativeRoute,
    NativeTab, NativeTheme,
};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};

use crate::state::{AppMode, AppState, BackDestination, LocalHostId, MobileTab};

pub fn is_native() -> bool {
    web_sys::window().is_some_and(|window| {
        js_sys::Reflect::has(&window, &"__tydeNativeHost".into()).unwrap_or(false)
    })
}

fn back_surface_version() -> Result<Option<u32>, String> {
    let window = web_sys::window().ok_or("Native host window is unavailable")?;
    let api = js_sys::Reflect::get(&window, &"__tydeNativeHost".into())
        .map_err(|_| "Native host bridge is unavailable")?;
    let value = js_sys::Reflect::get(&api, &"backSurfaceVersion".into())
        .map_err(|_| "Native Back capability is unavailable")?;
    if value.is_undefined() {
        return Ok(None);
    }
    if value.as_f64() == Some(f64::from(NATIVE_BACK_SURFACE_VERSION)) {
        Ok(Some(NATIVE_BACK_SURFACE_VERSION))
    } else {
        Err("Native Back surface contract is incompatible".into())
    }
}

pub async fn call<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    endpoint: NativeEndpoint,
    payload: &T,
) -> Result<R, String> {
    let value = post(endpoint, payload)?;
    let value = wasm_bindgen_futures::JsFuture::from(
        value
            .dyn_into::<js_sys::Promise>()
            .map_err(|_| "Native host did not return a promise")?,
    )
    .await
    .map_err(|error| operation_error(endpoint, error))?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("Invalid native result: {error}"))
}

fn post<T: serde::Serialize>(endpoint: NativeEndpoint, payload: &T) -> Result<JsValue, String> {
    let window = web_sys::window().ok_or("Native host window is unavailable")?;
    let api = js_sys::Reflect::get(&window, &"__tydeNativeHost".into())
        .map_err(|_| "Native presentation bridge is unavailable")?;
    let version = js_sys::Reflect::get(&api, &"version".into())
        .map_err(|_| "Native presentation bridge has no version")?;
    if version.as_f64() != Some(f64::from(NATIVE_BRIDGE_VERSION)) {
        return Err("Native app and bundled client bridge versions differ".into());
    }
    let function = js_sys::Reflect::get(&api, &"post".into())
        .map_err(|_| "Native host post is unavailable")?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| "Native host post is invalid")?;
    let name = serde_wasm_bindgen::to_value(&endpoint).map_err(|error| error.to_string())?;
    let payload = serde_json::to_string(payload).map_err(|error| error.to_string())?;
    function
        .call2(&api, &name, &payload.into())
        .map_err(|error| operation_error(endpoint, error))
}

fn operation_error(endpoint: NativeEndpoint, error: JsValue) -> String {
    match error.dyn_ref::<js_sys::Error>() {
        Some(error) => format!(
            "Native {endpoint} failed: {}",
            String::from(error.message())
        ),
        None => format!("Native {endpoint} rejected with an invalid non-Error value"),
    }
}

fn report(state: &AppState, message: String) {
    log::error!("Native host boundary failed: {message}");
    state
        .mobile_shell_error
        .set(Some(crate::state::MobileShellError {
            code: protocol::MobileAccessErrorCode::Internal,
            message,
        }));
}

pub fn install(state: AppState) {
    if !is_native() {
        return;
    }
    let live_back = match back_surface_version() {
        Ok(version) => version.is_some(),
        Err(error) => {
            report(&state, error);
            return;
        }
    };
    // The immutable installed v6 shell owns chrome unless it explicitly transfers it.
    let owner = (|| {
        let window = web_sys::window().ok_or("Native host window is unavailable")?;
        let api = js_sys::Reflect::get(&window, &"__tydeNativeHost".into())
            .map_err(|_| "Native host bridge is unavailable".to_owned())?;
        let value = js_sys::Reflect::get(&api, &"chatHeaderOwner".into())
            .map_err(|_| "Native chat header ownership is unavailable".to_owned())?;
        let chat = serde_wasm_bindgen::from_value::<
            Option<mobile_shell_types::native::NativeChatHeaderOwner>,
        >(value)
        .map_err(|error| format!("Invalid native chat header ownership: {error}"))?;
        let root = js_sys::Reflect::get(&api, &"rootStatusOwner".into())
            .map_err(|_| "Native root status ownership is unavailable".to_owned())?;
        let root = serde_wasm_bindgen::from_value::<
            Option<mobile_shell_types::native::NativeRootStatusOwner>,
        >(root)
        .map_err(|error| format!("Invalid native root status ownership: {error}"))?;
        let dock = js_sys::Reflect::get(&api, &"chatDockOwner".into())
            .map_err(|_| "Native chat dock ownership is unavailable".to_owned())?;
        let dock = serde_wasm_bindgen::from_value::<
            Option<mobile_shell_types::native::NativeChatDockOwner>,
        >(dock)
        .map_err(|error| format!("Invalid native chat dock ownership: {error}"))?;
        Ok((chat, root, dock))
    })();
    match owner {
        Ok((chat, root, dock)) => {
            state.native_chat_header_owner.set(chat);
            state.native_root_status_owner.set(root);
            state.native_chat_dock_owner.set(dock);
        }
        Err(error) => {
            report(&state, error);
            return;
        }
    }
    let listener_state = state.clone();
    let callback =
        Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |event: web_sys::CustomEvent| {
            match serde_wasm_bindgen::from_value::<NativeIntent>(event.detail()) {
                Ok(intent) => handle_intent(&listener_state, intent),
                Err(error) => report(&listener_state, format!("Invalid native intent: {error}")),
            }
        });
    let Some(window) = web_sys::window() else {
        report(&state, "Native host window is unavailable".into());
        return;
    };
    if window
        .add_event_listener_with_callback("tyde-native-intent", callback.as_ref().unchecked_ref())
        .is_err()
    {
        report(&state, "Could not subscribe to native navigation".into());
        return;
    }
    let held = send_wrapper::SendWrapper::new((window, callback));
    on_cleanup(move || {
        let (window, callback) = held.take();
        let _ = window.remove_event_listener_with_callback(
            "tyde-native-intent",
            callback.as_ref().unchecked_ref(),
        );
    });
    let modal_state = state.clone();
    let modal_barrier = Memo::new(move |_| modal_open(&modal_state));
    let revision_state = state.clone();
    let navigation_revision = Memo::new(move |previous: Option<&u64>| {
        revision_state.app_mode.track();
        revision_state.active_tab.track();
        revision_state.viewing_chat.track();
        revision_state.active_agent.track();
        revision_state.chat_back_stack.track();
        revision_state.chat_draft_id.track();
        revision_state.native_layout.track();
        revision_state.native_sidebar_open.track();
        let _ = modal_barrier.get();
        previous.map_or(1, |revision| revision + 1)
    });
    let back_ready_revision = RwSignal::new(None::<u64>);
    let readiness_state = state.clone();
    Effect::new(move |_| {
        if !live_back {
            return;
        }
        let revision = navigation_revision.get();
        readiness_state.native_back_progress.set(None);
        back_ready_revision.set(None);
        if readiness_state.back_destination().is_none() || sidebar_visible(&readiness_state) {
            return;
        }
        let frame_state = readiness_state.clone();
        let callback = Closure::once(move || {
            if navigation_revision.get_untracked() != revision {
                return;
            }
            let laid_out = (|| -> Result<(), String> {
                let document = web_sys::window()
                    .and_then(|window| window.document())
                    .ok_or("Native Back document is unavailable")?;
                let targets = document
                    .query_selector_all("[data-native-back-destination]")
                    .map_err(|_| "Native Back destination query failed")?;
                if targets.length() != 1 {
                    return Err("Native Back requires exactly one live destination surface".into());
                }
                let target = targets
                    .item(0)
                    .ok_or("Native Back destination is unavailable")?
                    .dyn_into::<web_sys::Element>()
                    .map_err(|_| "Native Back destination is not an element")?;
                let app = target
                    .closest(".mobile-app[data-native-host]")
                    .map_err(|_| "Native Back owner query failed")?
                    .ok_or("Native Back document owner is unavailable")?
                    .dyn_into::<web_sys::HtmlElement>()
                    .map_err(|_| "Native Back owner is invalid")?;
                crate::shell::synchronize_native_surfaces(&app)
                    .map_err(|_| "Native Back destination geometry could not synchronize")?;
                let bounds = target.get_bounding_client_rect();
                if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
                    return Err("Native Back destination was not laid out".into());
                }
                Ok(())
            })();
            match laid_out {
                Ok(()) => back_ready_revision.set(Some(revision)),
                Err(error) => report(&frame_state, error),
            }
        });
        let Some(window) = web_sys::window() else {
            report(&readiness_state, "Native Back window is unavailable".into());
            return;
        };
        match window.request_animation_frame(callback.as_ref().unchecked_ref()) {
            Ok(frame) => {
                let held = send_wrapper::SendWrapper::new((window, frame, callback));
                on_cleanup(move || {
                    let (window, frame, callback) = held.take();
                    if window.cancel_animation_frame(frame).is_err() {
                        log::error!("Native Back layout frame could not be cancelled");
                    }
                    drop(callback);
                });
            }
            Err(_) => report(
                &readiness_state,
                "Could not schedule native Back layout".into(),
            ),
        }
    });
    if live_back {
        let back_state = state.clone();
        let progress =
            Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |event: web_sys::CustomEvent| {
                match serde_wasm_bindgen::from_value::<NativeBackUpdate>(event.detail()) {
                    Ok(update)
                        if update.version == NATIVE_BACK_SURFACE_VERSION
                            && update.progress <= 1000
                            && update.source_revision == navigation_revision.get_untracked() =>
                    {
                        match update.phase {
                            NativeBackPhase::Progress => {
                                back_state.native_back_progress.set(Some(update.progress))
                            }
                            NativeBackPhase::Cancel => back_state.native_back_progress.set(None),
                            NativeBackPhase::Complete => {
                                if back_state.native_back_progress.get_untracked().is_none() {
                                    report(
                                        &back_state,
                                        "Native Back completed without an active transition".into(),
                                    );
                                    return;
                                }
                                handle_intent(&back_state, NativeIntent::Back);
                            }
                        }
                    }
                    _ => report(
                        &back_state,
                        "Native Back was invalidated by a navigation change".into(),
                    ),
                }
            });
        let Some(window) = web_sys::window() else {
            report(&state, "Native host window is unavailable".into());
            return;
        };
        if window
            .add_event_listener_with_callback("tyde-native-back", progress.as_ref().unchecked_ref())
            .is_err()
        {
            report(&state, "Could not subscribe to native Back progress".into());
            return;
        }
        let held = send_wrapper::SendWrapper::new((window, progress));
        on_cleanup(move || {
            let (window, progress) = held.take();
            let _ = window.remove_event_listener_with_callback(
                "tyde-native-back",
                progress.as_ref().unchecked_ref(),
            );
        });
    }
    let projection_state = state.clone();
    Effect::new(move |_| {
        let presentation = presentation(
            &projection_state,
            navigation_revision.get(),
            back_ready_revision.get(),
            live_back,
        );
        let state = projection_state.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = call::<_, ()>(NativeEndpoint::Presentation, &presentation).await {
                report(&state, error);
            }
        });
    });
}

// Hold references to the existing modal owners, never a second copy of their open state.
pub fn track_modal(open: impl Into<Signal<bool>>) {
    if !is_native() {
        return;
    }
    let sources = expect_context::<AppState>().native_modal_sources;
    let open = open.into();
    sources.update(|sources| sources.push(open));
    on_cleanup(move || sources.update(|sources| sources.retain(|source| *source != open)));
}

pub fn track_unsaved(open: impl Into<Signal<bool>>) {
    if !is_native() {
        return;
    }
    let sources = expect_context::<AppState>().native_unsaved_sources;
    let open = open.into();
    sources.update(|sources| sources.push(open));
    on_cleanup(move || sources.update(|sources| sources.retain(|source| *source != open)));
}

pub fn can_leave_workspace(state: &AppState) -> bool {
    state.chat_input.get().is_empty()
        && state.chat_images.get().is_empty()
        && state.pending_submissions.get().is_empty()
        && state.draft_session_settings.get().0.is_empty()
        && state.draft_backend_override.get().is_none()
        && state.draft_custom_agent_id.get().is_none()
        && !state.project_picker_open.get()
        && !state.session_settings_open.get()
        && matches!(state.voice_ui.get(), crate::voice::MobileVoiceState::Idle)
        && !state
            .native_unsaved_sources
            .with(|sources| sources.iter().any(|source| source.get()))
}

fn modal_open(state: &AppState) -> bool {
    state
        .native_modal_sources
        .with(|sources| sources.iter().any(|open| open.get()))
}

pub(crate) fn handle_intent(state: &AppState, intent: NativeIntent) {
    if modal_open(state)
        && matches!(
            intent,
            NativeIntent::Home
                | NativeIntent::Agents
                | NativeIntent::Sessions
                | NativeIntent::Settings
                | NativeIntent::Back
                | NativeIntent::Hosts
                | NativeIntent::NewChat
                | NativeIntent::RenameAgent
                | NativeIntent::SendActions
                | NativeIntent::SessionSettings
                | NativeIntent::CloseAgent
                | NativeIntent::ToggleSidebar
        )
    {
        return;
    }
    let tab = match intent {
        NativeIntent::Home => Some(MobileTab::Home),
        NativeIntent::Agents => Some(MobileTab::Agents),
        NativeIntent::Sessions => Some(MobileTab::Sessions),
        NativeIntent::Settings => Some(MobileTab::Settings),
        _ => None,
    };
    if let Some(tab) = tab {
        state.active_tab.set(tab);
        if state.app_mode.get_untracked() == AppMode::HostPicker {
            state.app_mode.set(AppMode::Workspace);
        }
        return;
    }
    match intent {
        NativeIntent::Hosts => {
            state.active_tab.set(MobileTab::Home);
            state.app_mode.set(AppMode::HostPicker);
        }
        NativeIntent::Back => {
            batch(|| {
                if state.app_mode.get_untracked() == AppMode::HostPicker {
                    state.app_mode.set(AppMode::Workspace);
                } else {
                    state.back_from_chat();
                }
                state.native_back_progress.set(None);
            });
        }
        NativeIntent::NewChat => crate::actions::begin_new_chat(state),
        NativeIntent::ChatDockShell => state
            .native_chat_dock_owner
            .set(Some(mobile_shell_types::native::NativeChatDockOwner::Shell)),
        NativeIntent::ChatDockClient => state.native_chat_dock_owner.set(Some(
            mobile_shell_types::native::NativeChatDockOwner::Client,
        )),
        NativeIntent::RegularWidth => state.native_layout.set(NativeLayout::RegularPortrait),
        NativeIntent::RegularLandscape => state.native_layout.set(NativeLayout::RegularLandscape),
        NativeIntent::CompactWidth => state.native_layout.set(NativeLayout::Compact),
        NativeIntent::ToggleSidebar => {
            if sidebar_available(state) {
                state.native_sidebar_open.update(|open| *open = !*open);
            }
        }
        NativeIntent::Foreground => crate::app::page_foreground(state.clone()),
        NativeIntent::Background => {
            crate::app::page_background();
            if state.recovery_status.get_untracked() == crate::recovery::RecoveryStatus::Ready {
                let state = state.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(error) = crate::recovery::commit_current().await {
                        crate::recovery::report_write_failure(&state, error);
                    }
                });
            }
        }
        NativeIntent::SendActions => state.chat_send_menu_open.set(true),
        NativeIntent::RenameAgent | NativeIntent::SessionSettings | NativeIntent::CloseAgent => {
            state.native_chat_action.set(Some(intent));
        }
        NativeIntent::Home
        | NativeIntent::Agents
        | NativeIntent::Sessions
        | NativeIntent::Settings => {}
    }
}

pub fn creation_published(
    state: &AppState,
    host: &LocalHostId,
    stream: &protocol::StreamPath,
    agent_id: protocol::AgentId,
) {
    state.native_chat_destination.update(|pending| {
        if let Some(target) = pending
            && target.local_host_id == *host
            && target.stream == *stream
        {
            target.created_agent = Some(agent_id);
        }
    });
    open_created_chat(state);
}

pub fn open_created_chat(state: &AppState) {
    let Some(target) = state.native_chat_destination.get_untracked() else {
        return;
    };
    if state.active_local_host_id.get_untracked().as_ref() != Some(&target.local_host_id)
        || state.chat_draft_id.get_untracked() != target.draft_id
        || state.chat_edit_id.get_untracked() != target.source_edit_id
        || state.active_agent.get_untracked() != target.source_agent
        || target
            .source_draft
            .as_ref()
            .is_some_and(|draft| state.chat_input.with_untracked(|current| current != draft))
    {
        state.native_chat_destination.set(None);
        return;
    }
    let Some(agent_id) = target.created_agent else {
        return;
    };
    if !state.agents.with_untracked(|agents| {
        agents
            .iter()
            .any(|agent| agent.local_host_id == target.local_host_id && agent.agent_id == agent_id)
    }) {
        return;
    }
    batch(|| {
        state.native_chat_destination.set(None);
        if target.source_draft.is_some() {
            state.set_chat_input(String::new());
            state.chat_images.set(Vec::new());
            if target.clears_draft_options {
                state.draft_backend_override.set(None);
                state.draft_custom_agent_id.set(None);
                state
                    .draft_session_settings
                    .set(protocol::SessionSettingsValues::default());
            }
            state.chat_draft_id.set(uuid::Uuid::new_v4());
        }
        state.active_agent.set(Some(crate::state::ActiveAgentRef {
            local_host_id: target.local_host_id,
            agent_id,
        }));
        state.chat_back_stack.set(Vec::new());
        state.chat_origin_tab.set(target.tab);
        state.viewing_chat.set(true);
    });
}

pub fn open_host(state: &AppState, local_host_id: LocalHostId) {
    let changed = state.active_local_host_id.get_untracked().as_ref() != Some(&local_host_id);
    if changed
        && !matches!(state.app_mode.get_untracked(), AppMode::Pairing(_))
        && !untrack(|| can_leave_workspace(state))
    {
        report(
            state,
            "Finish or save your current work before switching hosts".into(),
        );
        return;
    }
    batch(|| {
        let changed = state.active_local_host_id.get_untracked().as_ref() != Some(&local_host_id);
        if state
            .active_project
            .get_untracked()
            .as_ref()
            .is_some_and(|project| project.local_host_id != local_host_id)
        {
            state.active_project.set(None);
        }
        if state
            .active_agent
            .get_untracked()
            .as_ref()
            .is_some_and(|agent| agent.local_host_id != local_host_id)
        {
            state.active_agent.set(None);
            state.viewing_chat.set(false);
        }
        if state
            .native_chat_destination
            .get_untracked()
            .as_ref()
            .is_some_and(|destination| destination.local_host_id != local_host_id)
        {
            state.native_chat_destination.set(None);
        }
        if changed {
            state.chat_draft_id.set(uuid::Uuid::new_v4());
            state.set_chat_input(String::new());
            state.draft_backend_override.set(None);
            state.draft_custom_agent_id.set(None);
            state
                .draft_session_settings
                .set(protocol::SessionSettingsValues::default());
            state.viewing_chat.set(false);
            state.chat_back_stack.set(Vec::new());
            state.native_chat_action.set(None);
            state.active_local_host_id.set(Some(local_host_id));
        }
        state.active_tab.set(MobileTab::Home);
        state.app_mode.set(AppMode::Workspace);
    });
}

pub fn chat_visible(state: &AppState) -> bool {
    state.app_mode.get() == AppMode::Workspace
        && state.viewing_chat.get()
        && state.active_tab.get() == state.chat_origin_tab.get()
}

pub fn root_status_owned(state: &AppState) -> bool {
    is_native()
        && state.native_root_status_owner.get()
            == Some(mobile_shell_types::native::NativeRootStatusOwner::Shell)
        && matches!(
            state.app_mode.get(),
            AppMode::Workspace | AppMode::HostPicker
        )
        && state.active_local_host_id.get().is_some()
        && !chat_visible(state)
}

pub fn sidebar_available(state: &AppState) -> bool {
    chat_visible(state)
        && state.active_tab.get() == MobileTab::Agents
        && state.native_layout.get() == NativeLayout::RegularLandscape
}

pub fn sidebar_visible(state: &AppState) -> bool {
    sidebar_available(state) && state.native_sidebar_open.get()
}

fn home_title(state: &AppState) -> String {
    match state.active_host_label() {
        Some(label) => label,
        None => "Home".into(),
    }
}

fn presentation(
    state: &AppState,
    navigation_revision: u64,
    back_ready_revision: Option<u64>,
    live_back: bool,
) -> NativePresentation {
    let tab = state.active_tab.get();
    let hosts = state.app_mode.get() == AppMode::HostPicker;
    let chat = !hosts && chat_visible(state);
    let (title, subtitle) = if hosts {
        ("Hosts".into(), None)
    } else if chat {
        state.native_chat_heading.get()
    } else {
        (
            match tab {
                MobileTab::Home if root_status_owned(state) => home_title(state),
                MobileTab::Home => "Home".to_owned(),
                MobileTab::Agents => "Agents".to_owned(),
                MobileTab::Sessions => "Sessions".to_owned(),
                MobileTab::Settings => "Settings".to_owned(),
            },
            None,
        )
    };
    let connected_status = matches!(
        state.active_host_connection_status(),
        crate::state::ConnectionStatus::Connected
    )
    .then(|| mobile_shell_types::native::NativeConnectedStatus {
        round_trip_ms: state.active_local_host_id.get().and_then(|host| {
            state
                .heartbeat_round_trip_ms_by_host
                .with(|values| values.get(&host).copied())
        }),
    });
    let chat_connection_status = if crate::components::connection_banner::connection_pending(
        &state.active_host_connection_status(),
    ) {
        Some(mobile_shell_types::native::NativeChatConnectionStatus {
            phase: mobile_shell_types::native::NativeChatConnectionPhase::Pending,
            round_trip_ms: None,
        })
    } else {
        connected_status.as_ref().map(|status| {
            mobile_shell_types::native::NativeChatConnectionStatus {
                phase: mobile_shell_types::native::NativeChatConnectionPhase::Connected,
                round_trip_ms: status.round_trip_ms,
            }
        })
    };
    NativePresentation {
        back_surface_version: live_back.then_some(NATIVE_BACK_SURFACE_VERSION),
        navigation_revision: live_back.then_some(navigation_revision),
        back_ready_revision: if live_back { back_ready_revision } else { None },
        back_destination: (if live_back {
            state.back_destination()
        } else {
            None
        })
        .map(|target| match target {
            BackDestination::Root(tab) => NativeBackDestination {
                tab: match tab {
                    MobileTab::Home => NativeTab::Home,
                    MobileTab::Agents => NativeTab::Agents,
                    MobileTab::Sessions => NativeTab::Sessions,
                    MobileTab::Settings => NativeTab::Settings,
                },
                route: NativeRoute::Root,
                connected_status: if state.native_root_status_owner.get()
                    == Some(mobile_shell_types::native::NativeRootStatusOwner::Shell)
                {
                    connected_status.clone()
                } else {
                    None
                },
                chat_dock: None,
                title: match tab {
                    MobileTab::Home => home_title(state),
                    MobileTab::Agents => "Agents".into(),
                    MobileTab::Sessions => "Sessions".into(),
                    MobileTab::Settings => "Settings".into(),
                },
            },
            BackDestination::Chat(owner) => NativeBackDestination {
                tab: match tab {
                    MobileTab::Home => NativeTab::Home,
                    MobileTab::Agents => NativeTab::Agents,
                    MobileTab::Sessions => NativeTab::Sessions,
                    MobileTab::Settings => NativeTab::Settings,
                },
                route: NativeRoute::Chat,
                title: owner
                    .as_ref()
                    .and_then(|owner| {
                        state.agents.with(|agents| {
                            agents
                                .iter()
                                .find(|agent| agent.agent_ref() == owner.as_agent_ref())
                                .map(|agent| agent.name.clone())
                        })
                    })
                    .unwrap_or_else(|| {
                        if owner.is_some() {
                            "Conversation".into()
                        } else {
                            "New Chat".into()
                        }
                    }),
                connected_status: None,
                chat_dock: (state.native_chat_dock_owner.get()
                    == Some(mobile_shell_types::native::NativeChatDockOwner::Shell))
                .then(|| {
                    let sidebar_available = tab == MobileTab::Agents
                        && state.native_layout.get() == NativeLayout::RegularLandscape;
                    mobile_shell_types::native::NativeBackChatDock {
                        back_label: state.back_destination_chat_back_label(),
                        connection_status: chat_connection_status.clone(),
                        sidebar_available,
                        sidebar_open: sidebar_available && state.native_sidebar_open.get(),
                        chat_actions: owner.is_some(),
                    }
                }),
            },
        }),
        version: NATIVE_BRIDGE_VERSION,
        client_ready: matches!(
            state.recovery_status.get(),
            crate::recovery::RecoveryStatus::Disabled | crate::recovery::RecoveryStatus::Ready
        ),
        workspace: matches!(
            state.app_mode.get(),
            AppMode::Workspace | AppMode::HostPicker
        ) && state.active_local_host_id.get().is_some(),
        tab: match tab {
            MobileTab::Home => NativeTab::Home,
            MobileTab::Agents => NativeTab::Agents,
            MobileTab::Sessions => NativeTab::Sessions,
            MobileTab::Settings => NativeTab::Settings,
        },
        route: if hosts {
            NativeRoute::Hosts
        } else if chat {
            NativeRoute::Chat
        } else {
            NativeRoute::Root
        },
        title,
        subtitle,
        chat_actions: chat && state.active_agent.get().is_some(),
        modal: modal_open(state),
        sidebar_available: sidebar_available(state),
        sidebar_open: sidebar_visible(state),
        chat_header_owner: state.native_chat_header_owner.get(),
        chat_dock_owner: state.native_chat_dock_owner.get(),
        chat_back_label: (chat
            && state.native_chat_dock_owner.get()
                == Some(mobile_shell_types::native::NativeChatDockOwner::Shell))
        .then(|| state.chat_back_label()),
        chat_connection_status: if chat
            && state.native_chat_dock_owner.get()
                == Some(mobile_shell_types::native::NativeChatDockOwner::Shell)
        {
            chat_connection_status
        } else {
            None
        },
        root_status_owner: state.native_root_status_owner.get(),
        root_connected_status: if root_status_owned(state) {
            connected_status
        } else {
            None
        },
        theme: if state.theme.get() == "light" {
            NativeTheme::Light
        } else {
            NativeTheme::Dark
        },
    }
}

pub fn direct_origin(qr_uri: &str) -> Result<String, String> {
    let url = web_sys::Url::new(qr_uri)
        .map_err(|_| "Native direct pairing requires the host's complete HTTPS pairing link")?;
    if url.protocol() != "https:"
        || url.host().is_empty()
        || !url.username().is_empty()
        || !url.password().is_empty()
        || url.pathname() != "/tyde/"
        || !url.search().is_empty()
    {
        return Err("Native direct pairing requires an HTTPS host link without credentials or query parameters".into());
    }
    Ok(url.origin())
}

pub fn validate_environment() -> Result<(), String> {
    let window = web_sys::window().ok_or("The app window is unavailable")?;
    let document = window.document().ok_or("The app document is unavailable")?;
    let required = document
        .query_selector("meta[name='tyde-native-host']")
        .map_err(|_| "Native package marker could not be read")?
        .is_some();
    let present = js_sys::Reflect::has(&window, &"__tydeNativeHost".into())
        .map_err(|_| "Native host environment could not be read")?;
    if required && !present {
        return Err("This native package requires its system navigation host".into());
    }
    if present {
        let api = js_sys::Reflect::get(&window, &"__tydeNativeHost".into())
            .map_err(|_| "Native host bridge is unavailable")?;
        let version = js_sys::Reflect::get(&api, &"version".into())
            .map_err(|_| "Native host bridge version is unavailable")?;
        if version.as_f64() != Some(f64::from(NATIVE_BRIDGE_VERSION)) {
            return Err("Native app and client bridge versions differ".into());
        }
        back_surface_version()?;
    }
    Ok(())
}
