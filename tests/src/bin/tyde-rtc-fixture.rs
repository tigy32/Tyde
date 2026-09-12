use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use protocol::{MobileIceServer, MobileRtcDescription, MobileSdpKind};
use rtc_transport::{Peer, authenticate_description, verify_description};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn offer(
    State(ice): State<MobileIceServer>,
    Json(description): Json<MobileRtcDescription>,
) -> Response {
    match answer(ice, description).await {
        Ok(answer) => Json(answer).into_response(),
        Err(error) => {
            eprintln!("browser TURN fixture failed: {error}");
            (StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
    }
}

async fn answer(
    ice: MobileIceServer,
    description: MobileRtcDescription,
) -> Result<MobileRtcDescription, rtc_transport::Error> {
    let session = description.session_id.clone();
    verify_description(&description, &session, MobileSdpKind::Offer, &[53; 32])?;
    let mut peer = Peer::new(&[ice]).await?;
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
    let (relay, ice) = tests::rtc::relay().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = Router::new()
        .route(
            "/ice",
            get(|State(ice): State<MobileIceServer>| async { Json(vec![ice]) }),
        )
        .route(
            "/offer",
            post(offer).options(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(axum::middleware::map_response(
            |mut response: Response| async {
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    HeaderValue::from_static("*"),
                );
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    HeaderValue::from_static("content-type"),
                );
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    HeaderValue::from_static("GET, POST, OPTIONS"),
                );
                response
            },
        ))
        .with_state(ice);
    std::fs::write(ready_path, format!("http://{address}"))?;
    axum::serve(listener, app).await?;
    relay.close().await?;
    Ok(())
}
