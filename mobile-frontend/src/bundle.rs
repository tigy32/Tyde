use leptos::prelude::*;
use protocol::{RejectPayload, StreamPath, TydeReleaseVersion, WelcomePayload};
use wasm_bindgen_futures::spawn_local;

use crate::bridge::loader::{self, BrowserOutcome, UnavailableReason};
use crate::state::{AppMode, AppState, MobileTab};

#[derive(Clone, Debug, PartialEq)]
pub enum HostRelease {
    Welcome {
        stream: StreamPath,
        payload: WelcomePayload,
    },
    Rejected(RejectPayload),
}

impl HostRelease {
    fn target(&self) -> (Option<TydeReleaseVersion>, u32) {
        match self {
            Self::Welcome { payload, .. } => {
                (payload.release_version.clone(), payload.protocol_version)
            }
            Self::Rejected(payload) => (
                payload.release_version.clone(),
                payload.server_protocol_version,
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BundleStatus {
    Idle,
    Deferred(TydeReleaseVersion),
    Preparing(TydeReleaseVersion),
    Unavailable(UnavailableReason),
    Reloading,
}

impl BundleStatus {
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Deferred(version) => Some(format!(
                "Host release {version} is pending. Finish or save your work, then return Home to switch safely."
            )),
            Self::Preparing(version) => Some(format!("Preparing exact host release {version}…")),
            Self::Unavailable(reason) => Some(format!(
                "Exact host client unavailable: {}. No other release was selected.",
                reason.message()
            )),
            Self::Reloading => Some("Switching to the selected host's exact release…".into()),
        }
    }
}

fn safe_to_reload(state: &AppState) -> bool {
    // Home has no composer/editor. Local attachments and queued edits belong
    // to ChatView, so empty global text alone never grants reload permission.
    state.app_mode.get() == AppMode::Workspace
        && state.active_tab.get() == MobileTab::Home
        && !state.viewing_chat.get()
        && state.boot_handoff_complete.get()
        && !state.project_picker_open.get()
        && !state.session_settings_open.get()
        && state.chat_input.get().is_empty()
        && state.pending_submissions.get().is_empty()
        && state.draft_session_settings.get().0.is_empty()
        && state.draft_backend_override.get().is_none()
        && state.draft_custom_agent_id.get().is_none()
        && matches!(state.voice_ui.get(), crate::voice::MobileVoiceState::Idle)
        && state.mobile_shell_error.get().is_none()
}

pub fn restore_selection(state: &AppState) {
    match loader::read_selection() {
        Ok(host) => state.active_local_host_id.set(host),
        Err(reason) => state.bundle_status.set(BundleStatus::Unavailable(reason)),
    }
}

pub fn install(state: AppState) {
    let generation = RwSignal::new(0u64);
    let input_state = state.clone();
    let inputs = Memo::new(move |_| {
        let host = input_state.active_local_host_id.get();
        let release = host
            .as_ref()
            .and_then(|host| input_state.host_releases.get().get(host).cloned());
        (host, release, safe_to_reload(&input_state))
    });
    on_cleanup(move || {
        let _ = loader::cancel();
    });
    Effect::new(move |_| {
        let (host, release, safe) = inputs.get();
        let next = generation.get_untracked() + 1;
        generation.set(next);
        let _ = loader::cancel();
        if let Err(reason) = loader::save_selection(host.as_ref()) {
            state.bundle_status.set(BundleStatus::Unavailable(reason));
            return;
        }
        let Some(host) = host else {
            state.bundle_status.set(BundleStatus::Idle);
            return;
        };
        let Some(release) = release else {
            state.bundle_status.set(BundleStatus::Idle);
            return;
        };
        let (version, protocol) = release.target();
        if let Err(reason) = loader::record_release(&host, version.as_ref(), protocol) {
            state.bundle_status.set(BundleStatus::Unavailable(reason));
            return;
        }
        let Some(version) = version else {
            state
                .bundle_status
                .set(BundleStatus::Unavailable(UnavailableReason::MissingRelease));
            return;
        };
        match loader::boot_version() {
            Ok(boot) if boot == version && matches!(release, HostRelease::Welcome { .. }) => {
                state
                    .bundle_status
                    .set(match loader::confirm(&host, &version, protocol) {
                        BrowserOutcome::Matching => BundleStatus::Idle,
                        BrowserOutcome::Unavailable { reason } => BundleStatus::Unavailable(reason),
                        _ => BundleStatus::Unavailable(UnavailableReason::Bridge),
                    });
                return;
            }
            Ok(_) => {}
            Err(reason) => {
                state.bundle_status.set(BundleStatus::Unavailable(reason));
                return;
            }
        }
        if !safe {
            state.bundle_status.set(BundleStatus::Deferred(version));
            return;
        }
        log::info!("mobile_bundle_sync preparing selected-host release");
        state
            .bundle_status
            .set(BundleStatus::Preparing(version.clone()));
        let state = state.clone();
        spawn_local(async move {
            let result = loader::prepare(&host, &version, protocol).await;
            if generation.try_get_untracked() != Some(next) {
                return;
            }
            // Signals can change before the reactive effect flushes. Recheck
            // the actual inputs immediately before the synchronous commit.
            if state.active_local_host_id.get_untracked().as_ref() != Some(&host)
                || state.host_releases.get_untracked().get(&host) != Some(&release)
                || !untrack(|| safe_to_reload(&state))
            {
                let _ = loader::cancel();
                return;
            }
            let result = match result {
                BrowserOutcome::Ready { ticket } => loader::commit(ticket, &host),
                other => other,
            };
            let status = match result {
                BrowserOutcome::Reloading => BundleStatus::Reloading,
                BrowserOutcome::Unavailable { reason } => {
                    log::warn!("mobile_bundle_sync unavailable reason={reason:?}");
                    BundleStatus::Unavailable(reason)
                }
                _ => BundleStatus::Unavailable(UnavailableReason::Bridge),
            };
            state.bundle_status.set(status);
        });
    });
}
