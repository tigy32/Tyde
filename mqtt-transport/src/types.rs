use std::fmt;
use std::io::Cursor;
#[cfg(feature = "test-support")]
use std::net::IpAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
pub use protocol::DEFAULT_MOBILE_MQTT_BROKER_URL;
use protocol::{BrokerUrl, TYDE_VERSION};
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

pub const MQTT_TRANSPORT_PROTOCOL_VERSION: u32 = 2;
pub const MOBILE_QR_VERSION: u32 = 2;
pub const MQTT_VERSION: u8 = 5;
pub const MQTT_QOS_AT_LEAST_ONCE: u8 = 1;
pub const MQTT_RETAIN: bool = false;
pub const MQTT_CLEAN_START: bool = true;
const PAIRING_URI_PREFIX: &str = "tyde-pair://v1?";

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
        }
    }

    pub fn encode_cbor(&self) -> Result<Vec<u8>, TransportTypeError> {
        encode_cbor("MobilePairingQrPayload", self)
    }

    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, TransportTypeError> {
        let payload: Self = decode_cbor("MobilePairingQrPayload", bytes)?;
        if payload.v != MOBILE_QR_VERSION {
            return Err(TransportTypeError::InvalidPairingUri {
                message: format!(
                    "unsupported mobile pairing QR version {}, expected {}",
                    payload.v, MOBILE_QR_VERSION
                ),
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
    fn default_broker_endpoint_is_emqx_mqtts() {
        let endpoint = default_mobile_broker_endpoint();
        assert_eq!(endpoint.url.as_str(), DEFAULT_MOBILE_MQTT_BROKER_URL);
        assert_eq!(endpoint.url.as_str(), "mqtts://broker.emqx.io:8883");
        assert_ne!(endpoint.url.as_str(), "wss://broker.tyde.dev/relay");
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
