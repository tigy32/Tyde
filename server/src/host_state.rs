use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use std::future::Future;
use std::panic::{AssertUnwindSafe, Location, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

/// Opt-in, content-free HostState operation diagnostics. Events carry only
/// operation/leaf correlation numbers, source call sites, typed request kinds
/// and durations.
pub(crate) const HOST_STATE_TRACE_TARGET: &str = "tyde_host_state";

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

type Transition<T> = Box<dyn for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, OperationOutcome> + Send>;

struct QueuedOperation<T> {
    operation: Operation,
    transition: Transition<T>,
}

#[derive(Clone, Copy)]
struct Operation {
    id: u64,
    site: &'static Location<'static>,
    enqueued_at: Instant,
}

#[derive(Clone, Copy)]
enum OperationOutcome {
    Completed,
    Panicked,
    CancelledBeforeStart,
    CancelledWhileRunning,
}

impl OperationOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Panicked => "panicked",
            Self::CancelledBeforeStart => "cancelled_before_start",
            Self::CancelledWhileRunning => "cancelled_while_running",
        }
    }
}

/// A typed leaf service a HostState operation may await.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum HostStateLeaf {
    SwarmRegistry = 1,
    SessionStore = 2,
}

impl HostStateLeaf {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::SwarmRegistry),
            2 => Some(Self::SessionStore),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SwarmRegistry => "swarm_registry",
            Self::SessionStore => "session_store",
        }
    }
}

#[derive(Default)]
struct OperationDiagnostics {
    next_operation: AtomicU64,
    next_leaf_request: AtomicU64,
    queued: AtomicUsize,
    executing: AtomicU64,
    awaiting_leaf: AtomicU8,
}

/// Lock-free view of the HostState owner for correlating an unrelated
/// request with whatever the owner is doing at that moment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HostStateActivity {
    pub(crate) executing_operation: Option<u64>,
    pub(crate) queued_operations: usize,
    pub(crate) awaiting_leaf: Option<HostStateLeaf>,
}

#[derive(Clone)]
struct ExecutingOperation {
    id: u64,
    diagnostics: Arc<OperationDiagnostics>,
}

tokio::task_local! {
    static EXECUTING_OPERATION: ExecutingOperation;
}

/// Awaits a leaf-service request. Inside a HostState operation it records the
/// awaited leaf, its typed request kind and the reply/cancellation, all
/// correlated with the executing operation. A poll wrapper rather than an
/// `async fn`, so the wrapped request is stored once instead of copied into
/// every enclosing future.
pub(crate) fn awaited_leaf<F: Future>(
    leaf: HostStateLeaf,
    request: &'static str,
    future: F,
) -> AwaitedLeaf<F> {
    let wait = EXECUTING_OPERATION
        .try_with(Clone::clone)
        .ok()
        .map(|executing| {
            let leaf_request = executing
                .diagnostics
                .next_leaf_request
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            let previous = executing
                .diagnostics
                .awaiting_leaf
                .swap(leaf as u8, Ordering::AcqRel);
            tracing::debug!(
                target: HOST_STATE_TRACE_TARGET,
                event = "leaf_await",
                operation = executing.id,
                leaf = leaf.as_str(),
                request,
                leaf_request,
            );
            LeafWait {
                executing,
                leaf,
                request,
                leaf_request,
                previous,
                started: Instant::now(),
                replied: false,
            }
        });
    AwaitedLeaf { future, wait }
}

pin_project_lite::pin_project! {
    pub(crate) struct AwaitedLeaf<F> {
        #[pin]
        future: F,
        wait: Option<LeafWait>,
    }
}

impl<F: Future> Future for AwaitedLeaf<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        let this = self.project();
        let output = std::task::ready!(this.future.poll(cx));
        if let Some(mut wait) = this.wait.take() {
            wait.replied = true;
        }
        std::task::Poll::Ready(output)
    }
}

struct LeafWait {
    executing: ExecutingOperation,
    leaf: HostStateLeaf,
    request: &'static str,
    leaf_request: u64,
    previous: u8,
    started: Instant,
    replied: bool,
}

impl Drop for LeafWait {
    fn drop(&mut self) {
        self.executing
            .diagnostics
            .awaiting_leaf
            .store(self.previous, Ordering::Release);
        let event = if self.replied {
            "leaf_replied"
        } else {
            "leaf_cancelled"
        };
        tracing::debug!(
            target: HOST_STATE_TRACE_TARGET,
            event,
            operation = self.executing.id,
            leaf = self.leaf.as_str(),
            request = self.request,
            leaf_request = self.leaf_request,
            wait_us = self.started.elapsed().as_micros() as u64,
        );
    }
}

pub(super) struct StateOwner<T> {
    transitions: mpsc::UnboundedSender<QueuedOperation<T>>,
    diagnostics: Arc<OperationDiagnostics>,
}

pub(super) struct StateActor<T> {
    state: T,
    inbox: mpsc::UnboundedReceiver<QueuedOperation<T>>,
    diagnostics: Arc<OperationDiagnostics>,
}

impl<T: Send + 'static> StateOwner<T> {
    pub(super) fn new(state: T) -> (Self, StateActor<T>) {
        let (transitions, inbox) = mpsc::unbounded_channel();
        let diagnostics = Arc::new(OperationDiagnostics::default());
        (
            Self {
                transitions,
                diagnostics: Arc::clone(&diagnostics),
            },
            StateActor {
                state,
                inbox,
                diagnostics,
            },
        )
    }

    pub(super) fn activity(&self) -> HostStateActivity {
        let executing = self.diagnostics.executing.load(Ordering::Acquire);
        HostStateActivity {
            executing_operation: (executing != 0).then_some(executing),
            queued_operations: self.diagnostics.queued.load(Ordering::Acquire),
            awaiting_leaf: HostStateLeaf::from_code(
                self.diagnostics.awaiting_leaf.load(Ordering::Acquire),
            ),
        }
    }

    fn enqueue(
        &self,
        site: &'static Location<'static>,
        transition: Transition<T>,
    ) -> Result<(), mpsc::error::SendError<()>> {
        let operation = Operation {
            id: self
                .diagnostics
                .next_operation
                .fetch_add(1, Ordering::Relaxed)
                + 1,
            site,
            enqueued_at: Instant::now(),
        };
        let queued = self.diagnostics.queued.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::debug!(
            target: HOST_STATE_TRACE_TARGET,
            event = "enqueue",
            operation = operation.id,
            site_file = site.file(),
            site_line = site.line(),
            queued,
        );
        self.transitions
            .send(QueuedOperation {
                operation,
                transition,
            })
            .map_err(|_| {
                self.diagnostics.queued.fetch_sub(1, Ordering::AcqRel);
                mpsc::error::SendError(())
            })
    }

    #[track_caller]
    pub(super) fn with<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> impl Future<Output = R> {
        self.dispatch(
            Location::caller(),
            move |state| Box::pin(async move { transition(state) }),
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    #[track_caller]
    pub(super) fn with_gated_reply<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<ReplyTestGate>,
    ) -> impl Future<Output = R> {
        self.dispatch(
            Location::caller(),
            move |state| Box::pin(async move { transition(state) }),
            #[cfg(feature = "test-support")]
            gate,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    #[track_caller]
    pub(super) fn with_start_gated<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<StartTestGate>,
    ) -> impl Future<Output = R> {
        self.dispatch(
            Location::caller(),
            move |state| Box::pin(async move { transition(state) }),
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            gate,
        )
    }

    // Release obligations survive cancellation of the caller waiting for the reply.
    #[track_caller]
    pub(super) fn with_required<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> impl Future<Output = R> {
        let site = Location::caller();
        async move {
            let (reply, result) = oneshot::channel();
            self.enqueue(
                site,
                Box::new(move |state| {
                    Box::pin(async move {
                        let outcome = catch_unwind(AssertUnwindSafe(|| transition(state)));
                        let traced = if outcome.is_ok() {
                            OperationOutcome::Completed
                        } else {
                            OperationOutcome::Panicked
                        };
                        let _ = reply.send(outcome);
                        traced
                    })
                }),
            )
            .unwrap_or_else(|_| panic!("host state owner stopped while a host handle is live"));
            match result.await {
                Ok(Ok(value)) => value,
                Ok(Err(panic)) => resume_unwind(panic),
                Err(_) => panic!("host state owner dropped a required transition"),
            }
        }
    }

    // Only acyclic leaf-service RPCs may suspend a turn. Agent replies and
    // callbacks into this owner must be awaited outside it.
    #[track_caller]
    pub(super) fn with_async<R: Send + 'static>(
        &self,
        transition: impl for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, R> + Send + 'static,
    ) -> impl Future<Output = R> {
        self.dispatch(
            Location::caller(),
            transition,
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    #[track_caller]
    pub(super) fn with_async_gated_reply<R: Send + 'static>(
        &self,
        transition: impl for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, R> + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<ReplyTestGate>,
    ) -> impl Future<Output = R> {
        self.dispatch(
            Location::caller(),
            transition,
            #[cfg(feature = "test-support")]
            gate,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    async fn dispatch<R: Send + 'static>(
        &self,
        site: &'static Location<'static>,
        transition: impl for<'a> FnOnce(&'a mut T) -> BoxFuture<'a, R> + Send + 'static,
        #[cfg(feature = "test-support")] gate: Option<ReplyTestGate>,
        #[cfg(feature = "test-support")] start_gate: Option<StartTestGate>,
    ) -> R {
        let (mut reply, result) = oneshot::channel();
        self.enqueue(
            site,
            Box::new(move |state| {
                Box::pin(async move {
                    if reply.is_closed() {
                        return OperationOutcome::CancelledBeforeStart;
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
                        () = reply.closed() => return OperationOutcome::CancelledWhileRunning,
                        outcome = AssertUnwindSafe(transition(state)).catch_unwind() => outcome,
                    };
                    let traced = if outcome.is_ok() {
                        OperationOutcome::Completed
                    } else {
                        OperationOutcome::Panicked
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
                        return traced;
                    }
                    let _ = reply.send(outcome);
                    traced
                })
            }),
        )
        .unwrap_or_else(|_| panic!("host state owner stopped while a host handle is live"));
        match result.await {
            Ok(Ok(value)) => value,
            Ok(Err(panic)) => resume_unwind(panic),
            Err(_) => panic!("host state owner dropped a transition"),
        }
    }

    #[track_caller]
    pub(super) fn apply(
        &self,
        transition: impl FnOnce(&mut T) + Send + 'static,
    ) -> Result<(), String> {
        self.enqueue(
            Location::caller(),
            Box::new(move |state| {
                Box::pin(async move {
                    transition(state);
                    OperationOutcome::Completed
                })
            }),
        )
        .map_err(|_| "host state owner stopped before a release transition".to_owned())
    }
}

impl<T: Send + 'static> StateActor<T> {
    pub(super) fn start(mut self) -> Result<(), String> {
        let worker = async move {
            while let Some(QueuedOperation {
                operation,
                transition,
            }) = self.inbox.recv().await
            {
                self.diagnostics.queued.fetch_sub(1, Ordering::AcqRel);
                self.diagnostics
                    .executing
                    .store(operation.id, Ordering::Release);
                let started = Instant::now();
                tracing::debug!(
                    target: HOST_STATE_TRACE_TARGET,
                    event = "start",
                    operation = operation.id,
                    site_file = operation.site.file(),
                    site_line = operation.site.line(),
                    queue_wait_us = started.duration_since(operation.enqueued_at).as_micros() as u64,
                );
                let outcome = EXECUTING_OPERATION
                    .scope(
                        ExecutingOperation {
                            id: operation.id,
                            diagnostics: Arc::clone(&self.diagnostics),
                        },
                        transition(&mut self.state),
                    )
                    .await;
                self.diagnostics.executing.store(0, Ordering::Release);
                tracing::debug!(
                    target: HOST_STATE_TRACE_TARGET,
                    event = "end",
                    operation = operation.id,
                    outcome = outcome.as_str(),
                    run_us = started.elapsed().as_micros() as u64,
                );
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
