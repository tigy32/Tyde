use super::*;
use crate::tychat::{Pairing, SecretBotState, Session};
use protocol::{TychatBridgeStatus, TychatCommandPayload, TychatSettingsApplication};

pub(super) async fn validate_tychat_settings(
    state: &HostState,
    settings: &settings_model::HostSettings,
) -> Result<(), String> {
    let config = &settings.tychat;
    let Some(kind) = config.backend_kind else {
        return if config.enabled {
            Err("Choose a Tychat backend before enabling".into())
        } else {
            Ok(())
        };
    };
    if !crate::backend::capabilities_for_backend_kind(kind)
        .contains(tyde_agent_adapter::BackendCapability::MidTurnSteering)
    {
        return Err("Tychat requires a backend that can steer mid-turn".into());
    }
    if !settings.enabled_backends.contains(&kind) {
        return Err("Tychat backend must be enabled".into());
    }
    if let Some(id) = config.custom_agent_id.as_ref()
        && state.custom_agent_store.lock().await.get(id).is_none()
    {
        return Err("Tychat agent's custom agent no longer exists".into());
    }
    let mut values = match config.launch_profile_id.as_ref() {
        Some(id) => {
            let profile = resolve_launch_profile_from_catalog(
                &launch_profile_catalog_for_settings(state, settings),
                id,
            )?;
            if profile.backend_kind != kind {
                return Err("Tychat profile and backend must match".into());
            }
            profile.session_settings
        }
        None => protocol::SessionSettingsValues::default(),
    };
    values.0.extend(config.session_settings.0.clone());
    if !values.0.is_empty() {
        match session_schema_resolution_for_backend(state, kind, config.launch_profile_id.as_ref())
        {
            SessionSchemaResolution::Ready(schema) => {
                crate::backend::validate_session_settings_values(&schema, &values)?
            }
            _ => return Err("Tychat session settings schema is not ready".into()),
        }
    }
    Ok(())
}

impl HostHandle {
    #[cfg(feature = "test-support")]
    pub async fn install_tychat_outbound_ack_gate(&self) -> InstalledSpawnOperationTestGate {
        let gate = new_spawn_operation_test_gate();
        *self
            .state
            .lock()
            .await
            .tychat
            .outbound_ack_gate
            .lock()
            .await = Some(gate.shared());
        gate
    }

    #[cfg(feature = "test-support")]
    pub(crate) async fn wait_tychat_outbound_ack_for_test(&self) {
        let gate = self
            .state
            .lock()
            .await
            .tychat
            .outbound_ack_gate
            .lock()
            .await
            .take();
        if let Some(gate) = gate {
            wait_for_spawn_operation_test_gate_inner(&gate).await;
        }
    }

    pub async fn tychat_settings(&self) -> Result<protocol::TychatSettings, String> {
        Ok(self
            .state
            .lock()
            .await
            .settings_store
            .lock()
            .await
            .get()?
            .tychat)
    }

    pub async fn tychat_state(&self) -> protocol::TychatStatePayload {
        let service = self.state.lock().await.tychat.clone();
        service.state.lock().await.snapshot.clone()
    }

    /// Subscribe before reading a snapshot; each change is a wake to read again.
    pub async fn subscribe_tychat(&self) -> watch::Receiver<u64> {
        self.state.lock().await.tychat.subscribe()
    }

    pub async fn tychat_bot_state(
        &self,
    ) -> Option<(protocol::TychatPairingId, String, SecretBotState)> {
        let service = self.state.lock().await.tychat.clone();
        service
            .state
            .lock()
            .await
            .journal
            .pairing
            .as_ref()
            .map(|pairing| {
                (
                    pairing.generation.clone(),
                    pairing.api_base_url.clone(),
                    pairing.secret.clone(),
                )
            })
    }

    /// Phase B passes only a successfully redeemed and verified BotState here.
    pub async fn install_tychat_pairing(
        &self,
        secret: SecretBotState,
        fingerprints: protocol::TychatFingerprints,
    ) -> Result<protocol::TychatPairingId, String> {
        if secret.0.is_empty() || fingerprints.bot.is_empty() || fingerprints.owner.is_empty() {
            return Err("Incomplete Tychat pairing".into());
        }
        let service = self.state.lock().await.tychat.clone();
        service.acquire_process_lock()?;
        let generation = protocol::TychatPairingId(uuid::Uuid::new_v4().to_string());
        {
            let _guard = service.lifecycle.lock().await;
            let mut state = service.state.lock().await;
            if state.journal.pairing.is_some() {
                return Err("Unpair before pairing another bot".into());
            }
            let mut journal = state.journal.clone();
            journal.pairing = Some(Pairing {
                generation: generation.clone(),
                secret,
                api_base_url: service.api_base.to_string(),
                fingerprints: fingerprints.clone(),
            });
            state.commit(journal)?;
            state.snapshot.fingerprints = Some(fingerprints);
            state.snapshot.status = TychatBridgeStatus::AwaitingOwnerConfirmation;
        }
        service.notify();
        if let Err(reason) = self.reconcile_tychat(false).await {
            service.fail(reason).await;
        }
        Ok(generation)
    }

    pub async fn persist_tychat_bot_state(
        &self,
        generation: &protocol::TychatPairingId,
        secret: SecretBotState,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        let mut state = service.state.lock().await;
        let mut journal = state.journal.clone();
        let pairing = journal
            .pairing
            .as_mut()
            .filter(|pairing| &pairing.generation == generation)
            .ok_or_else(|| "Stale Tychat pairing".to_owned())?;
        let bot: tychat_bot::BotState =
            serde_json::from_slice(&secret.0).map_err(|_| "Invalid Tychat bot state")?;
        let fingerprints = protocol::TychatFingerprints {
            bot: bot.bot_fingerprint().to_owned(),
            owner: bot.owner_fingerprint(),
        };
        pairing.secret = secret;
        pairing.fingerprints = fingerprints.clone();
        state.commit(journal)?;
        let changed = state.snapshot.fingerprints.as_ref() != Some(&fingerprints);
        state.snapshot.fingerprints = Some(fingerprints);
        drop(state);
        if changed {
            service.notify();
        }
        Ok(())
    }

    pub async fn set_tychat_bridge_status(
        &self,
        generation: &protocol::TychatPairingId,
        status: TychatBridgeStatus,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        let mut state = service.state.lock().await;
        if !state
            .journal
            .pairing
            .as_ref()
            .is_some_and(|pairing| &pairing.generation == generation)
        {
            return Err("Stale Tychat pairing".into());
        }
        if matches!(status, TychatBridgeStatus::Unpaired) {
            return Err("Use Unpair to remove a Tychat pairing".into());
        }
        let changed = state.snapshot.status != status;
        state.snapshot.status = status;
        drop(state);
        if changed {
            service.notify();
        }
        Ok(())
    }

    pub async fn tychat_outbound(
        &self,
        generation: &protocol::TychatPairingId,
    ) -> Result<protocol::TychatOutboundSnapshot, String> {
        let service = self.state.lock().await.tychat.clone();
        let state = service.state.lock().await;
        if !state
            .journal
            .pairing
            .as_ref()
            .is_some_and(|pairing| &pairing.generation == generation)
        {
            return Err("Stale Tychat pairing".into());
        }
        Ok(protocol::TychatOutboundSnapshot {
            typing: state.typing,
            pending: state.journal.outbox.clone(),
        })
    }

    pub async fn acknowledge_tychat_outbound(
        &self,
        generation: &protocol::TychatPairingId,
        message_id: protocol::TychatOutboundId,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        let mut state = service.state.lock().await;
        if !state
            .journal
            .pairing
            .as_ref()
            .is_some_and(|pairing| &pairing.generation == generation)
        {
            return Err("Stale Tychat pairing".into());
        }
        let mut journal = state.journal.clone();
        journal
            .outbox
            .retain(|message| message.message_id != message_id);
        state.commit(journal)?;
        drop(state);
        service.notify();
        Ok(())
    }

    pub async fn deliver_tychat_message(
        &self,
        generation: &protocol::TychatPairingId,
        message: protocol::TychatOwnerMessage,
    ) -> Result<protocol::TychatDeliveryReceipt, String> {
        let service = self.state.lock().await.tychat.clone();
        let _guard = service.lifecycle.lock().await;
        let settings = self.state.lock().await.settings_store.lock().await.get()?;
        if !settings.tychat.enabled {
            return Err("Tychat is disabled".into());
        }
        if message.text.trim().is_empty() || message.message_id.0.is_empty() {
            return Err("Tychat message is empty".into());
        }
        let agent_id = {
            let state = service.state.lock().await;
            if !state
                .journal
                .pairing
                .as_ref()
                .is_some_and(|pairing| &pairing.generation == generation)
            {
                return Err("Stale Tychat pairing".into());
            }
            if !matches!(state.snapshot.status, TychatBridgeStatus::Connected) {
                return Err("Tychat bridge is not connected".into());
            }
            if let Some(receipt) = state.journal.deliveries.get(&message.message_id) {
                return Ok(receipt.clone());
            }
            state
                .snapshot
                .agent_id
                .clone()
                .ok_or_else(|| "Tychat agent is unavailable".to_owned())?
        };
        let handle = self
            .state
            .lock()
            .await
            .registry
            .agent_handle(&agent_id)
            .ok_or("Tychat agent is unavailable")?;
        let receipt = handle.deliver_tychat(message).await?;
        let mut state = service.state.lock().await;
        let mut journal = state.journal.clone();
        journal
            .deliveries
            .insert(receipt.message_id.clone(), receipt.clone());
        state.commit(journal)?;
        state.snapshot.last_delivery = Some(receipt.clone());
        tracing::info!(path = ?receipt.path, "Tychat owner message delivered");
        drop(state);
        service.notify();
        Ok(receipt)
    }

    pub(crate) async fn tychat_command(&self, command: TychatCommandPayload) -> Result<(), String> {
        match command {
            TychatCommandPayload::Pair { code } => {
                // Pair consumes the code; exclude settings changes until its secret is durable.
                let _settings_guard = self.settings_apply_lock.lock().await;
                if self.tychat_bot_state().await.is_some() {
                    return Err("Unpair before pairing another bot".into());
                }
                let service = self.state.lock().await.tychat.clone();
                let base = service.api_base.clone();
                service.acquire_process_lock()?;
                let paired = match tychat_bot::pair(&base, code.trim()).await {
                    Ok(paired) => paired,
                    Err(error) => {
                        service.release_process_lock();
                        service.state.lock().await.snapshot.status =
                            crate::tychat_bridge::error_status(&error);
                        service.notify();
                        return Err("Tychat pairing failed; see the bridge status".into());
                    }
                };
                self.install_tychat_pairing(
                    crate::tychat_bridge::encode(&paired.state)?,
                    protocol::TychatFingerprints {
                        bot: paired.bot_fingerprint,
                        owner: paired.owner_fingerprint,
                    },
                )
                .await?;
                Ok(())
            }
            TychatCommandPayload::ResetAgent => self.reconcile_tychat(true).await,
            TychatCommandPayload::Unpair => {
                let service = self.state.lock().await.tychat.clone();
                service.bridge.disconnect().await?;
                self.forget_tychat_pairing(None).await
            }
        }
    }

    pub(crate) async fn forget_tychat_pairing(
        &self,
        generation: Option<&protocol::TychatPairingId>,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        let _guard = service.lifecycle.lock().await;
        let agent_id = {
            let state = service.state.lock().await;
            if state.journal.pairing.is_none() {
                return Ok(());
            }
            if let Some(generation) = generation
                && !state
                    .journal
                    .pairing
                    .as_ref()
                    .is_some_and(|pairing| &pairing.generation == generation)
            {
                return Err("Stale Tychat pairing".into());
            }
            state.snapshot.agent_id.clone()
        };
        if let Some(id) = agent_id {
            self.close_agent_with_host_visibility(&id, false).await;
        }
        let mut state = service.state.lock().await;
        state.commit(crate::tychat::Journal::default())?;
        state.snapshot = protocol::TychatStatePayload {
            backend_capabilities: crate::tychat::steering_capabilities(),
            ..Default::default()
        };
        state.typing = false;
        drop(state);
        service.release_process_lock();
        service.notify();
        Ok(())
    }

    pub(super) async fn start_tychat(&self) {
        let service = self.state.lock().await.tychat.clone();
        let mut changed = service.subscribe();
        let host = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = host.restart.stopped.cancelled() => break,
                    result = changed.changed() => {
                        if result.is_err() { break; }
                        let snapshot = service.state.lock().await.snapshot.clone();
                        let state = host.state.lock().await;
                        for subscriber in state.host_streams.values() {
                            if subscriber.bootstrapped {
                                let _ = subscriber.stream.send_value(FrameKind::TychatState, serde_json::to_value(&snapshot).expect("Tychat snapshot serializes"));
                            }
                        }
                    }
                }
            }
        });
        if let Err(reason) = self.reconcile_tychat(false).await {
            self.state.lock().await.tychat.fail(reason).await;
        }
        #[cfg(feature = "test-support")]
        if self.state.lock().await.tychat_bridge_disabled {
            return;
        }
        let service = self.state.lock().await.tychat.clone();
        service.bridge.start(self.clone(), service.clone()).await;
    }

    pub(super) async fn reconcile_tychat(&self, reset: bool) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        let _guard = service.lifecycle.lock().await;
        let settings = self.state.lock().await.settings_store.lock().await.get()?;
        let (paired, saved, live) = {
            let state = service.state.lock().await;
            (
                state.journal.pairing.is_some(),
                state.journal.session.clone(),
                state.snapshot.agent_id.clone(),
            )
        };
        if !paired || !settings.tychat.enabled {
            if reset {
                return Err("Enable and pair Tychat before resetting its agent".into());
            }
            if let Some(id) = live {
                self.close_agent_with_host_visibility(&id, false).await;
            }
            let mut state = service.state.lock().await;
            let changed = state.snapshot.agent_id.is_some()
                || state.snapshot.settings_application != TychatSettingsApplication::Inactive
                || state.typing;
            state.snapshot.agent_id = None;
            state.snapshot.settings_application = TychatSettingsApplication::Inactive;
            state.typing = false;
            drop(state);
            if changed {
                service.notify();
            }
            return Ok(());
        }
        if live.is_none() || reset {
            let runtime_settings = if reset {
                &settings.tychat
            } else {
                saved
                    .as_ref()
                    .map_or(&settings.tychat, |session| &session.settings)
            };
            if let Some(kind) = runtime_settings.backend_kind {
                tracing::info!(backend = ?kind, "Resolving Tychat session settings schema before lifecycle admission");
                self.resolve_session_schema_for_spawn(
                    kind,
                    runtime_settings.launch_profile_id.as_ref(),
                )
                .await
                .map_err(|error| error.message)?;
            }
        }
        let desired_profile = {
            let state = self.state.lock().await;
            if saved.is_none() || reset {
                validate_tychat_settings(&state, &settings).await?;
            }
            settings
                .tychat
                .launch_profile_id
                .as_ref()
                .map(|id| {
                    resolve_launch_profile_from_catalog(
                        &launch_profile_catalog_for_settings(&state, &settings),
                        id,
                    )
                })
                .transpose()
        };
        if let Some(id) = live {
            if reset {
                self.close_agent_with_host_visibility(&id, false).await;
            } else {
                let saved = saved.ok_or("Tychat live session has no durable binding")?;
                let mut application = if saved.settings.backend_kind != settings.tychat.backend_kind
                    || saved.settings.launch_profile_id != settings.tychat.launch_profile_id
                    || saved.settings.custom_agent_id != settings.tychat.custom_agent_id
                    || !desired_profile
                        .as_ref()
                        .is_ok_and(|profile| profile == &saved.launch_profile)
                {
                    TychatSettingsApplication::AppliesOnReset
                } else {
                    TychatSettingsApplication::Live
                };
                if application == TychatSettingsApplication::Live
                    && saved.settings.session_settings != settings.tychat.session_settings
                {
                    let handle = self
                        .state
                        .lock()
                        .await
                        .registry
                        .agent_handle(&id)
                        .ok_or("Tychat agent is unavailable")?;
                    let mut values = match settings.tychat.launch_profile_id.as_ref() {
                        Some(profile_id) => {
                            let state = self.state.lock().await;
                            resolve_launch_profile_from_catalog(
                                &launch_profile_catalog_for_settings(&state, &settings),
                                profile_id,
                            )?
                            .session_settings
                        }
                        None => protocol::SessionSettingsValues::default(),
                    };
                    values.0.extend(settings.tychat.session_settings.0.clone());
                    match handle.apply_tychat_settings(values).await {
                        Ok(true) => {
                            let mut state = service.state.lock().await;
                            let mut journal = state.journal.clone();
                            if let Some(session) = journal.session.as_mut() {
                                session.settings = settings.tychat.clone();
                            }
                            state.commit(journal)?;
                        }
                        Ok(false) => application = TychatSettingsApplication::AppliesOnReset,
                        Err(reason) => application = TychatSettingsApplication::Failed { reason },
                    }
                }
                service.state.lock().await.snapshot.settings_application = application;
                service.notify();
                return Ok(());
            }
        }
        if reset {
            let mut state = service.state.lock().await;
            let mut journal = state.journal.clone();
            journal.session = None;
            state.commit(journal)?;
            state.snapshot.agent_id = None;
        }
        let saved = if reset { None } else { saved };
        let backend = settings
            .tychat
            .backend_kind
            .ok_or("Choose a Tychat backend")?;
        let params = match &saved {
            Some(session) => SpawnAgentParams::Resume {
                session_id: session.session_id.clone(),
                prompt: None,
            },
            None => SpawnAgentParams::New {
                workspace_roots: vec![
                    crate::paths::home_dir()?
                        .join(".tyde")
                        .to_string_lossy()
                        .into_owned(),
                ],
                prompt: crate::tychat::STARTUP_MESSAGE.into(),
                images: None,
                backend_kind: backend,
                launch_profile_id: settings.tychat.launch_profile_id.clone(),
                cost_hint: None,
                access_mode: protocol::BackendAccessMode::Unrestricted,
                session_settings: Some(settings.tychat.session_settings.clone()),
            },
        };
        let id = self
            .spawn_agent_with_origin(
                SpawnAgentPayload {
                    name: Some("Tychat agent".into()),
                    parent_agent_id: None,
                    project_id: None,
                    custom_agent_id: match &params {
                        SpawnAgentParams::New { .. } => settings.tychat.custom_agent_id.clone(),
                        _ => None,
                    },
                    params,
                },
                AgentOrigin::Tychat,
            )
            .await
            .map_err(|error| error.to_string())?;
        let handle = self
            .state
            .lock()
            .await
            .registry
            .agent_handle(&id)
            .ok_or("Tychat agent startup disappeared")?;
        let session_id = match handle.wait_for_tychat_startup().await {
            Ok(session_id) => session_id,
            Err(error) => {
                self.close_agent_with_host_visibility(&id, false).await;
                return Err(error);
            }
        };
        let mut state = service.state.lock().await;
        let mut journal = state.journal.clone();
        let (effective, launch_profile) = match saved {
            Some(session) => (session.settings, session.launch_profile),
            None => (settings.tychat.clone(), desired_profile.clone()?),
        };
        journal.session = Some(Session {
            agent_id: id.clone(),
            session_id,
            settings: effective.clone(),
            launch_profile: launch_profile.clone(),
        });
        state.commit(journal)?;
        state.snapshot.agent_id = Some(id.clone());
        state.snapshot.settings_application = if effective == settings.tychat
            && desired_profile
                .as_ref()
                .is_ok_and(|profile| profile == &launch_profile)
        {
            TychatSettingsApplication::Live
        } else {
            TychatSettingsApplication::AppliesOnReset
        };
        drop(state);
        let activity = {
            let host = self.state.lock().await;
            host.registry
                .agent_status_handle(&id)
                .ok_or("Tychat status is missing")?
                .snapshot()
                .await
                .activity()
        };
        let projected_typing = service.state.lock().await.typing;
        tracing::info!(
            ?activity,
            projected_typing,
            "Tychat initial activity projection"
        );
        self.fan_out_agent_turn_state(&id).await;
        service.notify();
        Ok(())
    }
}
