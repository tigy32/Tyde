use std::time::Duration;

use futures_util::{StreamExt, future::Abortable};
use protocol::types::{MOBILE_RTC_CHANNEL_ID, MOBILE_RTC_CHANNEL_LABEL, MobileIceServer};
use tokio::sync::{mpsc, watch};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelState,
    RtcDataChannelType, RtcIceGatheringState, RtcIceServer, RtcIceTransportPolicy,
    RtcPeerConnection, RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit,
};

use crate::{
    BUFFER_BYTES, CHUNK_BYTES, Error, RtcStream, StreamEndpoint, WINDOW_CHUNKS, failure,
    stream_pair,
};

pub struct Peer {
    connection: RtcPeerConnection,
    channel: RtcDataChannel,
    messages: mpsc::Receiver<Vec<u8>>,
    changed: watch::Receiver<()>,
    failure: watch::Receiver<Option<String>>,
    handlers: Vec<Closure<dyn FnMut(Event)>>,
}

fn js_failure(stage: &'static str, value: JsValue) -> Error {
    failure(stage, format!("{value:?}"))
}

impl Peer {
    pub async fn new(servers: &[MobileIceServer]) -> Result<Self, Error> {
        if servers.is_empty() {
            return Err(failure("configuration", "TURN credentials are required"));
        }
        let ice_servers = js_sys::Array::new();
        for server in servers {
            let ice = RtcIceServer::new();
            let urls = server
                .urls
                .iter()
                .map(|url| JsValue::from_str(url.as_str()))
                .collect::<js_sys::Array>();
            ice.set_urls(&urls);
            ice.set_username(&server.username);
            ice.set_credential(&server.credential);
            ice_servers.push(&ice);
        }
        let config = RtcConfiguration::new();
        config.set_ice_servers(&ice_servers);
        config.set_ice_transport_policy(RtcIceTransportPolicy::Relay);
        let connection = RtcPeerConnection::new_with_configuration(&config)
            .map_err(|error| js_failure("create peer", error))?;
        let options = RtcDataChannelInit::new();
        options.set_negotiated(true);
        options.set_id(MOBILE_RTC_CHANNEL_ID);
        options.set_ordered(true);
        options.set_protocol(MOBILE_RTC_CHANNEL_LABEL);
        let channel = connection
            .create_data_channel_with_data_channel_dict(MOBILE_RTC_CHANNEL_LABEL, &options);
        channel.set_binary_type(RtcDataChannelType::Arraybuffer);
        channel.set_buffered_amount_low_threshold((BUFFER_BYTES / 2) as u32);

        let (changed_tx, changed) = watch::channel(());
        let (failure_tx, failure_rx) = watch::channel(None);
        let (messages_tx, messages) = mpsc::channel(BUFFER_BYTES / CHUNK_BYTES);
        let mut handlers = Vec::new();
        let notify = changed_tx.clone();
        let state = Closure::<dyn FnMut(Event)>::new(move |_| {
            notify.send_replace(());
        });
        connection.set_onicegatheringstatechange(Some(state.as_ref().unchecked_ref()));
        connection.set_onconnectionstatechange(Some(state.as_ref().unchecked_ref()));
        channel.set_onopen(Some(state.as_ref().unchecked_ref()));
        channel.set_onclose(Some(state.as_ref().unchecked_ref()));
        channel.set_onbufferedamountlow(Some(state.as_ref().unchecked_ref()));
        handlers.push(state);

        let notify = changed_tx.clone();
        let errors = failure_tx.clone();
        let error = Closure::<dyn FnMut(Event)>::new(move |_| {
            errors.send_replace(Some("WebRTC data channel reported an error".to_owned()));
            notify.send_replace(());
        });
        channel.set_onerror(Some(error.as_ref().unchecked_ref()));
        handlers.push(error);

        let receive_connection = connection.clone();
        let message = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let received = (|| {
                let event = event
                    .dyn_into::<MessageEvent>()
                    .map_err(|_| "invalid WebRTC message event".to_owned())?;
                let data = event
                    .data()
                    .dyn_into::<js_sys::ArrayBuffer>()
                    .map_err(|_| "expected binary WebRTC data".to_owned())?;
                if data.byte_length() as usize > CHUNK_BYTES + 1 {
                    return Err("WebRTC message exceeds the Tyde chunk limit".to_owned());
                }
                messages_tx
                    .try_send(js_sys::Uint8Array::new(&data).to_vec())
                    .map_err(|error| format!("WebRTC receive buffer unavailable: {error}"))
            })();
            if let Err(error) = received {
                failure_tx.send_replace(Some(error));
                receive_connection.close();
            }
            changed_tx.send_replace(());
        });
        channel.set_onmessage(Some(message.as_ref().unchecked_ref()));
        handlers.push(message);
        Ok(Self {
            connection,
            channel,
            messages,
            changed,
            failure: failure_rx,
            handlers,
        })
    }

    fn check(&self) -> Result<(), Error> {
        if let Some(message) = self.failure.borrow().as_ref() {
            return Err(failure("connection state", message));
        }
        let state = self.connection.connection_state();
        if matches!(
            state,
            RtcPeerConnectionState::Failed
                | RtcPeerConnectionState::Closed
                | RtcPeerConnectionState::Disconnected
        ) {
            return Err(failure(
                "connection state",
                format!("peer entered {state:?}"),
            ));
        }
        if self.channel.ready_state() == RtcDataChannelState::Closed {
            return Err(failure("connection state", "data channel closed"));
        }
        Ok(())
    }

    pub async fn offer(&mut self) -> Result<String, Error> {
        let offer = JsFuture::from(self.connection.create_offer())
            .await
            .map_err(|error| js_failure("create offer", error))?;
        self.set_local(offer.unchecked_into()).await
    }

    pub async fn answer(&mut self, offer: String) -> Result<String, Error> {
        let description = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        description.set_sdp(&offer);
        JsFuture::from(self.connection.set_remote_description(&description))
            .await
            .map_err(|error| js_failure("apply offer", error))?;
        let answer = JsFuture::from(self.connection.create_answer())
            .await
            .map_err(|error| js_failure("create answer", error))?;
        self.set_local(answer.unchecked_into()).await
    }

    pub async fn set_answer(&self, answer: String) -> Result<(), Error> {
        let description = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        description.set_sdp(&answer);
        JsFuture::from(self.connection.set_remote_description(&description))
            .await
            .map_err(|error| js_failure("apply answer", error))?;
        Ok(())
    }

    async fn set_local(&mut self, description: RtcSessionDescriptionInit) -> Result<String, Error> {
        JsFuture::from(self.connection.set_local_description(&description))
            .await
            .map_err(|error| js_failure("apply local description", error))?;
        let gather = async {
            loop {
                self.check()?;
                if self.connection.ice_gathering_state() == RtcIceGatheringState::Complete {
                    break;
                }
                self.changed
                    .changed()
                    .await
                    .map_err(|error| failure("gather TURN candidates", error))?;
            }
            let description = self
                .connection
                .local_description()
                .ok_or_else(|| failure("gather TURN candidates", "local description is missing"))?;
            let sdp = description.sdp();
            if !sdp
                .lines()
                .any(|line| line.starts_with("a=candidate:") && line.contains(" typ relay"))
            {
                return Err(failure(
                    "gather TURN candidates",
                    "TURN did not allocate a relay candidate",
                ));
            }
            Ok(sdp)
        };
        wasmtimer::tokio::timeout(Duration::from_secs(20), gather)
            .await
            .map_err(|error| failure("gather TURN candidates", error))?
    }

    pub async fn into_stream(self) -> Result<RtcStream, Error> {
        self.into_stream_for(Duration::from_secs(8 * 60 * 60)).await
    }

    pub(crate) async fn into_stream_for(mut self, lifetime: Duration) -> Result<RtcStream, Error> {
        let open = async {
            loop {
                self.check()?;
                if self.channel.ready_state() == RtcDataChannelState::Open {
                    return Ok(());
                }
                self.changed
                    .changed()
                    .await
                    .map_err(|error| failure("open data channel", error))?;
            }
        };
        wasmtimer::tokio::timeout(Duration::from_secs(25), open)
            .await
            .map_err(|error| failure("open data channel", error))??;
        tracing::info!("mobile WebRTC data channel opened through TURN");
        let (stream, worker, registration) = stream_pair();
        let errors = worker.inbound.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let run = async {
                tokio::select! {
                    result = self.run(worker) => result,
                    _ = wasmtimer::tokio::sleep(lifetime) => Err(failure("credentials", "TURN credential renewal is required")),
                }
            };
            if let Ok(Err(error)) = Abortable::new(run, registration).await {
                tracing::warn!(%error, "mobile WebRTC byte stream failed");
                if let Err(send_error) = errors.send(Err(error.to_string())).await {
                    tracing::debug!(%send_error, "WebRTC stream reader already closed");
                }
            }
        });
        Ok(stream)
    }

    async fn run(&mut self, mut worker: StreamEndpoint) -> Result<(), Error> {
        let mut outbound = None;
        let mut pending = std::collections::VecDeque::new();
        let mut pending_data = std::collections::VecDeque::new();
        loop {
            self.check()?;
            if self.channel.buffered_amount() as usize <= BUFFER_BYTES - CHUNK_BYTES
                && let Some(outgoing) = outbound.take()
            {
                let outgoing: crate::Outbound = outgoing;
                let mut bytes = Vec::with_capacity(outgoing.bytes.len() + 1);
                bytes.push(0);
                bytes.extend_from_slice(&outgoing.bytes);
                self.channel
                    .send_with_u8_array(&bytes)
                    .map_err(|error| js_failure("send data", error))?;
                pending.push_back(outgoing.accepted);
            }
            tokio::select! {
                permit = worker.inbound.reserve(), if !pending_data.is_empty() => {
                    let permit = permit.map_err(|error| failure("reserve received bytes", error))?;
                    let bytes = pending_data.pop_front().ok_or_else(|| failure("receive data", "pending chunk disappeared"))?;
                    self.channel.send_with_u8_array(&[1]).map_err(|error| js_failure("acknowledge data", error))?;
                    permit.send(Ok(bytes));
                }
                result = self.changed.changed() => {
                    result.map_err(|error| failure("connection state", error))?;
                }
                message = self.messages.recv() => {
                    match message {
                        Some(bytes) => match bytes.first() {
                            Some(0) if bytes.len() > 1 => {
                                if pending_data.len() == WINDOW_CHUNKS {
                                    return Err(failure("receive data", "peer exceeded the unacknowledged chunk window"));
                                }
                                pending_data.push_back(bytes[1..].to_vec());
                            }
                            Some(1) if bytes.len() == 1 => {
                                let ack = pending.pop_front().ok_or_else(|| failure("receive data", "unexpected acknowledgement"))?;
                                if ack.send(Ok(())).is_err() { return Ok(()); }
                            }
                            _ => return Err(failure("receive data", "invalid Tyde data channel record")),
                        },
                        None => return Err(failure("receive data", "message receiver closed")),
                    }
                }
                next = worker.outbound.next(), if outbound.is_none() => {
                    match next {
                        Some(chunk) => outbound = Some(chunk),
                        None => return Ok(()),
                    }
                }
            }
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.channel.set_onmessage(None);
        self.channel.set_onopen(None);
        self.channel.set_onclose(None);
        self.channel.set_onerror(None);
        self.channel.set_onbufferedamountlow(None);
        self.connection.set_onicegatheringstatechange(None);
        self.connection.set_onconnectionstatechange(None);
        let channel = self.channel.clone();
        let connection = self.connection.clone();
        channel.close();
        wasm_bindgen_futures::spawn_local(async move {
            let drain = async {
                while channel.ready_state() != RtcDataChannelState::Closed {
                    wasmtimer::tokio::sleep(Duration::from_millis(10)).await;
                }
            };
            if let Err(error) = wasmtimer::tokio::timeout(Duration::from_secs(5), drain).await {
                tracing::warn!(%error, "WebRTC acknowledgement drain timed out before close");
            }
            connection.close();
        });
        self.handlers.clear();
    }
}

#[cfg(test)]
mod wasm_tests {
    use super::*;
    use protocol::types::{MobileRtcDescription, MobileRtcSessionId, MobileSdpKind};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wasm_bindgen_test::*;
    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn browser_native_turn_interop_preserves_bulk_and_reconnect() {
        let endpoint =
            option_env!("TYDE_RTC_TEST_URL").expect("dev.sh must start the real TURN fixture");
        let client = reqwest::Client::new();
        let ice: Vec<MobileIceServer> = client
            .get(format!("{endpoint}/ice"))
            .send()
            .await
            .expect("real fixture")
            .json()
            .await
            .expect("TURN configuration");
        for _ in 0..2 {
            let mut peer = Peer::new(&ice).await.expect("browser peer");
            let offer = peer.offer().await.expect("browser TURN offer");
            let session = MobileRtcSessionId(uuid::Uuid::new_v4().to_string());
            let signed = crate::authenticate_description(
                session.clone(),
                MobileSdpKind::Offer,
                offer,
                &[53; 32],
            )
            .expect("authenticate offer");
            let answer: MobileRtcDescription = client
                .post(format!("{endpoint}/offer"))
                .json(&signed)
                .send()
                .await
                .expect("native peer")
                .json()
                .await
                .expect("signed answer");
            crate::verify_description(&answer, &session, MobileSdpKind::Answer, &[53; 32])
                .expect("authenticate native peer");
            peer.set_answer(answer.sdp)
                .await
                .expect("apply native answer");
            let stream = peer
                .into_stream()
                .await
                .expect("open browser/native data channel");
            let (mut reader, mut writer) = tokio::io::split(stream);
            let bytes: Vec<u8> = (0..4 * 1024 * 1024)
                .map(|index| (index % 251) as u8)
                .collect();
            let mut received = vec![0; bytes.len()];
            let send = async {
                for (index, chunk) in bytes.chunks(256 * 1024).enumerate() {
                    writer.write_all(chunk).await.expect("send bulk");
                    console_log!("browser TURN wrote {} bytes", (index + 1) * 256 * 1024);
                }
                writer.flush().await.expect("peer acknowledged bulk");
            };
            let receive = async {
                wasmtimer::tokio::sleep(Duration::from_millis(200)).await;
                for (index, chunk) in received.chunks_mut(256 * 1024).enumerate() {
                    reader.read_exact(chunk).await.expect("read native echo");
                    console_log!("browser TURN received {} bytes", (index + 1) * 256 * 1024);
                }
            };
            wasmtimer::tokio::timeout(Duration::from_secs(30), async {
                futures_util::join!(send, receive);
            })
            .await
            .expect("TURN transfer must complete with backpressure");
            assert_eq!(
                received, bytes,
                "browser/native TURN must preserve every byte"
            );
        }
    }
}
