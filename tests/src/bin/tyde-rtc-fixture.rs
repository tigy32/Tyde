use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use protocol::{MobileRtcDescription, MobileSdpKind};
use rtc_transport::{Peer, authenticate_description, verify_description};
use std::sync::Arc;
use tests::rtc::RelayFixture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn offer(
    State(relay): State<Arc<RelayFixture>>,
    Json(description): Json<MobileRtcDescription>,
) -> Response {
    match answer(relay, description).await {
        Ok(answer) => Json(answer).into_response(),
        Err(error) => {
            eprintln!("browser TURN fixture failed: {error}");
            (StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
    }
}

async fn answer(
    relay: Arc<RelayFixture>,
    description: MobileRtcDescription,
) -> Result<MobileRtcDescription, rtc_transport::Error> {
    let session = description.session_id.clone();
    verify_description(&description, &session, MobileSdpKind::Offer, &[53; 32])?;
    let mut peer =
        Peer::with_tls_roots(std::slice::from_ref(&relay.tls_ice), relay.roots.clone()).await?;
    let answer = peer.answer(description.sdp).await?;
    let signed = authenticate_description(session, MobileSdpKind::Answer, answer, &[53; 32])?;
    tokio::spawn(async move {
        let echo = async {
            let mut stream = peer.into_stream().await.map_err(std::io::Error::other)?;
            let mut buffer = vec![0; 16 * 1024];
            let mut total = 0;
            loop {
                let count = stream.read(&mut buffer).await?;
                if count == 0 {
                    return Ok::<(), std::io::Error>(());
                }
                total += count;
                if total <= 16 * 1024 || total % (256 * 1024) == 0 {
                    eprintln!("browser TURN fixture received {total} bytes");
                }
                stream.write_all(&buffer[..count]).await?;
                stream.flush().await?;
                if total <= 16 * 1024 || total % (256 * 1024) == 0 {
                    eprintln!("browser TURN fixture echo acknowledged {total} bytes");
                }
            }
        };
        if let Err(error) = echo.await {
            eprintln!("browser TURN fixture echo ended: {error}");
        }
    });
    Ok(signed)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("off,tyde::connection_bootstrap=info,tyde::rtc_flow=debug")
        .with_ansi(false)
        .init();
    let ready_path = std::env::args()
        .nth(1)
        .ok_or("expected readiness file argument")?;
    let relay = Arc::new(tests::rtc::relay().await);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let unanswered_url = format!("http://{address}/unanswered/signal");
    let unanswered_ice = relay.browser_ice.clone();
    let publish_started = Arc::new(tokio::sync::Notify::new());
    let observed_publish = publish_started.clone();
    let app = Router::new()
        .route(
            "/unanswered/pairings/{pairing}/webrtc",
            post(
                move |Path(pairing): Path<String>,
                      Json(request): Json<protocol::MobileRtcCredentialsRequest>| {
                    let url = unanswered_url.clone();
                    let ice = unanswered_ice.clone();
                    async move {
                        assert_eq!(request.role, protocol::MobilePeerRole::Mobile);
                        assert_eq!(
                            request.protocol_version,
                            protocol::MOBILE_RTC_PROTOCOL_VERSION
                        );
                        Json(protocol::MobileRtcCredentials {
                            protocol_version: protocol::MOBILE_RTC_PROTOCOL_VERSION,
                            pairing_id: protocol::MobilePairingId(pairing),
                            role: protocol::MobilePeerRole::Mobile,
                            signaling_url: protocol::MobileSignalingUrl(url),
                            signaling_token: "owned-unanswered-fixture".into(),
                            ice_servers: vec![ice],
                            expires_at_ms: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64
                                + 3_600_000,
                        })
                    }
                },
            )
            .options(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/unanswered/signal",
            post(move |Json(command): Json<protocol::MobileSignalCommand>| {
                let started = publish_started.clone();
                async move {
                    match command {
                        protocol::MobileSignalCommand::Publish { description } => {
                            verify_description(
                                &description,
                                &description.session_id,
                                MobileSdpKind::Offer,
                                &[53; 32],
                            )
                            .unwrap();
                            started.notify_one();
                        }
                    }
                    std::future::pending::<Json<protocol::MobileSignalEvent>>().await
                }
            })
            .options(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/unanswered/publish-started",
            get(move || {
                let started = observed_publish.clone();
                async move {
                    started.notified().await;
                    StatusCode::NO_CONTENT
                }
            }),
        )
        .route(
            "/interrupt-traffic",
            post(|State(relay): State<Arc<RelayFixture>>| async move {
                Json(
                    relay
                        .interrupt_traffic(std::time::Duration::from_secs(8))
                        .await,
                )
            })
            .options(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/ice",
            get(|State(relay): State<Arc<RelayFixture>>| async move {
                Json(vec![relay.browser_ice.clone()])
            }),
        )
        .route(
            "/offer",
            post(offer).options(|| async { StatusCode::NO_CONTENT }),
        )
        .with_state(relay.clone())
        .merge(tests::rtc_reconnect::router(relay.clone()))
        .layer(axum::middleware::map_response(
            |headers: HeaderMap, mut response: Response| async move {
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    headers
                        .get(header::ORIGIN)
                        .cloned()
                        .unwrap_or(HeaderValue::from_static("*")),
                );
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    HeaderValue::from_static("content-type, x-tycode-pairing-auth, authorization"),
                );
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    HeaderValue::from_static("GET, POST, OPTIONS"),
                );
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                    HeaderValue::from_static("true"),
                );
                response
            },
        ));
    std::fs::write(ready_path, format!("http://{address}"))?;
    tokio::select! {
        result = async { axum::serve(listener, app).await } => result?,
        () = shutdown() => {}
    }
    relay.close().await;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("fixture termination signal");
        signal.recv().await;
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .expect("fixture shutdown signal");
}
