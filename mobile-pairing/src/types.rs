use std::fmt;
use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use protocol::{MobilePairingOfferId, TYDE_VERSION};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

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

    #[error("unsupported mobile transport protocol version {actual}; expected {expected}")]
    TransportProtocolVersionMismatch { actual: u32, expected: u32 },

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

pub const MOBILE_MANAGED_QR_VERSION: u32 = 3;
pub const MOBILE_DIRECT_QR_VERSION: u32 = 4;
const LEGACY_PAIRING_URI_PREFIX: &str = "tyde-pair://v1?";
const DIRECT_PAIRING_URI_PREFIX: &str = "tyde-pair://v3?";
const MANAGED_PAIRING_URI_PREFIX: &str = "tyde-pair://v2?";
/// Origin-root web loader that turns the host's pairing QR into a generic
/// HTTPS link the native iOS/Android Camera can open. The PSK-bearing
/// `tyde-pair://…` URI rides in the URL FRAGMENT (after `#`) so it is never
/// sent to the S3/CloudFront origin; the loader clears the fragment on read.
pub const MOBILE_PAIRING_WEB_BASE_URL: &str = "https://tycode.dev/tyde/";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedMobilePairingQrPayload {
    pub v: u32,
    pub protocol_version: u32,
    pub transport_protocol_version: u32,
    pub tyde_version: protocol::Version,
    pub release_version: protocol::TydeReleaseVersion,
    pub offer_id: MobilePairingOfferId,
    pub offer_secret: String,
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
        host_label: String,
        expires_at_ms: u64,
    ) -> Self {
        Self::new_with_key(ManagedMobilePairingQrPayloadParams {
            protocol_version,
            release_version,
            offer_id,
            offer_secret,
            psk: PreSharedKey::random(),
            host_label,
            expires_at_ms,
        })
    }

    pub fn new_with_key(params: ManagedMobilePairingQrPayloadParams) -> Self {
        Self {
            v: MOBILE_MANAGED_QR_VERSION,
            protocol_version: params.protocol_version,
            transport_protocol_version: protocol::MOBILE_RTC_PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            release_version: params.release_version,
            offer_id: params.offer_id,
            offer_secret: params.offer_secret,
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
        if self.transport_protocol_version != protocol::MOBILE_RTC_PROTOCOL_VERSION {
            return Err(TransportTypeError::TransportProtocolVersionMismatch {
                actual: self.transport_protocol_version,
                expected: protocol::MOBILE_RTC_PROTOCOL_VERSION,
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

/// A pairing offer for a host that serves the mobile web app itself.
///
/// Unlike the managed and legacy payloads this carries no transport
/// coordinates and, deliberately, no origin. By the time a phone parses this it
/// has already been loaded from the host's own origin, so `location.origin` is
/// the authoritative address; an origin named inside the payload would just be
/// a redirect target the host cannot vouch for.
///
/// The secret is single use and short lived. It buys exactly one exchange at
/// the host for a durable device token, so a photographed QR stops being a
/// credential the moment it is redeemed or expires.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectMobilePairingQrPayload {
    pub v: u32,
    pub protocol_version: u32,
    pub tyde_version: protocol::Version,
    pub release_version: protocol::TydeReleaseVersion,
    pub offer_id: MobilePairingOfferId,
    pub offer_secret: String,
    pub host_label: String,
    pub expires_at_ms: u64,
}

impl fmt::Debug for DirectMobilePairingQrPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectMobilePairingQrPayload")
            .field("v", &self.v)
            .field("protocol_version", &self.protocol_version)
            .field("tyde_version", &self.tyde_version)
            .field("release_version", &self.release_version)
            .field("offer_id", &self.offer_id)
            .field("offer_secret", &"<redacted>")
            .field("host_label", &self.host_label)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl DirectMobilePairingQrPayload {
    pub fn new(
        protocol_version: u32,
        release_version: protocol::TydeReleaseVersion,
        offer_id: MobilePairingOfferId,
        offer_secret: String,
        host_label: String,
        expires_at_ms: u64,
    ) -> Self {
        Self {
            v: MOBILE_DIRECT_QR_VERSION,
            protocol_version,
            tyde_version: TYDE_VERSION,
            release_version,
            offer_id,
            offer_secret,
            host_label,
            expires_at_ms,
        }
    }

    pub fn encode_cbor(&self) -> Result<Vec<u8>, TransportTypeError> {
        encode_cbor("DirectMobilePairingQrPayload", self)
    }

    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, TransportTypeError> {
        let payload: Self = decode_cbor("DirectMobilePairingQrPayload", bytes)?;
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), TransportTypeError> {
        if self.v != MOBILE_DIRECT_QR_VERSION {
            return Err(TransportTypeError::PairingQrVersionMismatch {
                actual: self.v,
                expected: MOBILE_DIRECT_QR_VERSION,
            });
        }
        if self.offer_secret.trim().is_empty() {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "direct pairing offer secret must not be empty".to_owned(),
            });
        }
        if self.host_label.trim().is_empty() {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "direct pairing host label must not be empty".to_owned(),
            });
        }
        if self.expires_at_ms == 0 {
            return Err(TransportTypeError::InvalidPairingUri {
                message: "direct pairing expiry must not be zero".to_owned(),
            });
        }
        Ok(())
    }

    pub fn to_uri(&self) -> Result<String, TransportTypeError> {
        self.validate()?;
        let cbor = self.encode_cbor()?;
        let encoded = URL_SAFE_NO_PAD.encode(cbor);
        Ok(format!("{DIRECT_PAIRING_URI_PREFIX}{encoded}"))
    }

    pub fn from_uri(uri: &str) -> Result<Self, TransportTypeError> {
        let encoded = uri.strip_prefix(DIRECT_PAIRING_URI_PREFIX).ok_or_else(|| {
            TransportTypeError::InvalidPairingUri {
                message: format!("URI must start with {DIRECT_PAIRING_URI_PREFIX}"),
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
                    type_name: "DirectMobilePairingQrPayload URI payload",
                    message: err.to_string(),
                })?;
        Self::decode_cbor(&cbor)
    }

    /// Builds the HTTPS link encoded into the host's QR, pointing at the
    /// operator-declared origin rather than `tycode.dev`. The secret rides in
    /// the FRAGMENT, which browsers never send to the server, so it stays out
    /// of the reverse proxy's access log on the way in.
    pub fn to_pairing_url(&self, origin: &str) -> Result<String, TransportTypeError> {
        let origin = validate_direct_origin(origin)?;
        Ok(format!("{origin}/tyde/#{}", self.to_uri()?))
    }
}

/// Checks an operator-configured public origin. The host sits behind a proxy
/// and cannot discover its own external name, so this value is declared rather
/// than detected; a wrong one produces QR codes that pair to nothing, which is
/// worth catching where it is typed.
pub fn validate_direct_origin(origin: &str) -> Result<String, TransportTypeError> {
    let trimmed = origin.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(TransportTypeError::InvalidPairingUri {
            message: "direct hosting public origin must not be empty".to_owned(),
        });
    }
    let parsed = url::Url::parse(trimmed).map_err(|err| TransportTypeError::InvalidPairingUri {
        message: format!("direct hosting public origin {trimmed:?} is invalid: {err}"),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(TransportTypeError::InvalidPairingUri {
            message: format!(
                "direct hosting public origin scheme {:?} is unsupported; expected http:// or https://",
                parsed.scheme()
            ),
        });
    }
    if parsed.host_str().is_none() {
        return Err(TransportTypeError::InvalidPairingUri {
            message: "direct hosting public origin is missing a host".to_owned(),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(TransportTypeError::InvalidPairingUri {
            message: "direct hosting public origin must not embed credentials".to_owned(),
        });
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(TransportTypeError::InvalidPairingUri {
            message: "direct hosting public origin must not carry a query or fragment".to_owned(),
        });
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(TransportTypeError::InvalidPairingUri {
            message: format!(
                "direct hosting public origin must be a bare origin, got path {:?}",
                parsed.path()
            ),
        });
    }
    Ok(trimmed.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobilePairingQrOffer {
    ManagedService(ManagedMobilePairingQrPayload),
    Direct(DirectMobilePairingQrPayload),
    LegacyRepairRequired,
}

impl MobilePairingQrOffer {
    /// Supported QR scan entry point. Managed service offers are returned as
    /// connectable. Legacy v1 payloads are recognized only so
    /// callers can surface an explicit repair/re-pair flow.
    pub fn from_uri(uri: &str) -> Result<Self, TransportTypeError> {
        if uri.starts_with(MANAGED_PAIRING_URI_PREFIX) {
            return ManagedMobilePairingQrPayload::from_uri(uri).map(Self::ManagedService);
        }
        if uri.starts_with(DIRECT_PAIRING_URI_PREFIX) {
            return DirectMobilePairingQrPayload::from_uri(uri).map(Self::Direct);
        }
        if uri.starts_with(LEGACY_PAIRING_URI_PREFIX) {
            return Ok(Self::LegacyRepairRequired);
        }
        Err(TransportTypeError::InvalidPairingUri {
            message: format!(
                "URI must start with {MANAGED_PAIRING_URI_PREFIX}, {DIRECT_PAIRING_URI_PREFIX} or {LEGACY_PAIRING_URI_PREFIX}"
            ),
        })
    }

    pub fn from_any(input: &str) -> Result<Self, TransportTypeError> {
        let trimmed = input.trim();
        if is_pairing_uri(trimmed) {
            return Self::from_uri(trimmed);
        }
        if let Some((_, fragment)) = trimmed.split_once('#')
            && is_pairing_uri(fragment)
        {
            return Self::from_uri(fragment);
        }
        Self::from_uri(trimmed)
    }
}

fn is_pairing_uri(value: &str) -> bool {
    value.starts_with(MANAGED_PAIRING_URI_PREFIX)
        || value.starts_with(DIRECT_PAIRING_URI_PREFIX)
        || value.starts_with(LEGACY_PAIRING_URI_PREFIX)
}

/// Reads the stable version header before version-specific offer fields.
/// This does not validate an offer or authorize redemption.
pub fn mobile_pairing_qr_protocol_version(input: &str) -> Result<Option<u32>, TransportTypeError> {
    let trimmed = input.trim();
    let uri = if is_pairing_uri(trimmed) {
        trimmed
    } else {
        trimmed
            .split_once('#')
            .map_or(trimmed, |(_, fragment)| fragment)
    };
    let Some(encoded) = uri
        .strip_prefix(MANAGED_PAIRING_URI_PREFIX)
        .or_else(|| uri.strip_prefix(DIRECT_PAIRING_URI_PREFIX))
    else {
        return Ok(None);
    };
    let bytes =
        URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|error| TransportTypeError::InvalidBase64 {
                type_name: "mobile pairing QR header",
                message: error.to_string(),
            })?;
    #[derive(Deserialize)]
    struct Header {
        protocol_version: u32,
    }
    let header: Header = decode_cbor("mobile pairing QR header", &bytes)?;
    Ok(Some(header.protocol_version))
}

pub fn parse_mobile_pairing_qr_offer(
    input: &str,
) -> Result<MobilePairingQrOffer, TransportTypeError> {
    MobilePairingQrOffer::from_any(input)
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
