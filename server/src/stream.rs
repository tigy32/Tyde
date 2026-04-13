use protocol::{Envelope, FrameKind, StreamPath};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub(crate) struct Stream {
    path: StreamPath,
    tx: mpsc::Sender<Envelope>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamClosed;

impl Stream {
    pub fn new(path: StreamPath, tx: mpsc::Sender<Envelope>) -> Self {
        Self { path, tx }
    }

    pub fn with_path(&self, path: StreamPath) -> Self {
        Self {
            path,
            tx: self.tx.clone(),
        }
    }

    pub fn path(&self) -> &StreamPath {
        &self.path
    }

    pub async fn send_value(
        &self,
        kind: FrameKind,
        payload: serde_json::Value,
    ) -> Result<(), StreamClosed> {
        let envelope = Envelope {
            stream: self.path.clone(),
            kind,
            seq: 0,
            payload,
        };
        self.tx.send(envelope).await.map_err(|_| StreamClosed)
    }
}
