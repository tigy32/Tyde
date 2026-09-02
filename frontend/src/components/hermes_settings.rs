//! Bespoke settings page for the Hermes backend.
//!
//! Hermes publishes no typed deep-config schema; instead its backend-native
//! settings snapshot carries a typed [`HermesNativeSettingsDoc`] (see
//! `protocol::hermes_config`) describing every discovered profile, each
//! profile's editable `config.yaml` projection, and the live provider states
//! probed from that profile's gateway. This page renders that document as a
//! profile switcher plus per-profile cards (providers/credentials, model
//! defaults, OpenRouter routing, fallback chain, agent, tool search).
//!
//! Two save flows, kept deliberately separate:
//!
//! - **Config edits** accumulate locally in per-profile drafts and are sent
//!   only when the user presses Save. Dirty state is the difference between
//!   the drafts and the live snapshot document.
//! - **Credential actions** (save API key / disconnect) save immediately. A
//!   credential save carries the ORIGINAL snapshot config sections plus the
//!   queued action — never the local draft — so pressing "Save key" cannot
//!   silently commit unrelated, still-unreviewed config edits. Local edits
//!   stay dirty across a credential save.
//!
//! Both flows reuse the shared `native_settings_save_state` machinery: the
//! save is recorded `Pending` against the pre-save snapshot document, the
//! server force-publishes a fresh snapshot after every native save (which
//! clears the gate in the `BackendConfigSnapshots` dispatch handler), and a
//! typed `SettingsWrite` error flips the state to `Failed` with the server's
//! message. API keys travel only inside the wire payload; they are never
//! logged, never stored in a signal, and never rendered back into the DOM.
//!
//! Tyde policy: agents run unrestricted, so no approvals/permissions
//! configuration is surfaced here even though Hermes has such settings.

use leptos::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, NativeSettingsSaveState};

use protocol::hermes_config::{
    HERMES_DEFAULT_PROFILE, HERMES_NATIVE_SETTINGS_VERSION, HermesCredentialAction,
    HermesFallbackProvider, HermesNativeSettingsDoc, HermesProfileAction, HermesProfileConfig,
    HermesProfileSettings, HermesProviderState,
};
use protocol::{
    BackendConfigSnapshotStatus, BackendKind, BackendNativeSettingsSnapshot,
    BackendNativeSettingsWritePayload, FrameKind,
};
use serde_json::Value;

/// Entry point used by `settings_panel::backend_page_body` for
/// `BackendKind::Hermes`. Returns a self-contained view that subscribes to the
/// host's Hermes native-settings snapshot itself, so snapshot republishes
/// rerender only this page body — the caller's closure (and therefore this
/// component's local edit state) survives saves.
pub fn hermes_settings_page_body(host_id: &str) -> AnyView {
    view! { <HermesSettingsBody host_id=host_id.to_owned() /> }.into_any()
}

#[component]
fn HermesSettingsBody(host_id: String) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Declared here so it survives snapshot republishes, which rebuild the body
    // closure below. No API key is among these: the key text is deliberately
    // never a signal — it lives only in the uncontrolled password input and is
    // read out once when the user confirms.
    let view_state = HermesViewState {
        selected_profile: RwSignal::new(None),
        drafts: RwSignal::new(HashMap::new()),
        tab: RwSignal::new(HermesTab::Providers),
        key_dialog: RwSignal::new(None),
        provider_query: RwSignal::new(String::new()),
        new_profile: RwSignal::new(None),
        delete_profile: RwSignal::new(None),
    };
    let drafts = view_state.drafts;

    // Prune drafts that no longer carry an edit against the live snapshot:
    // after a successful save the republished config equals the draft, and
    // keeping the stale entry would resurrect as phantom "unsaved changes" if
    // the server-side config later changed externally. Also drops drafts for
    // profiles that no longer exist. Reads drafts untracked so user edits
    // (which already self-prune in `update_profile_config`) never loop here.
    {
        let state = state.clone();
        let host = host_id.clone();
        Effect::new(move |_| {
            let Some(doc) = state
                .backend_native_settings
                .get()
                .get(&host)
                .and_then(|m| m.get(&BackendKind::Hermes))
                .and_then(|snapshot| snapshot.settings.clone())
                .and_then(|value| serde_json::from_value::<HermesNativeSettingsDoc>(value).ok())
            else {
                return;
            };
            let needs_prune = drafts.with_untracked(|map| {
                map.iter().any(|(name, cfg)| {
                    doc.profiles
                        .iter()
                        .find(|p| p.name == *name)
                        .is_none_or(|p| p.config == *cfg)
                })
            });
            if !needs_prune {
                return;
            }
            drafts.update(|map| {
                map.retain(|name, cfg| {
                    doc.profiles
                        .iter()
                        .find(|p| p.name == *name)
                        .is_some_and(|p| p.config != *cfg)
                });
            });
        });
    }

    let state_for_snapshot = state.clone();
    let host_for_snapshot = host_id.clone();
    let snapshot: Memo<Option<BackendNativeSettingsSnapshot>> = Memo::new(move |_| {
        state_for_snapshot
            .backend_native_settings
            .get()
            .get(&host_for_snapshot)
            .and_then(|m| m.get(&BackendKind::Hermes))
            .cloned()
    });

    move || {
        let Some(snap) = snapshot.get() else {
            return view! {
                <p class="settings-description">
                    "Waiting for Hermes settings from the selected host…"
                </p>
            }
            .into_any();
        };
        if snap.status == BackendConfigSnapshotStatus::Unavailable {
            let message = snap.message.clone().unwrap_or_else(|| {
                "Hermes settings are unavailable on the selected host.".to_owned()
            });
            return view! {
                <div class="settings-native-unavailable">
                    <p class="settings-native-unavailable-text">{message}</p>
                </div>
            }
            .into_any();
        }
        let Some(raw) = snap.settings.clone() else {
            // Ready but no document — never fabricate defaults; say so.
            return view! {
                <p class="settings-description">
                    "Hermes reported its settings are ready but sent no current values."
                </p>
            }
            .into_any();
        };
        let doc = match serde_json::from_value::<HermesNativeSettingsDoc>(raw) {
            Ok(doc) => doc,
            Err(error) => {
                return view! {
                    <div class="settings-native-error" role="alert">
                        {format!(
                            "Tyde could not read the Hermes settings document published by \
                             this host: {error}"
                        )}
                    </div>
                }
                .into_any();
            }
        };
        if doc.version != HERMES_NATIVE_SETTINGS_VERSION {
            return view! {
                <div class="settings-native-error" role="alert">
                    {format!(
                        "This host publishes Hermes settings in format version {}, but this \
                         version of Tyde understands version {}. Update Tyde and the host to \
                         matching versions to edit these settings.",
                        doc.version, HERMES_NATIVE_SETTINGS_VERSION
                    )}
                </div>
            }
            .into_any();
        }
        if doc.profiles.is_empty() {
            return view! {
                <p class="settings-description">
                    "Hermes reported no profiles on the selected host."
                </p>
            }
            .into_any();
        }

        // Shared save-state machinery: an in-flight save shows as `saving`
        // while its recorded base still equals the live snapshot document; a
        // failed save (send failure or a typed server refusal) surfaces its
        // message in the save bar.
        let save_state = state
            .native_settings_save_state
            .get()
            .get(&host_id)
            .and_then(|m| m.get(&BackendKind::Hermes))
            .cloned();
        let saving = matches!(
            &save_state,
            Some(NativeSettingsSaveState::Pending { base, .. })
                if Some(base) == snap.settings.as_ref()
        );
        let save_error = match save_state {
            Some(NativeSettingsSaveState::Failed { message }) => Some(message),
            _ => None,
        };

        // Mirror of the typed-schema page's disabled-backend banner. The
        // banner only informs: these settings edit Hermes's own configuration
        // on the host, which stays meaningful (and may be needed) while the
        // backend is not offered for new chats, so the editor is not locked.
        let enabled = state
            .selected_host_settings()
            .is_none_or(|settings| settings.enabled_backends.contains(&BackendKind::Hermes));

        editor_view(
            &state,
            &host_id,
            Arc::new(doc),
            saving,
            save_error,
            enabled,
            view_state,
        )
    }
}

/// Page view state that outlives snapshot republishes. Grouped so the pieces
/// that need it take one parameter instead of five.
#[derive(Clone, Copy)]
struct HermesViewState {
    selected_profile: RwSignal<Option<String>>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    tab: RwSignal<HermesTab>,
    /// Open credential dialog, as (profile, preselected provider). `None` for
    /// the provider means the searchable catalogue; `Some` opens straight to
    /// that provider's key field or sign-in instructions.
    key_dialog: RwSignal<Option<(String, Option<String>)>>,
    provider_query: RwSignal<String>,
    /// Name being typed into the create-profile field, or `None` when the
    /// field is closed. Separate from `selected_profile` so an in-progress
    /// name survives a snapshot republish.
    new_profile: RwSignal<Option<String>>,
    /// Profile whose delete confirmation is open. Deleting is destructive
    /// enough to need a typed confirmation, and `window.prompt` is silently
    /// no-op'd inside Tauri's webview, so the confirmation is an in-page modal.
    delete_profile: RwSignal<Option<String>>,
}

// ---------------------------------------------------------------------------
// Save plumbing
// ---------------------------------------------------------------------------

/// The freshest Hermes settings document (typed + raw), read untracked. Save
/// paths always re-read this at action time rather than trusting values
/// captured when the view was built, so a save can never be based on a
/// snapshot older than the one on screen.
fn current_doc_untracked(
    state: &AppState,
    host_id: &str,
) -> Option<(HermesNativeSettingsDoc, Value)> {
    let raw = state
        .backend_native_settings
        .get_untracked()
        .get(host_id)
        .and_then(|m| m.get(&BackendKind::Hermes))
        .and_then(|snapshot| snapshot.settings.clone())?;
    let doc = serde_json::from_value::<HermesNativeSettingsDoc>(raw.clone()).ok()?;
    Some((doc, raw))
}

/// Record a visible save failure for Hermes on `host_id`. Failing silently is
/// never acceptable here — the user pressed a button that claims to persist.
fn mark_save_failed(state: &AppState, host_id: &str, message: &str) {
    state.native_settings_save_state.update(|states| {
        states.entry(host_id.to_owned()).or_default().insert(
            BackendKind::Hermes,
            NativeSettingsSaveState::Failed {
                message: message.to_owned(),
            },
        );
    });
}

/// Send one whole Hermes settings document to the host. `base` is the raw
/// snapshot document the save was built from; the pending gate clears when the
/// server republishes (which it does after every native save). Values are
/// never logged — the document can carry a freshly queued API key.
fn send_hermes_save(state: &AppState, host_id: &str, base: Value, doc: &HermesNativeSettingsDoc) {
    let value = match serde_json::to_value(doc) {
        Ok(value) => value,
        Err(error) => {
            log::error!("failed to serialize Hermes settings document: {error}");
            mark_save_failed(state, host_id, "Failed to prepare the settings document.");
            return;
        }
    };
    // Nothing changed and nothing was queued: sending would only lock the page.
    if value == base {
        return;
    }
    // One in-flight whole-document save at a time; the buttons are disabled
    // while pending, but synthetic events could still reach here.
    let already_pending = state
        .native_settings_save_state
        .get_untracked()
        .get(host_id)
        .and_then(|m| m.get(&BackendKind::Hermes))
        .is_some_and(|save| matches!(save, NativeSettingsSaveState::Pending { .. }));
    if already_pending {
        return;
    }
    let Some(host_stream) = state.host_stream_untracked(host_id) else {
        mark_save_failed(
            state,
            host_id,
            "Failed to save settings. The selected host is not connected.",
        );
        return;
    };

    let write_id = super::settings_panel::next_settings_write_id();
    state.native_settings_save_state.update(|states| {
        states.entry(host_id.to_owned()).or_default().insert(
            BackendKind::Hermes,
            NativeSettingsSaveState::Pending {
                base,
                write_id: write_id.clone(),
            },
        );
    });

    let state = state.clone();
    let host_id = host_id.to_owned();
    spawn_local(async move {
        let payload = BackendNativeSettingsWritePayload {
            write_id,
            backend: BackendKind::Hermes,
            settings: value,
        };
        if let Err(error) = send_frame(
            &host_id,
            host_stream,
            FrameKind::BackendNativeSettingsWrite,
            &payload,
        )
        .await
        {
            log::error!("failed to send Hermes BackendNativeSettings: {error}");
            mark_save_failed(
                &state,
                &host_id,
                "Failed to save settings. Check the connection and try again.",
            );
        }
    });
}

/// Immediate credential save. Carries the ORIGINAL snapshot config sections
/// plus the one queued action, never the local drafts: confirming a key or a
/// disconnect must not silently commit unrelated config edits, which stay
/// dirty in the save bar until the user saves or discards them explicitly.
fn queue_credential_action(state: &AppState, host_id: &str, action: HermesCredentialAction) {
    let Some((mut doc, base)) = current_doc_untracked(state, host_id) else {
        mark_save_failed(
            state,
            host_id,
            "Cannot update credentials: no current Hermes settings document.",
        );
        return;
    };
    doc.actions = vec![action];
    for profile in &mut doc.profiles {
        profile.base_config = Some(profile.config.clone());
    }
    send_hermes_save(state, host_id, base, &doc);
}

/// Immediate profile create/delete. Like a credential action this carries the
/// ORIGINAL snapshot config sections, never the local drafts: adding or
/// removing a profile must not silently commit unrelated config edits.
fn queue_profile_action(state: &AppState, host_id: &str, action: HermesProfileAction) {
    let Some((mut doc, base)) = current_doc_untracked(state, host_id) else {
        mark_save_failed(
            state,
            host_id,
            "Cannot change profiles: no current Hermes settings document.",
        );
        return;
    };
    doc.profile_actions = vec![action];
    for profile in &mut doc.profiles {
        profile.base_config = Some(profile.config.clone());
    }
    send_hermes_save(state, host_id, base, &doc);
}

/// Ask the host to re-probe Hermes. Nothing is saved: this exists because a
/// change made outside Tyde (a `hermes` CLI login, a hand-edited config, a
/// profile created in a terminal) is otherwise invisible until the next save.
fn refresh_hermes_settings(state: &AppState, host_id: &str) {
    let Some(host_stream) = state.host_stream_untracked(host_id) else {
        mark_save_failed(
            state,
            host_id,
            "Cannot refresh: the selected host is not connected.",
        );
        return;
    };
    let state = state.clone();
    let host_id = host_id.to_owned();
    spawn_local(async move {
        if let Err(error) =
            crate::send::backend_settings_refresh(&host_id, host_stream, BackendKind::Hermes).await
        {
            log::error!("failed to send Hermes BackendSettingsRefresh: {error}");
            mark_save_failed(
                &state,
                &host_id,
                "Failed to refresh settings. Check the connection and try again.",
            );
        }
    });
}

/// Replace the Tyde-owned list of providers this profile must not offer. This
/// is a Tyde host setting, not Hermes configuration: Hermes has no provider
/// enable/disable flag, so hiding one can only ever be Tyde's own decision.
fn set_disabled_providers(state: &AppState, host_id: &str, profile: &str, providers: Vec<String>) {
    if state.host_stream_untracked(host_id).is_none() {
        mark_save_failed(
            state,
            host_id,
            "Cannot update providers: the selected host is not connected.",
        );
        return;
    }
    super::settings_panel::send_hermes_disabled_providers(state, profile, providers);
}

/// The provider slugs Tyde must not offer for `profile` on this host.
fn disabled_providers(state: &AppState, profile: &str) -> Vec<String> {
    state
        .selected_host_settings()
        .and_then(|settings| settings.hermes_disabled_providers.get(profile).cloned())
        .unwrap_or_default()
}

/// Explicit Save: current snapshot document with each drafted profile's config
/// section replaced. Fallback rows left fully blank are pruned (from the draft
/// too, so the draft matches the republished document and the page reads
/// clean after the save lands). Never carries credential actions.
fn save_config_edits(
    state: &AppState,
    host_id: &str,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
) {
    let Some((mut doc, base)) = current_doc_untracked(state, host_id) else {
        mark_save_failed(
            state,
            host_id,
            "Cannot save: no current Hermes settings document.",
        );
        return;
    };
    drafts.update(|map| {
        for cfg in map.values_mut() {
            cfg.fallback_providers
                .retain(|f| !(f.provider.trim().is_empty() && f.model.trim().is_empty()));
        }
    });
    let map = drafts.get_untracked();
    // A half-filled fallback row would be written as an empty string and then
    // rejected by the loader on the next snapshot, wedging the page — refuse
    // it here with a pointed message (the server refuses it too).
    for (profile_name, cfg) in &map {
        for (idx, fallback) in cfg.fallback_providers.iter().enumerate() {
            if fallback.provider.trim().is_empty() || fallback.model.trim().is_empty() {
                mark_save_failed(
                    state,
                    host_id,
                    &format!(
                        "Fallback #{} in profile '{}' needs both a provider and a model.",
                        idx + 1,
                        profile_display_name(profile_name),
                    ),
                );
                return;
            }
        }
    }
    for profile in &mut doc.profiles {
        // The unedited snapshot config rides along as the base so the server
        // can refuse a save built on a stale snapshot.
        profile.base_config = Some(profile.config.clone());
        if let Some(draft) = map.get(&profile.name) {
            profile.config = draft.clone();
        }
    }
    doc.actions.clear();
    doc.profile_actions.clear();
    send_hermes_save(state, host_id, base, &doc);
}

// ---------------------------------------------------------------------------
// Draft helpers
// ---------------------------------------------------------------------------

/// The config the controls should show for `profile`: the draft when one
/// exists, else the snapshot's config.
fn effective_config(
    doc: &HermesNativeSettingsDoc,
    drafts: &HashMap<String, HermesProfileConfig>,
    profile: &str,
) -> HermesProfileConfig {
    if let Some(draft) = drafts.get(profile) {
        return draft.clone();
    }
    doc.profiles
        .iter()
        .find(|p| p.name == profile)
        .map(|p| p.config.clone())
        .unwrap_or_default()
}

/// Apply one edit to a profile's draft config. A draft that lands back on the
/// snapshot value is removed, so `drafts` only ever holds real edits and the
/// dirty flag stays honest.
fn update_profile_config(
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    doc: &HermesNativeSettingsDoc,
    profile: &str,
    edit: impl FnOnce(&mut HermesProfileConfig),
) {
    let Some(base) = doc
        .profiles
        .iter()
        .find(|p| p.name == profile)
        .map(|p| &p.config)
    else {
        return;
    };
    let mut cfg = drafts
        .with_untracked(|map| map.get(profile).cloned())
        .unwrap_or_else(|| base.clone());
    edit(&mut cfg);
    drafts.update(|map| {
        if cfg == *base {
            map.remove(profile);
        } else {
            map.insert(profile.to_owned(), cfg);
        }
    });
}

/// Reactive accessor for one projection of the effective profile config.
fn config_value<T>(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: String,
    read: impl Fn(&HermesProfileConfig) -> T + Clone + Send + Sync + 'static,
) -> impl Fn() -> T + Clone + Send + Sync + 'static {
    move || read(&effective_config(&doc, &drafts.get(), &profile))
}

/// Commit callback for one field of the effective profile config.
fn config_committer<X>(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: String,
    write: impl Fn(&mut HermesProfileConfig, X) + Clone + Send + Sync + 'static,
) -> impl Fn(X) + Clone + Send + Sync + 'static {
    move |value| {
        let write = write.clone();
        update_profile_config(drafts, &doc, &profile, |cfg| write(cfg, value));
    }
}

// ---------------------------------------------------------------------------
// Page assembly
// ---------------------------------------------------------------------------

fn profile_display_name(name: &str) -> String {
    if name == HERMES_DEFAULT_PROFILE {
        "Default".to_owned()
    } else {
        name.to_owned()
    }
}

/// Small chip subtitle: the profile's live provider/model as reported by its
/// gateway, falling back to the configured model, then "Hermes default".
fn profile_subtitle(profile: &HermesProfileSettings) -> String {
    match (&profile.active_provider, &profile.active_model) {
        (Some(provider), Some(model)) => format!("{provider} · {model}"),
        (None, Some(model)) => model.clone(),
        _ => match (&profile.config.model.provider, &profile.config.model.model) {
            (Some(provider), Some(model)) => format!("{provider} · {model}"),
            (_, Some(model)) => model.clone(),
            (Some(provider), None) => provider.clone(),
            _ => "Hermes default".to_owned(),
        },
    }
}

// Unset labels for the controls whose key can be absent from `config.yaml`.
//
// "Hermes default" alone was actively confusing next to an explicit "Auto",
// because for both of these Hermes's own default IS `auto` — the two entries
// looked like different behaviors when they select the same one. The
// difference is durability, not behavior: leaving the key out follows Hermes
// if it ever changes its default, while picking a value pins it. Naming the
// default makes that visible instead of leaving the user to guess.
//
// Values verified against the Hermes source these settings write to:
// `hermes_cli/config.py` (`coding_context: "auto"`, `max_turns: 90`) and
// `tools/tool_search.py` (`ToolSearchConfig::from_raw` → `enabled: "auto"`,
// `threshold_pct: 10.0`).
const HERMES_DEFAULT_CODING_CONTEXT: &str = "Hermes default (auto)";
const HERMES_DEFAULT_TOOL_SEARCH: &str = "Hermes default (auto)";
const HERMES_DEFAULT_MAX_TURNS: &str = "Hermes default (90)";
const HERMES_DEFAULT_THRESHOLD_PCT: &str = "Hermes default (10)";

/// Keep a select's current value selectable even when it is not one of the
/// known options (e.g. a config written by hand or a newer Hermes). Without
/// this the control would render as "Hermes default" while the config says
/// otherwise — and a save would then silently keep a value the user never saw.
fn ensure_current_option(options: &mut Vec<(String, String)>, current: Option<String>) {
    if let Some(current) = current
        && !current.is_empty()
        && !options.iter().any(|(value, _)| *value == current)
    {
        options.push((current.clone(), format!("{current} (unrecognized)")));
    }
}

/// The page's sections. Hermes exposes far more configuration than reads well
/// in one column, so each tab owns a coherent question: *who* can serve models,
/// *what* model to use, and *how* the agent loop behaves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HermesTab {
    Providers,
    Model,
    Agent,
}

impl HermesTab {
    const ALL: [HermesTab; 3] = [HermesTab::Providers, HermesTab::Model, HermesTab::Agent];

    fn label(self) -> &'static str {
        match self {
            HermesTab::Providers => "Providers",
            HermesTab::Model => "Model",
            HermesTab::Agent => "Agent",
        }
    }

    /// Whether this tab holds an unsaved edit, so the strip can point at the
    /// tab a pending save belongs to rather than leaving the user to hunt for
    /// it. Credentials are not draftable — they save immediately — so the
    /// Providers tab is never dirty.
    fn is_dirty(self, base: &HermesProfileConfig, draft: &HermesProfileConfig) -> bool {
        match self {
            HermesTab::Providers => false,
            HermesTab::Model => {
                base.model != draft.model
                    || base.fallback_providers != draft.fallback_providers
                    || base.provider_routing != draft.provider_routing
            }
            HermesTab::Agent => base.agent != draft.agent || base.tool_search != draft.tool_search,
        }
    }
}

fn editor_view(
    state: &AppState,
    host_id: &str,
    doc: Arc<HermesNativeSettingsDoc>,
    saving: bool,
    save_error: Option<String>,
    enabled: bool,
    view_state: HermesViewState,
) -> AnyView {
    let HermesViewState {
        selected_profile,
        drafts,
        tab,
        key_dialog,
        provider_query,
        new_profile,
        delete_profile,
    } = view_state;

    let disabled_banner = (!enabled).then(|| {
        view! {
            <div class="settings-hermes-banner" role="note">
                "Hermes is disabled on the selected host, so it isn't offered for new chats. \
                 These settings edit Hermes's own configuration and remain editable."
            </div>
        }
    });

    let profile_names: Vec<String> = doc.profiles.iter().map(|p| p.name.clone()).collect();
    let first_profile = profile_names[0].clone();
    let effective_profile = {
        let names = profile_names.clone();
        let first = first_profile.clone();
        Signal::derive(move || {
            selected_profile
                .get()
                .filter(|name| names.contains(name))
                .unwrap_or_else(|| first.clone())
        })
    };

    // One <select> rather than a chip per profile: the list grows without
    // bound (every profile is a directory the user can create) and a wrapping
    // chip grid pushed the actual settings below the fold at five profiles.
    // The subtitle rides in the option label because a <select> cannot render
    // two lines; losing it entirely would hide which model each profile
    // actually resolves to.
    let profile_options: Vec<(String, String)> = doc
        .profiles
        .iter()
        .map(|profile| {
            (
                profile.name.clone(),
                format!(
                    "{} — {}",
                    profile_display_name(&profile.name),
                    profile_subtitle(profile)
                ),
            )
        })
        .collect();

    let profile_select = select_control(
        Some("Hermes profile".to_owned()),
        Some("settings-hermes-profile-select"),
        // No empty entry: a profile is always selected.
        None,
        saving,
        Signal::derive(move || profile_options.clone()),
        effective_profile,
        Callback::new(move |name: String| selected_profile.set(Some(name))),
    );

    let profile_controls = profile_bar_controls(
        state,
        host_id,
        effective_profile,
        selected_profile,
        new_profile,
        delete_profile,
        saving,
    );

    let dirty_tabs = {
        let doc = doc.clone();
        Memo::new(move |_| {
            let name = effective_profile.get();
            let map = drafts.get();
            let (Some(profile), Some(draft)) =
                (doc.profiles.iter().find(|p| p.name == name), map.get(&name))
            else {
                return Vec::new();
            };
            HermesTab::ALL
                .into_iter()
                .filter(|t| t.is_dirty(&profile.config, draft))
                .collect::<Vec<_>>()
        })
    };

    let tab_refs: [NodeRef<leptos::html::Button>; 3] =
        [NodeRef::new(), NodeRef::new(), NodeRef::new()];
    let on_tab_keydown = move |ev: web_sys::KeyboardEvent| {
        let delta = match ev.key().as_str() {
            "ArrowRight" | "ArrowDown" => 1_i32,
            "ArrowLeft" | "ArrowUp" => -1,
            _ => return,
        };
        ev.prevent_default();
        let len = HermesTab::ALL.len() as i32;
        let current = HermesTab::ALL
            .iter()
            .position(|t| *t == tab.get_untracked())
            .unwrap_or(0) as i32;
        let next = ((current + delta) % len + len) % len;
        tab.set(HermesTab::ALL[next as usize]);
        if let Some(button) = tab_refs[next as usize].get_untracked() {
            let _ = button.focus();
        }
    };

    let tabs = HermesTab::ALL
        .into_iter()
        .enumerate()
        .map(|(idx, this)| {
            let is_active = Signal::derive(move || tab.get() == this);
            let is_dirty = Signal::derive(move || dirty_tabs.get().contains(&this));
            view! {
                <button
                    type="button"
                    role="tab"
                    node_ref=tab_refs[idx]
                    class=move || {
                        if is_active.get() {
                            "settings-hermes-tab settings-hermes-tab-active"
                        } else {
                            "settings-hermes-tab"
                        }
                    }
                    aria-selected=move || is_active.get().to_string()
                    tabindex=move || if is_active.get() { "0" } else { "-1" }
                    on:click=move |_| tab.set(this)
                >
                    {this.label()}
                    {move || {
                        is_dirty
                            .get()
                            .then(|| {
                                view! {
                                    <span
                                        class="settings-hermes-tab-dot"
                                        aria-label="has unsaved changes"
                                    ></span>
                                }
                            })
                    }}
                </button>
            }
        })
        .collect::<Vec<_>>();

    let panel = {
        let state = state.clone();
        let host_id = host_id.to_owned();
        let doc = doc.clone();
        move || {
            let active = effective_profile.get();
            let Some(profile) = doc.profiles.iter().find(|p| p.name == active) else {
                return ().into_any();
            };
            match tab.get() {
                HermesTab::Providers => {
                    providers_panel(&state, &host_id, profile, key_dialog, saving)
                }
                HermesTab::Model => view! {
                    {model_card(doc.clone(), drafts, profile)}
                    {fallback_card(doc.clone(), drafts, profile)}
                    {routing_card(doc.clone(), drafts, &profile.name)}
                }
                .into_any(),
                HermesTab::Agent => view! {
                    {agent_card(doc.clone(), drafts, profile)}
                    {tool_search_card(doc.clone(), drafts, &profile.name)}
                }
                .into_any(),
            }
        }
    };

    // Rendered at page level, not inside the panel, so it survives tab
    // switches and can be opened from a provider row or the Add button alike.
    let dialog = provider_dialog(
        state,
        host_id,
        doc.clone(),
        key_dialog,
        provider_query,
        saving,
    );
    let delete_dialog = delete_profile_dialog(
        state,
        host_id,
        doc.clone(),
        delete_profile,
        selected_profile,
        saving,
    );
    let save_bar = save_bar(state, host_id, doc, drafts, saving, save_error);

    view! {
        <div class="settings-hermes-page">
            {disabled_banner}
            <div class="settings-hermes-profilebar">
                <span class="settings-hermes-profilebar-label" id="hermes-profile-label">
                    "Profile"
                </span>
                <div class="settings-hermes-profiles">
                    {profile_select}
                    {profile_controls}
                </div>
            </div>
            <div
                class="settings-hermes-tabs"
                role="tablist"
                aria-label="Hermes settings sections"
                on:keydown=on_tab_keydown
            >
                {tabs}
            </div>
            <div class="settings-hermes-panel" role="tabpanel">{panel}</div>
            {dialog}
            {delete_dialog}
            {save_bar}
        </div>
    }
    .into_any()
}

/// One profile-bar button, shared by add/delete/refresh.
///
/// These are one function rather than three inline `view!` blocks on purpose:
/// each distinct `view!` monomorphizes its own deeply nested view type into the
/// wasm test binary, which runs every frontend test in a single browser
/// instance already close to its memory ceiling. Erasing to `AnyView` behind a
/// shared signature keeps that binary flat as this page grows.
fn profile_action_button(
    label: &'static str,
    danger: bool,
    title: Option<&'static str>,
    saving: bool,
    on_click: Callback<()>,
) -> AnyView {
    let class = if danger {
        "settings-btn settings-btn-danger settings-hermes-profile-action"
    } else {
        "settings-btn settings-hermes-profile-action"
    };
    view! {
        <button
            type="button"
            class=class
            disabled=saving
            title=title
            on:click=move |_| {
                if !saving {
                    on_click.run(());
                }
            }
        >
            {label}
        </button>
    }
    .into_any()
}

/// Add / delete / refresh beside the profile picker.
///
/// Deleting is guarded by a typed confirmation, not a plain yes/no: a Hermes
/// profile is a whole `HERMES_HOME`, so removing it destroys that profile's
/// sessions, credentials, state and memories — not just its configuration —
/// and there is nothing to undo afterwards.
fn profile_bar_controls(
    state: &AppState,
    host_id: &str,
    effective_profile: Signal<String>,
    selected_profile: RwSignal<Option<String>>,
    new_profile: RwSignal<Option<String>>,
    delete_profile: RwSignal<Option<String>>,
    saving: bool,
) -> AnyView {
    let add_button = profile_action_button(
        "Add profile…",
        false,
        None,
        saving,
        Callback::new(move |()| {
            if new_profile.get_untracked().is_none() {
                new_profile.set(Some(String::new()));
            }
        }),
    );

    let delete_button = move || {
        let active = effective_profile.get();
        if active == HERMES_DEFAULT_PROFILE {
            // The default profile IS ~/.hermes; there is no delete to offer.
            return ().into_any();
        }
        profile_action_button(
            "Delete profile",
            true,
            None,
            saving,
            Callback::new(move |()| delete_profile.set(Some(active.clone()))),
        )
    };

    let refresh_button = {
        let state = state.clone();
        let host_id = host_id.to_owned();
        profile_action_button(
            "Refresh",
            false,
            Some("Re-read profiles, providers and config from the host"),
            saving,
            Callback::new(move |()| refresh_hermes_settings(&state, &host_id)),
        )
    };

    // Gated on open/closed only. Tracking `new_profile` itself here would
    // rebuild the field on every keystroke and throw away the caret; the text
    // is read reactively by `prop:value` instead, which is the same split the
    // provider dialog uses.
    let create_open = Memo::new(move |_| new_profile.get().is_some());
    let create_row = {
        let state = state.clone();
        let host_id = host_id.to_owned();
        move || {
            if !create_open.get() {
                return ().into_any();
            }
            let state = state.clone();
            let host_id = host_id.clone();
            let submit = move || {
                let name = new_profile.get_untracked().unwrap_or_default();
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                // Read at submit time, so a profile switch while the field is
                // open copies the profile actually on screen.
                let source = effective_profile.get_untracked();
                new_profile.set(None);
                // Select it now so the page lands on the new profile as soon
                // as the host republishes with it.
                selected_profile.set(Some(name.clone()));
                queue_profile_action(
                    &state,
                    &host_id,
                    HermesProfileAction::CreateProfile {
                        name,
                        copy_config_from: Some(source),
                    },
                );
            };
            let submit_on_key = submit.clone();
            let on_key = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
                "Enter" => {
                    ev.prevent_default();
                    submit_on_key();
                }
                "Escape" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    new_profile.set(None);
                }
                _ => {}
            };
            let on_submit = move |_| submit();
            view! {
                <div class="settings-hermes-profile-create">
                    <input
                        type="text"
                        class="settings-input"
                        placeholder="new-profile-name"
                        aria-label="New Hermes profile name"
                        autocomplete="off"
                        prop:value=move || new_profile.get().unwrap_or_default()
                        on:input=move |ev| new_profile.set(Some(event_target_value(&ev)))
                        on:keydown=on_key
                    />
                    <button
                        type="button"
                        class="settings-btn settings-btn-primary"
                        disabled=saving
                        on:click=on_submit
                    >
                        "Create"
                    </button>
                    <button
                        type="button"
                        class="settings-btn"
                        on:click=move |_| new_profile.set(None)
                    >
                        "Cancel"
                    </button>
                    <p class="settings-description">
                        {move || format!(
                            "Copies {}'s config.yaml into a new Hermes home. Credentials, \
                             sessions and history are not copied.",
                            profile_display_name(&effective_profile.get()),
                        )}
                    </p>
                </div>
            }
            .into_any()
        }
    };

    view! {
        <div class="settings-hermes-profile-actions">
            {add_button}
            {delete_button}
            {refresh_button}
        </div>
        {create_row}
    }
    .into_any()
}

/// The overlay + modal box both dialogs on this page sit in.
///
/// Shared rather than written twice: the backdrop-dismiss and Escape handling
/// are a contract worth having in one place (a modal owns Escape outright —
/// without `stop_propagation` the app's global handler also tears down the
/// settings overlay behind it), and a second inline copy would monomorphize
/// another deep view type into an already-large wasm test binary.
/// Erased parameters, for the reason spelled out on [`select_control`].
fn modal_shell(
    aria_label: &'static str,
    on_keydown: Callback<web_sys::KeyboardEvent>,
    on_backdrop: Callback<()>,
    body: AnyView,
) -> AnyView {
    view! {
        <div class="settings-confirm-overlay" on:click=move |_| on_backdrop.run(())>
            <div
                class="settings-confirm-modal settings-hermes-dialog"
                role="dialog"
                aria-modal="true"
                aria-label=aria_label
                tabindex="-1"
                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                on:keydown=move |ev: web_sys::KeyboardEvent| on_keydown.run(ev)
            >
                {body}
            </div>
        </div>
    }
    .into_any()
}

/// Typed delete confirmation.
///
/// A plain yes/no is not enough here. Deleting a Hermes profile removes an
/// entire `HERMES_HOME`: the profile's chat sessions, saved credentials,
/// `state.db` and memories go with it, and none of it is recoverable. The
/// dialog therefore names the exact directory and only enables the destructive
/// button once the user has typed the profile name back.
fn delete_profile_dialog(
    state: &AppState,
    host_id: &str,
    doc: Arc<HermesNativeSettingsDoc>,
    delete_profile: RwSignal<Option<String>>,
    selected_profile: RwSignal<Option<String>>,
    saving: bool,
) -> AnyView {
    let state = state.clone();
    let host_id = host_id.to_owned();
    let shell = move || {
        let Some(target) = delete_profile.get() else {
            return ().into_any();
        };
        let home_dir = doc
            .profiles
            .iter()
            .find(|p| p.name == target)
            .map(|p| p.home_dir.clone())
            .unwrap_or_default();

        let typed = RwSignal::new(String::new());
        let confirm_ref = NodeRef::<leptos::html::Input>::new();
        Effect::new(move |_| {
            if let Some(input) = confirm_ref.get() {
                let _ = input.focus();
            }
        });

        let close = move || {
            typed.set(String::new());
            delete_profile.set(None);
        };
        let matches = {
            let target = target.clone();
            Signal::derive(move || typed.get().trim() == target)
        };

        let state = state.clone();
        let host_id = host_id.clone();
        let confirm = {
            let target = target.clone();
            move || {
                if saving || !matches.get_untracked() {
                    return;
                }
                delete_profile.set(None);
                // Drop the selection first: the profile is about to vanish
                // from the republished document.
                selected_profile.set(None);
                queue_profile_action(
                    &state,
                    &host_id,
                    HermesProfileAction::DeleteProfile {
                        name: target.clone(),
                    },
                );
            }
        };
        let confirm_on_key = confirm.clone();
        let on_keydown = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
            "Escape" => {
                // A modal owns Escape outright; without this the app's global
                // handler would also tear down the settings overlay behind it.
                ev.prevent_default();
                ev.stop_propagation();
                close();
            }
            "Enter" => {
                ev.prevent_default();
                confirm_on_key();
            }
            _ => {}
        };
        let on_confirm = move |_| confirm();

        let body = view! {
            <h3 class="settings-confirm-title">
                {format!("Delete the \"{target}\" Hermes profile?")}
            </h3>
            <p class="settings-confirm-description">
                "This deletes the profile's entire Hermes home directory, including its \
                 chat sessions, saved credentials, state database and memories — not \
                 just its configuration. It cannot be undone."
            </p>
            <p class="settings-hermes-dialog-hint">
                <code>{home_dir}</code>
            </p>
            <div class="settings-native-field">
                <span class="settings-form-label">
                    {format!("Type \"{target}\" to confirm")}
                </span>
                <input
                    type="text"
                    class="settings-input"
                    aria-label="Confirm the profile name"
                    autocomplete="off"
                    node_ref=confirm_ref
                    prop:value=move || typed.get()
                    on:input=move |ev| typed.set(event_target_value(&ev))
                />
            </div>
            <div class="settings-hermes-dialog-footer">
                <button type="button" class="settings-btn" on:click=move |_| close()>
                    "Cancel"
                </button>
                <button
                    type="button"
                    class="settings-btn settings-btn-danger"
                    disabled=move || saving || !matches.get()
                    on:click=on_confirm
                >
                    "Delete profile"
                </button>
            </div>
        }
        .into_any();
        modal_shell(
            "Delete Hermes profile",
            Callback::new(on_keydown),
            Callback::new(move |()| close()),
            body,
        )
    };
    view! { {shell} }.into_any()
}

/// Card scaffold shared by every section. Reuses the native-group visual
/// language so this page sits naturally next to the other backend pages.
fn card(title: &str, description: Option<&str>, body: AnyView) -> AnyView {
    view! {
        <section class="settings-native-group settings-hermes-card">
            <div class="settings-native-group-header">
                <span class="settings-native-group-title">{title.to_owned()}</span>
            </div>
            {description
                .map(|d| view! { <p class="settings-native-group-desc">{d.to_owned()}</p> })}
            {body}
        </section>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Providers tab + credential dialog
// ---------------------------------------------------------------------------
//
// A Hermes host reports its whole provider catalogue — fifty-odd entries — of
// which a couple are actually configured. The tab therefore shows only what
// this profile can use right now; the catalogue lives behind "Add a provider…",
// where a search box is the right tool for fifty items. Everything a provider
// row can offer (add a key, replace a key, read the sign-in instructions) goes
// through that one dialog, so there is exactly one place an API key is ever
// typed and exactly one code path that queues one.

/// Human-readable name for a Hermes auth mechanism. The raw slug is kept when
/// Hermes reports one this build doesn't know — inventing a friendly label for
/// an unknown mechanism would misdescribe what the user has to do.
fn auth_label(auth_type: Option<&str>) -> String {
    match auth_type {
        Some("api_key") => "API key".to_owned(),
        Some("oauth_device_code") => "Device code".to_owned(),
        Some("oauth_external") | Some("external_process") => "Hermes sign-in".to_owned(),
        Some(other) => other.to_owned(),
        None => "Managed by Hermes".to_owned(),
    }
}

fn providers_panel(
    state: &AppState,
    host_id: &str,
    profile: &HermesProfileSettings,
    key_dialog: RwSignal<Option<(String, Option<String>)>>,
    saving: bool,
) -> AnyView {
    let error = profile.providers_error.clone().map(|message| {
        view! {
            <div class="settings-native-error" role="alert">
                {format!("Provider status could not be read for this profile: {message}")}
            </div>
        }
    });

    let Some(providers) = profile.providers.clone() else {
        let body = profile
            .providers_error
            .is_none()
            .then(|| {
                view! {
                    <p class="settings-description">
                        "Provider status is unavailable for this profile."
                    </p>
                }
            })
            .into_any();
        return card(
            "Providers",
            Some(PROVIDERS_DESCRIPTION),
            view! { {error} {body} }.into_any(),
        );
    };

    let connected: Vec<HermesProviderState> = providers
        .iter()
        .filter(|p| p.authenticated)
        .cloned()
        .collect();
    let available = providers.len() - connected.len();
    let profile_name = profile.name.clone();
    let disabled = disabled_providers(state, &profile_name);

    let open_catalogue = {
        let profile_name = profile_name.clone();
        move |_| key_dialog.set(Some((profile_name.clone(), None)))
    };
    let add_button = (available > 0).then(|| {
        view! {
            <button
                type="button"
                class="settings-btn settings-btn-primary"
                disabled=saving
                on:click=open_catalogue
            >
                "Add a provider…"
            </button>
        }
    });

    let off_count = connected
        .iter()
        .filter(|p| disabled.contains(&p.slug))
        .count();
    let summary = {
        let base = match (connected.len(), available) {
            (0, _) => "Nothing connected yet".to_owned(),
            (1, 0) => "1 provider connected".to_owned(),
            (n, 0) => format!("{n} providers connected"),
            (1, more) => format!("1 provider connected · {more} more available"),
            (n, more) => format!("{n} providers connected · {more} more available"),
        };
        if off_count == 0 {
            base
        } else {
            format!("{base} · {off_count} off in Tyde")
        }
    };

    let body = if connected.is_empty() {
        let open_empty = {
            let profile_name = profile_name.clone();
            move |_| key_dialog.set(Some((profile_name.clone(), None)))
        };
        view! {
            <div class="settings-hermes-empty">
                <p class="settings-hermes-empty-title">"No providers are connected"</p>
                <p class="settings-hermes-empty-body">
                    "This profile can't serve models until one provider has credentials. "
                    {format!("Hermes offers {available} on this host.")}
                </p>
                <button
                    type="button"
                    class="settings-btn settings-btn-primary"
                    disabled=saving
                    on:click=open_empty
                >
                    "Add a provider…"
                </button>
            </div>
        }
        .into_any()
    } else {
        let rows = connected
            .iter()
            .map(|provider| {
                connected_provider_row(
                    state,
                    host_id,
                    &profile_name,
                    provider,
                    &disabled,
                    key_dialog,
                    saving,
                )
            })
            .collect::<Vec<_>>();
        view! { <div class="settings-hermes-provider-list">{rows}</div> }.into_any()
    };

    card(
        "Providers",
        Some(PROVIDERS_DESCRIPTION),
        view! {
            {error}
            <div class="settings-hermes-provider-toolbar">
                <span class="settings-hermes-provider-summary">{summary}</span>
                {add_button}
            </div>
            {body}
        }
        .into_any(),
    )
}

const PROVIDERS_DESCRIPTION: &str = "Model providers this profile can use. Credentials are stored by Hermes inside the \
     profile's own home directory.";

/// One configured provider. Every row reaching here is authenticated, so there
/// is no not-connected branch to render.
fn connected_provider_row(
    state: &AppState,
    host_id: &str,
    profile_name: &str,
    provider: &HermesProviderState,
    disabled: &[String],
    key_dialog: RwSignal<Option<(String, Option<String>)>>,
    saving: bool,
) -> AnyView {
    let slug = provider.slug.clone();
    let show_slug = provider.slug != provider.name;
    let model_count = if provider.model_count == 1 {
        "1 model".to_owned()
    } else {
        format!("{} models", provider.model_count)
    };
    let auth = auth_label(provider.auth_type.as_deref());
    let is_off = disabled.contains(&slug);

    // Hermes has no provider enable/disable flag, so this is Tyde's own list.
    // It is the only control that reliably turns a provider off: a provider
    // Hermes auto-detects (GitHub Copilot via the `gh` CLI login) reappears
    // after a disconnect, because the credential was never Hermes's to delete.
    let toggle_button = {
        let state = state.clone();
        let host_id = host_id.to_owned();
        let profile_name = profile_name.to_owned();
        let slug = slug.clone();
        let mut next: Vec<String> = disabled.to_vec();
        if is_off {
            next.retain(|hidden| *hidden != slug);
        } else {
            next.push(slug.clone());
        }
        let on_click = move |_| {
            if saving {
                return;
            }
            set_disabled_providers(&state, &host_id, &profile_name, next.clone());
        };
        view! {
            <button
                type="button"
                class="settings-btn"
                aria-pressed=(!is_off).to_string()
                disabled=saving
                on:click=on_click
            >
                {if is_off { "Enable in Tyde" } else { "Disable in Tyde" }}
            </button>
        }
        .into_any()
    };

    // Replacing a key opens the same dialog, pre-selected — one credential
    // surface, one code path that can carry a key.
    let replace_button = (provider.auth_type.as_deref() == Some("api_key")).then(|| {
        let target = (profile_name.to_owned(), Some(slug.clone()));
        let on_click = move |_| key_dialog.set(Some(target.clone()));
        view! {
            <button type="button" class="settings-btn" disabled=saving on:click=on_click>
                "Replace key…"
            </button>
        }
    });

    let disconnect_button = {
        let profile_name = profile_name.to_owned();
        let slug = slug.clone();
        let name = provider.name.clone();
        let state = state.clone();
        let host_id = host_id.to_owned();
        let on_click = move |_| {
            if saving {
                return;
            }
            let state = state.clone();
            let host_id = host_id.clone();
            let profile_name = profile_name.clone();
            let slug = slug.clone();
            let name = name.clone();
            spawn_local(async move {
                let message = format!(
                    "Remove {name}'s credentials from the \"{profile_name}\" Hermes profile? \
                     The credentials are deleted from that profile. Sources Hermes detects \
                     automatically (for example GitHub Copilot via the gh CLI login) may be \
                     detected again by Hermes afterwards — to keep a provider out of Tyde \
                     regardless, use \"Disable in Tyde\" instead."
                );
                if !crate::bridge::confirm_dialog(&format!("Disconnect {name}"), &message).await {
                    return;
                }
                queue_credential_action(
                    &state,
                    &host_id,
                    HermesCredentialAction::Disconnect {
                        profile: profile_name,
                        provider: slug,
                    },
                );
            });
        };
        view! {
            <button
                type="button"
                class="settings-btn settings-btn-danger"
                disabled=saving
                on:click=on_click
            >
                "Disconnect"
            </button>
        }
    };

    view! {
        <div class="settings-hermes-provider-row">
            <div class="settings-hermes-provider-line">
                <div class="settings-hermes-provider-info">
                    <span class="settings-hermes-provider-name">
                        {provider.name.clone()}
                        {show_slug.then(|| {
                            view! {
                                <span class="settings-hermes-provider-slug">
                                    {provider.slug.clone()}
                                </span>
                            }
                        })}
                    </span>
                    <span class="settings-hermes-provider-meta">
                        <span class="settings-hermes-provider-models">{model_count}</span>
                        <span class="settings-hermes-provider-hint">{auth}</span>
                    </span>
                </div>
                <div class="settings-hermes-provider-actions">
                    <span class=if is_off {
                        "settings-hermes-badge settings-hermes-badge-off"
                    } else {
                        "settings-hermes-badge settings-hermes-badge-connected"
                    }>
                        {if is_off { "Off in Tyde" } else { "Connected" }}
                    </span>
                    <span class="settings-hermes-provider-action-slot">
                        {toggle_button}
                        {replace_button}
                        {disconnect_button}
                    </span>
                </div>
            </div>
            {is_off.then(|| {
                view! {
                    <p class="settings-hermes-provider-note">
                        "Hidden from Tyde's Hermes model picker. Hermes still has the \
                         credentials, so a hermes session started outside Tyde can use it."
                    </p>
                }
            })}
        </div>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Add-a-provider dialog
// ---------------------------------------------------------------------------

/// What the dialog's steps need from their shell. Grouped so each step takes
/// one parameter rather than the shell's whole set of handles.
#[derive(Clone, Copy)]
struct DialogCtx {
    key_dialog: RwSignal<Option<(String, Option<String>)>>,
    provider_query: RwSignal<String>,
    search_ref: NodeRef<leptos::html::Input>,
    key_ref: NodeRef<leptos::html::Input>,
    on_close: Callback<()>,
    saving: bool,
}

/// Modal over the whole page: pick a provider from the catalogue, then either
/// paste its API key or read how to complete Hermes's own sign-in flow.
///
/// The key input is uncontrolled exactly as the old inline editor was — its
/// value never touches a signal, is read once on confirm, and is cleared before
/// the action is queued. Never prefilled, never logged, never rendered back.
fn provider_dialog(
    state: &AppState,
    host_id: &str,
    doc: Arc<HermesNativeSettingsDoc>,
    key_dialog: RwSignal<Option<(String, Option<String>)>>,
    provider_query: RwSignal<String>,
    saving: bool,
) -> AnyView {
    let state = state.clone();
    let host_id = host_id.to_owned();
    // Keyed on open/closed only, so moving between the catalogue and a
    // provider's detail view swaps the contents without rebuilding the shell
    // (and without the focus bookkeeping below firing on every step).
    let is_open = Memo::new(move |_| key_dialog.get().is_some());

    let shell = move || {
        if !is_open.get() {
            return ().into_any();
        }
        let state = state.clone();
        let host_id = host_id.clone();
        let doc = doc.clone();

        let search_ref = NodeRef::<leptos::html::Input>::new();
        let key_ref = NodeRef::<leptos::html::Input>::new();
        // Re-runs when either node mounts, so focus follows the step.
        Effect::new(move |_| {
            if let Some(input) = key_ref.get() {
                let _ = input.focus();
            } else if let Some(input) = search_ref.get() {
                let _ = input.focus();
            }
        });

        let close = move || {
            if let Some(input) = key_ref.get_untracked() {
                input.set_value("");
            }
            provider_query.set(String::new());
            key_dialog.set(None);
        };
        // Each step owns its own footer, so the dismiss action sits beside that
        // step's primary action rather than on a second row below it.
        let ctx = DialogCtx {
            key_dialog,
            provider_query,
            search_ref,
            key_ref,
            on_close: Callback::new(move |()| close()),
            saving,
        };
        let on_keydown = move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Escape" {
                // A modal owns Escape outright; without this the app's global
                // handler would also tear down the settings overlay behind it.
                ev.prevent_default();
                ev.stop_propagation();
                close();
            }
        };

        let contents = move || {
            let Some((profile_name, selected)) = key_dialog.get() else {
                return ().into_any();
            };
            let Some(profile) = doc.profiles.iter().find(|p| p.name == profile_name) else {
                return ().into_any();
            };
            let providers = profile.providers.clone().unwrap_or_default();

            match selected.and_then(|slug| providers.iter().find(|p| p.slug == slug).cloned()) {
                Some(provider) => {
                    provider_detail_view(&state, &host_id, &profile_name, &provider, ctx)
                }
                None => {
                    let candidates: Vec<HermesProviderState> =
                        providers.into_iter().filter(|p| !p.authenticated).collect();
                    provider_catalogue_view(&profile_name, candidates, ctx)
                }
            }
        };

        modal_shell(
            "Add a provider",
            Callback::new(on_keydown),
            Callback::new(move |()| close()),
            view! { {contents} }.into_any(),
        )
    };
    view! { {shell} }.into_any()
}

fn provider_catalogue_view(
    profile_name: &str,
    candidates: Vec<HermesProviderState>,
    ctx: DialogCtx,
) -> AnyView {
    let DialogCtx {
        key_dialog,
        provider_query,
        search_ref,
        on_close,
        ..
    } = ctx;
    let total = candidates.len();
    let profile_name = profile_name.to_owned();
    let heading_profile = profile_name.clone();

    let rows = move || {
        let needle = provider_query.get().trim().to_lowercase();
        let hits = candidates
            .iter()
            .filter(|p| {
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.slug.to_lowercase().contains(&needle)
            })
            .collect::<Vec<_>>();
        if hits.is_empty() {
            return view! {
                <p class="settings-description">"No provider matches that search."</p>
            }
            .into_any();
        }
        hits.into_iter()
            .map(|provider| {
                let target = (profile_name.clone(), Some(provider.slug.clone()));
                let on_click = move |_| key_dialog.set(Some(target.clone()));
                let show_slug = provider.slug != provider.name;
                view! {
                    <button
                        type="button"
                        class="settings-hermes-dialog-row"
                        on:click=on_click
                    >
                        // Hermes's setup hint is deliberately not here: at
                        // fifty rows it is a wall of near-identical text. The
                        // next step shows it in full.
                        <span class="settings-hermes-dialog-row-main">
                            <span class="settings-hermes-provider-name">
                                {provider.name.clone()}
                                {show_slug.then(|| {
                                    view! {
                                        <span class="settings-hermes-provider-slug">
                                            {provider.slug.clone()}
                                        </span>
                                    }
                                })}
                            </span>
                        </span>
                        <span class="settings-hermes-dialog-row-auth">
                            {auth_label(provider.auth_type.as_deref())}
                        </span>
                    </button>
                }
            })
            .collect::<Vec<_>>()
            .into_any()
    };

    view! {
        <h3 class="settings-confirm-title">"Add a provider"</h3>
        <p class="settings-confirm-description">
            {format!(
                "{total} providers are available to the \"{}\" profile. Pick one to add its \
                 credentials.",
                profile_display_name(&heading_profile),
            )}
        </p>
        <input
            type="search"
            class="settings-input"
            placeholder="Search providers…"
            aria-label="Search providers"
            autocomplete="off"
            node_ref=search_ref
            prop:value=move || provider_query.get()
            on:input=move |ev| provider_query.set(event_target_value(&ev))
        />
        <div class="settings-hermes-dialog-list">{rows}</div>
        <div class="settings-hermes-dialog-footer">
            <button type="button" class="settings-btn" on:click=move |_| on_close.run(())>
                "Close"
            </button>
        </div>
    }
    .into_any()
}

/// Everything that is not an API-key provider gets instructions rather than a
/// field, because Tyde cannot drive Hermes's own sign-in flows from here and
/// must not pretend otherwise.
fn provider_detail_view(
    state: &AppState,
    host_id: &str,
    profile_name: &str,
    provider: &HermesProviderState,
    ctx: DialogCtx,
) -> AnyView {
    let DialogCtx {
        key_dialog,
        key_ref,
        on_close,
        saving,
        ..
    } = ctx;
    let name = provider.name.clone();
    let slug = provider.slug.clone();
    let is_api_key = provider.auth_type.as_deref() == Some("api_key");
    let auth = auth_label(provider.auth_type.as_deref());

    let back_target = (profile_name.to_owned(), None);
    let on_back = move |_| key_dialog.set(Some(back_target.clone()));
    let back = view! {
        <button type="button" class="settings-hermes-dialog-back" on:click=on_back>
            "← All providers"
        </button>
    };

    // Hermes's own setup hint, shown only where it isn't a restatement of what
    // this view already says. An API-key provider that reports its env var is
    // fully described by the "Stored as …" line under the field.
    let show_hint = !is_api_key || provider.key_env.is_none();
    let hint = provider
        .warning
        .clone()
        .filter(|_| show_hint)
        .map(|hint| view! { <p class="settings-hermes-dialog-hint">{hint}</p> });
    let has_hint = hint.is_some();

    let body = if is_api_key {
        let state = state.clone();
        let host_id = host_id.to_owned();
        let profile_name = profile_name.to_owned();
        let slug = slug.clone();
        // One save path, shared by the button and by Enter in the field. The
        // key is read from the uncontrolled input, cleared, and handed straight
        // to the credential action — it is never stored anywhere in between.
        let save = move || {
            if saving {
                return;
            }
            let Some(input) = key_ref.get_untracked() else {
                return;
            };
            let key = input.value().trim().to_owned();
            if key.is_empty() {
                return;
            }
            input.set_value("");
            key_dialog.set(None);
            queue_credential_action(
                &state,
                &host_id,
                HermesCredentialAction::SaveApiKey {
                    profile: profile_name.clone(),
                    provider: slug.clone(),
                    api_key: key,
                },
            );
        };
        let save_on_key = save.clone();
        let on_key = move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Enter" {
                ev.prevent_default();
                save_on_key();
            }
        };
        let on_save = move |_| save();
        view! {
            <div class="settings-native-field">
                <span class="settings-form-label">"API key"</span>
                <input
                    type="password"
                    class="settings-input"
                    placeholder="Paste the key"
                    autocomplete="off"
                    node_ref=key_ref
                    on:keydown=on_key
                />
                {provider.key_env.clone().map(|env| {
                    view! {
                        <p class="settings-description">
                            {format!("Stored as {env} in this profile's .env")}
                        </p>
                    }
                })}
            </div>
            <div class="settings-hermes-dialog-footer">
                <button type="button" class="settings-btn" on:click=move |_| on_close.run(())>
                    "Cancel"
                </button>
                <button
                    type="button"
                    class="settings-btn settings-btn-primary"
                    disabled=saving
                    on:click=on_save
                >
                    "Save key"
                </button>
            </div>
        }
        .into_any()
    } else if has_hint {
        // Hermes told us exactly what to do; the only thing this view adds is
        // which profile to do it in. Repeating the instruction in our own
        // words underneath it would be the same sentence twice.
        view! {
            <p class="settings-confirm-description">
                {format!(
                    "{name} connects through Hermes itself ({auth}). Complete it in the \"{}\" \
                     profile:",
                    profile_display_name(profile_name),
                )}
            </p>
        }
        .into_any()
    } else {
        view! {
            <p class="settings-confirm-description">
                {format!("{name} connects through Hermes itself ({auth}). Run ")}
                <code>"hermes model"</code>
                {format!(
                    " in the \"{}\" profile and choose {name} to finish connecting it.",
                    profile_display_name(profile_name),
                )}
            </p>
        }
        .into_any()
    };

    // The hint sits after the lead-in for non-key providers so it reads as the
    // step to take, and before the field for key providers.
    view! {
        {back}
        <h3 class="settings-confirm-title">{name}</h3>
        {is_api_key.then_some(hint.clone())}
        {body}
        {(!is_api_key).then_some(hint)}
        {(!is_api_key).then(|| {
            view! {
                <div class="settings-hermes-dialog-footer">
                    <button
                        type="button"
                        class="settings-btn"
                        on:click=move |_| on_close.run(())
                    >
                        "Close"
                    </button>
                </div>
            }
        })}
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Config cards
// ---------------------------------------------------------------------------

fn model_card(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: &HermesProfileSettings,
) -> AnyView {
    let name = profile.name.clone();

    // Provider: a dropdown of the probed provider slugs when the probe
    // succeeded, else a free-text input — one control per situation, never
    // both. An unknown current value is kept selectable so opening the page
    // can never silently change it.
    let provider_control = if let Some(providers) = &profile.providers {
        let mut options: Vec<(String, String)> = providers
            .iter()
            .map(|p| {
                let label = if p.slug == p.name {
                    p.slug.clone()
                } else {
                    format!("{} ({})", p.name, p.slug)
                };
                (p.slug.clone(), label)
            })
            .collect();
        let current = effective_config(&doc, &drafts.get_untracked(), &name)
            .model
            .provider;
        if let Some(current) = current
            && !options.iter().any(|(slug, _)| *slug == current)
        {
            options.push((current.clone(), format!("{current} (not probed)")));
        }
        select_field(
            "Provider",
            None,
            "Hermes default",
            options,
            config_value(doc.clone(), drafts, name.clone(), |c| {
                c.model.provider.clone().unwrap_or_default()
            }),
            config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                c.model.provider = v;
            }),
        )
    } else {
        text_field(
            "Provider",
            "Hermes default",
            None,
            config_value(doc.clone(), drafts, name.clone(), |c| {
                c.model.provider.clone().unwrap_or_default()
            }),
            config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                c.model.provider = v;
            }),
        )
    };

    // Default model: a dropdown of the selected provider's own models when the
    // probe reported them, so the user picks an id Hermes will actually
    // accept. Falls back to free text when the provider is unknown or reports
    // no models — inventing an empty dropdown would make a valid config
    // unreachable.
    let model_control = {
        let current = effective_config(&doc, &drafts.get_untracked(), &name);
        let provider_models = current
            .model
            .provider
            .as_ref()
            .and_then(|slug| {
                profile
                    .providers
                    .as_ref()?
                    .iter()
                    .find(|p| p.slug == *slug)
                    .map(|p| p.models.clone())
            })
            .unwrap_or_default();
        if provider_models.is_empty() {
            text_field(
                "Default model",
                "Hermes default",
                None,
                config_value(doc.clone(), drafts, name.clone(), |c| {
                    c.model.model.clone().unwrap_or_default()
                }),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| c.model.model = v),
            )
        } else {
            let mut options: Vec<(String, String)> = provider_models
                .into_iter()
                .map(|model| (model.clone(), model))
                .collect();
            ensure_current_option(&mut options, current.model.model.clone());
            select_field(
                "Default model",
                None,
                "Hermes default",
                options,
                config_value(doc.clone(), drafts, name.clone(), |c| {
                    c.model.model.clone().unwrap_or_default()
                }),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| c.model.model = v),
            )
        }
    };

    let body = view! {
        <div class="settings-hermes-grid">
            {provider_control}
            {model_control}
            {text_field(
                "Base URL",
                "Provider default",
                None,
                config_value(doc.clone(), drafts, name.clone(), |c| {
                    c.model.base_url.clone().unwrap_or_default()
                }),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| c.model.base_url = v),
            )}
            {number_field(
                "Context length",
                Some("Context window override in tokens."),
                "The model's own limit",
                config_value(doc.clone(), drafts, name.clone(), |c| c.model.context_length),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                    c.model.context_length = v;
                }),
            )}
            {number_field(
                "Max output tokens",
                Some("Output token cap."),
                "The model's own limit",
                config_value(doc.clone(), drafts, name.clone(), |c| c.model.max_tokens),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                    c.model.max_tokens = v;
                }),
            )}
        </div>
    }
    .into_any();

    card(
        "Model defaults",
        Some("Default provider and model for new Hermes sessions using this profile."),
        body,
    )
}

fn routing_card(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: &str,
) -> AnyView {
    let name = profile.to_owned();
    let mut sort_options = vec![
        ("price".to_owned(), "Price".to_owned()),
        ("throughput".to_owned(), "Throughput".to_owned()),
        ("latency".to_owned(), "Latency".to_owned()),
    ];
    ensure_current_option(
        &mut sort_options,
        effective_config(&doc, &drafts.get_untracked(), &name)
            .provider_routing
            .sort,
    );
    let body = view! {
        <div class="settings-hermes-grid">
        {select_field(
            "Sort upstream providers by",
            None,
            "Hermes default",
            sort_options,
            config_value(doc.clone(), drafts, name.clone(), |c| {
                c.provider_routing.sort.clone().unwrap_or_default()
            }),
            config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                c.provider_routing.sort = v;
            }),
        )}
        {chip_list(
            "Only use",
            Some("Whitelist of upstream providers OpenRouter may route to."),
            "Add upstream provider…",
            // OpenRouter's upstream provider names are not in any payload
            // Hermes gives us, so these two stay free text.
            Vec::new(),
            config_value(doc.clone(), drafts, name.clone(), |c| {
                c.provider_routing.only.clone()
            }),
            config_committer(doc.clone(), drafts, name.clone(), |c, list| {
                c.provider_routing.only = list;
            }),
        )}
        {chip_list(
            "Ignore",
            Some("Upstream providers OpenRouter must never route to."),
            "Add upstream provider…",
            Vec::new(),
            config_value(doc.clone(), drafts, name.clone(), |c| {
                c.provider_routing.ignore.clone()
            }),
            config_committer(doc.clone(), drafts, name.clone(), |c, list| {
                c.provider_routing.ignore = list;
            }),
        )}
        </div>
    }
    .into_any();

    card(
        "Routing",
        Some(
            "OpenRouter routing preferences: how OpenRouter picks the upstream provider \
             serving a model.",
        ),
        body,
    )
}

fn fallback_card(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: &HermesProfileSettings,
) -> AnyView {
    let name = profile.name.clone();
    let providers = Arc::new(profile.providers.clone().unwrap_or_default());

    // Rebuild rows when the row count changes *or* a row names a different
    // provider. The second half matters because each row's model dropdown is
    // built from whichever provider that row currently names: without it, a
    // row would keep offering the previous provider's models after the
    // provider was switched. Editing only the model leaves the rows standing,
    // so the control in use is not torn out from under the user. Existing
    // entries keep their extra fields (base_url, api_mode, …) untouched — only
    // provider/model are edited here; the rest round-trips on the struct.
    let row_providers = {
        let doc = doc.clone();
        let name = name.clone();
        Memo::new(move |_| {
            effective_config(&doc, &drafts.get(), &name)
                .fallback_providers
                .iter()
                .map(|fallback| fallback.provider.clone())
                .collect::<Vec<_>>()
        })
    };

    let rows = {
        let doc = doc.clone();
        let name = name.clone();
        let providers = providers.clone();
        move || {
            (0..row_providers.get().len())
                .map(|idx| fallback_row(doc.clone(), drafts, name.clone(), idx, providers.clone()))
                .collect::<Vec<_>>()
        }
    };

    let on_add = {
        let doc = doc.clone();
        let name = name.clone();
        move |_| {
            update_profile_config(drafts, &doc, &name, |cfg| {
                cfg.fallback_providers.push(HermesFallbackProvider {
                    provider: String::new(),
                    model: String::new(),
                    extra: Default::default(),
                });
            });
        }
    };

    let body = view! {
        <div class="settings-hermes-fallback-rows">{rows}</div>
        <div>
            <button type="button" class="settings-btn" on:click=on_add>
                "Add fallback"
            </button>
        </div>
    }
    .into_any();

    card(
        "Fallback chain",
        Some("Provider/model pairs tried in order when the primary model is unavailable."),
        body,
    )
}

/// One half of a fallback row: a dropdown of `choices`, or a free-text input
/// when the host reported none.
///
/// A fallback entry is only valid with both halves filled (the server refuses a
/// half-filled one), so the empty option is a "Choose …" prompt rather than an
/// unset value. A current value the host did not offer stays selectable and is
/// labelled as such — dropping it would silently rewrite a working config the
/// moment the page rendered.
/// Erased parameters, for the reason spelled out on [`select_control`].
fn fallback_field(
    aria_label: String,
    text_placeholder: &'static str,
    prompt: &'static str,
    unknown_note: &'static str,
    choices: Vec<(String, String)>,
    value: Signal<String>,
    commit: Callback<String>,
) -> AnyView {
    if choices.is_empty() {
        return input_control("text", None, text_placeholder.to_owned(), value, commit);
    }
    // A current value the host did not offer is appended rather than dropped,
    // so opening the page can never silently rewrite a working config.
    let mut options = choices;
    let current = value.get_untracked();
    if !current.is_empty() && !options.iter().any(|(v, _)| *v == current) {
        options.push((current.clone(), format!("{current} ({unknown_note})")));
    }
    select_control(
        Some(aria_label),
        None,
        Some(prompt.to_owned()),
        false,
        Signal::derive(move || options.clone()),
        value,
        commit,
    )
}

fn fallback_row(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: String,
    idx: usize,
    providers: Arc<Vec<HermesProviderState>>,
) -> AnyView {
    let provider_value = config_value(doc.clone(), drafts, profile.clone(), move |c| {
        c.fallback_providers
            .get(idx)
            .map(|f| f.provider.clone())
            .unwrap_or_default()
    });
    let model_value = config_value(doc.clone(), drafts, profile.clone(), move |c| {
        c.fallback_providers
            .get(idx)
            .map(|f| f.model.clone())
            .unwrap_or_default()
    });
    let commit_provider =
        config_committer(doc.clone(), drafts, profile.clone(), move |c, v: String| {
            if let Some(entry) = c.fallback_providers.get_mut(idx) {
                entry.provider = v;
            }
        });
    let commit_model =
        config_committer(doc.clone(), drafts, profile.clone(), move |c, v: String| {
            if let Some(entry) = c.fallback_providers.get_mut(idx) {
                entry.model = v;
            }
        });

    // Both halves are the same control with different data, so they share one
    // builder: two inline `view!` blocks here would monomorphize two more deep
    // view types into a wasm test binary that is already near its ceiling.
    let current_provider = provider_value();
    let provider_choices: Vec<(String, String)> = providers
        .iter()
        .map(|p| {
            let label = if p.slug == p.name {
                p.slug.clone()
            } else {
                format!("{} ({})", p.name, p.slug)
            };
            (p.slug.clone(), label)
        })
        .collect();
    let provider_control = fallback_field(
        format!("Fallback {} provider", idx + 1),
        "provider",
        "Choose a provider",
        "not probed",
        provider_choices,
        Signal::derive(provider_value),
        Callback::new(move |v: String| commit_provider(v)),
    );

    // The model list follows whichever provider this row currently names, so
    // the pair is always internally consistent.
    let model_choices: Vec<(String, String)> = providers
        .iter()
        .find(|p| p.slug == current_provider)
        .map(|p| {
            p.models
                .iter()
                .map(|model| (model.clone(), model.clone()))
                .collect()
        })
        .unwrap_or_default();
    let model_control = fallback_field(
        format!("Fallback {} model", idx + 1),
        "model",
        "Choose a model",
        "not offered by this provider",
        model_choices,
        Signal::derive(model_value),
        Callback::new(move |v: String| commit_model(v)),
    );

    let on_remove = {
        let doc = doc.clone();
        let profile = profile.clone();
        move |_| {
            update_profile_config(drafts, &doc, &profile, |cfg| {
                if idx < cfg.fallback_providers.len() {
                    cfg.fallback_providers.remove(idx);
                }
            });
        }
    };

    view! {
        <div class="settings-hermes-fallback-row">
            {provider_control}
            {model_control}
            <button
                type="button"
                class="settings-btn"
                aria-label=format!("Remove fallback {}", idx + 1)
                on:click=on_remove
            >
                "Remove"
            </button>
        </div>
    }
    .into_any()
}

fn agent_card(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: &HermesProfileSettings,
) -> AnyView {
    let name = profile.name.clone();
    let mut context_options = vec![
        ("auto".to_owned(), "Auto".to_owned()),
        ("focus".to_owned(), "Focus".to_owned()),
        ("on".to_owned(), "On".to_owned()),
        ("off".to_owned(), "Off".to_owned()),
    ];
    ensure_current_option(
        &mut context_options,
        effective_config(&doc, &drafts.get_untracked(), &name)
            .agent
            .coding_context,
    );

    // The catalogue turns this from "type a name and hope" into a picker.
    // Without it (probe failed) the free-text list is still the right control,
    // so the two share one committer.
    let toolset_choices: Vec<(String, String)> = profile
        .toolsets
        .as_ref()
        .map(|toolsets| {
            toolsets
                .iter()
                .map(|toolset| {
                    let label = match &toolset.description {
                        Some(description) => {
                            format!("{} — {description}", toolset.name)
                        }
                        None => toolset.name.clone(),
                    };
                    (toolset.name.clone(), label)
                })
                .collect()
        })
        .unwrap_or_default();

    let body = view! {
        <div class="settings-hermes-grid">
            {number_field(
                "Max turns",
                Some("Cap on agent loop turns per request."),
                HERMES_DEFAULT_MAX_TURNS,
                config_value(doc.clone(), drafts, name.clone(), |c| c.agent.max_turns),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                    c.agent.max_turns = v;
                }),
            )}
            {select_field(
                "Coding context",
                None,
                HERMES_DEFAULT_CODING_CONTEXT,
                context_options,
                config_value(doc.clone(), drafts, name.clone(), |c| {
                    c.agent.coding_context.clone().unwrap_or_default()
                }),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                    c.agent.coding_context = v;
                }),
            )}
        </div>
        {chip_list(
            "Disabled toolsets",
            Some("Toolsets the agent must not load."),
            "Add toolset…",
            toolset_choices,
            config_value(doc.clone(), drafts, name.clone(), |c| {
                c.agent.disabled_toolsets.clone()
            }),
            config_committer(doc.clone(), drafts, name.clone(), |c, list| {
                c.agent.disabled_toolsets = list;
            }),
        )}
    }
    .into_any();

    card(
        "Agent",
        Some("Agent loop limits and coding-context behavior."),
        body,
    )
}

fn tool_search_card(
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    profile: &str,
) -> AnyView {
    let name = profile.to_owned();
    let mut enabled_options = vec![
        ("auto".to_owned(), "Auto".to_owned()),
        ("on".to_owned(), "On".to_owned()),
        ("off".to_owned(), "Off".to_owned()),
    ];
    ensure_current_option(
        &mut enabled_options,
        effective_config(&doc, &drafts.get_untracked(), &name)
            .tool_search
            .enabled,
    );
    let body = view! {
        <div class="settings-hermes-grid">
            {select_field(
                "Enabled",
                None,
                HERMES_DEFAULT_TOOL_SEARCH,
                enabled_options,
                config_value(doc.clone(), drafts, name.clone(), |c| {
                    c.tool_search.enabled.clone().unwrap_or_default()
                }),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                    c.tool_search.enabled = v;
                }),
            )}
            {float_field(
                "Activation threshold (%)",
                Some("Percent of the context window at which auto mode activates."),
                HERMES_DEFAULT_THRESHOLD_PCT,
                config_value(doc.clone(), drafts, name.clone(), |c| {
                    c.tool_search.threshold_pct
                }),
                config_committer(doc.clone(), drafts, name.clone(), |c, v| {
                    c.tool_search.threshold_pct = v;
                }),
            )}
        </div>
    }
    .into_any();

    card(
        "Tool Search",
        Some("Progressive tool disclosure for large tool surfaces."),
        body,
    )
}

// ---------------------------------------------------------------------------
// Save bar
// ---------------------------------------------------------------------------

fn save_bar(
    state: &AppState,
    host_id: &str,
    doc: Arc<HermesNativeSettingsDoc>,
    drafts: RwSignal<HashMap<String, HermesProfileConfig>>,
    saving: bool,
    save_error: Option<String>,
) -> AnyView {
    let dirty = {
        let doc = doc.clone();
        Memo::new(move |_| {
            let map = drafts.get();
            doc.profiles
                .iter()
                .any(|p| map.get(&p.name).is_some_and(|d| *d != p.config))
        })
    };

    let error_banner = save_error.map(|message| {
        view! {
            <div class="settings-native-error" role="alert">{message}</div>
        }
    });
    let saving_note =
        saving.then(|| view! { <span class="settings-hermes-saving-note">"Saving…"</span> });
    let dirty_note = move || {
        dirty
            .get()
            .then(|| view! { <span class="settings-hermes-dirty-note">"Unsaved changes"</span> })
    };

    let on_discard = move |_| drafts.set(HashMap::new());
    let on_save = {
        let state = state.clone();
        let host_id = host_id.to_owned();
        move |_| {
            if saving || !dirty.get_untracked() {
                return;
            }
            save_config_edits(&state, &host_id, drafts);
        }
    };

    view! {
        <div class="settings-hermes-savebar-wrap">
            {error_banner}
            <div class="settings-hermes-savebar">
                {dirty_note}
                <span class="settings-hermes-savebar-spacer"></span>
                {saving_note}
                <button
                    type="button"
                    class="settings-btn"
                    disabled=move || saving || !dirty.get()
                    on:click=on_discard
                >
                    "Discard"
                </button>
                <button
                    type="button"
                    class="settings-btn settings-btn-primary"
                    disabled=move || saving || !dirty.get()
                    on:click=on_save
                >
                    "Save"
                </button>
            </div>
        </div>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Field widgets
// ---------------------------------------------------------------------------

fn labeled_field(label: &str, hint: Option<&str>, control: AnyView) -> AnyView {
    view! {
        <div class="settings-native-field">
            <span class="settings-form-label">{label.to_owned()}</span>
            {control}
            {hint.map(|h| view! { <p class="settings-description">{h.to_owned()}</p> })}
        </div>
    }
    .into_any()
}

/// The one `<input>` on this page.
///
/// Text, integer and float fields were three near-identical `view!` blocks
/// taking `impl Fn` parameters, so each of their eleven call sites
/// monomorphized its own copy of the input tree. Erased handles plus a single
/// builder give one instantiation for all of them — see [`select_control`] for
/// why that matters here.
///
/// The raw string is what crosses the boundary; parsing lives with the caller
/// that knows the type. Blank always means "unset", which removes the key from
/// Hermes's config; an unparseable number is ignored rather than committed as
/// garbage, exactly as the typed fields did before.
fn input_control(
    input_type: &'static str,
    step: Option<&'static str>,
    placeholder: String,
    value: Signal<String>,
    commit: Callback<String>,
) -> AnyView {
    view! {
        <input
            type=input_type
            step=step
            class="settings-input settings-native-input"
            placeholder=placeholder
            autocomplete="off"
            prop:value=move || value.get()
            on:change=move |ev| commit.run(event_target_value(&ev).trim().to_owned())
        />
    }
    .into_any()
}

/// Text input committing a trimmed `Option<String>` on change (blur/Enter).
/// Blank commits `None`, which removes the key from Hermes's config.
fn text_field(
    label: &str,
    placeholder: &str,
    hint: Option<&str>,
    value: impl Fn() -> String + Send + Sync + 'static,
    commit: impl Fn(Option<String>) + Send + Sync + 'static,
) -> AnyView {
    let control = input_control(
        "text",
        None,
        placeholder.to_owned(),
        Signal::derive(value),
        Callback::new(move |raw: String| commit((!raw.is_empty()).then_some(raw))),
    );
    labeled_field(label, hint, control)
}

/// Numeric input committing `Option<f64>` on change (Hermes models this as a
/// float; fractional values are valid). Blank commits `None`.
fn float_field(
    label: &str,
    hint: Option<&str>,
    placeholder: &str,
    value: impl Fn() -> Option<f64> + Send + Sync + 'static,
    commit: impl Fn(Option<f64>) + Send + Sync + 'static,
) -> AnyView {
    let control = input_control(
        "number",
        Some("any"),
        placeholder.to_owned(),
        Signal::derive(move || value().map(|n| n.to_string()).unwrap_or_default()),
        Callback::new(move |raw: String| {
            if raw.is_empty() {
                commit(None);
            } else if let Ok(parsed) = raw.parse::<f64>() {
                commit(Some(parsed));
            }
        }),
    );
    labeled_field(label, hint, control)
}

/// Numeric input committing `Option<i64>` on change. Blank commits `None`.
fn number_field(
    label: &str,
    hint: Option<&str>,
    placeholder: &str,
    value: impl Fn() -> Option<i64> + Send + Sync + 'static,
    commit: impl Fn(Option<i64>) + Send + Sync + 'static,
) -> AnyView {
    let control = input_control(
        "number",
        Some("1"),
        placeholder.to_owned(),
        Signal::derive(move || value().map(|n| n.to_string()).unwrap_or_default()),
        Callback::new(move |raw: String| {
            if raw.is_empty() {
                commit(None);
            } else if let Ok(parsed) = raw.parse::<i64>() {
                commit(Some(parsed));
            }
        }),
    );
    labeled_field(label, hint, control)
}

/// The one `<select>` on this page.
///
/// Every dropdown here — config field, fallback half, chip picker, profile
/// switcher — is the same shape: an empty entry, then options, with selection
/// driven by a reactive `selected` prop on each option so the current value
/// renders regardless of mount order. They share this builder rather than
/// spelling out four `view!` blocks, because each distinct one monomorphizes
/// its own deeply nested view type into a wasm test binary that runs every
/// frontend test in a single browser instance and is already at its memory
/// ceiling. `options` is a closure so a caller whose list depends on other
/// state (the toolset picker hides what is already chosen) stays reactive.
/// Every parameter is a type-erased handle (`Signal` / `Callback`) rather than
/// an `impl Fn` generic. That distinction is the whole point: a generic
/// parameter monomorphizes the entire body — including this `view!` tree — once
/// per call site, which makes a "shared" helper *bigger* than the inline blocks
/// it replaced. Erased handles give exactly one instantiation.
fn select_control(
    aria_label: Option<String>,
    extra_class: Option<&'static str>,
    // `None` omits the empty entry, for a control whose value is never unset
    // (the profile switcher always has a profile selected).
    empty_label: Option<String>,
    disabled: bool,
    options: Signal<Vec<(String, String)>>,
    value: Signal<String>,
    commit: Callback<String>,
) -> AnyView {
    let class = match extra_class {
        Some(extra) => format!("settings-select {extra}"),
        None => "settings-select".to_owned(),
    };
    let option_views = move || {
        options
            .get()
            .into_iter()
            .map(|(option_value, option_label)| {
                let selected = {
                    let option_value = option_value.clone();
                    move || value.get() == option_value
                };
                view! {
                    <option value=option_value prop:selected=selected>{option_label}</option>
                }
            })
            .collect::<Vec<_>>()
    };
    let empty_option =
        empty_label.map(|label| view! { <option value="" prop:selected=move || value.get().is_empty()>{label}</option> });
    view! {
        <select
            class=class
            aria-label=aria_label
            disabled=disabled
            on:change=move |ev| commit.run(event_target_value(&ev))
        >
            {empty_option}
            {option_views}
        </select>
    }
    .into_any()
}

/// Labelled select committing `Option<String>` ("" ↔ `None`, which removes the
/// key from Hermes's config).
fn select_field(
    label: &str,
    hint: Option<&str>,
    unset_label: &str,
    options: Vec<(String, String)>,
    value: impl Fn() -> String + Clone + Send + Sync + 'static,
    commit: impl Fn(Option<String>) + Send + Sync + 'static,
) -> AnyView {
    let control = select_control(
        None,
        None,
        Some(unset_label.to_owned()),
        false,
        Signal::derive(move || options.clone()),
        Signal::derive(value),
        Callback::new(move |selected: String| commit((!selected.is_empty()).then_some(selected))),
    );
    labeled_field(label, hint, control)
}

/// Editable chip list: chips with a remove button, plus an input + Add button
/// (Enter also adds). Duplicates are ignored; the whole edited list is
/// committed at once.
///
/// `choices` upgrades the input to a dropdown of the values the host actually
/// reports, already-added ones filtered out. It is empty when the host could
/// not tell us (a failed probe), and then the free-text input is the honest
/// control: refusing to accept a value we cannot confirm would make a working
/// config uneditable.
fn chip_list(
    label: &str,
    hint: Option<&str>,
    placeholder: &str,
    choices: Vec<(String, String)>,
    items: impl Fn() -> Vec<String> + Clone + Send + Sync + 'static,
    commit: impl Fn(Vec<String>) + Clone + Send + Sync + 'static,
) -> AnyView {
    let entry = RwSignal::new(String::new());
    let items_memo = Memo::new({
        let items = items.clone();
        move |_| items()
    });

    let chips = {
        let items = items.clone();
        let commit = commit.clone();
        move || {
            let items = items.clone();
            let commit = commit.clone();
            items_memo
                .get()
                .into_iter()
                .enumerate()
                .map(|(idx, item)| {
                    let on_remove = {
                        let items = items.clone();
                        let commit = commit.clone();
                        move |_| {
                            let mut list = items();
                            if idx < list.len() {
                                list.remove(idx);
                                commit(list);
                            }
                        }
                    };
                    view! {
                        <span class="settings-hermes-chip">
                            <span class="settings-hermes-chip-text">{item.clone()}</span>
                            <button
                                type="button"
                                class="settings-hermes-chip-remove"
                                aria-label=format!("Remove {item}")
                                on:click=on_remove
                            >
                                "×"
                            </button>
                        </span>
                    }
                })
                .collect::<Vec<_>>()
        }
    };

    let add = {
        let items = items.clone();
        let commit = commit.clone();
        move || {
            let value = entry.get_untracked().trim().to_owned();
            if value.is_empty() {
                return;
            }
            let mut list = items();
            if !list.contains(&value) {
                list.push(value);
                commit(list);
            }
            entry.set(String::new());
        }
    };
    let add_on_key = {
        let add = add.clone();
        move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Enter" {
                ev.prevent_default();
                add();
            }
        }
    };
    let add_on_click = {
        let add = add.clone();
        move |_| add()
    };

    let input = if choices.is_empty() {
        view! {
            <input
                type="text"
                class="settings-input"
                placeholder=placeholder.to_owned()
                aria-label=placeholder.to_owned()
                autocomplete="off"
                prop:value=move || entry.get()
                on:input=move |ev| entry.set(event_target_value(&ev))
                on:keydown=add_on_key
            />
        }
        .into_any()
    } else {
        // Already-added values are chips, not repeat offers.
        select_control(
            Some(placeholder.to_owned()),
            None,
            Some(placeholder.to_owned()),
            false,
            Signal::derive(move || {
                let chosen = items_memo.get();
                choices
                    .iter()
                    .filter(|(value, _)| !chosen.contains(value))
                    .cloned()
                    .collect()
            }),
            Signal::derive(move || entry.get()),
            Callback::new(move |selected: String| entry.set(selected)),
        )
    };

    let control = view! {
        <div class="settings-hermes-chips">{chips}</div>
        <div class="settings-hermes-chip-add">
            {input}
            <button type="button" class="settings-btn" on:click=add_on_click>
                "Add"
            </button>
        </div>
    }
    .into_any();
    labeled_field(label, hint, control)
}

// ---------------------------------------------------------------------------
// Frontend UI tests (load-bearing — see CLAUDE.md / AGENTS.md). They assert
// what the user perceives: rendered profile/provider text, connection badge
// counts, input values swapping with the selected profile, and that a typed
// API key never appears anywhere in the DOM.
// ---------------------------------------------------------------------------

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlInputElement};

    wasm_bindgen_test_configure!(run_in_browser);

    /// Inject the production stylesheet once per test session so the page
    /// renders with real styling.
    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-hermes")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-hermes");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: fixed; top: 0; left: 0; width: 800px; height: 600px; \
                 overflow: auto; z-index: 2147483647; background: white;",
            )
            .unwrap();
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

    /// Two profiles; the default one has two providers: an authenticated
    /// `api_key` provider and an unconfigured OAuth provider with a warning.
    fn fixture_doc() -> HermesNativeSettingsDoc {
        HermesNativeSettingsDoc {
            version: HERMES_NATIVE_SETTINGS_VERSION,
            profiles: vec![
                HermesProfileSettings {
                    name: HERMES_DEFAULT_PROFILE.to_owned(),
                    home_dir: "/home/u/.hermes".to_owned(),
                    base_config: None,
                    config: HermesProfileConfig {
                        model: protocol::hermes_config::HermesModelConfig {
                            provider: Some("openrouter".to_owned()),
                            model: Some("anthropic/claude-sonnet-4".to_owned()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    providers: Some(vec![
                        HermesProviderState {
                            slug: "openrouter".to_owned(),
                            name: "OpenRouter".to_owned(),
                            authenticated: true,
                            auth_type: Some("api_key".to_owned()),
                            key_env: Some("OPENROUTER_API_KEY".to_owned()),
                            warning: None,
                            model_count: 42,
                            // Deliberately empty: this profile exercises the
                            // free-text model field a host whose probe reports
                            // no model list still has to offer.
                            models: Vec::new(),
                        },
                        HermesProviderState {
                            slug: "copilot".to_owned(),
                            name: "GitHub Copilot".to_owned(),
                            authenticated: false,
                            auth_type: Some("oauth_device_code".to_owned()),
                            key_env: None,
                            warning: Some("Run gh auth login to enable Copilot".to_owned()),
                            model_count: 0,
                            models: Vec::new(),
                        },
                    ]),
                    providers_error: None,
                    active_model: Some("anthropic/claude-sonnet-4".to_owned()),
                    active_provider: Some("openrouter".to_owned()),
                    toolsets: None,
                },
                HermesProfileSettings {
                    name: "work".to_owned(),
                    home_dir: "/home/u/.hermes/profiles/work".to_owned(),
                    base_config: None,
                    config: HermesProfileConfig {
                        model: protocol::hermes_config::HermesModelConfig {
                            model: Some("openai/gpt-5".to_owned()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    providers: None,
                    providers_error: None,
                    active_model: None,
                    active_provider: None,
                    toolsets: None,
                },
            ],
            profile_actions: Vec::new(),
            actions: Vec::new(),
        }
    }

    /// One provider row, spelled out so each test can compose the exact mix of
    /// row shapes (connected/not, key/OAuth) whose alignment it cares about.
    fn provider(
        slug: &str,
        name: &str,
        authenticated: bool,
        auth_type: &str,
        warning: Option<&str>,
    ) -> HermesProviderState {
        HermesProviderState {
            slug: slug.to_owned(),
            name: name.to_owned(),
            authenticated,
            auth_type: Some(auth_type.to_owned()),
            key_env: (auth_type == "api_key").then(|| format!("{}_API_KEY", slug.to_uppercase())),
            warning: warning.map(str::to_owned),
            model_count: if authenticated { 7 } else { 0 },
            models: if authenticated {
                (1..=7).map(|n| format!("{slug}/model-{n}")).collect()
            } else {
                Vec::new()
            },
        }
    }

    fn doc_with_providers(providers: Vec<HermesProviderState>) -> HermesNativeSettingsDoc {
        HermesNativeSettingsDoc {
            version: HERMES_NATIVE_SETTINGS_VERSION,
            profiles: vec![HermesProfileSettings {
                name: HERMES_DEFAULT_PROFILE.to_owned(),
                home_dir: "/home/u/.hermes".to_owned(),
                base_config: None,
                config: HermesProfileConfig::default(),
                providers: Some(providers),
                providers_error: None,
                active_model: None,
                active_provider: None,
                toolsets: None,
            }],
            profile_actions: Vec::new(),
            actions: Vec::new(),
        }
    }

    /// Every value the user can currently see in a form control, across both
    /// inputs and selects. The "other profile's value must not render" checks
    /// need this: a value that moved from an `<input>` into a `<select>` is
    /// still on screen, and a check that only looked at inputs would call that
    /// a pass.
    fn control_values(container: &HtmlElement) -> Vec<String> {
        let mut values = input_values(container);
        let nodes = container.query_selector_all("select").unwrap();
        for i in 0..nodes.length() {
            if let Some(select) = nodes
                .item(i)
                .and_then(|node| node.dyn_into::<web_sys::HtmlSelectElement>().ok())
            {
                values.push(select.value());
            }
        }
        values
    }

    /// Pick a profile the way the user does — the profile switcher is a
    /// dropdown, so this sets and fires `change` on it.
    fn select_profile(container: &HtmlElement, name: &str) {
        let select: web_sys::HtmlSelectElement = container
            .query_selector(".settings-hermes-profile-select")
            .unwrap()
            .expect("profile select")
            .dyn_into()
            .unwrap();
        select.set_value(name);
        select
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
    }

    fn install_doc(state: &AppState, doc: HermesNativeSettingsDoc) {
        let snapshot = BackendNativeSettingsSnapshot {
            backend_kind: BackendKind::Hermes,
            status: BackendConfigSnapshotStatus::Ready,
            settings: Some(serde_json::to_value(doc).unwrap()),
            groups: Vec::new(),
            message: None,
            advisories: Vec::new(),
        };
        state.backend_native_settings.update(|by_host| {
            by_host
                .entry("h".to_owned())
                .or_default()
                .insert(BackendKind::Hermes, snapshot);
        });
        state.selected_host_id.set(Some("h".to_owned()));
    }

    fn container_text(container: &HtmlElement) -> String {
        container.text_content().unwrap_or_default()
    }

    fn input_values(container: &HtmlElement) -> Vec<String> {
        let nodes = container.query_selector_all("input").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlInputElement>().ok())
            .map(|input| input.value())
            .collect()
    }

    fn button_with_text(container: &HtmlElement, needle: &str) -> HtmlElement {
        let nodes = container.query_selector_all("button").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|button| button.text_content().unwrap_or_default().contains(needle))
            .unwrap_or_else(|| panic!("no button containing {needle:?}"))
    }

    fn elements(container: &HtmlElement, selector: &str) -> Vec<HtmlElement> {
        let nodes = container.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
            .collect()
    }

    /// Type into an input the way the component's `on:input` handler sees it.
    fn type_into(input: &HtmlInputElement, value: &str) {
        input.set_value(value);
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }

    /// Commit a value the way the config fields see it — they listen for
    /// `change` (blur/Enter), not for every keystroke.
    fn commit_input(input: &HtmlInputElement, value: &str) {
        input.set_value(value);
        input
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
    }

    fn mount_doc(container: &HtmlElement, doc: HermesNativeSettingsDoc) -> impl Sized {
        mount_doc_with_disabled(container, doc, Vec::new())
    }

    /// Mount with a Tyde-owned disabled-provider list for the default profile.
    /// The list lives in host settings, not in the Hermes document, because
    /// Hermes has no provider enable/disable flag of its own.
    fn mount_doc_with_disabled(
        container: &HtmlElement,
        doc: HermesNativeSettingsDoc,
        disabled: Vec<String>,
    ) -> impl Sized {
        let container = container.clone();
        mount_to(container, move || {
            let state = AppState::new();
            install_doc(&state, doc.clone());
            if !disabled.is_empty() {
                let settings = settings_model::HostSettings {
                    enabled_backends: vec![BackendKind::Hermes],
                    default_backend: None,
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    mobile_broker_auth: Default::default(),
                    mobile_direct_hosting_enabled: false,
                    mobile_direct_bind_addr: None,
                    mobile_direct_bundle_dir: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    tyde_agent_control_max_depth: settings_model::default_agent_control_max_depth(),
                    delegation_launch_profile_order:
                        settings_model::default_delegation_launch_profile_order(),
                    complexity_tiers_enabled: false,
                    backend_tier_configs: HashMap::new(),
                    background_agent_features: Default::default(),
                    supervisor: Default::default(),
                    code_intel: Default::default(),
                    backend_config: HashMap::new(),
                    launch_profiles: Default::default(),
                    voice: Default::default(),
                    hermes_disabled_providers: HashMap::from([(
                        HERMES_DEFAULT_PROFILE.to_owned(),
                        disabled.clone(),
                    )]),
                };
                state.host_settings_by_host.update(|by_host| {
                    by_host.insert("h".to_owned(), settings);
                });
            }
            provide_context(state);
            hermes_settings_page_body("h")
        })
    }

    #[wasm_bindgen_test]
    async fn renders_profiles_and_connected_providers() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        let text = container_text(&container);
        // The profile switcher is a dropdown (it used to be one chip per
        // profile, which stopped fitting once profiles could be created from
        // here). The contract it carries is unchanged and asserted here in
        // full: every discovered profile is offered, the default one is
        // labelled "Default" rather than by its internal slug, and each entry
        // names the model that profile resolves to — so a profile can still be
        // identified without selecting it first.
        let options = elements(&container, ".settings-hermes-profile-select option");
        let labels: Vec<String> = options
            .iter()
            .map(|option| option.text_content().unwrap_or_default())
            .collect();
        assert_eq!(
            labels.len(),
            2,
            "every discovered profile must be offered: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.contains("Default") && l.contains("anthropic/claude-sonnet-4")),
            "the default profile must be named and show its active model: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.contains("work") && l.contains("openai/gpt-5")),
            "a named profile must be named and show its configured model: {labels:?}"
        );

        // The Providers tab lists what this profile can actually use, with its
        // status and model count. Providers still needing a credential are not
        // rows here — they live in the Add-a-provider dialog, which
        // `unconfigured_providers_are_offered_in_the_add_dialog` covers.
        assert!(text.contains("OpenRouter"), "provider name missing: {text}");
        assert_eq!(
            elements(&container, ".settings-hermes-badge-connected").len(),
            1,
            "expected one Connected badge, one per authenticated provider: {text}"
        );
        assert_eq!(
            elements(&container, ".settings-hermes-badge-muted").len(),
            0,
            "the Providers tab lists only connected providers: {text}"
        );
        assert!(
            !text.contains("GitHub Copilot"),
            "an unconfigured provider belongs in the add dialog, not the tab: {text}"
        );
        assert!(text.contains("42 models"), "model count missing: {text}");
        assert!(
            text.contains("1 provider connected"),
            "the tab should say how many providers are connected: {text}"
        );
    }

    /// The other half of the old single-list contract: every provider Hermes
    /// reports is still reachable, still shows its connection state (by being
    /// offered as an addition), and still carries Hermes's own setup hint —
    /// now in the dialog that replaced the fifty-row list.
    #[wasm_bindgen_test]
    async fn unconfigured_providers_are_offered_in_the_add_dialog() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        assert!(
            container
                .query_selector("[role='dialog']")
                .unwrap()
                .is_none(),
            "the dialog must not be open until asked for"
        );
        button_with_text(&container, "Add a provider").click();
        next_tick().await;

        let dialog_text = |container: &HtmlElement| {
            container
                .query_selector("[role='dialog']")
                .unwrap()
                .expect("add-provider dialog")
                .text_content()
                .unwrap_or_default()
        };
        let text = dialog_text(&container);
        assert!(
            text.contains("GitHub Copilot"),
            "the unconfigured provider must be offered: {text}"
        );
        assert!(
            text.contains("Device code"),
            "the row must say how this provider authenticates: {text}"
        );
        assert!(
            !text.contains("OpenRouter"),
            "an already-connected provider is not something to add: {text}"
        );

        // Picking it explains what to do — including Hermes's own hint, which
        // carries information the auth mechanism alone does not.
        button_with_text(&container, "GitHub Copilot").click();
        next_tick().await;
        let text = dialog_text(&container);
        assert!(
            text.contains("Run gh auth login to enable Copilot"),
            "Hermes's own setup hint must still reach the user: {text}"
        );
        assert!(
            text.contains("Default"),
            "the instruction must name the profile it applies to: {text}"
        );
        assert!(
            container
                .query_selector("[role='dialog'] input[type='password']")
                .unwrap()
                .is_none(),
            "a provider Tyde cannot key from here must not offer a key field"
        );
    }

    #[wasm_bindgen_test]
    async fn switching_profiles_swaps_model_defaults() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        // Model defaults live on their own tab now.
        button_with_text(&container, "Model").click();
        next_tick().await;

        // The default profile's configured model is shown as an editable value.
        // Checked across inputs AND selects: the model control renders as a
        // dropdown when the provider reports its models, and the point of the
        // negative assertions below is that the other profile's value is not on
        // screen at all — in any control.
        let values = control_values(&container);
        assert!(
            values.iter().any(|v| v == "anthropic/claude-sonnet-4"),
            "default profile's model not editable: {values:?}"
        );
        assert!(
            !values.iter().any(|v| v == "openai/gpt-5"),
            "other profile's model must not render while Default is selected: {values:?}"
        );

        select_profile(&container, "work");
        next_tick().await;

        let values = control_values(&container);
        assert!(
            values.iter().any(|v| v == "openai/gpt-5"),
            "work profile's model not shown after switching: {values:?}"
        );
        assert!(
            !values.iter().any(|v| v == "anthropic/claude-sonnet-4"),
            "default profile's model still rendered after switching: {values:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn queued_api_key_is_never_rendered() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        // The authenticated api_key provider offers a key replacement flow.
        button_with_text(&container, "Replace key").click();
        next_tick().await;

        let key_input: HtmlInputElement = container
            .query_selector("input[type='password']")
            .unwrap()
            .expect("inline key input after opening the editor")
            .dyn_into()
            .unwrap();
        // The input is never prefilled.
        assert_eq!(key_input.value(), "", "key input must start empty");

        let secret = "sk-secret-test-123";
        key_input.set_value(secret);
        button_with_text(&container, "Save key").click();
        next_tick().await;

        // The key must not appear anywhere in the DOM — not as text, not in
        // any attribute — and the editor is gone (its input discarded).
        let html = container.inner_html();
        assert!(!html.contains(secret), "queued API key leaked into the DOM");
        assert!(
            !container_text(&container).contains(secret),
            "queued API key leaked into rendered text"
        );
        assert!(
            container
                .query_selector("input[type='password']")
                .unwrap()
                .is_none(),
            "key editor should close after queueing"
        );
    }

    /// Provider rows carry different controls (two buttons or one), and their
    /// connection status must still read as one scannable column rather than
    /// drifting left and right with each row's own button widths.
    #[wasm_bindgen_test]
    async fn provider_status_badges_share_one_column() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(
            &container,
            doc_with_providers(vec![
                // api_key: "Replace key…" and "Disconnect".
                provider("openrouter", "OpenRouter", true, "api_key", None),
                // OAuth: "Disconnect" only.
                provider("copilot", "GitHub Copilot", true, "oauth_device_code", None),
                provider("bedrock", "AWS Bedrock", true, "aws", None),
            ]),
        );
        next_tick().await;

        let badges = elements(&container, ".settings-hermes-badge");
        assert_eq!(badges.len(), 3, "one status badge per provider row");
        let rights: Vec<f64> = badges
            .iter()
            .map(|badge| badge.get_bounding_client_rect().right())
            .collect();
        let first = rights[0];
        assert!(
            rights.iter().all(|right| (right - first).abs() < 1.0),
            "status badges must line up in one column regardless of how many \
             action buttons their row has; right edges were {rights:?}"
        );

        let buttons: Vec<f64> = elements(&container, ".settings-hermes-provider-action-slot")
            .iter()
            .map(|slot| slot.get_bounding_client_rect().right())
            .collect();
        assert_eq!(buttons.len(), 3, "expected one action slot per row");
        let first_button = buttons[0];
        assert!(
            buttons
                .iter()
                .all(|right| (right - first_button).abs() < 1.0),
            "provider action buttons must share a right edge; got {buttons:?}"
        );

        // A fixed action column must still fit its widest pairing. The row that
        // offers both "Replace key…" and "Disconnect" is the tight one, and a
        // column sized for a single button silently ellipsised both labels.
        for button in elements(
            &container,
            ".settings-hermes-provider-action-slot .settings-btn",
        ) {
            let label = button.text_content().unwrap_or_default();
            assert!(
                button.scroll_width() <= button.client_width(),
                "action button label {label:?} is clipped ({}px of content in {}px of box)",
                button.scroll_width(),
                button.client_width(),
            );
        }
    }

    /// A host reporting dozens of providers must not bury the page: the tab
    /// stays short, and the catalogue is searchable in the dialog.
    #[wasm_bindgen_test]
    async fn add_dialog_searches_the_whole_catalogue() {
        ensure_styles_loaded();
        let container = make_container();
        let mut providers = vec![provider("bedrock", "AWS Bedrock", true, "aws", None)];
        for idx in 0..12 {
            providers.push(provider(
                &format!("vendor{idx}"),
                &format!("Vendor {idx}"),
                false,
                "api_key",
                None,
            ));
        }
        let _handle = mount_doc(&container, doc_with_providers(providers));
        next_tick().await;

        assert_eq!(
            elements(&container, ".settings-hermes-provider-row").len(),
            1,
            "only connected providers belong on the tab"
        );
        assert!(
            container_text(&container).contains("12 more available"),
            "the tab should say how many more providers exist: {}",
            container_text(&container)
        );

        button_with_text(&container, "Add a provider").click();
        next_tick().await;
        let rows = |container: &HtmlElement| {
            elements(container, ".settings-hermes-dialog-row")
                .iter()
                .map(|row| row.text_content().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(rows(&container).len(), 12, "every candidate is offered");

        let search: HtmlInputElement = container
            .query_selector("[role='dialog'] input[type='search']")
            .unwrap()
            .expect("catalogue search input")
            .dyn_into()
            .unwrap();
        type_into(&search, "vendor7");
        next_tick().await;
        let hits = rows(&container);
        assert_eq!(
            hits.len(),
            1,
            "search should narrow the catalogue: {hits:?}"
        );
        assert!(
            hits[0].contains("Vendor 7"),
            "search matched the wrong provider: {hits:?}"
        );

        type_into(&search, "");
        next_tick().await;
        assert_eq!(
            rows(&container).len(),
            12,
            "clearing the search restores the full catalogue"
        );
    }

    /// Hermes reports a provider as connected whenever it can find a
    /// credential, and re-detects auto-harvested ones (GitHub Copilot via the
    /// `gh` CLI) after a disconnect. Tyde's own disable list is therefore the
    /// only control that reliably turns one off, so the row must say plainly
    /// that it is off and that the reach of that is Tyde only.
    #[wasm_bindgen_test]
    async fn a_disabled_provider_reads_as_off_and_says_what_that_covers() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc_with_disabled(
            &container,
            doc_with_providers(vec![
                provider("copilot", "GitHub Copilot", true, "oauth_device_code", None),
                provider("bedrock", "AWS Bedrock", true, "aws", None),
            ]),
            vec!["copilot".to_owned()],
        );
        next_tick().await;

        let text = container_text(&container);
        assert_eq!(
            elements(&container, ".settings-hermes-badge-off").len(),
            1,
            "exactly the disabled provider should read as off: {text}"
        );
        assert_eq!(
            elements(&container, ".settings-hermes-badge-connected").len(),
            1,
            "an enabled provider must still read as connected: {text}"
        );
        // The reach of the switch is stated, not implied: this hides the
        // provider in Tyde and does not remove Hermes's credential.
        assert!(
            text.contains("hermes session started outside Tyde"),
            "the row must say the provider is only hidden from Tyde: {text}"
        );
        // Both directions are reachable, so this is not a one-way door.
        button_with_text(&container, "Enable in Tyde");
        button_with_text(&container, "Disable in Tyde");
    }

    /// Deleting a Hermes profile removes a whole `HERMES_HOME`. The dialog has
    /// to name that cost and refuse to fire until the user types the profile
    /// name — a misclick must not be able to destroy chat history.
    #[wasm_bindgen_test]
    async fn deleting_a_profile_needs_the_name_typed_back() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        // The default profile IS ~/.hermes, so it is not deletable at all.
        assert!(
            !container_text(&container).contains("Delete profile"),
            "the default profile must not offer a delete"
        );

        select_profile(&container, "work");
        next_tick().await;
        button_with_text(&container, "Delete profile").click();
        next_tick().await;

        let dialog = container
            .query_selector("[role='dialog']")
            .unwrap()
            .expect("delete confirmation dialog")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let dialog_text = dialog.text_content().unwrap_or_default();
        // What is lost has to be stated, because none of it is recoverable and
        // "delete profile" alone reads like it only drops configuration.
        for expected in ["sessions", "credentials", "cannot be undone"] {
            assert!(
                dialog_text.contains(expected),
                "the dialog must say {expected:?} is at stake: {dialog_text}"
            );
        }
        assert!(
            dialog_text.contains("/home/u/.hermes/profiles/work"),
            "the dialog must name the exact directory: {dialog_text}"
        );

        let confirm = elements(&container, "[role='dialog'] .settings-btn-danger")
            .into_iter()
            .next()
            .expect("delete button");
        assert!(
            confirm.has_attribute("disabled"),
            "the destructive button must start disabled"
        );

        let confirm_input: HtmlInputElement = dialog
            .query_selector("input[type='text']")
            .unwrap()
            .expect("confirmation input")
            .dyn_into()
            .unwrap();
        type_into(&confirm_input, "wor");
        next_tick().await;
        assert!(
            elements(&container, "[role='dialog'] .settings-btn-danger")[0]
                .has_attribute("disabled"),
            "a partial name must not arm the delete"
        );

        type_into(&confirm_input, "work");
        next_tick().await;
        assert!(
            !elements(&container, "[role='dialog'] .settings-btn-danger")[0]
                .has_attribute("disabled"),
            "the exact profile name must arm the delete"
        );
    }

    /// Adding a profile must say what it copies. A Hermes profile is a whole
    /// home directory, and a user who assumes credentials come along would
    /// find the new profile unable to serve a model.
    #[wasm_bindgen_test]
    async fn adding_a_profile_states_what_is_copied() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        button_with_text(&container, "Add profile").click();
        next_tick().await;

        let text = container_text(&container);
        assert!(
            text.contains("config.yaml"),
            "the create field must say the config is copied: {text}"
        );
        assert!(
            text.contains("Credentials, sessions and history are not copied"),
            "the create field must say what is NOT copied: {text}"
        );
    }

    /// "Hermes default" sat next to an explicit "Auto" in the same dropdown
    /// while both selected the same behavior, which read as two different
    /// modes. The unset entry must name the default it actually resolves to.
    #[wasm_bindgen_test]
    async fn unset_options_name_the_real_hermes_default() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        button_with_text(&container, "Agent").click();
        next_tick().await;

        let offered: Vec<String> = elements(&container, "select option")
            .iter()
            .map(|option| option.text_content().unwrap_or_default())
            .collect();
        // Both selects on this tab (coding context, tool search) default to
        // `auto` in Hermes, which is exactly the pair that used to look like
        // two distinct modes.
        assert_eq!(
            offered
                .iter()
                .filter(|o| o.as_str() == "Hermes default (auto)")
                .count(),
            2,
            "each unset option must name the default it resolves to: {offered:?}"
        );
        assert!(
            !offered.iter().any(|o| o.as_str() == "Hermes default"),
            "no unset option may be left as a bare 'Hermes default': {offered:?}"
        );
        // The placeholder for a numeric field carries its default the same way.
        let max_turns: HtmlInputElement = container
            .query_selector("input[type='number']")
            .unwrap()
            .expect("max turns input")
            .dyn_into()
            .unwrap();
        assert_eq!(
            max_turns.placeholder(),
            "Hermes default (90)",
            "a blank numeric field must say what Hermes will use instead"
        );
    }

    /// The default-model field was free text, so a typo produced a config
    /// Hermes would reject at spawn time. When the probe reports a provider's
    /// models, the field offers exactly those.
    #[wasm_bindgen_test]
    async fn default_model_offers_the_providers_own_models() {
        ensure_styles_loaded();
        let container = make_container();
        let mut doc =
            doc_with_providers(vec![provider("bedrock", "AWS Bedrock", true, "aws", None)]);
        doc.profiles[0].config.model.provider = Some("bedrock".to_owned());
        doc.profiles[0].config.model.model = Some("bedrock/model-3".to_owned());
        let _handle = mount_doc(&container, doc);
        next_tick().await;

        button_with_text(&container, "Model").click();
        next_tick().await;

        let offered: Vec<String> = elements(&container, "select option")
            .iter()
            .map(|option| option.text_content().unwrap_or_default())
            .collect();
        assert!(
            offered.iter().any(|o| o == "bedrock/model-3"),
            "the configured model must be offered: {offered:?}"
        );
        assert!(
            offered.iter().any(|o| o == "bedrock/model-7"),
            "every model the provider reports must be offered: {offered:?}"
        );
        assert!(
            control_values(&container).contains(&"bedrock/model-3".to_owned()),
            "the configured model must be the selected one"
        );
    }

    /// Disabled toolsets was a free-text field, so the only way to know a
    /// toolset's name was to have it memorised. The probe's catalogue turns it
    /// into a picker, with each toolset's own description.
    #[wasm_bindgen_test]
    async fn disabled_toolsets_offers_the_probed_catalogue() {
        ensure_styles_loaded();
        let container = make_container();
        let mut doc = fixture_doc();
        doc.profiles[0].toolsets = Some(vec![
            protocol::hermes_config::HermesToolsetInfo {
                name: "browser".to_owned(),
                description: Some("Drive a web browser".to_owned()),
                tool_count: 9,
            },
            protocol::hermes_config::HermesToolsetInfo {
                name: "spotify".to_owned(),
                description: None,
                tool_count: 4,
            },
        ]);
        doc.profiles[0].config.agent.disabled_toolsets = vec!["spotify".to_owned()];
        let _handle = mount_doc(&container, doc);
        next_tick().await;

        button_with_text(&container, "Agent").click();
        next_tick().await;

        let offered: Vec<String> = elements(&container, "select option")
            .iter()
            .map(|option| option.text_content().unwrap_or_default())
            .collect();
        assert!(
            offered
                .iter()
                .any(|o| o.contains("browser") && o.contains("Drive a web browser")),
            "a toolset must be offered with its description: {offered:?}"
        );
        // Already-disabled toolsets are chips, not repeat offers.
        assert!(
            !offered.iter().any(|o| o.contains("spotify")),
            "an already-disabled toolset must not be offered again: {offered:?}"
        );
        assert!(
            container_text(&container).contains("spotify"),
            "the already-disabled toolset must still render as a chip"
        );
    }

    /// The save bar says "Unsaved changes"; the tab strip says *where*.
    #[wasm_bindgen_test]
    async fn tab_strip_marks_the_section_holding_unsaved_edits() {
        ensure_styles_loaded();
        let container = make_container();
        let _handle = mount_doc(&container, fixture_doc());
        next_tick().await;

        assert_eq!(
            elements(&container, ".settings-hermes-tab-dot").len(),
            0,
            "no tab should be marked before an edit"
        );

        button_with_text(&container, "Agent").click();
        next_tick().await;
        let max_turns: HtmlInputElement = container
            .query_selector("input[type='number']")
            .unwrap()
            .expect("max turns input")
            .dyn_into()
            .unwrap();
        commit_input(&max_turns, "42");
        next_tick().await;

        // Leave the tab entirely: the mark has to survive, or it is useless.
        button_with_text(&container, "Providers").click();
        next_tick().await;

        let marked: Vec<String> = elements(&container, ".settings-hermes-tab")
            .iter()
            .filter(|tab| {
                tab.query_selector(".settings-hermes-tab-dot")
                    .unwrap()
                    .is_some()
            })
            .map(|tab| tab.text_content().unwrap_or_default())
            .collect();
        assert_eq!(
            marked.len(),
            1,
            "exactly the edited section should be marked, got {marked:?}"
        );
        assert!(
            marked[0].contains("Agent"),
            "the Agent tab holds the edit, but {marked:?} was marked"
        );
        assert!(
            container_text(&container).contains("Unsaved changes"),
            "the save bar should agree that there is an edit"
        );
    }
}
