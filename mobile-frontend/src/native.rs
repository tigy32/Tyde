use leptos::prelude::*;
use mobile_shell_types::native::{
    NATIVE_BRIDGE_VERSION, NativeEndpoint, NativeIntent, NativeLayout, NativePresentation,
    NativeRoute, NativeTab, NativeTheme,
};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};

use crate::state::{AppMode, AppState, LocalHostId, MobileTab};

pub fn is_native() -> bool {
    web_sys::window().is_some_and(|window| {
        js_sys::Reflect::has(&window, &"__tydeNativeHost".into()).unwrap_or(false)
    })
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
    let projection_state = state.clone();
    Effect::new(move |_| {
        let presentation = presentation(&projection_state);
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

fn modal_open(state: &AppState) -> bool {
    state
        .native_modal_sources
        .with(|sources| sources.iter().any(|open| open.get()))
}

fn handle_intent(state: &AppState, intent: NativeIntent) {
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
            if state.app_mode.get_untracked() == AppMode::HostPicker {
                state.app_mode.set(AppMode::Workspace);
            } else {
                state.native_chat_destination.set(None);
                state.viewing_chat.set(false);
            }
        }
        NativeIntent::NewChat => crate::actions::begin_new_chat(state),
        NativeIntent::RegularWidth => state.native_layout.set(NativeLayout::RegularPortrait),
        NativeIntent::RegularLandscape => state.native_layout.set(NativeLayout::RegularLandscape),
        NativeIntent::CompactWidth => state.native_layout.set(NativeLayout::Compact),
        NativeIntent::ToggleSidebar => {
            if sidebar_available(state) {
                state.native_sidebar_open.update(|open| *open = !*open);
            }
        }
        NativeIntent::Foreground => crate::app::page_foreground(state.clone()),
        NativeIntent::Background => crate::app::page_background(),
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
            state.chat_input.set(String::new());
            state.chat_draft_id.set(uuid::Uuid::new_v4());
        }
        state.active_agent.set(Some(crate::state::ActiveAgentRef {
            local_host_id: target.local_host_id,
            agent_id,
        }));
        state.chat_origin_tab.set(target.tab);
        state.viewing_chat.set(true);
    });
}

pub fn open_host(state: &AppState, local_host_id: LocalHostId) {
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
            state.chat_input.set(String::new());
            state.draft_backend_override.set(None);
            state.draft_custom_agent_id.set(None);
            state
                .draft_session_settings
                .set(protocol::SessionSettingsValues::default());
            state.viewing_chat.set(false);
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

pub fn sidebar_available(state: &AppState) -> bool {
    chat_visible(state)
        && state.active_tab.get() == MobileTab::Agents
        && state.native_layout.get() == NativeLayout::RegularLandscape
}

pub fn sidebar_visible(state: &AppState) -> bool {
    sidebar_available(state) && state.native_sidebar_open.get()
}

fn presentation(state: &AppState) -> NativePresentation {
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
                MobileTab::Home => "Home",
                MobileTab::Agents => "Agents",
                MobileTab::Sessions => "Sessions",
                MobileTab::Settings => "Settings",
            }
            .into(),
            None,
        )
    };
    NativePresentation {
        version: NATIVE_BRIDGE_VERSION,
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

pub fn check_host_release(release: &protocol::TydeReleaseVersion) -> Result<(), String> {
    if is_native() && release.as_str() != mobile_shell_types::native::NATIVE_CLIENT_RELEASE {
        return Err(
            "This native app bundles a different host release. Update the app and host together."
                .into(),
        );
    }
    Ok(())
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
    }
    Ok(())
}
