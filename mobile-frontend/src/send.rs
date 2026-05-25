use std::cell::RefCell;
use std::collections::HashMap;

use protocol::{Envelope, FrameKind, StreamPath};
use serde::Serialize;

use crate::bridge;
use crate::state::LocalHostId;

// WASM is single-threaded; per-(host, stream) monotonic sequence numbers are
// the protocol invariant.
thread_local! {
    static SEQ_MAP: RefCell<HashMap<(LocalHostId, StreamPath), u64>> = RefCell::new(HashMap::new());
}

fn next_seq(host: &LocalHostId, stream: &StreamPath) -> u64 {
    SEQ_MAP.with(|map| {
        let mut map = map.borrow_mut();
        let counter = map.entry((host.clone(), stream.clone())).or_insert(0);
        let v = *counter;
        *counter += 1;
        v
    })
}

pub fn reset_seq_for_host(host: &LocalHostId) {
    SEQ_MAP.with(|map| {
        map.borrow_mut().retain(|(h, _), _| h != host);
    });
}

pub async fn send_frame<T: Serialize>(
    host: &LocalHostId,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
) -> Result<(), String> {
    let seq = next_seq(host, &stream);
    log::info!(
        "mobile_frame_tx host={} stream={} seq={} kind={}",
        host,
        stream,
        seq,
        kind
    );
    let envelope = Envelope::from_payload(stream, kind, seq, payload).map_err(|e| e.to_string())?;
    let line = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
    bridge::send_host_line(host, &line).await
}
