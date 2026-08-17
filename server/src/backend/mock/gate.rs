//! Test-controlled parking points for scripted turns.

use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore, mpsc};

#[derive(Debug, Clone)]
pub(super) struct MockGate(Arc<MockGateInner>);

#[derive(Debug)]
pub(super) struct MockGateInner {
    entered_tx: mpsc::UnboundedSender<()>,
    entered_rx: Mutex<mpsc::UnboundedReceiver<()>>,
    release: Semaphore,
}

impl MockGate {
    pub(super) async fn wait(&self) {
        let _ = self.0.entered_tx.send(());
        if let Ok(permit) = self.0.release.acquire().await {
            permit.forget();
        }
    }
}

/// Test-side handle for a scripted turn gate.
#[derive(Debug)]
pub struct MockGateHandle {
    inner: Arc<MockGateInner>,
}

impl MockGateHandle {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let (entered_tx, entered_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(MockGateInner {
                entered_tx,
                entered_rx: Mutex::new(entered_rx),
                release: Semaphore::new(0),
            }),
        }
    }

    /// The script-side gate sharing this handle's state.
    pub(super) fn gate(&self) -> MockGate {
        MockGate(Arc::clone(&self.inner))
    }

    /// Wait until a scripted turn is parked on this gate. Do not wrap this in
    /// an event-deadline timeout; it is the happens-before edge itself.
    pub async fn wait_until_entered(&self) {
        self.inner
            .entered_rx
            .lock()
            .await
            .recv()
            .await
            .expect("mock gate closed before a turn entered it");
    }

    pub fn release_one(&self) {
        self.inner.release.add_permits(1);
    }
}

impl Drop for MockGateHandle {
    fn drop(&mut self) {
        self.inner.release.close();
    }
}
