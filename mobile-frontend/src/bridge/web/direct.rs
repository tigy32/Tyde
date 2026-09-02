//! Direct-hosting transport: pairing redemption and the protocol WebSocket.
//!
//! When the host serves this bundle itself there is no broker and no managed
//! service to mint credentials. The phone is already loaded from the host's
//! origin, so it pairs and connects straight back to it.

use std::cell::RefCell;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use protocol::{MobileDirectErrorResponse, MobileDirectPairRequest, MobileDirectPairResponse};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{BinaryType, MessageEvent, WebSocket};

use super::service::HttpMethod;

/// Names the wire the client speaks. The host echoes it back on upgrade.
const WS_PROTOCOL: &str = "tyde.v1";
/// Carries the device token. The browser cannot set request headers on a
/// WebSocket, and a token in the query string would land in every reverse
/// proxy access log on the way in; subprotocols travel in
/// `Sec-WebSocket-Protocol`, which proxies do not log by default.
const WS_TOKEN_PREFIX: &str = "tyde.token.";

/// The origin this bundle was served from. In direct hosting that is the host
/// itself, which makes it a better address than anything the QR could claim —
/// a payload-supplied origin would just be a redirect nobody vouched for.
pub fn document_origin() -> Result<String, String> {
    web_sys::window()
        .ok_or_else(|| "no browser window".to_owned())?
        .location()
        .origin()
        .map_err(|_| "could not read the page origin".to_owned())
}

/// Exchanges a pairing offer secret for a durable device token.
pub async fn redeem_direct_offer(
    origin: &str,
    request: &MobileDirectPairRequest,
) -> Result<MobileDirectPairResponse, String> {
    let body = serde_json::to_string(request)
        .map_err(|error| format!("could not encode the pairing request: {error}"))?;
    let response = super::service::send(
        HttpMethod::Post,
        &format!("{origin}/tyde/pair"),
        Some(body.as_bytes()),
        &[],
    )
    .await
    .map_err(|error| format!("could not reach the host: {error}"))?;

    if response.status >= 300 {
        // The host answers failures in the protocol's own error vocabulary, so
        // report its message rather than inventing one from the status code.
        return Err(
            match serde_json::from_str::<MobileDirectErrorResponse>(&response.body) {
                Ok(error) => error.message,
                Err(_) => format!(
                    "the host refused the pairing code (HTTP {})",
                    response.status
                ),
            },
        );
    }

    serde_json::from_str(&response.body)
        .map_err(|error| format!("the host sent a pairing response we could not read: {error}"))
}

struct Inbound {
    frames: std::collections::VecDeque<Vec<u8>>,
    closed: bool,
    waker: Option<Waker>,
}

impl Inbound {
    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

/// A byte stream over a browser WebSocket, so the ordinary Tyde handshake and
/// connection loop run over it unchanged.
pub struct DirectWebSocketStream {
    socket: WebSocket,
    inbound: Rc<RefCell<Inbound>>,
    partial: Vec<u8>,
    partial_offset: usize,
    // Dropping these detaches the handlers, so they live as long as the stream.
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(web_sys::Event)>,
    _on_error: Closure<dyn FnMut(web_sys::Event)>,
}

impl DirectWebSocketStream {
    /// Opens the protocol WebSocket and resolves once the socket is open, so a
    /// caller never writes a handshake into a socket that is still connecting.
    pub async fn connect(origin: &str, device_token: &str) -> Result<Self, String> {
        let url = websocket_url(origin)?;
        let protocols = js_sys::Array::of2(
            &WS_PROTOCOL.into(),
            &format!("{WS_TOKEN_PREFIX}{device_token}").into(),
        );
        let socket = WebSocket::new_with_str_sequence(&url, &protocols)
            .map_err(|_| format!("could not open a connection to {url}"))?;
        socket.set_binary_type(BinaryType::Arraybuffer);

        let inbound = Rc::new(RefCell::new(Inbound {
            frames: std::collections::VecDeque::new(),
            closed: false,
            waker: None,
        }));

        // One close handler, installed once. An earlier version registered a
        // second one while waiting for the socket to open, which replaced this
        // one and left `closed` unset forever — a dropped connection then hung
        // the next read instead of reporting EOF.
        let (open_tx, open_rx) = futures_channel::oneshot::channel::<Result<(), String>>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));

        let message_inbound = Rc::clone(&inbound);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() else {
                return;
            };
            let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
            let mut inbound = message_inbound.borrow_mut();
            inbound.frames.push_back(bytes);
            inbound.wake();
        });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let close_inbound = Rc::clone(&inbound);
        let close_open_tx = Rc::clone(&open_tx);
        let on_close = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            let mut inbound = close_inbound.borrow_mut();
            inbound.closed = true;
            inbound.wake();
            // A refused upgrade — bad or revoked token — closes before it ever
            // opens, so this is the path an unpaired device takes.
            if let Some(tx) = close_open_tx.borrow_mut().take() {
                let _ = tx.send(Err(
                    "the host refused this device; pair with it again".to_owned()
                ));
            }
        });
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        let error_inbound = Rc::clone(&inbound);
        let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            let mut inbound = error_inbound.borrow_mut();
            inbound.closed = true;
            inbound.wake();
        });
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let on_open = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            if let Some(tx) = open_tx.borrow_mut().take() {
                let _ = tx.send(Ok(()));
            }
        });
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        let opened = open_rx
            .await
            .unwrap_or_else(|_| Err("the connection was dropped while opening".to_owned()));
        socket.set_onopen(None);
        opened?;

        Ok(Self {
            socket,
            inbound,
            partial: Vec::new(),
            partial_offset: 0,
            _on_message: on_message,
            _on_close: on_close,
            _on_error: on_error,
        })
    }
}

/// Turns the page origin into the protocol endpoint, keeping the scheme's
/// security: a page served over https must not downgrade its socket to ws://.
fn websocket_url(origin: &str) -> Result<String, String> {
    let rest = origin
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            origin
                .strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .ok_or_else(|| format!("origin {origin} is not http(s)"))?;
    Ok(format!("{}/tyde/ws", rest.trim_end_matches('/')))
}

impl AsyncRead for DirectWebSocketStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.partial_offset >= self.partial.len() {
            let next = {
                let mut inbound = self.inbound.borrow_mut();
                match inbound.frames.pop_front() {
                    Some(frame) => Some(frame),
                    None if inbound.closed => None,
                    None => {
                        inbound.waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                }
            };
            match next {
                // A closed socket with nothing buffered is a clean EOF for the
                // protocol reader above.
                None => return Poll::Ready(Ok(())),
                Some(frame) => {
                    self.partial = frame;
                    self.partial_offset = 0;
                }
            }
        }

        let available = &self.partial[self.partial_offset..];
        let take = available.len().min(buf.remaining());
        buf.put_slice(&available[..take]);
        self.partial_offset += take;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for DirectWebSocketStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.socket.send_with_u8_array(buf) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection to the host closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // `send` already handed the bytes to the browser's send queue.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = self.socket.close();
        Poll::Ready(Ok(()))
    }
}

impl Drop for DirectWebSocketStream {
    fn drop(&mut self) {
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        self.socket.set_onerror(None);
        let _ = self.socket.close();
    }
}
