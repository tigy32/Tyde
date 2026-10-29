use std::pin::Pin;

use protocol::AgentInput;

use super::{Backend, SendOutcome, WorkspaceRelocationError};

pub(crate) type BackendHandle = Box<dyn BackendSender>;

/// Type-erased backend handle for agent input and acknowledged settings edits.
pub(crate) trait BackendSender: Send + Sync + 'static {
    fn set_workspace_roots(
        &mut self,
        roots: Vec<String>,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<(), WorkspaceRelocationError>> + Send + 'static,
        >,
    >;
    fn workspace_relocation_wake(&self) -> Option<tokio::sync::watch::Receiver<()>>;
    fn compaction_capability(&self) -> crate::backend::BackendCompactionCapability;
    fn begin_compaction<'a>(
        &'a self,
        request: crate::backend::BackendCompactionRequest,
    ) -> Pin<
        Box<dyn std::future::Future<Output = crate::backend::BackendCompactionStart> + Send + 'a>,
    >;
    fn send_with_outcome<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>>;
    fn submit_with_accounting<'a>(
        &'a self,
        input: AgentInput,
        accounting: &'a mut protocol::TaskUsageAccounting,
    ) -> Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>> {
        Box::pin(async move {
            let starts_request = matches!(&input, AgentInput::SendMessage(message) if message.tool_response.is_none());
            let outcome = self.send_with_outcome(input).await;
            if starts_request && matches!(outcome, SendOutcome::Accepted) {
                accounting.generation = accounting.generation.saturating_add(1);
                accounting.pending_submission = true;
            }
            outcome
        })
    }
    fn update_session_settings<'a>(
        &'a mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    fn steer<'a>(
        &'a self,
        payload: protocol::SendMessagePayload,
    ) -> Pin<Box<dyn std::future::Future<Output = crate::backend::SteerOutcome> + Send + 'a>>;
    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn cancel_background_task<'a>(
        &'a self,
        tool_call_id: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = crate::backend::CancelBackgroundTaskOutcome>
                + Send
                + 'a,
        >,
    >;
    fn shutdown(self: Box<Self>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
    #[cfg(feature = "test-support")]
    fn mock_control(&self) -> Option<crate::backend::mock::MockControl>;
}

impl<B: Backend> BackendSender for B {
    fn set_workspace_roots(
        &mut self,
        roots: Vec<String>,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<(), WorkspaceRelocationError>> + Send + 'static,
        >,
    > {
        let operation = Backend::set_workspace_roots(self, roots);
        Box::pin(async move {
            tracing::info!(target: "tyde_project_entry_diagnostic", "Backend relocation called");
            let result = operation.await;
            tracing::info!(target: "tyde_project_entry_diagnostic", accepted = result.is_ok(),
                busy = matches!(result, Err(WorkspaceRelocationError::Busy)), "Backend relocation returned");
            result
        })
    }

    fn workspace_relocation_wake(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Backend::workspace_relocation_wake(self)
    }

    fn compaction_capability(&self) -> crate::backend::BackendCompactionCapability {
        Backend::compaction_capability(self)
    }

    fn begin_compaction<'a>(
        &'a self,
        request: crate::backend::BackendCompactionRequest,
    ) -> Pin<
        Box<dyn std::future::Future<Output = crate::backend::BackendCompactionStart> + Send + 'a>,
    > {
        Box::pin(Backend::begin_compaction(self, request))
    }

    fn send_with_outcome<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>> {
        Box::pin(Backend::send_with_outcome(self, input))
    }

    fn update_session_settings<'a>(
        &'a mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(Backend::update_session_settings(self, payload))
    }

    fn steer<'a>(
        &'a self,
        payload: protocol::SendMessagePayload,
    ) -> Pin<Box<dyn std::future::Future<Output = crate::backend::SteerOutcome> + Send + 'a>> {
        Box::pin(async move {
            tracing::info!(target: "tyde_project_entry_diagnostic", "Backend steer called");
            let result = Backend::steer(self, payload).await;
            tracing::info!(target: "tyde_project_entry_diagnostic",
                accepted = matches!(result, crate::backend::SteerOutcome::Accepted),
                no_active_turn = matches!(result, crate::backend::SteerOutcome::NoActiveTurn(_)),
                unsupported = matches!(result, crate::backend::SteerOutcome::Unsupported(_)), "Backend steer returned");
            result
        })
    }

    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            tracing::info!(target: "tyde_project_entry_diagnostic", "Backend interrupt called");
            let result = Backend::interrupt(self).await;
            tracing::info!(target: "tyde_project_entry_diagnostic", accepted = result, "Backend interrupt returned");
            result
        })
    }

    fn cancel_background_task<'a>(
        &'a self,
        tool_call_id: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = crate::backend::CancelBackgroundTaskOutcome>
                + Send
                + 'a,
        >,
    > {
        Box::pin(Backend::cancel_background_task(self, tool_call_id))
    }

    fn shutdown(self: Box<Self>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            Backend::shutdown(*self).await;
        })
    }

    #[cfg(feature = "test-support")]
    fn mock_control(&self) -> Option<crate::backend::mock::MockControl> {
        Backend::mock_control(self)
    }
}
