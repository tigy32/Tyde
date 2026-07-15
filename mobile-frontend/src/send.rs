use std::cell::RefCell;
use std::collections::HashMap;

use protocol::{Envelope, FrameKind, StreamPath};
use serde::Serialize;

use crate::bridge::{self, Accepted, SendRejected};
use crate::state::LocalHostId;

#[derive(Debug)]
pub enum SendFrameError {
    Encoding(String),
    Rejected(SendRejected),
}

impl std::fmt::Display for SendFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encoding(message) => write!(f, "failed to encode host frame: {message}"),
            Self::Rejected(reason) => reason.fmt(f),
        }
    }
}

impl std::error::Error for SendFrameError {}

impl From<SendRejected> for SendFrameError {
    fn from(reason: SendRejected) -> Self {
        Self::Rejected(reason)
    }
}

// WASM is single-threaded; per-(host, stream) monotonic sequence numbers are
// the protocol invariant.
thread_local! {
    static SEQ_MAP: RefCell<HashMap<(LocalHostId, StreamPath), u64>> = RefCell::new(HashMap::new());
}

fn current_seq(host: &LocalHostId, stream: &StreamPath) -> u64 {
    SEQ_MAP.with(|map| {
        map.borrow()
            .get(&(host.clone(), stream.clone()))
            .copied()
            .unwrap_or(0)
    })
}

fn commit_seq(host: &LocalHostId, stream: &StreamPath, accepted_seq: u64) {
    SEQ_MAP.with(|map| {
        let mut map = map.borrow_mut();
        let counter = map.entry((host.clone(), stream.clone())).or_insert(0);
        debug_assert_eq!(*counter, accepted_seq);
        *counter = accepted_seq.wrapping_add(1);
    });
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
) -> Result<Accepted, SendFrameError> {
    let seq = current_seq(host, &stream);
    log::info!(
        "mobile_frame_tx host={} stream={} seq={} kind={}",
        host,
        stream,
        seq,
        kind
    );
    let sequence_stream = stream.clone();
    let envelope = Envelope::from_payload(stream, kind, seq, payload)
        .map_err(|error| SendFrameError::Encoding(error.to_string()))?;
    let line = serde_json::to_string(&envelope)
        .map_err(|error| SendFrameError::Encoding(error.to_string()))?;
    let accepted = bridge::send_host_line(host, &line)
        .await
        .map_err(SendFrameError::Rejected)?;
    commit_seq(host, &sequence_stream, seq);
    Ok(accepted)
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;

    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn rejected_admission_does_not_consume_protocol_sequence() {
        let host = LocalHostId("host-sequence".to_owned());
        let stream = StreamPath("/host/sequence".to_owned());
        reset_seq_for_host(&host);

        {
            let _guard = bridge::test_reject_sends();
            let result = send_frame(
                &host,
                stream.clone(),
                FrameKind::ClientError,
                &serde_json::json!({"attempt": 1}),
            )
            .await;
            assert!(matches!(
                result,
                Err(SendFrameError::Rejected(SendRejected::ConnectionClosed))
            ));
        }
        assert_eq!(current_seq(&host, &stream), 0);

        let _guard = bridge::test_capture_sends();
        let accepted = send_frame(
            &host,
            stream.clone(),
            FrameKind::ClientError,
            &serde_json::json!({"attempt": 2}),
        )
        .await
        .expect("the capture bridge accepts the frame");
        assert_eq!(accepted.connection_instance_id, 1);
        let lines = bridge::test_sent_lines();
        let envelope: Envelope =
            serde_json::from_str(&lines[0]).expect("captured line is a typed envelope");
        assert_eq!(envelope.seq, 0);
        assert_eq!(current_seq(&host, &stream), 1);
    }
}
