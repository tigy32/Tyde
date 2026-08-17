use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use protocol::{
    BrokerUrl, ManagedBrokerClientId, ManagedBrokerCredentials, ManagedBrokerEndpoint,
    ManagedBrokerRole, ManagedBrokerTopicNamespace,
};

use crate::error::MqttTransportError;
use crate::framing::{
    DIRECTION_CLIENT_TO_HOST, DIRECTION_CREDIT_CLIENT_TO_HOST, DIRECTION_CREDIT_HOST_TO_CLIENT,
    DIRECTION_HOST_TO_CLIENT,
};
use crate::topic::{
    client_to_host_topic, host_to_client_topic, managed_client_to_host_topic,
    managed_host_to_client_topic, managed_topic_for_direction, validate_managed_topic_namespace,
};
use crate::types::{BrokerAuth, BrokerEndpoint, PreSharedKey, RoomId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MqttConnectConfig {
    pub endpoint: BrokerEndpoint,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub role: ParticipantRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedMqttConnectConfig {
    pub broker: ManagedBrokerEndpoint,
    pub credentials: ManagedBrokerCredentials,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub role: ParticipantRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectionPlan {
    pub(crate) config: MqttConnectConfig,
    pub(crate) broker: LinkBrokerConfig,
    pub(crate) topics: TopicScheme,
    pub(crate) session_expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkBrokerConfig {
    pub(crate) url: BrokerUrl,
    pub(crate) auth: LinkBrokerAuth,
    pub(crate) client_id: LinkClientId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkBrokerAuth {
    Legacy(BrokerAuth),
    Managed(ManagedWssConnectStrategy),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ManagedWssConnectStrategy {
    upgrade_auth: ManagedWssUpgradeAuth,
}

#[derive(Clone, PartialEq, Eq)]
enum ManagedWssUpgradeAuth {
    ServiceIssuedUrl(BrokerUrl),
}

impl fmt::Debug for ManagedWssConnectStrategy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedWssConnectStrategy")
            .field("upgrade_auth", &"service-issued URL query")
            .finish()
    }
}

impl ManagedWssConnectStrategy {
    pub(crate) fn http_upgrade_url(&self) -> &BrokerUrl {
        match &self.upgrade_auth {
            ManagedWssUpgradeAuth::ServiceIssuedUrl(url) => url,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkClientId {
    Random(ParticipantRole),
    Exact(ManagedBrokerClientId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TopicScheme {
    Legacy,
    Managed {
        namespace: ManagedBrokerTopicNamespace,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedConnectionMode {
    Direct,
    Ephemeral,
}

impl ConnectionPlan {
    pub(crate) fn legacy(config: MqttConnectConfig) -> Self {
        Self {
            broker: LinkBrokerConfig {
                url: config.endpoint.url.clone(),
                auth: LinkBrokerAuth::Legacy(config.endpoint.auth.clone()),
                client_id: LinkClientId::Random(config.role),
            },
            topics: TopicScheme::Legacy,
            session_expires_at_ms: None,
            config,
        }
    }

    pub(crate) fn managed(config: ManagedMqttConnectConfig) -> Result<Self, MqttTransportError> {
        Self::managed_for_mode(config, ManagedConnectionMode::Direct)
    }

    pub(crate) fn managed_ephemeral(
        config: ManagedMqttConnectConfig,
    ) -> Result<Self, MqttTransportError> {
        Self::managed_for_mode(config, ManagedConnectionMode::Ephemeral)
    }

    fn managed_for_mode(
        config: ManagedMqttConnectConfig,
        mode: ManagedConnectionMode,
    ) -> Result<Self, MqttTransportError> {
        let connect = validate_managed_config(&config, mode)?;
        let session_expires_at_ms = config.credentials.expires_at_ms;
        let endpoint = BrokerEndpoint {
            url: config.broker.endpoint.clone(),
            auth: BrokerAuth::Anonymous,
        };
        Ok(Self {
            config: MqttConnectConfig {
                endpoint,
                room: config.room,
                psk: config.psk,
                role: config.role,
            },
            broker: LinkBrokerConfig {
                url: config.broker.endpoint,
                auth: LinkBrokerAuth::Managed(connect),
                client_id: LinkClientId::Exact(config.credentials.client_id),
            },
            topics: TopicScheme::Managed {
                namespace: config.credentials.scope.namespace,
            },
            session_expires_at_ms: Some(session_expires_at_ms),
        })
    }

    pub(crate) fn session_renewal_after(&self, now_ms: u64) -> Option<Duration> {
        self.session_expires_at_ms
            .map(|expires_at_ms| managed_session_renewal_after(expires_at_ms, now_ms))
    }

    /// Exact service-issued client IDs are single-connection identities at AWS IoT.
    pub(crate) fn can_open_parallel_links(&self) -> bool {
        matches!(self.broker.client_id, LinkClientId::Random(_))
    }
}

/// Renew managed credentials this long before their nominal expiry. AWS IoT
/// re-invokes the custom authorizer roughly every 300 s with the original
/// CONNECT token, and the authorizer refuses grants close to expiry — so a
/// session must be handed fresh credentials well before `expires_at_ms`, not
/// 60 s before it (which in production scheduled renewal ~4 minutes after the
/// broker had already de-authorized the connection). With the standard 900 s
/// grant TTL this renews at ~540 s, ahead of the second authorizer refresh.
const MANAGED_SESSION_RENEWAL_MARGIN_MS: u64 = 360_000;

fn managed_session_renewal_after(expires_at_ms: u64, now_ms: u64) -> Duration {
    Duration::from_millis(
        expires_at_ms
            .saturating_sub(MANAGED_SESSION_RENEWAL_MARGIN_MS)
            .saturating_sub(now_ms),
    )
}

impl TopicScheme {
    pub(crate) fn inbound_topic(
        &self,
        role: ParticipantRole,
        room: &RoomId,
    ) -> Result<String, MqttTransportError> {
        match self {
            Self::Legacy => Ok(role.inbound_topic(room)),
            Self::Managed { namespace } => match role {
                ParticipantRole::Host => managed_client_to_host_topic(namespace, room),
                ParticipantRole::Client => managed_host_to_client_topic(namespace, room),
            },
        }
    }

    pub(crate) fn outbound_topic(
        &self,
        role: ParticipantRole,
        room: &RoomId,
    ) -> Result<String, MqttTransportError> {
        match self {
            Self::Legacy => Ok(role.outbound_topic(room)),
            Self::Managed { namespace } => match role {
                ParticipantRole::Host => managed_host_to_client_topic(namespace, room),
                ParticipantRole::Client => managed_client_to_host_topic(namespace, room),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantRole {
    Host,
    Client,
}

impl ParticipantRole {
    pub(crate) const fn client_id_prefix(self) -> &'static str {
        match self {
            Self::Host => "tyde-host",
            Self::Client => "tyde-mobile",
        }
    }

    pub(crate) const fn outbound_direction(self) -> u8 {
        match self {
            Self::Host => DIRECTION_HOST_TO_CLIENT,
            Self::Client => DIRECTION_CLIENT_TO_HOST,
        }
    }

    pub(crate) const fn inbound_direction(self) -> u8 {
        match self {
            Self::Host => DIRECTION_CLIENT_TO_HOST,
            Self::Client => DIRECTION_HOST_TO_CLIENT,
        }
    }

    pub(crate) const fn outbound_credit_direction(self) -> u8 {
        match self {
            Self::Host => DIRECTION_CREDIT_HOST_TO_CLIENT,
            Self::Client => DIRECTION_CREDIT_CLIENT_TO_HOST,
        }
    }

    pub(crate) const fn inbound_credit_direction(self) -> u8 {
        match self {
            Self::Host => DIRECTION_CREDIT_CLIENT_TO_HOST,
            Self::Client => DIRECTION_CREDIT_HOST_TO_CLIENT,
        }
    }

    pub(crate) fn inbound_topic(self, room: &RoomId) -> String {
        match self {
            Self::Host => client_to_host_topic(room),
            Self::Client => host_to_client_topic(room),
        }
    }

    pub(crate) fn outbound_topic(self, room: &RoomId) -> String {
        match self {
            Self::Host => host_to_client_topic(room),
            Self::Client => client_to_host_topic(room),
        }
    }

    pub(crate) const fn managed_broker_role(self) -> ManagedBrokerRole {
        match self {
            Self::Host => ManagedBrokerRole::Host,
            Self::Client => ManagedBrokerRole::Mobile,
        }
    }
}

fn validate_managed_config(
    config: &ManagedMqttConnectConfig,
    mode: ManagedConnectionMode,
) -> Result<ManagedWssConnectStrategy, MqttTransportError> {
    validate_managed_broker_endpoint(&config.broker)?;
    validate_managed_topic_namespace(&config.credentials.scope.namespace)?;
    if config.credentials.scope.role != config.role.managed_broker_role() {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker credential role {:?} does not match MQTT participant role {:?}",
                config.credentials.scope.role, config.role
            ),
        });
    }
    if config.credentials.issued_at_ms >= config.credentials.expires_at_ms {
        return Err(MqttTransportError::Configuration {
            message: "managed broker credentials must expire after their issue time".to_owned(),
        });
    }
    validate_managed_client_id(config)?;
    validate_expected_managed_filters(config, mode)?;
    validate_managed_connect_auth(&config.credentials.connect, &config.broker)
}

fn validate_expected_managed_filters(
    config: &ManagedMqttConnectConfig,
    mode: ManagedConnectionMode,
) -> Result<(), MqttTransportError> {
    let namespace = config.credentials.scope.namespace.as_str();
    let wildcard_host_to_client = format!("{namespace}/rooms/+/host-to-client");
    let wildcard_client_to_host = format!("{namespace}/rooms/+/client-to-host");
    let exact_host_to_client = managed_topic_for_direction(
        &config.credentials.scope.namespace,
        &config.room,
        crate::topic::TopicDirection::HostToClient,
    )?;
    let exact_client_to_host = managed_topic_for_direction(
        &config.credentials.scope.namespace,
        &config.room,
        crate::topic::TopicDirection::ClientToHost,
    )?;
    let (expected_publish, expected_subscribe) = match (config.role, mode) {
        (ParticipantRole::Host, ManagedConnectionMode::Direct) => (
            vec![wildcard_host_to_client, exact_host_to_client],
            vec![wildcard_client_to_host, exact_client_to_host],
        ),
        (ParticipantRole::Client, ManagedConnectionMode::Direct) => (
            vec![wildcard_client_to_host, exact_client_to_host],
            vec![wildcard_host_to_client, exact_host_to_client],
        ),
        (ParticipantRole::Host, ManagedConnectionMode::Ephemeral) => {
            (vec![wildcard_host_to_client], vec![wildcard_client_to_host])
        }
        (ParticipantRole::Client, ManagedConnectionMode::Ephemeral) => {
            (vec![wildcard_client_to_host], vec![wildcard_host_to_client])
        }
    };
    if !single_filter_is_one_of(&config.credentials.scope.publish, &expected_publish) {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "{} for {:?} must publish only to {:?}",
                managed_filter_context(mode),
                config.role,
                expected_publish
            ),
        });
    }
    if !single_filter_is_one_of(&config.credentials.scope.subscribe, &expected_subscribe) {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "{} for {:?} must subscribe only to {:?}",
                managed_filter_context(mode),
                config.role,
                expected_subscribe
            ),
        });
    }
    validate_no_unexpected_managed_filter_wildcards(&config.credentials.scope.publish[0])?;
    validate_no_unexpected_managed_filter_wildcards(&config.credentials.scope.subscribe[0])?;
    Ok(())
}

fn managed_filter_context(mode: ManagedConnectionMode) -> &'static str {
    match mode {
        ManagedConnectionMode::Direct => "managed broker credentials",
        ManagedConnectionMode::Ephemeral => {
            "managed ephemeral broker credentials with data-room negotiation"
        }
    }
}

fn single_filter_is_one_of(filters: &[String], allowed: &[String]) -> bool {
    filters.len() == 1 && allowed.iter().any(|expected| filters[0] == *expected)
}

fn validate_no_unexpected_managed_filter_wildcards(filter: &str) -> Result<(), MqttTransportError> {
    if filter.contains('#') {
        return Err(MqttTransportError::Configuration {
            message: format!("managed broker topic filter {filter:?} must not contain #"),
        });
    }
    let wildcard_count = filter.split('/').filter(|segment| *segment == "+").count();
    if wildcard_count > 1 || (wildcard_count == 1 && !filter.contains("/rooms/+/")) {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker topic filter {filter:?} may wildcard only the room segment"
            ),
        });
    }
    Ok(())
}

fn validate_managed_client_id(config: &ManagedMqttConnectConfig) -> Result<(), MqttTransportError> {
    let client_id = config.credentials.client_id.as_str();
    let namespace = config.credentials.scope.namespace.as_str();
    let suffix = client_id
        .strip_prefix(namespace)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .ok_or_else(|| MqttTransportError::Configuration {
            message: format!(
                "managed broker client id {client_id:?} must be under topic namespace {namespace:?}"
            ),
        })?;
    if suffix.is_empty()
        || suffix.starts_with('/')
        || suffix.ends_with('/')
        || suffix.split('/').any(str::is_empty)
        || suffix.contains('+')
        || suffix.contains('#')
    {
        return Err(MqttTransportError::Configuration {
            message: format!("managed broker client id {client_id:?} has an invalid shape"),
        });
    }

    let expected_role_segment = match config.role {
        ParticipantRole::Host => "host",
        ParticipantRole::Client => "mobile",
    };
    if suffix.split('/').next() != Some(expected_role_segment) {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker client id {client_id:?} must use role segment {expected_role_segment:?}"
            ),
        });
    }
    let expected_grant_suffix = format!("/{}", config.credentials.grant_id.as_str());
    if !client_id.ends_with(&expected_grant_suffix) {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker client id {client_id:?} must end with grant id {:?}",
                config.credentials.grant_id.as_str()
            ),
        });
    }
    Ok(())
}

fn validate_managed_connect_auth(
    auth: &protocol::ManagedBrokerConnectAuth,
    broker: &ManagedBrokerEndpoint,
) -> Result<ManagedWssConnectStrategy, MqttTransportError> {
    let websocket_url = auth.websocket_url.as_ref().ok_or_else(|| {
        MqttTransportError::Configuration {
            message:
                "managed WSS MQTT requires service-issued connect.websocket_url; refusing to use the base broker endpoint"
                    .to_owned(),
        }
    })?;
    validate_managed_websocket_url_for_broker(websocket_url, broker)?;
    Ok(ManagedWssConnectStrategy {
        upgrade_auth: ManagedWssUpgradeAuth::ServiceIssuedUrl(websocket_url.clone()),
    })
}

fn validate_managed_websocket_url_shape(
    websocket_url: &BrokerUrl,
) -> Result<(), MqttTransportError> {
    let parsed = url::Url::parse(websocket_url.as_str()).map_err(|err| {
        MqttTransportError::Configuration {
            message: format!("managed broker connect.websocket_url is invalid: {err}"),
        }
    })?;
    if parsed.scheme() != "wss" {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker connect.websocket_url must use wss://; got {:?}",
                parsed.scheme()
            ),
        });
    }
    if parsed.host_str().is_none() {
        return Err(MqttTransportError::Configuration {
            message: "managed broker connect.websocket_url is missing a host".to_owned(),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(MqttTransportError::Configuration {
            message:
                "managed broker connect.websocket_url must not embed URL username/password credentials"
                    .to_owned(),
        });
    }
    if parsed.fragment().is_some() {
        return Err(MqttTransportError::Configuration {
            message: "managed broker connect.websocket_url must not include a fragment".to_owned(),
        });
    }
    if parsed.path() != "/mqtt" {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker connect.websocket_url path {:?} is unsupported; expected /mqtt",
                parsed.path()
            ),
        });
    }
    validate_managed_websocket_query(&parsed, None)?;
    Ok(())
}

fn validate_managed_broker_endpoint(
    broker: &ManagedBrokerEndpoint,
) -> Result<(), MqttTransportError> {
    let parsed = parse_managed_url(&broker.endpoint, "managed broker endpoint")?;
    if parsed.scheme() != "wss" {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed AWS IoT broker endpoint must use wss://; got {:?}",
                parsed.scheme()
            ),
        });
    }
    if parsed.host_str().is_none() {
        return Err(MqttTransportError::Configuration {
            message: "managed AWS IoT broker endpoint is missing a host".to_owned(),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(MqttTransportError::Configuration {
            message:
                "managed AWS IoT broker endpoint must not embed URL username/password credentials"
                    .to_owned(),
        });
    }
    if parsed.fragment().is_some() {
        return Err(MqttTransportError::Configuration {
            message: "managed AWS IoT broker endpoint must not include a fragment".to_owned(),
        });
    }
    if parsed.path() != "/mqtt" {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed AWS IoT broker endpoint path {:?} is unsupported; expected /mqtt",
                parsed.path()
            ),
        });
    }
    if parsed.query().is_some() {
        return Err(MqttTransportError::Configuration {
            message: "managed AWS IoT broker endpoint must not include query parameters".to_owned(),
        });
    }
    Ok(())
}

fn parse_managed_url(
    value: &BrokerUrl,
    field: &'static str,
) -> Result<url::Url, MqttTransportError> {
    url::Url::parse(value.as_str()).map_err(|err| MqttTransportError::Configuration {
        message: format!("{field} is invalid: {err}"),
    })
}

fn validate_managed_websocket_url_for_broker(
    websocket_url: &BrokerUrl,
    broker: &ManagedBrokerEndpoint,
) -> Result<(), MqttTransportError> {
    validate_managed_websocket_url_shape(websocket_url)?;
    validate_managed_websocket_url_matches_endpoint(websocket_url, &broker.endpoint)?;
    let websocket = parse_managed_url(websocket_url, "managed broker connect.websocket_url")?;
    validate_managed_websocket_query(&websocket, Some(broker.authorizer_name.as_str()))
}

fn validate_managed_websocket_url_matches_endpoint(
    websocket_url: &BrokerUrl,
    endpoint_url: &BrokerUrl,
) -> Result<(), MqttTransportError> {
    let websocket = parse_managed_url(websocket_url, "managed broker connect.websocket_url")?;
    let endpoint = parse_managed_url(endpoint_url, "managed broker endpoint")?;
    if !same_managed_websocket_base(&websocket, &endpoint) {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker connect.websocket_url base {} must match broker endpoint {}",
                safe_url_base_context(&websocket),
                safe_url_base_context(&endpoint)
            ),
        });
    }
    Ok(())
}

fn same_managed_websocket_base(websocket: &url::Url, endpoint: &url::Url) -> bool {
    websocket.scheme() == endpoint.scheme()
        && websocket.host_str() == endpoint.host_str()
        && websocket.port() == endpoint.port()
        && websocket.path() == endpoint.path()
        && websocket.username().is_empty()
        && endpoint.username().is_empty()
        && websocket.password().is_none()
        && endpoint.password().is_none()
        && endpoint.query().is_none()
}

fn safe_url_base_context(parsed: &url::Url) -> String {
    let host = parsed.host_str().unwrap_or("<missing-host>");
    match parsed.port() {
        Some(port) => format!(
            "(scheme={:?}, host={host:?}, port={port}, path={:?})",
            parsed.scheme(),
            parsed.path()
        ),
        None => format!(
            "(scheme={:?}, host={host:?}, path={:?})",
            parsed.scheme(),
            parsed.path()
        ),
    }
}

fn validate_managed_websocket_query(
    parsed: &url::Url,
    expected_authorizer: Option<&str>,
) -> Result<(), MqttTransportError> {
    let authorizer = required_managed_query_value(parsed, "x-amz-customauthorizer-name")?;
    if let Some(expected_authorizer) = expected_authorizer
        && authorizer != expected_authorizer
    {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker connect.websocket_url authorizer {authorizer:?} does not match broker authorizer {expected_authorizer:?}"
            ),
        });
    }
    if let Some(token_key) = single_managed_query_value(parsed, "token-key-name")?
        && token_key != "tycode-grant"
    {
        return Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker connect.websocket_url token-key-name {token_key:?} is unsupported; expected \"tycode-grant\""
            ),
        });
    }
    required_managed_query_value(parsed, "tycode-grant")?;
    Ok(())
}

fn required_managed_query_value(
    parsed: &url::Url,
    key: &str,
) -> Result<String, MqttTransportError> {
    let value = single_managed_query_value(parsed, key)?.ok_or_else(|| {
        MqttTransportError::Configuration {
            message: format!("managed broker connect.websocket_url is missing {key}"),
        }
    })?;
    if value.trim().is_empty() {
        return Err(MqttTransportError::Configuration {
            message: format!("managed broker connect.websocket_url {key} must not be empty"),
        });
    }
    Ok(value)
}

fn single_managed_query_value(
    parsed: &url::Url,
    key: &str,
) -> Result<Option<String>, MqttTransportError> {
    let values = parsed
        .query_pairs()
        .filter_map(|(name, value)| (name == key).then(|| value.into_owned()))
        .collect::<Vec<_>>();
    match values.len() {
        0 => Ok(None),
        1 => Ok(values.into_iter().next()),
        _ => Err(MqttTransportError::Configuration {
            message: format!(
                "managed broker connect.websocket_url must not repeat query parameter {key}"
            ),
        }),
    }
}

pub(crate) fn link_broker_url(broker: &LinkBrokerConfig) -> &BrokerUrl {
    match &broker.auth {
        LinkBrokerAuth::Managed(auth) => auth.http_upgrade_url(),
        LinkBrokerAuth::Legacy(_) => &broker.url,
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrowserConnectPacketOptions {
    pub(crate) keep_alive_secs: u16,
    pub(crate) receive_maximum: u16,
    pub(crate) max_packet_size: u32,
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn encode_browser_connect_packet(
    broker: &LinkBrokerConfig,
    options: BrowserConnectPacketOptions,
) -> Result<bytes::BytesMut, MqttTransportError> {
    use mqttbytes::v5::{Connect, ConnectProperties};
    use rand::RngCore;
    use rand::rngs::OsRng;

    let client_id = match &broker.client_id {
        LinkClientId::Random(role) => {
            let mut random = [0_u8; 16];
            OsRng.fill_bytes(&mut random);
            let mut hex = String::with_capacity(random.len() * 2);
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            for byte in random {
                hex.push(DIGITS[(byte >> 4) as usize] as char);
                hex.push(DIGITS[(byte & 0x0f) as usize] as char);
            }
            format!("{}-{hex}", role.client_id_prefix())
        }
        LinkClientId::Exact(client_id) => client_id.as_str().to_owned(),
    };
    let mut connect = Connect::new(client_id);
    connect.keep_alive = options.keep_alive_secs;
    connect.clean_session = true;
    connect.properties = Some(ConnectProperties {
        session_expiry_interval: Some(0),
        receive_maximum: Some(options.receive_maximum),
        max_packet_size: Some(options.max_packet_size),
        topic_alias_max: None,
        request_response_info: None,
        request_problem_info: None,
        user_properties: Vec::new(),
        authentication_method: None,
        authentication_data: None,
    });
    match &broker.auth {
        LinkBrokerAuth::Legacy(BrokerAuth::Anonymous) => {}
        LinkBrokerAuth::Legacy(BrokerAuth::UsernamePassword { username, password }) => {
            connect.set_login(username.clone(), password.clone());
        }
        LinkBrokerAuth::Managed(_) => {}
    }

    let mut buffer = bytes::BytesMut::new();
    connect
        .write(&mut buffer)
        .map_err(|err| MqttTransportError::Configuration {
            message: format!("failed to encode MQTT CONNECT packet: {err:?}"),
        })?;
    Ok(buffer)
}
