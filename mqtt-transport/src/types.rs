use std::fmt;
use std::io::Cursor;
#[cfg(feature = "test-support")]
use std::net::IpAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
pub use protocol::DEFAULT_MOBILE_MQTT_BROKER_URL;
use protocol::{BrokerUrl, ManagedBrokerEndpoint, MobilePairingOfferId, TYDE_VERSION};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub const ROOM_ID_LEN: usize = 16;
pub const PRE_SHARED_KEY_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportTypeError {
    #[error("{type_name} must not be empty")]
    Empty { type_name: &'static str },

    #[error("{type_name} is not base64url-no-pad: {message}")]
    InvalidBase64 {
        type_name: &'static str,
        message: String,
    },

    #[error("{type_name} length {actual} is invalid; expected {expected}")]
    InvalidLength {
        type_name: &'static str,
        expected: usize,
        actual: usize,
    },

    #[error("invalid mobile pairing URI: {message}")]
    InvalidPairingUri { message: String },

    #[error("unsupported mobile pairing QR version {actual}; expected {expected}")]
    PairingQrVersionMismatch { actual: u32, expected: u32 },

    #[error("unsupported MQTT transport protocol version {actual}; expected {expected}")]
    TransportProtocolVersionMismatch { actual: u32, expected: u32 },

    #[error("invalid MQTT broker URL: {message}")]
    InvalidBrokerUrl { message: String },

    #[error("failed to encode {type_name} as CBOR: {message}")]
    CborEncode {
        type_name: &'static str,
        message: String,
    },

    #[error("failed to decode {type_name} from CBOR: {message}")]
    CborDecode {
        type_name: &'static str,
        message: String,
    },
}

pub const MQTT_TRANSPORT_PROTOCOL_VERSION: u32 = 3;
pub const LEGACY_MOBILE_QR_VERSION: u32 = 2;
pub const MOBILE_QR_VERSION: u32 = LEGACY_MOBILE_QR_VERSION;
pub const MOBILE_MANAGED_QR_VERSION: u32 = 3;
pub const MQTT_VERSION: u8 = 5;
pub const MQTT_QOS_AT_LEAST_ONCE: u8 = 1;
pub const MQTT_RETAIN: bool = false;
pub const MQTT_CLEAN_START: bool = true;
const LEGACY_PAIRING_URI_PREFIX: &str = "tyde-pair://v1?";
const MANAGED_PAIRING_URI_PREFIX: &str = "tyde-pair://v2?";
const PAIRING_URI_PREFIX: &str = LEGACY_PAIRING_URI_PREFIX;
/// Origin-root web loader that turns the host's pairing QR into a generic
/// HTTPS link the native iOS/Android Camera can open. The PSK-bearing
/// `tyde-pair://…` URI rides in the URL FRAGMENT (after `#`) so it is never
/// sent to the S3/CloudFront origin; the loader clears the fragment on read.
pub const MOBILE_PAIRING_WEB_BASE_URL: &str = "https://tycode.dev/tyde/";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrokerEndpoint {
    pub url: BrokerUrl,
    pub auth: BrokerAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrokerAuth {
    Anonymous,
    UsernamePassword { username: String, password: String },
}

pub fn default_mobile_broker_endpoint() -> BrokerEndpoint {
    BrokerEndpoint {
        url: BrokerUrl::new(DEFAULT_MOBILE_MQTT_BROKER_URL)
            .expect("default mobile MQTT broker URL is valid"),
        auth: BrokerAuth::Anonymous,
    }
}

pub fn is_legacy_public_broker_endpoint(endpoint: &BrokerEndpoint) -> bool {
    endpoint.url.as_str() == DEFAULT_MOBILE_MQTT_BROKER_URL
        && matches!(endpoint.auth, BrokerAuth::Anonymous)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MqttTransportPolicy {
    pub mqtt_version: u8,
    pub qos: u8,
    pub retain: bool,
    pub clean_start: bool,
}

impl Default for MqttTransportPolicy {
    fn default() -> Self {
        Self {
            mqtt_version: MQTT_VERSION,
            qos: MQTT_QOS_AT_LEAST_ONCE,
            retain: MQTT_RETAIN,
            clean_start: MQTT_CLEAN_START,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePairingQrPayload {
    pub v: u32,
    pub protocol_version: u32,
    pub tyde_version: protocol::Version,
    pub broker: BrokerEndpoint,
    pub policy: MqttTransportPolicy,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub host_label: String,
    /// Exact, prerelease-capable host build version used by the web/PWA loader
    /// to select the matching versioned bundle. `Option` for backward
    /// compatibility with QR codes from older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_version: Option<protocol::TydeReleaseVersion>,
}

impl MobilePairingQrPayload {
    pub fn new(
        protocol_version: u32,
        broker: BrokerEndpoint,
        room: RoomId,
        psk: PreSharedKey,
        host_label: String,
    ) -> Self {
        Self {
            v: MOBILE_QR_VERSION,
            protocol_version,
            tyde_version: TYDE_VERSION,
            broker,
            policy: MqttTransportPolicy::default(),
            room,
            psk,
            host_label,
            // Populated by the host from its real build version (see
            // `server::host_release_version`); `new` leaves it unset.
            release_version: None,
        }
    }

    pub fn encode_cbor(&self) -> Result<Vec<u8>, TransportTypeError> {
        encode_cbor("MobilePairingQrPayload", self)
    }

    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, TransportTypeError> {
        let payload: Self = decode_cbor("MobilePairingQrPayload", bytes)?;
        if payload.v != MOBILE_QR_VERSION {
            return Err(TransportTypeError::PairingQrVersionMismatch {
                actual: payload.v,
                expected: MOBILE_QR_VERSION,
            });
        }
        if payload.policy != MqttTransportPolicy::default() {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "unsupported MQTT transport policy in pairing QR".to_owned(),
            });
        }
        validate_broker_url(&payload.broker.url)?;
        Ok(payload)
    }

    pub fn to_uri(&self) -> Result<String, TransportTypeError> {
        let cbor = self.encode_cbor()?;
        let encoded = URL_SAFE_NO_PAD.encode(cbor);
        Ok(format!("{PAIRING_URI_PREFIX}{encoded}"))
    }

    pub fn from_uri(uri: &str) -> Result<Self, TransportTypeError> {
        let encoded = uri.strip_prefix(PAIRING_URI_PREFIX).ok_or_else(|| {
            TransportTypeError::InvalidPairingUri {
                message: format!("URI must start with {PAIRING_URI_PREFIX}"),
            }
        })?;
        if encoded.is_empty() {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "URI payload must not be empty".to_owned(),
            });
        }
        let cbor =
            URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|err| TransportTypeError::InvalidBase64 {
                    type_name: "MobilePairingQrPayload URI payload",
                    message: err.to_string(),
                })?;
        Self::decode_cbor(&cbor)
    }

    /// Builds the generic HTTPS pairing link encoded into the host's QR. The
    /// PSK-bearing `tyde-pair://…` URI is placed in the URL FRAGMENT so the
    /// native Camera opens the web loader at the origin without ever sending
    /// the secret to the S3/CloudFront origin (fragments are not transmitted in
    /// the HTTP request). The loader reads and clears the fragment on load.
    pub fn to_pairing_url(&self) -> Result<String, TransportTypeError> {
        Ok(format!("{MOBILE_PAIRING_WEB_BASE_URL}#{}", self.to_uri()?))
    }

    /// Legacy direct decoder retained for old local/dev pairings. New QR scan
    /// flows should call [`MobilePairingQrOffer::from_any`] so legacy public
    /// broker payloads are classified as repair-required instead of connected.
    pub fn from_any(input: &str) -> Result<Self, TransportTypeError> {
        let trimmed = input.trim();
        if trimmed.starts_with(PAIRING_URI_PREFIX) {
            return Self::from_uri(trimmed);
        }
        if let Some((_, fragment)) = trimmed.split_once('#')
            && fragment.starts_with(PAIRING_URI_PREFIX)
        {
            return Self::from_uri(fragment);
        }
        Self::from_uri(trimmed)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedMobilePairingQrPayload {
    pub v: u32,
    pub protocol_version: u32,
    pub transport_protocol_version: u32,
    pub tyde_version: protocol::Version,
    pub release_version: protocol::TydeReleaseVersion,
    pub offer_id: MobilePairingOfferId,
    pub offer_secret: String,
    pub broker: ManagedBrokerEndpoint,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub host_label: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ManagedMobilePairingQrPayloadParams {
    pub protocol_version: u32,
    pub release_version: protocol::TydeReleaseVersion,
    pub offer_id: MobilePairingOfferId,
    pub offer_secret: String,
    pub broker: ManagedBrokerEndpoint,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub host_label: String,
    pub expires_at_ms: u64,
}

impl fmt::Debug for ManagedMobilePairingQrPayloadParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedMobilePairingQrPayloadParams")
            .field("protocol_version", &self.protocol_version)
            .field("release_version", &self.release_version)
            .field("offer_id", &self.offer_id)
            .field("offer_secret", &"<redacted>")
            .field("broker", &self.broker)
            .field("room", &self.room)
            .field("psk", &"<redacted>")
            .field("host_label", &self.host_label)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl fmt::Debug for ManagedMobilePairingQrPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedMobilePairingQrPayload")
            .field("v", &self.v)
            .field("protocol_version", &self.protocol_version)
            .field(
                "transport_protocol_version",
                &self.transport_protocol_version,
            )
            .field("tyde_version", &self.tyde_version)
            .field("release_version", &self.release_version)
            .field("offer_id", &self.offer_id)
            .field("offer_secret", &"<redacted>")
            .field("broker", &self.broker)
            .field("room", &self.room)
            .field("psk", &"<redacted>")
            .field("host_label", &self.host_label)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl ManagedMobilePairingQrPayload {
    pub fn new(
        protocol_version: u32,
        release_version: protocol::TydeReleaseVersion,
        offer_id: MobilePairingOfferId,
        offer_secret: String,
        broker: ManagedBrokerEndpoint,
        host_label: String,
        expires_at_ms: u64,
    ) -> Self {
        Self::new_with_rendezvous(ManagedMobilePairingQrPayloadParams {
            protocol_version,
            release_version,
            offer_id,
            offer_secret,
            broker,
            room: RoomId::random(),
            psk: PreSharedKey::random(),
            host_label,
            expires_at_ms,
        })
    }

    pub fn new_with_rendezvous(params: ManagedMobilePairingQrPayloadParams) -> Self {
        Self {
            v: MOBILE_MANAGED_QR_VERSION,
            protocol_version: params.protocol_version,
            transport_protocol_version: MQTT_TRANSPORT_PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            release_version: params.release_version,
            offer_id: params.offer_id,
            offer_secret: params.offer_secret,
            broker: params.broker,
            room: params.room,
            psk: params.psk,
            host_label: params.host_label,
            expires_at_ms: params.expires_at_ms,
        }
    }

    pub fn encode_cbor(&self) -> Result<Vec<u8>, TransportTypeError> {
        encode_cbor("ManagedMobilePairingQrPayload", self)
    }

    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, TransportTypeError> {
        let payload: Self = decode_cbor("ManagedMobilePairingQrPayload", bytes)?;
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), TransportTypeError> {
        if self.v != MOBILE_MANAGED_QR_VERSION {
            return Err(TransportTypeError::PairingQrVersionMismatch {
                actual: self.v,
                expected: MOBILE_MANAGED_QR_VERSION,
            });
        }
        if self.transport_protocol_version != MQTT_TRANSPORT_PROTOCOL_VERSION {
            return Err(TransportTypeError::TransportProtocolVersionMismatch {
                actual: self.transport_protocol_version,
                expected: MQTT_TRANSPORT_PROTOCOL_VERSION,
            });
        }
        if self.offer_secret.trim().is_empty() {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "managed pairing offer secret must not be empty".to_owned(),
            });
        }
        if self.host_label.trim().is_empty() {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "managed pairing host label must not be empty".to_owned(),
            });
        }
        if self.expires_at_ms == 0 {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "managed pairing expiry must not be zero".to_owned(),
            });
        }
        validate_broker_url(&self.broker.endpoint)?;
        let parsed = url::Url::parse(self.broker.endpoint.as_str()).map_err(|err| {
            TransportTypeError::InvalidBrokerUrl {
                message: format!(
                    "managed broker URL {:?} is invalid: {err}",
                    self.broker.endpoint.as_str()
                ),
            }
        })?;
        if parsed.scheme() != "wss" {
            return Err(TransportTypeError::InvalidBrokerUrl {
                message: format!(
                    "managed pairing broker URL scheme {:?} is unsupported; expected wss://",
                    parsed.scheme()
                ),
            });
        }
        Ok(())
    }

    pub fn to_uri(&self) -> Result<String, TransportTypeError> {
        self.validate()?;
        let cbor = self.encode_cbor()?;
        let encoded = URL_SAFE_NO_PAD.encode(cbor);
        Ok(format!("{MANAGED_PAIRING_URI_PREFIX}{encoded}"))
    }

    pub fn from_uri(uri: &str) -> Result<Self, TransportTypeError> {
        let encoded = uri
            .strip_prefix(MANAGED_PAIRING_URI_PREFIX)
            .ok_or_else(|| TransportTypeError::InvalidPairingUri {
                message: format!("URI must start with {MANAGED_PAIRING_URI_PREFIX}"),
            })?;
        if encoded.is_empty() {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "URI payload must not be empty".to_owned(),
            });
        }
        let cbor =
            URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|err| TransportTypeError::InvalidBase64 {
                    type_name: "ManagedMobilePairingQrPayload URI payload",
                    message: err.to_string(),
                })?;
        Self::decode_cbor(&cbor)
    }

    pub fn to_pairing_url(&self) -> Result<String, TransportTypeError> {
        Ok(format!("{MOBILE_PAIRING_WEB_BASE_URL}#{}", self.to_uri()?))
    }

    pub fn from_any(input: &str) -> Result<Self, TransportTypeError> {
        let trimmed = input.trim();
        if trimmed.starts_with(MANAGED_PAIRING_URI_PREFIX) {
            return Self::from_uri(trimmed);
        }
        if let Some((_, fragment)) = trimmed.split_once('#')
            && fragment.starts_with(MANAGED_PAIRING_URI_PREFIX)
        {
            return Self::from_uri(fragment);
        }
        Self::from_uri(trimmed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobilePairingQrOffer {
    ManagedService(ManagedMobilePairingQrPayload),
    LegacyPublicBrokerRepairRequired(MobilePairingQrPayload),
}

impl MobilePairingQrOffer {
    /// Supported QR scan entry point. Managed service offers are returned as
    /// connectable. Legacy v1 public-broker payloads remain parseable only so
    /// callers can surface an explicit repair/re-pair flow.
    pub fn from_uri(uri: &str) -> Result<Self, TransportTypeError> {
        if uri.starts_with(MANAGED_PAIRING_URI_PREFIX) {
            return ManagedMobilePairingQrPayload::from_uri(uri).map(Self::ManagedService);
        }
        if uri.starts_with(LEGACY_PAIRING_URI_PREFIX) {
            return MobilePairingQrPayload::from_uri(uri)
                .map(Self::LegacyPublicBrokerRepairRequired);
        }
        Err(TransportTypeError::InvalidPairingUri {
            message: format!(
                "URI must start with {MANAGED_PAIRING_URI_PREFIX} or {LEGACY_PAIRING_URI_PREFIX}"
            ),
        })
    }

    pub fn from_any(input: &str) -> Result<Self, TransportTypeError> {
        let trimmed = input.trim();
        if trimmed.starts_with(MANAGED_PAIRING_URI_PREFIX)
            || trimmed.starts_with(LEGACY_PAIRING_URI_PREFIX)
        {
            return Self::from_uri(trimmed);
        }
        if let Some((_, fragment)) = trimmed.split_once('#')
            && (fragment.starts_with(MANAGED_PAIRING_URI_PREFIX)
                || fragment.starts_with(LEGACY_PAIRING_URI_PREFIX))
        {
            return Self::from_uri(fragment);
        }
        Self::from_uri(trimmed)
    }
}

pub fn parse_mobile_pairing_qr_offer(
    input: &str,
) -> Result<MobilePairingQrOffer, TransportTypeError> {
    MobilePairingQrOffer::from_any(input)
}

pub fn validate_broker_url(broker_url: &BrokerUrl) -> Result<(), TransportTypeError> {
    let parsed = url::Url::parse(broker_url.as_str()).map_err(|err| {
        TransportTypeError::InvalidBrokerUrl {
            message: format!("broker URL {:?} is invalid: {err}", broker_url.as_str()),
        }
    })?;
    if parsed.host_str().is_none() {
        return Err(TransportTypeError::InvalidBrokerUrl {
            message: "broker URL is missing a host".to_owned(),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(TransportTypeError::InvalidBrokerUrl {
            message: "broker credentials must be supplied out-of-band, not embedded in the URL"
                .to_owned(),
        });
    }
    if parsed.fragment().is_some() {
        return Err(TransportTypeError::InvalidBrokerUrl {
            message: "broker URL fragments are not valid MQTT transport configuration".to_owned(),
        });
    }
    match parsed.scheme() {
        "mqtts" if parsed.path() == "/" || parsed.path().is_empty() => Ok(()),
        "mqtts" => Err(TransportTypeError::InvalidBrokerUrl {
            message: "mqtts:// broker URLs must not include a path".to_owned(),
        }),
        "wss" => Ok(()),
        "mqtt" | "tcp" if loopback_plaintext_allowed(&parsed) => Ok(()),
        "mqtt" | "tcp" | "ws" => Err(TransportTypeError::InvalidBrokerUrl {
            message: format!(
                "broker URL scheme {:?} is insecure; only mqtts:// and wss:// are allowed",
                parsed.scheme()
            ),
        }),
        scheme => Err(TransportTypeError::InvalidBrokerUrl {
            message: format!(
                "broker URL scheme {scheme:?} is unsupported; expected mqtts:// or wss://"
            ),
        }),
    }
}

fn loopback_plaintext_allowed(parsed: &url::Url) -> bool {
    #[cfg(feature = "test-support")]
    {
        parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost") || {
                host.parse::<IpAddr>()
                    .map(|addr| addr.is_loopback())
                    .unwrap_or(false)
            }
        })
    }
    #[cfg(not(feature = "test-support"))]
    {
        let _ = parsed;
        false
    }
}

fn encode_cbor<T: Serialize>(
    type_name: &'static str,
    value: &T,
) -> Result<Vec<u8>, TransportTypeError> {
    let mut encoded = Vec::new();
    ciborium::into_writer(value, &mut encoded).map_err(|err| TransportTypeError::CborEncode {
        type_name,
        message: err.to_string(),
    })?;
    Ok(encoded)
}

fn decode_cbor<T: for<'de> Deserialize<'de>>(
    type_name: &'static str,
    bytes: &[u8],
) -> Result<T, TransportTypeError> {
    let mut cursor = Cursor::new(bytes);
    let value =
        ciborium::from_reader(&mut cursor).map_err(|err| TransportTypeError::CborDecode {
            type_name,
            message: err.to_string(),
        })?;
    if cursor.position() != bytes.len() as u64 {
        return Err(TransportTypeError::CborDecode {
            type_name,
            message: "trailing bytes after CBOR payload".to_owned(),
        });
    }
    Ok(value)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoomId(pub [u8; ROOM_ID_LEN]);

impl RoomId {
    pub fn random() -> Self {
        let mut bytes = [0_u8; ROOM_ID_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_base64url_no_pad(value: &str) -> Result<Self, TransportTypeError> {
        let bytes =
            URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|err| TransportTypeError::InvalidBase64 {
                    type_name: "RoomId",
                    message: err.to_string(),
                })?;
        let actual = bytes.len();
        let bytes: [u8; ROOM_ID_LEN] =
            bytes
                .try_into()
                .map_err(|_| TransportTypeError::InvalidLength {
                    type_name: "RoomId",
                    expected: ROOM_ID_LEN,
                    actual,
                })?;
        Ok(Self(bytes))
    }

    pub fn as_base64url_no_pad(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; ROOM_ID_LEN] {
        &self.0
    }
}

impl fmt::Debug for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RoomId")
            .field(&self.as_base64url_no_pad())
            .finish()
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_base64url_no_pad())
    }
}

impl Serialize for RoomId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_base64url_no_pad())
    }
}

impl<'de> Deserialize<'de> for RoomId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RoomIdVisitor;

        impl Visitor<'_> for RoomIdVisitor {
            type Value = RoomId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 16-byte RoomId encoded as base64url without padding")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                RoomId::from_base64url_no_pad(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(RoomIdVisitor)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreSharedKey(pub [u8; PRE_SHARED_KEY_LEN]);

impl PreSharedKey {
    pub fn random() -> Self {
        let mut bytes = [0_u8; PRE_SHARED_KEY_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_base64url_no_pad(value: &str) -> Result<Self, TransportTypeError> {
        let bytes =
            URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|err| TransportTypeError::InvalidBase64 {
                    type_name: "PreSharedKey",
                    message: err.to_string(),
                })?;
        Self::from_slice(&bytes)
    }

    pub fn from_slice(value: &[u8]) -> Result<Self, TransportTypeError> {
        let actual = value.len();
        let bytes: [u8; PRE_SHARED_KEY_LEN] =
            value
                .try_into()
                .map_err(|_| TransportTypeError::InvalidLength {
                    type_name: "PreSharedKey",
                    expected: PRE_SHARED_KEY_LEN,
                    actual,
                })?;
        Ok(Self(bytes))
    }

    pub fn as_base64url_no_pad(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; PRE_SHARED_KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for PreSharedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreSharedKey(<redacted>)")
    }
}

impl Serialize for PreSharedKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.as_base64url_no_pad())
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for PreSharedKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            deserializer.deserialize_str(PreSharedKeyStringVisitor)
        } else {
            deserializer.deserialize_bytes(PreSharedKeyBytesVisitor)
        }
    }
}

struct PreSharedKeyStringVisitor;

impl Visitor<'_> for PreSharedKeyStringVisitor {
    type Value = PreSharedKey;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a 32-byte PreSharedKey encoded as base64url without padding")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        PreSharedKey::from_base64url_no_pad(value).map_err(E::custom)
    }
}

struct PreSharedKeyBytesVisitor;

impl<'de> Visitor<'de> for PreSharedKeyBytesVisitor {
    type Value = PreSharedKey;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("exactly 32 raw PreSharedKey bytes")
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        PreSharedKey::from_slice(value).map_err(E::custom)
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        PreSharedKey::from_slice(&value).map_err(E::custom)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes = Vec::with_capacity(PRE_SHARED_KEY_LEN);
        while let Some(byte) = seq.next_element::<u8>()? {
            bytes.push(byte);
        }
        PreSharedKey::from_slice(&bytes).map_err(A::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::PROTOCOL_VERSION;

    #[test]
    fn default_broker_endpoint_is_emqx_wss() {
        let endpoint = default_mobile_broker_endpoint();
        assert_eq!(endpoint.url.as_str(), DEFAULT_MOBILE_MQTT_BROKER_URL);
        assert_eq!(endpoint.url.as_str(), "wss://broker.emqx.io:8084/mqtt");
        assert_ne!(endpoint.url.as_str(), "mqtts://broker.emqx.io:8883");
        assert_eq!(endpoint.auth, BrokerAuth::Anonymous);
        validate_broker_url(&endpoint.url).expect("default broker URL is valid");
    }

    #[test]
    fn pairing_qr_round_trips_mqtt_endpoint_policy_room_and_psk() {
        let endpoint = default_mobile_broker_endpoint();
        let room = RoomId([7_u8; ROOM_ID_LEN]);
        let psk = PreSharedKey::from_slice(&[9_u8; PRE_SHARED_KEY_LEN]).expect("psk");
        let payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            endpoint.clone(),
            room,
            psk.clone(),
            "Tyde Host".to_owned(),
        );

        let uri = payload.to_uri().expect("encode QR URI");
        let decoded = MobilePairingQrPayload::from_uri(&uri).expect("decode QR URI");

        assert_eq!(decoded.broker, endpoint);
        assert_eq!(decoded.policy, MqttTransportPolicy::default());
        assert_eq!(decoded.room, room);
        assert_eq!(decoded.psk, psk);
    }

    #[test]
    fn pairing_qr_decodes_legacy_cbor_without_release_version() {
        // A legacy host serializes a payload that has no `release_version` field
        // at all; decoding must succeed with `release_version == None`.
        #[derive(serde::Serialize)]
        struct LegacyQr {
            v: u32,
            protocol_version: u32,
            tyde_version: protocol::Version,
            broker: BrokerEndpoint,
            policy: MqttTransportPolicy,
            room: RoomId,
            psk: PreSharedKey,
            host_label: String,
        }

        let endpoint = default_mobile_broker_endpoint();
        let legacy = LegacyQr {
            v: MOBILE_QR_VERSION,
            protocol_version: PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            broker: endpoint.clone(),
            policy: MqttTransportPolicy::default(),
            room: RoomId([3_u8; ROOM_ID_LEN]),
            psk: PreSharedKey::from_slice(&[4_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            host_label: "Legacy Host".to_owned(),
        };

        let mut bytes = Vec::new();
        ciborium::into_writer(&legacy, &mut bytes).expect("encode legacy CBOR");
        let decoded = MobilePairingQrPayload::decode_cbor(&bytes).expect("decode legacy CBOR");
        assert_eq!(decoded.release_version, None);
        assert_eq!(decoded.broker, endpoint);
        assert_eq!(decoded.host_label, "Legacy Host");
    }

    #[test]
    fn pairing_qr_round_trips_some_release_version_and_omits_none() {
        let version = protocol::TydeReleaseVersion::parse("0.8.19-beta.2").expect("valid version");
        let mut payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([5_u8; ROOM_ID_LEN]),
            PreSharedKey::from_slice(&[6_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            "Tyde Host".to_owned(),
        );
        payload.release_version = Some(version.clone());

        let uri = payload.to_uri().expect("encode QR URI");
        let decoded = MobilePairingQrPayload::from_uri(&uri).expect("decode QR URI");
        assert_eq!(decoded.release_version, Some(version));

        // `new()` leaves release_version None; skip_serializing_if must omit the
        // key entirely so the bytes match a legacy producer's.
        let none = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([7_u8; ROOM_ID_LEN]),
            PreSharedKey::from_slice(&[8_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            "Tyde Host".to_owned(),
        );
        let bytes = none.encode_cbor().expect("encode none");
        let value: ciborium::value::Value =
            ciborium::from_reader(bytes.as_slice()).expect("decode to CBOR value");
        let ciborium::value::Value::Map(entries) = value else {
            panic!("expected CBOR map");
        };
        assert!(
            !entries.iter().any(|(key, _)| matches!(
                key,
                ciborium::value::Value::Text(name) if name == "release_version"
            )),
            "release_version must be omitted when None"
        );
    }

    #[test]
    fn pairing_url_round_trips_through_from_any() {
        let endpoint = default_mobile_broker_endpoint();
        let room = RoomId([11_u8; ROOM_ID_LEN]);
        let psk = PreSharedKey::from_slice(&[13_u8; PRE_SHARED_KEY_LEN]).expect("psk");
        let version = protocol::TydeReleaseVersion::parse("1.2.3-rc.1").expect("valid version");
        let mut payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            endpoint.clone(),
            room,
            psk.clone(),
            "Tyde Host".to_owned(),
        );
        payload.release_version = Some(version.clone());

        let url = payload.to_pairing_url().expect("encode pairing URL");
        let decoded = MobilePairingQrPayload::from_any(&url).expect("decode pairing URL");

        assert_eq!(decoded.broker, endpoint);
        assert_eq!(decoded.room, room);
        assert_eq!(decoded.psk, psk);
        assert_eq!(decoded.release_version, Some(version));
    }

    #[test]
    fn pairing_url_keeps_psk_only_in_fragment() {
        // SECURITY: the PSK-bearing CBOR payload must ride in the URL FRAGMENT
        // (after `#`) so it is never sent to the S3/CloudFront origin. Assert
        // the part before `#` is EXACTLY the origin base, and that the base64
        // payload appears ONLY after the `#`.
        let psk = PreSharedKey::from_slice(&[42_u8; PRE_SHARED_KEY_LEN]).expect("psk");
        let payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([21_u8; ROOM_ID_LEN]),
            psk,
            "Tyde Host".to_owned(),
        );

        let url = payload.to_pairing_url().expect("encode pairing URL");
        let (before, fragment) = url.split_once('#').expect("URL must contain a fragment");

        assert_eq!(
            before, MOBILE_PAIRING_WEB_BASE_URL,
            "everything before `#` must be exactly the origin base URL"
        );

        let uri = payload.to_uri().expect("encode URI");
        let base64_payload = uri
            .strip_prefix(PAIRING_URI_PREFIX)
            .expect("URI has the pairing prefix");
        assert!(
            !base64_payload.is_empty(),
            "sanity: payload must be non-empty"
        );
        assert!(
            !before.contains(base64_payload),
            "base64 PSK payload must never appear before the `#`"
        );
        assert!(
            fragment.contains(base64_payload),
            "base64 PSK payload must appear in the fragment"
        );
        assert_eq!(
            fragment, uri,
            "the fragment must be exactly the raw tyde-pair:// URI"
        );
    }

    #[test]
    fn from_any_accepts_legacy_and_https_forms() {
        let payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([31_u8; ROOM_ID_LEN]),
            PreSharedKey::from_slice(&[33_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            "Tyde Host".to_owned(),
        );

        let legacy = payload.to_uri().expect("encode legacy URI");
        let https = payload.to_pairing_url().expect("encode https URL");

        let from_legacy = MobilePairingQrPayload::from_any(&legacy).expect("decode legacy form");
        let from_https = MobilePairingQrPayload::from_any(&https).expect("decode https form");
        assert_eq!(from_legacy, payload);
        assert_eq!(from_https, payload);

        // Junk surfaces the canonical "must start with" error.
        assert!(matches!(
            MobilePairingQrPayload::from_any("https://example.com/not-a-pair"),
            Err(TransportTypeError::InvalidPairingUri { .. })
        ));
    }

    fn managed_broker_endpoint() -> ManagedBrokerEndpoint {
        ManagedBrokerEndpoint {
            endpoint: BrokerUrl::new("wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt")
                .expect("broker url"),
            provider: protocol::ManagedBrokerProvider::AwsIotCore,
            region: protocol::ManagedBrokerRegion::new("us-west-2").expect("region"),
            authorizer_name: protocol::ManagedBrokerAuthorizerName::new("tycode-mobile-v1")
                .expect("authorizer"),
        }
    }

    #[test]
    fn managed_pairing_qr_round_trips_in_v2_fragment_without_debug_secret_leak() {
        let version = protocol::TydeReleaseVersion::parse("0.8.19").expect("release version");
        let room = RoomId([51_u8; ROOM_ID_LEN]);
        let psk = PreSharedKey::from_slice(&[53_u8; PRE_SHARED_KEY_LEN]).expect("psk");
        let encoded_psk = psk.as_base64url_no_pad();
        let payload = ManagedMobilePairingQrPayload::new_with_rendezvous(
            ManagedMobilePairingQrPayloadParams {
                protocol_version: PROTOCOL_VERSION,
                release_version: version.clone(),
                offer_id: MobilePairingOfferId::new("offer_01J").expect("offer id"),
                offer_secret: "offer_secret_from_qr".to_owned(),
                broker: managed_broker_endpoint(),
                room,
                psk: psk.clone(),
                host_label: "Tyde Host".to_owned(),
                expires_at_ms: 1_760_000_300_000,
            },
        );

        let url = payload.to_pairing_url().expect("managed pairing URL");
        let (before, fragment) = url.split_once('#').expect("URL fragment");
        assert_eq!(before, MOBILE_PAIRING_WEB_BASE_URL);
        assert!(fragment.starts_with(MANAGED_PAIRING_URI_PREFIX));
        let uri = payload.to_uri().expect("managed pairing URI");
        let encoded_payload = uri
            .strip_prefix(MANAGED_PAIRING_URI_PREFIX)
            .expect("managed URI prefix");
        assert!(
            !before.contains(encoded_payload),
            "managed QR payload must never appear before the URL fragment"
        );
        assert!(
            !before.contains("offer_secret_from_qr"),
            "offer secret must not appear before URL fragment"
        );
        assert!(
            !before.contains(&encoded_psk),
            "PSK must not appear before URL fragment"
        );
        assert_eq!(fragment, uri);

        let decoded = ManagedMobilePairingQrPayload::from_any(&url).expect("decode managed QR");
        assert_eq!(decoded.offer_secret, "offer_secret_from_qr");
        assert_eq!(decoded.release_version, version);
        assert_eq!(decoded.room, room);
        assert_eq!(decoded.psk, psk);

        let debug = format!("{payload:?}");
        assert!(
            !debug.contains("offer_secret_from_qr"),
            "debug output leaked offer secret: {debug}"
        );
        assert!(
            !debug.contains(&encoded_psk),
            "debug output leaked encoded PSK: {debug}"
        );
    }

    #[test]
    fn qr_offer_classifies_legacy_public_broker_as_repair_required() {
        let payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([41_u8; ROOM_ID_LEN]),
            PreSharedKey::from_slice(&[43_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            "Legacy Host".to_owned(),
        );
        let url = payload.to_pairing_url().expect("legacy pairing URL");

        match MobilePairingQrOffer::from_any(&url).expect("classify legacy QR") {
            MobilePairingQrOffer::LegacyPublicBrokerRepairRequired(decoded) => {
                assert_eq!(decoded.broker, default_mobile_broker_endpoint());
                assert_eq!(decoded.host_label, "Legacy Host");
            }
            MobilePairingQrOffer::ManagedService(_) => {
                panic!("legacy public broker QR must not be classified as managed")
            }
        }
    }

    #[test]
    fn supported_qr_entry_point_classifies_legacy_public_broker_as_repair_required() {
        let payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([44_u8; ROOM_ID_LEN]),
            PreSharedKey::from_slice(&[45_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            "Legacy Host".to_owned(),
        );
        let url = payload.to_pairing_url().expect("legacy pairing URL");

        assert!(matches!(
            parse_mobile_pairing_qr_offer(&url).expect("classify QR"),
            MobilePairingQrOffer::LegacyPublicBrokerRepairRequired(decoded)
                if is_legacy_public_broker_endpoint(&decoded.broker)
        ));
    }

    #[test]
    fn pairing_qr_version_mismatch_is_typed() {
        let mut payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([46_u8; ROOM_ID_LEN]),
            PreSharedKey::from_slice(&[47_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            "Legacy Host".to_owned(),
        );
        payload.v = MOBILE_QR_VERSION + 1;
        let bytes = payload.encode_cbor().expect("encode QR");

        assert!(matches!(
            MobilePairingQrPayload::decode_cbor(&bytes),
            Err(TransportTypeError::PairingQrVersionMismatch { actual, expected })
                if actual == MOBILE_QR_VERSION + 1 && expected == MOBILE_QR_VERSION
        ));
    }

    #[test]
    fn managed_pairing_qr_fails_closed_for_broker_secrets_in_url() {
        let mut payload = ManagedMobilePairingQrPayload::new_with_rendezvous(
            ManagedMobilePairingQrPayloadParams {
                protocol_version: PROTOCOL_VERSION,
                release_version: protocol::TydeReleaseVersion::parse("0.8.19")
                    .expect("release version"),
                offer_id: MobilePairingOfferId::new("offer_01J").expect("offer id"),
                offer_secret: "offer_secret_from_qr".to_owned(),
                broker: managed_broker_endpoint(),
                room: RoomId([61_u8; ROOM_ID_LEN]),
                psk: PreSharedKey::from_slice(&[63_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
                host_label: "Tyde Host".to_owned(),
                expires_at_ms: 1_760_000_300_000,
            },
        );
        payload.broker.endpoint =
            BrokerUrl::new("wss://user:password@example.com/mqtt").expect("broker url wrapper");

        let err = payload
            .to_uri()
            .expect_err("managed QR with URL credentials must fail closed");
        assert!(err.to_string().contains("credentials"));
    }

    #[test]
    fn managed_pairing_qr_requires_release_version() {
        #[derive(serde::Serialize)]
        struct ManagedQrWithoutReleaseVersion {
            v: u32,
            protocol_version: u32,
            transport_protocol_version: u32,
            tyde_version: protocol::Version,
            offer_id: MobilePairingOfferId,
            offer_secret: String,
            broker: ManagedBrokerEndpoint,
            room: RoomId,
            psk: PreSharedKey,
            host_label: String,
            expires_at_ms: u64,
        }

        let payload = ManagedQrWithoutReleaseVersion {
            v: MOBILE_MANAGED_QR_VERSION,
            protocol_version: PROTOCOL_VERSION,
            transport_protocol_version: MQTT_TRANSPORT_PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            offer_id: MobilePairingOfferId::new("offer_01J").expect("offer id"),
            offer_secret: "offer_secret_from_qr".to_owned(),
            broker: managed_broker_endpoint(),
            room: RoomId([65_u8; ROOM_ID_LEN]),
            psk: PreSharedKey::from_slice(&[67_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            host_label: "Tyde Host".to_owned(),
            expires_at_ms: 1_760_000_300_000,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&payload, &mut bytes).expect("encode QR");

        assert!(matches!(
            ManagedMobilePairingQrPayload::decode_cbor(&bytes),
            Err(TransportTypeError::CborDecode { .. })
        ));
    }

    #[test]
    fn managed_pairing_qr_requires_room_and_psk() {
        #[derive(serde::Serialize)]
        struct ManagedQrWithoutRendezvous {
            v: u32,
            protocol_version: u32,
            transport_protocol_version: u32,
            tyde_version: protocol::Version,
            release_version: protocol::TydeReleaseVersion,
            offer_id: MobilePairingOfferId,
            offer_secret: String,
            broker: ManagedBrokerEndpoint,
            host_label: String,
            expires_at_ms: u64,
        }

        let payload = ManagedQrWithoutRendezvous {
            v: MOBILE_MANAGED_QR_VERSION,
            protocol_version: PROTOCOL_VERSION,
            transport_protocol_version: MQTT_TRANSPORT_PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            release_version: protocol::TydeReleaseVersion::parse("0.8.19")
                .expect("release version"),
            offer_id: MobilePairingOfferId::new("offer_01J").expect("offer id"),
            offer_secret: "offer_secret_from_qr".to_owned(),
            broker: managed_broker_endpoint(),
            host_label: "Tyde Host".to_owned(),
            expires_at_ms: 1_760_000_300_000,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&payload, &mut bytes).expect("encode QR");

        assert!(matches!(
            ManagedMobilePairingQrPayload::decode_cbor(&bytes),
            Err(TransportTypeError::CborDecode { .. })
        ));
    }

    #[test]
    fn managed_pairing_qr_rejects_unsupported_transport_version() {
        let mut payload = ManagedMobilePairingQrPayload::new_with_rendezvous(
            ManagedMobilePairingQrPayloadParams {
                protocol_version: PROTOCOL_VERSION,
                release_version: protocol::TydeReleaseVersion::parse("0.8.19")
                    .expect("release version"),
                offer_id: MobilePairingOfferId::new("offer_01J").expect("offer id"),
                offer_secret: "offer_secret_from_qr".to_owned(),
                broker: managed_broker_endpoint(),
                room: RoomId([71_u8; ROOM_ID_LEN]),
                psk: PreSharedKey::from_slice(&[73_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
                host_label: "Tyde Host".to_owned(),
                expires_at_ms: 1_760_000_300_000,
            },
        );
        payload.transport_protocol_version = MQTT_TRANSPORT_PROTOCOL_VERSION + 1;

        assert!(matches!(
            payload.to_uri(),
            Err(TransportTypeError::TransportProtocolVersionMismatch {
                actual,
                expected
            }) if actual == MQTT_TRANSPORT_PROTOCOL_VERSION + 1
                && expected == MQTT_TRANSPORT_PROTOCOL_VERSION
        ));
    }

    #[test]
    fn managed_pairing_qr_rejects_empty_offer_id_from_cbor() {
        #[derive(serde::Serialize)]
        struct ManagedQrWithEmptyOfferId {
            v: u32,
            protocol_version: u32,
            transport_protocol_version: u32,
            tyde_version: protocol::Version,
            release_version: protocol::TydeReleaseVersion,
            offer_id: String,
            offer_secret: String,
            broker: ManagedBrokerEndpoint,
            room: RoomId,
            psk: PreSharedKey,
            host_label: String,
            expires_at_ms: u64,
        }

        let payload = ManagedQrWithEmptyOfferId {
            v: MOBILE_MANAGED_QR_VERSION,
            protocol_version: PROTOCOL_VERSION,
            transport_protocol_version: MQTT_TRANSPORT_PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            release_version: protocol::TydeReleaseVersion::parse("0.8.19")
                .expect("release version"),
            offer_id: String::new(),
            offer_secret: "offer_secret_from_qr".to_owned(),
            broker: managed_broker_endpoint(),
            room: RoomId([75_u8; ROOM_ID_LEN]),
            psk: PreSharedKey::from_slice(&[77_u8; PRE_SHARED_KEY_LEN]).expect("psk"),
            host_label: "Tyde Host".to_owned(),
            expires_at_ms: 1_760_000_300_000,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&payload, &mut bytes).expect("encode QR");

        assert!(matches!(
            ManagedMobilePairingQrPayload::decode_cbor(&bytes),
            Err(TransportTypeError::CborDecode { .. })
        ));
    }

    #[test]
    fn pre_shared_key_debug_redacts_secret_material() {
        let psk = PreSharedKey::from_slice(&[9_u8; PRE_SHARED_KEY_LEN]).expect("psk");
        let encoded = psk.as_base64url_no_pad();
        let debug = format!("{psk:?}");

        assert!(debug.contains("<redacted>"));
        assert!(
            !debug.contains(&encoded),
            "debug output leaked encoded PSK: {debug}"
        );
    }

    #[test]
    fn broker_url_validation_rejects_plaintext_public_schemes() {
        for url in [
            "mqtt://broker.example.test:1883",
            "ws://broker.example.test/mqtt",
        ] {
            let url = BrokerUrl::new(url).expect("broker url");
            let err = validate_broker_url(&url).expect_err("plaintext URL should fail");
            assert!(err.to_string().contains("insecure"));
        }
    }

    #[test]
    fn broker_url_validation_rejects_mqtts_path_and_embedded_credentials() {
        for (url, expected) in [
            (
                "mqtts://broker.example.test/relay",
                "must not include a path",
            ),
            (
                "mqtts://user:password@broker.example.test:8883",
                "credentials",
            ),
        ] {
            let url = BrokerUrl::new(url).expect("broker url");
            let err = validate_broker_url(&url).expect_err("invalid URL should fail");
            assert!(
                err.to_string().contains(expected),
                "expected {expected:?} in {err}"
            );
        }
    }
}
