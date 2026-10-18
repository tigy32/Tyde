use super::*;
use crate::tychat::{DeliveryTarget, Lifecycle, Pairing, SecretBotState, Session, Update};
use protocol::{TychatBridgeStatus, TychatCommandPayload, TychatSettingsApplication};

#[derive(Debug)]
enum TychatCloseReason {
    Unpaired,
    Disabled,
    Reset,
    StartupFailed,
}

pub(super) fn validate_tychat_settings(
    state: &HostState,
    settings: &settings_model::HostSettings,
    custom_agent_exists: bool,
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
    if !custom_agent_exists {
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
        self.install_tychat_test_gate(crate::tychat::TychatTestBoundary::OutboundAcknowledgement)
            .await
    }

    #[cfg(feature = "test-support")]
    pub async fn install_tychat_test_gate(
        &self,
        boundary: crate::tychat::TychatTestBoundary,
    ) -> InstalledSpawnOperationTestGate {
        let gate = new_spawn_operation_test_gate();
        self.state
            .lock()
            .await
            .tychat
            .install_test_gate(boundary, gate.shared());
        gate
    }

    #[cfg(feature = "test-support")]
    pub(crate) async fn wait_tychat_outbound_ack_for_test(&self) {
        self.wait_tychat_test_boundary(crate::tychat::TychatTestBoundary::OutboundAcknowledgement)
            .await;
    }

    #[cfg(feature = "test-support")]
    async fn wait_tychat_test_boundary(&self, boundary: crate::tychat::TychatTestBoundary) {
        let service = self.state.lock().await.tychat.clone();
        if let Some(gate) = service.take_test_gate(boundary).await {
            wait_for_spawn_operation_test_gate_inner(&gate).await;
        }
    }

    #[cfg(feature = "test-support")]
    pub async fn cancel_tychat_bridge_task_for_test(&self) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        service.bridge.cancel_task_for_test().await
    }

    pub async fn tychat_settings(&self) -> Result<protocol::TychatSettings, String> {
        let store = self.state.lock().await.settings_store.clone();
        Ok(store.get()?.tychat)
    }

    pub async fn tychat_state(&self) -> protocol::TychatStatePayload {
        self.state.lock().await.tychat.snapshot()
    }

    pub async fn subscribe_tychat(&self) -> watch::Receiver<u64> {
        self.state.lock().await.tychat.subscribe()
    }

    pub async fn tychat_bot_state(
        &self,
    ) -> Result<Option<(protocol::TychatPairingId, String, SecretBotState)>, String> {
        let service = self.state.lock().await.tychat.clone();
        Ok(service
            .pairing()
            .await?
            .map(|pairing| (pairing.generation, pairing.api_base_url, pairing.secret)))
    }

    /// Phase B passes only a successfully redeemed and verified BotState here.
    pub async fn install_tychat_pairing(
        &self,
        secret: SecretBotState,
        fingerprints: protocol::TychatFingerprints,
    ) -> Result<protocol::TychatPairingId, String> {
        let service = self.state.lock().await.tychat.clone();
        service
            .lifecycle(self.clone(), |reply| Lifecycle::Install {
                secret,
                fingerprints,
                reply,
            })
            .await?
    }

    pub(crate) async fn install_tychat_pairing_owned(
        &self,
        secret: SecretBotState,
        fingerprints: protocol::TychatFingerprints,
    ) -> Result<protocol::TychatPairingId, String> {
        if secret.0.is_empty() || fingerprints.bot.is_empty() || fingerprints.owner.is_empty() {
            return Err("Incomplete Tychat pairing".into());
        }
        let service = self.state.lock().await.tychat.clone();
        #[cfg(feature = "test-support")]
        self.wait_tychat_test_boundary(crate::tychat::TychatTestBoundary::PairingInstall)
            .await;
        let generation = protocol::TychatPairingId(uuid::Uuid::new_v4().to_string());
        service
            .update(Update::InstallPairing(Pairing {
                generation: generation.clone(),
                secret,
                api_base_url: service.api_base.to_string(),
                fingerprints,
            }))
            .await?;
        if let Err(reason) = self.reconcile_tychat_owned(false).await {
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
        service
            .update(Update::PersistBot {
                generation: generation.clone(),
                secret,
            })
            .await
    }

    pub async fn set_tychat_bridge_status(
        &self,
        generation: &protocol::TychatPairingId,
        status: TychatBridgeStatus,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        service
            .update(Update::BridgeStatus {
                generation: generation.clone(),
                status,
            })
            .await
    }

    pub async fn tychat_outbound(
        &self,
        generation: &protocol::TychatPairingId,
    ) -> Result<protocol::TychatOutboundSnapshot, String> {
        let service = self.state.lock().await.tychat.clone();
        service.outbound(generation).await
    }

    pub async fn acknowledge_tychat_outbound(
        &self,
        generation: &protocol::TychatPairingId,
        message_id: protocol::TychatOutboundId,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        service
            .update(Update::AcknowledgeOutbound {
                generation: generation.clone(),
                id: message_id,
            })
            .await
    }

    pub async fn deliver_tychat_message(
        &self,
        generation: &protocol::TychatPairingId,
        message: protocol::TychatOwnerMessage,
    ) -> Result<protocol::TychatDeliveryReceipt, String> {
        let service = self.state.lock().await.tychat.clone();
        service
            .lifecycle(self.clone(), |reply| Lifecycle::Deliver {
                generation: generation.clone(),
                message,
                reply,
            })
            .await?
    }

    pub(crate) async fn deliver_tychat_message_owned(
        &self,
        generation: &protocol::TychatPairingId,
        message: protocol::TychatOwnerMessage,
    ) -> Result<protocol::TychatDeliveryReceipt, String> {
        let service = self.state.lock().await.tychat.clone();
        if !self.tychat_settings().await?.enabled {
            return Err("Tychat is disabled".into());
        }
        if message.text.trim().is_empty() || message.message_id.0.is_empty() {
            return Err("Tychat message is empty".into());
        }
        let agent_id = match service.delivery_target(generation, &message).await? {
            DeliveryTarget::Agent(id) => id,
            DeliveryTarget::Receipt(receipt) => return Ok(receipt),
        };
        let handle = self
            .agent_handle(&agent_id)
            .await
            .ok_or("Tychat agent is unavailable")?;
        let learning_message = message.clone();
        let receipt = handle.deliver_tychat(message).await?;
        if let Err(reason) = self
            .accept_learning_input(
                &agent_id,
                protocol::HumanInputId(format!("tychat:{}", learning_message.message_id.0)),
                protocol::HumanInputSource::Tychat,
                learning_message.text,
            )
            .await
        {
            tracing::error!(reason = %reason, "Cannot persist accepted Tychat input for analysis");
        }
        service.update(Update::Delivery(receipt.clone())).await?;
        tracing::info!(path = ?receipt.path, "Tychat owner message delivered");
        Ok(receipt)
    }

    pub(crate) fn tychat_lifecycle_stop(&self) -> CancellationToken {
        self.restart.stopped.clone()
    }

    pub(super) async fn pair_tychat_now(&self, code: String) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        service
            .lifecycle(self.clone(), |reply| Lifecycle::Command {
                command: TychatCommandPayload::Pair { code },
                disconnected: None,
                reply,
            })
            .await?
    }

    pub(crate) async fn tychat_command(&self, command: TychatCommandPayload) -> Result<(), String> {
        if let TychatCommandPayload::Pair { code } = command {
            return self
                .queue_settings_apply(SettingsMutation::TychatPair(code))
                .await
                .map_err(|error| error.message);
        }
        // The bridge may await this queue; hold transport outside it through erasure,
        // including a generation installed by an earlier Pair still in the queue.
        let service = self.state.lock().await.tychat.clone();
        let disconnected = if matches!(command, TychatCommandPayload::Unpair) {
            let disconnected = service.bridge.disconnect().await?;
            #[cfg(feature = "test-support")]
            self.wait_tychat_test_boundary(crate::tychat::TychatTestBoundary::UnpairDisconnected)
                .await;
            Some(disconnected)
        } else {
            None
        };
        service
            .lifecycle(self.clone(), |reply| Lifecycle::Command {
                command,
                disconnected,
                reply,
            })
            .await?
    }

    pub(crate) async fn tychat_command_owned(
        &self,
        command: TychatCommandPayload,
    ) -> Result<(), String> {
        match command {
            TychatCommandPayload::Pair { code } => self.pair_tychat_owned(code).await,
            TychatCommandPayload::ResetAgent => self.reconcile_tychat_owned(true).await,
            TychatCommandPayload::Unpair => self.forget_tychat_pairing_owned(None).await,
        }
    }

    pub(crate) async fn pair_tychat_owned(&self, code: String) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        if service.pairing().await?.is_some() {
            return Err("Unpair before pairing another bot".into());
        }
        service.acquire_process_lock().await?;
        let paired = match tychat_bot::pair(&service.api_base, code.trim()).await {
            Ok(paired) => paired,
            Err(error) => {
                service
                    .update(Update::PairFailed(crate::tychat_bridge::error_status(
                        &error,
                    )))
                    .await?;
                return Err("Tychat pairing failed; see the bridge status".into());
            }
        };
        self.install_tychat_pairing_owned(
            crate::tychat_bridge::encode(&paired.state)?,
            protocol::TychatFingerprints {
                bot: paired.bot_fingerprint,
                owner: paired.owner_fingerprint,
            },
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn forget_tychat_pairing(
        &self,
        generation: Option<&protocol::TychatPairingId>,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        service
            .lifecycle(self.clone(), |reply| Lifecycle::Forget {
                generation: generation.cloned(),
                reply,
            })
            .await?
    }

    pub(crate) async fn forget_tychat_pairing_owned(
        &self,
        generation: Option<&protocol::TychatPairingId>,
    ) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        let Some(pairing) = service.pairing().await? else {
            return Ok(());
        };
        if generation.is_some_and(|generation| generation != &pairing.generation) {
            return Err("Stale Tychat pairing".into());
        }
        if let Some(id) = service.snapshot().agent_id {
            self.close_tychat_agent(&id, TychatCloseReason::Unpaired)
                .await?;
        }
        service.update(Update::Forget).await
    }

    pub(super) async fn start_tychat(&self) {
        let service = self.state.lock().await.tychat.clone();
        let mut changed = service.subscribe();
        let host = self.clone();
        let publisher = service.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = host.restart.stopped.cancelled() => break,
                    result = changed.changed() => {
                        if result.is_err() { break; }
                        let snapshot = publisher.snapshot();
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
            service.fail(reason).await;
        }
        #[cfg(feature = "test-support")]
        {
            let disabled = self.state.lock().await.tychat_bridge_disabled;
            if disabled {
                service.bridge.shutdown().await;
                return;
            }
        }
        let service = self.state.lock().await.tychat.clone();
        service.bridge.start(self.clone(), service.clone()).await;
    }

    pub(super) async fn reconcile_tychat(&self, reset: bool) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        service
            .lifecycle(self.clone(), |reply| Lifecycle::Reconcile { reset, reply })
            .await?
    }

    pub(crate) async fn reconcile_tychat_owned(&self, reset: bool) -> Result<(), String> {
        let service = self.state.lock().await.tychat.clone();
        let settings_store = self.state.lock().await.settings_store.clone();
        let settings = settings_store.get()?;
        let paired = service.pairing().await?.is_some();
        let (saved, live) = service.reconcile_state().await?;
        tracing::info!(
            paired,
            enabled = settings.tychat.enabled,
            saved_session = saved.is_some(),
            live_agent = live.is_some(),
            reset,
            "Tychat lifecycle reconciliation started"
        );
        if !paired || !settings.tychat.enabled {
            if reset {
                return Err("Enable and pair Tychat before resetting its agent".into());
            }
            if let Some(id) = live {
                self.close_tychat_agent(&id, TychatCloseReason::Disabled)
                    .await?;
            }
            service.update(Update::ClearAgent { reset: false }).await?;
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
        if saved.is_none() || reset {
            let custom_store = self.state.lock().await.custom_agent_store.clone();
            let custom_agents = custom_store.lock().await.clone();
            let custom_exists = match settings.tychat.custom_agent_id.as_ref() {
                Some(id) => custom_agents.get(id).is_some(),
                None => true,
            };
            let state = self.state.lock().await;
            validate_tychat_settings(&state, &settings, custom_exists)?;
        }
        let desired_profile = {
            let state = self.state.lock().await;
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
                self.close_tychat_agent(&id, TychatCloseReason::Reset)
                    .await?;
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
                            service
                                .update(Update::Application {
                                    settings: Some(settings.tychat.clone()),
                                    application: TychatSettingsApplication::Live,
                                })
                                .await?;
                        }
                        Ok(false) => application = TychatSettingsApplication::AppliesOnReset,
                        Err(reason) => application = TychatSettingsApplication::Failed { reason },
                    }
                }
                service
                    .update(Update::Application {
                        settings: None,
                        application,
                    })
                    .await?;
                return Ok(());
            }
        }
        if reset {
            service.update(Update::ClearAgent { reset: true }).await?;
        }
        let saved = if reset { None } else { saved };
        let restoring = saved.is_some();
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
                    launch_context: protocol::AgentLaunchContext::Standard,
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
            .map_err(|error| {
                tracing::warn!(code = ?error.code(), restoring, "Tychat lifecycle spawn failed");
                error.to_string()
            })?;
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
                tracing::warn!(failure = ?error, restoring, "Tychat lifecycle startup failed");
                self.close_tychat_agent(&id, TychatCloseReason::StartupFailed)
                    .await?;
                return Err(error.message().into());
            }
        };
        let (effective, launch_profile) = match saved {
            Some(session) => (session.settings, session.launch_profile),
            None => (settings.tychat.clone(), desired_profile.clone()?),
        };
        let application = if effective == settings.tychat
            && desired_profile
                .as_ref()
                .is_ok_and(|profile| profile == &launch_profile)
        {
            TychatSettingsApplication::Live
        } else {
            TychatSettingsApplication::AppliesOnReset
        };
        service
            .update(Update::Admit {
                session: Session {
                    agent_id: id.clone(),
                    session_id,
                    settings: effective,
                    launch_profile,
                },
                application,
            })
            .await?;
        tracing::info!(restoring, "Tychat lifecycle session admitted");
        let status = self
            .state
            .lock()
            .await
            .registry
            .agent_status_handle(&id)
            .ok_or("Tychat status is missing")?;
        let activity = status.snapshot().await.activity();
        tracing::info!(?activity, "Tychat initial activity projection");
        self.fan_out_agent_turn_state(&id).await;
        Ok(())
    }

    async fn close_tychat_agent(
        &self,
        id: &AgentId,
        reason: TychatCloseReason,
    ) -> Result<(), String> {
        tracing::info!(?reason, "Tychat lifecycle close requested");
        let closed = self
            .close_agent_with_host_visibility(id, false)
            .await
            .map_err(|error| {
                tracing::warn!(?reason, "Tychat lifecycle close failed");
                error.to_string()
            })?;
        tracing::info!(?reason, closed, "Tychat lifecycle close settled");
        Ok(())
    }
}
