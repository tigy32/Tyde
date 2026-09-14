use axum::{
    Json, Router,
    extract::State,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ready_path = std::env::args()
        .nth(1)
        .ok_or("expected readiness file argument")?;
    let relay = Arc::new(tests::rtc::relay().await);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = Router::new()
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
        .merge(tests::rtc_reconnect::router(
            relay.clone(),
            format!("http://{address}"),
        ))
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
