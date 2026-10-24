use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use tokio::sync::{mpsc, oneshot};

#[cfg(feature = "test-support")]
pub(super) type ReplyTestGate = (
    std::sync::Arc<super::SpawnOperationTestGateInner>,
    oneshot::Sender<()>,
);

#[cfg(feature = "test-support")]
pub(super) struct StartTestGate {
    pub(super) gate: std::sync::Arc<super::SpawnOperationTestGateInner>,
    pub(super) cancelled: oneshot::Sender<()>,
}

type Transition<T> = Box<dyn for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, ()> + Send>;

pub(super) struct StateOwner<T> {
    transitions: mpsc::UnboundedSender<Transition<T>>,
}

pub(super) struct StateActor<T> {
    state: T,
    inbox: mpsc::UnboundedReceiver<Transition<T>>,
}

impl<T: Send + 'static> StateOwner<T> {
    pub(super) fn new(state: T) -> (Self, StateActor<T>) {
        let (transitions, inbox) = mpsc::unbounded_channel();
        (Self { transitions }, StateActor { state, inbox })
    }

    pub(super) async fn with<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> R {
        self.with_gated_reply(
            transition,
            #[cfg(feature = "test-support")]
            None,
        )
        .await
    }

    pub(super) async fn with_gated_reply<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<ReplyTestGate>,
    ) -> R {
        self.with_async_gated_reply(
            move |state| Box::pin(async move { transition(state) }),
            #[cfg(feature = "test-support")]
            gate,
        )
        .await
    }

    pub(super) async fn with_start_gated<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<StartTestGate>,
    ) -> R {
        self.dispatch(
            move |state| Box::pin(async move { transition(state) }),
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            gate,
        )
        .await
    }

    // Release obligations survive cancellation of the caller waiting for the reply.
    pub(super) async fn with_required<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> R {
        let (reply, result) = oneshot::channel();
        self.transitions
            .send(Box::new(move |state| {
                Box::pin(async move {
                    let outcome = catch_unwind(AssertUnwindSafe(|| transition(state)));
                    let _ = reply.send(outcome);
                })
            }))
            .unwrap_or_else(|_| panic!("host state owner stopped while a host handle is live"));
        match result.await {
            Ok(Ok(value)) => value,
            Ok(Err(panic)) => resume_unwind(panic),
            Err(_) => panic!("host state owner dropped a required transition"),
        }
    }

    // Only acyclic leaf-service RPCs may suspend a turn. Agent replies and
    // callbacks into this owner must be awaited outside it.
    pub(super) async fn with_async<R: Send + 'static>(
        &self,
        transition: impl for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, R> + Send + 'static,
    ) -> R {
        self.with_async_gated_reply(
            transition,
            #[cfg(feature = "test-support")]
            None,
        )
        .await
    }

    pub(super) async fn with_async_gated_reply<R: Send + 'static>(
        &self,
        transition: impl for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, R> + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<ReplyTestGate>,
    ) -> R {
        self.dispatch(
            transition,
            #[cfg(feature = "test-support")]
            gate,
            #[cfg(feature = "test-support")]
            None,
        )
        .await
    }

    async fn dispatch<R: Send + 'static>(
        &self,
        transition: impl for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, R> + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<ReplyTestGate>,
        #[cfg(feature = "test-support")] start_gate: Option<StartTestGate>,
    ) -> R {
        let (mut reply, result) = oneshot::channel();
        self.transitions
            .send(Box::new(move |state| {
                Box::pin(async move {
                    if reply.is_closed() {
                        return;
                    }
                    #[cfg(feature = "test-support")]
                    if let Some(StartTestGate { gate, cancelled }) = start_gate {
                        let start = super::wait_for_spawn_operation_test_gate_inner(&gate);
                        tokio::pin!(start);
                        tokio::select! {
                            biased;
                            () = reply.closed() => {
                                let _ = cancelled.send(());
                                start.await;
                            }
                            () = &mut start => {}
                        }
                    }
                    let outcome = tokio::select! {
                        biased;
                        () = reply.closed() => return,
                        outcome = AssertUnwindSafe(transition(state)).catch_unwind() => outcome,
                    };
                    #[cfg(feature = "test-support")]
                    if let Some((gate, settled)) = gate {
                        tokio::spawn(async move {
                            let deliver = tokio::select! {
                                biased;
                                () = reply.closed() => false,
                                () = super::wait_for_spawn_operation_test_gate_inner(&gate) => true,
                            };
                            if deliver {
                                let _ = reply.send(outcome);
                            } else {
                                drop(outcome);
                            }
                            let _ = settled.send(());
                        });
                        return;
                    }
                    let _ = reply.send(outcome);
                })
            }))
            .unwrap_or_else(|_| panic!("host state owner stopped while a host handle is live"));
        match result.await {
            Ok(Ok(value)) => value,
            Ok(Err(panic)) => resume_unwind(panic),
            Err(_) => panic!("host state owner dropped a transition"),
        }
    }

    pub(super) fn apply(
        &self,
        transition: impl FnOnce(&mut T) + Send + 'static,
    ) -> Result<(), String> {
        self.transitions
            .send(Box::new(move |state| {
                Box::pin(async move {
                    transition(state);
                })
            }))
            .map_err(|_| "host state owner stopped before a release transition".to_owned())
    }
}

impl<T: Send + 'static> StateActor<T> {
    pub(super) fn start(mut self) -> Result<(), String> {
        let worker = async move {
            while let Some(transition) = self.inbox.recv().await {
                transition(&mut self.state).await;
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(worker);
        } else {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("cannot create host state runtime: {error}"))?;
            std::thread::Builder::new()
                .name("tyde-host-state".into())
                .spawn(move || runtime.block_on(worker))
                .map_err(|error| format!("cannot start host state owner: {error}"))?;
        }
        Ok(())
    }
}
