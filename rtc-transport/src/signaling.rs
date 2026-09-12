use protocol::types::{
    MOBILE_RTC_PROTOCOL_VERSION, MobilePeerRole, MobileRtcCredentials, MobileRtcSessionId,
    MobileSdpKind, MobileSignalCommand, MobileSignalEvent,
};
use std::time::Duration;

use crate::{Error, Peer, RtcStream, authenticate_description, failure, verify_description};

#[cfg(not(target_arch = "wasm32"))]
use tokio::time::{sleep, timeout};
#[cfg(target_arch = "wasm32")]
use wasmtimer::tokio::{sleep, timeout};

pub async fn connect(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
) -> Result<RtcStream, Error> {
    if credentials.protocol_version != MOBILE_RTC_PROTOCOL_VERSION {
        return Err(failure(
            "configuration",
            "unsupported WebRTC protocol version",
        ));
    }
    let url = url::Url::parse(credentials.signaling_url.as_str())
        .map_err(|error| failure("signaling URL", error))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(failure(
            "signaling URL",
            "expected HTTPS without embedded credentials",
        ));
    }
    let builder = reqwest::Client::builder();
    #[cfg(not(target_arch = "wasm32"))]
    let builder = builder.redirect(reqwest::redirect::Policy::none());
    let signal = Signaling {
        client: builder
            .build()
            .map_err(|error| failure("signaling client", error))?,
        credentials,
    };
    let remaining = || -> Result<Duration, Error> {
        #[cfg(target_arch = "wasm32")]
        let now = js_sys::Date::now() as u64;
        #[cfg(not(target_arch = "wasm32"))]
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| failure("credential clock", error))?
                .as_millis(),
        )
        .map_err(|error| failure("credential clock", error))?;
        let remaining = signal
            .credentials
            .expires_at_ms
            .checked_sub(now.saturating_add(60_000))
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| {
                failure(
                    "credentials",
                    "TURN credentials have reached their renewal deadline",
                )
            })?;
        Ok(Duration::from_millis(remaining))
    };
    let deadline = remaining()?;
    let mut peer = Peer::new(&signal.credentials.ice_servers).await?;
    let negotiation = async {
        match signal.credentials.role {
            MobilePeerRole::Mobile => {
                let session_id = MobileRtcSessionId(uuid::Uuid::new_v4().to_string());
                let offer = peer.offer().await?;
                signal
                    .publish(authenticate_description(
                        session_id.clone(),
                        MobileSdpKind::Offer,
                        offer,
                        pairing_key,
                    )?)
                    .await?;
                let description = signal.wait(Some(session_id.clone())).await?;
                verify_description(
                    &description,
                    &session_id,
                    MobileSdpKind::Answer,
                    pairing_key,
                )?;
                peer.set_answer(description.sdp).await?;
            }
            MobilePeerRole::Host => {
                let description = signal.wait(None).await?;
                let session_id = description.session_id.clone();
                verify_description(&description, &session_id, MobileSdpKind::Offer, pairing_key)?;
                let answer = peer.answer(description.sdp).await?;
                signal
                    .publish(authenticate_description(
                        session_id,
                        MobileSdpKind::Answer,
                        answer,
                        pairing_key,
                    )?)
                    .await?;
            }
        }
        peer.into_stream_for(remaining()?).await
    };
    let duration = match signal.credentials.role {
        MobilePeerRole::Mobile => Duration::from_secs(60),
        MobilePeerRole::Host => deadline,
    };
    timeout(duration, negotiation)
        .await
        .map_err(|_| failure("negotiation", "timed out"))?
}

struct Signaling {
    client: reqwest::Client,
    credentials: MobileRtcCredentials,
}

impl Signaling {
    async fn exchange(&self, command: MobileSignalCommand) -> Result<MobileSignalEvent, Error> {
        timeout(Duration::from_secs(15), async {
            let response = self
                .client
                .post(self.credentials.signaling_url.as_str())
                .bearer_auth(&self.credentials.signaling_token)
                .json(&command)
                .send()
                .await
                .map_err(|error| failure("signaling request", error))?;
            if !response.status().is_success() {
                return Err(failure(
                    "signaling",
                    format!("service refused request (HTTP {})", response.status()),
                ));
            }
            if response
                .content_length()
                .is_some_and(|length| length > 32 * 1024)
            {
                return Err(failure("signaling", "response exceeds 32 KiB"));
            }
            let body = response
                .bytes()
                .await
                .map_err(|error| failure("signaling response", error))?;
            if body.len() > 32 * 1024 {
                return Err(failure("signaling", "response exceeds 32 KiB"));
            }
            serde_json::from_slice(&body).map_err(|error| failure("signaling response", error))
        })
        .await
        .map_err(|_| failure("signaling request", "timed out"))?
    }

    async fn publish(
        &self,
        description: protocol::types::MobileRtcDescription,
    ) -> Result<(), Error> {
        match self
            .exchange(MobileSignalCommand::Publish { description })
            .await?
        {
            MobileSignalEvent::Waiting => Ok(()),
            MobileSignalEvent::Failed { message } => Err(failure("signaling", message)),
            _ => Err(failure("signaling", "unexpected publish response")),
        }
    }

    async fn wait(
        &self,
        session_id: Option<MobileRtcSessionId>,
    ) -> Result<protocol::types::MobileRtcDescription, Error> {
        loop {
            match self
                .exchange(MobileSignalCommand::Poll {
                    session_id: session_id.clone(),
                })
                .await?
            {
                MobileSignalEvent::Waiting => sleep(Duration::from_secs(1)).await,
                MobileSignalEvent::Description { description } => return Ok(description),
                MobileSignalEvent::Failed { message } => return Err(failure("signaling", message)),
            }
        }
    }
}
