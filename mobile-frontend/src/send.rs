use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use protocol::{Envelope, FrameKind, ProtocolFrame, StreamPath};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

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

struct OutboundRequest {
    kind: FrameKind,
    payload: serde_json::Value,
    binary: Option<Vec<u8>>,
    custody: Option<crate::recovery::InputCustody>,
    reply: oneshot::Sender<Result<Option<Accepted>, SendFrameError>>,
}

struct OutboundStream {
    sender: mpsc::UnboundedSender<OutboundRequest>,
    active: Rc<Cell<bool>>,
    #[cfg(all(test, target_arch = "wasm32"))]
    sequence: Rc<Cell<u64>>,
}

thread_local! {
    static STREAMS: RefCell<HashMap<(LocalHostId, StreamPath), OutboundStream>> = RefCell::new(HashMap::new());
}

#[cfg(all(test, target_arch = "wasm32"))]
fn current_seq(host: &LocalHostId, stream: &StreamPath) -> u64 {
    STREAMS.with(|streams| {
        streams
            .borrow()
            .get(&(host.clone(), stream.clone()))
            .map_or(0, |stream| stream.sequence.get())
    })
}

pub fn reset_seq_for_host(host: &LocalHostId) {
    STREAMS.with(|streams| {
        streams.borrow_mut().retain(|(owner, _), stream| {
            if owner != host {
                return true;
            }
            stream.active.set(false);
            false
        })
    });
}

async fn submit<T: Serialize>(
    host: &LocalHostId,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
    binary: Option<Vec<u8>>,
    custody: Option<crate::recovery::InputCustody>,
) -> Result<Option<Accepted>, SendFrameError> {
    let payload = serde_json::to_value(payload)
        .map_err(|error| SendFrameError::Encoding(error.to_string()))?;
    let sender = STREAMS.with(|streams| {
        let mut streams = streams.borrow_mut();
        let key = (host.clone(), stream.clone());
        streams
            .entry(key)
            .or_insert_with(|| {
                let (sender, mut receiver) = mpsc::unbounded_channel::<OutboundRequest>();
                let active = Rc::new(Cell::new(true));
                let sequence = Rc::new(Cell::new(0));
                let live = active.clone();
                let counter = sequence.clone();
                let host = host.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    while let Some(request) = receiver.recv().await {
                        let result: Result<Option<Accepted>, SendFrameError> = async {
                            if !live.get() {
                                return Err(SendRejected::ConnectionClosed.into());
                            }
                            let seq = counter.get();
                            let envelope = Envelope::from_payload(
                                stream.clone(),
                                request.kind,
                                seq,
                                &request.payload,
                            )
                            .map_err(|error| SendFrameError::Encoding(error.to_string()))?;
                            let accepted = match request.binary {
                                Some(binary) => {
                                    bridge::web::send_voice_frame(
                                        &host,
                                        ProtocolFrame { envelope, binary },
                                    )
                                    .await?;
                                    None
                                }
                                None => {
                                    let line =
                                        serde_json::to_string(&envelope).map_err(|error| {
                                            SendFrameError::Encoding(error.to_string())
                                        })?;
                                    Some(
                                        bridge::send_host_line(&host, &line, request.custody)
                                            .await?,
                                    )
                                }
                            };
                            counter.set(seq.wrapping_add(1));
                            Ok(accepted)
                        }
                        .await;
                        let _ = request.reply.send(result);
                    }
                });
                OutboundStream {
                    sender,
                    active,
                    #[cfg(all(test, target_arch = "wasm32"))]
                    sequence,
                }
            })
            .sender
            .clone()
    });
    let retained_payload = payload.clone();
    let retained_custody = custody.clone();
    let (reply, result) = oneshot::channel();
    let outcome = match sender.send(OutboundRequest {
        kind,
        payload,
        binary,
        custody,
        reply,
    }) {
        Ok(()) => match result.await {
            Ok(result) => result,
            Err(_) => Err(SendRejected::ConnectionClosed.into()),
        },
        Err(_) => Err(SendRejected::ConnectionClosed.into()),
    };
    if let Err(error) = &outcome {
        crate::recovery::retain_rejected_input(
            host,
            kind,
            retained_payload,
            retained_custody,
            matches!(
                error,
                SendFrameError::Rejected(SendRejected::ConnectionClosed)
            ),
        )
        .await;
    }
    outcome
}

pub async fn send_frame<T: Serialize>(
    host: &LocalHostId,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
) -> Result<Accepted, SendFrameError> {
    submit(host, stream, kind, payload, None, None)
        .await?
        .ok_or_else(|| {
            SendFrameError::Encoding("Text frame returned a binary-only completion".into())
        })
}

pub async fn send_composer_owned(
    host: &LocalHostId,
    stream: StreamPath,
    payload: &protocol::HumanInputPayload,
    custody: crate::recovery::InputCustody,
) -> Result<Accepted, SendFrameError> {
    let kind = match payload.action {
        protocol::HumanInputAction::Send => FrameKind::SendMessage,
        protocol::HumanInputAction::Steer => FrameKind::SteerMessage,
        protocol::HumanInputAction::InterruptAndSend => FrameKind::HumanInput,
    };
    let result = if kind == FrameKind::HumanInput {
        submit(host, stream, kind, payload, None, Some(custody)).await?
    } else {
        submit(host, stream, kind, &payload.input, None, Some(custody)).await?
    };
    result.ok_or_else(|| {
        SendFrameError::Encoding("Human input returned a binary-only completion".into())
    })
}

pub async fn send_frame_with_custody<T: Serialize>(
    host: &LocalHostId,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
    custody: Option<crate::recovery::InputCustody>,
) -> Result<Accepted, SendFrameError> {
    submit(host, stream, kind, payload, None, custody)
        .await?
        .ok_or_else(|| {
            SendFrameError::Encoding("Text frame returned a binary-only completion".into())
        })
}

pub async fn send_binary_frame<T: Serialize>(
    host: &LocalHostId,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
    binary: Vec<u8>,
) -> Result<(), SendFrameError> {
    submit(host, stream, kind, payload, Some(binary), None)
        .await
        .map(|_| ())
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
