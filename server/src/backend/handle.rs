use std::pin::Pin;

use protocol::AgentInput;

use super::{Backend, SendOutcome};

pub(crate) type BackendHandle = Box<dyn BackendSender>;

/// Type-erased backend handle for agent input and acknowledged settings edits.
pub(crate) trait BackendSender: Send + Sync + 'static {
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
    fn update_session_settings<'a>(
        &'a mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
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

    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::interrupt(self))
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
