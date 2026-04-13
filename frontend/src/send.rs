use std::cell::RefCell;
use std::collections::HashMap;

use protocol::{Envelope, FrameKind, StreamPath};
use serde::Serialize;

use crate::bridge;

// WASM is single-threaded, so RefCell is fine.
// Per-stream monotonic sequence numbers, as required by the protocol.
thread_local! {
    static SEQ_MAP: RefCell<HashMap<StreamPath, u64>> = RefCell::new(HashMap::new());
}

fn next_seq(stream: &StreamPath) -> u64 {
    SEQ_MAP.with(|map| {
        let mut map = map.borrow_mut();
        let counter = map.entry(stream.clone()).or_insert(0);
        let v = *counter;
        *counter += 1;
        v
    })
}

pub async fn send_frame<T: Serialize>(
    host_id: &str,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
) -> Result<(), String> {
    let seq = next_seq(&stream);
    let envelope =
        Envelope::from_payload(stream, kind, seq, payload).map_err(|e| e.to_string())?;
    let line = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
    bridge::send_host_line(bridge::SendHostLineRequest {
        host_id: host_id.to_owned(),
        line,
    })
    .await
}
