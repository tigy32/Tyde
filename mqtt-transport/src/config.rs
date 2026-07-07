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
    Managed(protocol::ManagedBrokerConnectAuth),
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
        validate_managed_config(&config, mode)?;
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
                auth: LinkBrokerAuth::Managed(config.credentials.connect),
                client_id: LinkClientId::Exact(config.credentials.client_id),
            },
            topics: TopicScheme::Managed {
                namespace: config.credentials.scope.namespace,
            },
        })
    }
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
) -> Result<(), MqttTransportError> {
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
    validate_managed_connect_auth(&config.credentials.connect)?;
    if let Some(websocket_url) = &config.credentials.connect.websocket_url {
        validate_managed_websocket_url_for_broker(websocket_url, &config.broker)?;
    }
    Ok(())
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
) -> Result<(), MqttTransportError> {
    if auth.username.is_none()
        && auth.password.is_none()
        && auth.websocket_url.is_none()
        && auth.headers.is_empty()
    {
        return Err(MqttTransportError::Configuration {
            message:
                "managed broker connect auth must include username/password, connect.websocket_url, or WebSocket headers"
                    .to_owned(),
        });
    }
    if auth.password.is_some() && auth.username.is_none() {
        return Err(MqttTransportError::Configuration {
            message: "managed broker MQTT password cannot be sent without a username".to_owned(),
        });
    }
    if let Some(websocket_url) = &auth.websocket_url {
        validate_managed_websocket_url_shape(websocket_url)?;
    }
    for (name, value) in &auth.headers {
        validate_websocket_header(name, value)?;
    }
    Ok(())
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn validate_browser_managed_connect_auth(
    auth: &protocol::ManagedBrokerConnectAuth,
) -> Result<(), MqttTransportError> {
    let websocket_url = auth.websocket_url.as_ref().ok_or_else(|| {
        MqttTransportError::Configuration {
            message:
                "browser WebSocket MQTT requires managed broker connect.websocket_url; refusing to use the base broker endpoint"
                    .to_owned(),
        }
    })?;
    validate_managed_websocket_url_shape(websocket_url)?;
    if auth.password.is_some() && auth.username.is_none() {
        return Err(MqttTransportError::Configuration {
            message: "managed broker MQTT password cannot be sent without a username".to_owned(),
        });
    }
    Ok(())
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn validate_browser_managed_connect_auth_for_broker(
    auth: &protocol::ManagedBrokerConnectAuth,
    endpoint: &BrokerUrl,
) -> Result<(), MqttTransportError> {
    validate_browser_managed_connect_auth(auth)?;
    let websocket_url = auth.websocket_url.as_ref().ok_or_else(|| {
        MqttTransportError::Configuration {
            message:
                "browser WebSocket MQTT requires managed broker connect.websocket_url; refusing to use the base broker endpoint"
                    .to_owned(),
        }
    })?;
    validate_managed_websocket_url_matches_endpoint(websocket_url, endpoint)
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

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn browser_link_broker_url(
    broker: &LinkBrokerConfig,
) -> Result<&BrokerUrl, MqttTransportError> {
    match &broker.auth {
        LinkBrokerAuth::Managed(auth) => {
            validate_browser_managed_connect_auth_for_broker(auth, &broker.url)?;
            auth.websocket_url
                .as_ref()
                .ok_or_else(|| MqttTransportError::Configuration {
                    message:
                        "browser WebSocket MQTT requires managed broker connect.websocket_url; refusing to use the base broker endpoint"
                            .to_owned(),
                })
        }
        LinkBrokerAuth::Legacy(_) => Ok(&broker.url),
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrowserConnectPacketOptions {
    pub(crate) keep_alive_secs: u16,
    pub(crate) receive_maximum: u16,
    pub(crate) max_packet_size: u32,
}

#[cfg(any(target_arch = "wasm32", test))]
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
        LinkBrokerAuth::Managed(auth) => {
            validate_browser_managed_connect_auth_for_broker(auth, &broker.url)?;
        }
    }

    let mut buffer = bytes::BytesMut::new();
    connect
        .write(&mut buffer)
        .map_err(|err| MqttTransportError::Configuration {
            message: format!("failed to encode MQTT CONNECT packet: {err:?}"),
        })?;
    Ok(buffer)
}

pub(crate) fn validate_websocket_header(name: &str, value: &str) -> Result<(), MqttTransportError> {
    if name.is_empty() {
        return Err(MqttTransportError::Configuration {
            message: "managed broker WebSocket header name must not be empty".to_owned(),
        });
    }
    if !name.bytes().all(is_http_header_name_byte) {
        return Err(MqttTransportError::Configuration {
            message: format!("managed broker WebSocket header name {name:?} is invalid"),
        });
    }
    if !value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
    {
        return Err(MqttTransportError::Configuration {
            message: format!("managed broker WebSocket header {name:?} has an invalid value"),
        });
    }
    Ok(())
}

fn is_http_header_name_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'..=b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'^'..=b'z'
            | b'|'
            | b'~'
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::{
        ManagedBrokerAuthorizerName, ManagedBrokerConnectAuth, ManagedBrokerCredentialScope,
        ManagedBrokerCredentials, ManagedBrokerGrantId, ManagedBrokerProvider, ManagedBrokerRegion,
    };

    use super::*;

    fn managed_config(role: ParticipantRole) -> ManagedMqttConnectConfig {
        let namespace = ManagedBrokerTopicNamespace::new("tyde/prod/pair_01J").expect("namespace");
        let (publish, subscribe, broker_role, client_id) = match role {
            ParticipantRole::Host => (
                "tyde/prod/pair_01J/rooms/+/host-to-client",
                "tyde/prod/pair_01J/rooms/+/client-to-host",
                ManagedBrokerRole::Host,
                "tyde/prod/pair_01J/host/grant_01J",
            ),
            ParticipantRole::Client => (
                "tyde/prod/pair_01J/rooms/+/client-to-host",
                "tyde/prod/pair_01J/rooms/+/host-to-client",
                ManagedBrokerRole::Mobile,
                "tyde/prod/pair_01J/mobile/dev_01J/grant_01J",
            ),
        };
        let mut headers = BTreeMap::new();
        headers.insert("x-tycode-grant".to_owned(), "signed-grant".to_owned());
        ManagedMqttConnectConfig {
            broker: ManagedBrokerEndpoint {
                endpoint: BrokerUrl::new("wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt")
                    .expect("broker url"),
                provider: ManagedBrokerProvider::AwsIotCore,
                region: ManagedBrokerRegion::new("us-west-2").expect("region"),
                authorizer_name: ManagedBrokerAuthorizerName::new("tycode-mobile-v1")
                    .expect("authorizer"),
            },
            credentials: ManagedBrokerCredentials {
                grant_id: ManagedBrokerGrantId::new("grant_01J").expect("grant id"),
                client_id: ManagedBrokerClientId::new(client_id).expect("client id"),
                connect: ManagedBrokerConnectAuth {
                    username: Some("x-amz-customauthorizer-name=tycode-mobile-v1".to_owned()),
                    password: Some("signed-grant".to_owned()),
                    websocket_url: Some(
                        BrokerUrl::new(
                            "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant"
                        )
                        .expect("websocket url"),
                    ),
                    headers,
                },
                scope: ManagedBrokerCredentialScope {
                    namespace,
                    role: broker_role,
                    publish: vec![publish.to_owned()],
                    subscribe: vec![subscribe.to_owned()],
                },
                issued_at_ms: 1,
                expires_at_ms: 2,
            },
            room: RoomId([7_u8; crate::types::ROOM_ID_LEN]),
            psk: PreSharedKey::from_slice(&[9_u8; crate::types::PRE_SHARED_KEY_LEN]).expect("psk"),
            role,
        }
    }

    #[test]
    fn managed_connection_plan_preserves_exact_client_id_and_scoped_topics() {
        let plan =
            ConnectionPlan::managed(managed_config(ParticipantRole::Host)).expect("managed plan");
        assert_eq!(
            plan.broker.client_id,
            LinkClientId::Exact(
                ManagedBrokerClientId::new("tyde/prod/pair_01J/host/grant_01J").expect("client id")
            )
        );
        assert_eq!(
            plan.topics
                .outbound_topic(ParticipantRole::Host, &plan.config.room)
                .expect("outbound topic"),
            "tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/host-to-client"
        );
        assert_eq!(
            plan.topics
                .inbound_topic(ParticipantRole::Host, &plan.config.room)
                .expect("inbound topic"),
            "tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/client-to-host"
        );
    }

    #[test]
    fn managed_connection_plan_accepts_exact_room_scoped_filters() {
        let mut host = managed_config(ParticipantRole::Host);
        host.credentials.scope.publish =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/host-to-client".to_owned()];
        host.credentials.scope.subscribe =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/client-to-host".to_owned()];
        ConnectionPlan::managed(host).expect("host exact-room grant must be accepted");

        let mut client = managed_config(ParticipantRole::Client);
        client.credentials.scope.publish =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/client-to-host".to_owned()];
        client.credentials.scope.subscribe =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/host-to-client".to_owned()];
        ConnectionPlan::managed(client).expect("mobile exact-room grant must be accepted");
    }

    #[test]
    fn managed_ephemeral_connection_plan_accepts_only_room_wildcard_filters() {
        ConnectionPlan::managed_ephemeral(managed_config(ParticipantRole::Host))
            .expect("host wildcard grant must be accepted for ephemeral connections");
        ConnectionPlan::managed_ephemeral(managed_config(ParticipantRole::Client))
            .expect("mobile wildcard grant must be accepted for ephemeral connections");
    }

    #[test]
    fn managed_ephemeral_connection_plan_rejects_exact_room_scoped_filters() {
        let mut host = managed_config(ParticipantRole::Host);
        host.credentials.scope.publish =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/host-to-client".to_owned()];
        host.credentials.scope.subscribe =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/client-to-host".to_owned()];
        let err = ConnectionPlan::managed_ephemeral(host)
            .expect_err("host exact-room grant cannot authorize the negotiated data room");
        assert!(err.to_string().contains("ephemeral"));
        assert!(err.to_string().contains("rooms/+"));

        let mut client = managed_config(ParticipantRole::Client);
        client.credentials.scope.publish =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/client-to-host".to_owned()];
        client.credentials.scope.subscribe =
            vec!["tyde/prod/pair_01J/rooms/BwcHBwcHBwcHBwcHBwcHBw/host-to-client".to_owned()];
        let err = ConnectionPlan::managed_ephemeral(client)
            .expect_err("mobile exact-room grant cannot authorize the negotiated data room");
        assert!(err.to_string().contains("ephemeral"));
        assert!(err.to_string().contains("rooms/+"));
    }

    #[test]
    fn managed_ephemeral_connection_plan_rejects_extra_broad_or_wrong_direction_filters() {
        let mut extra = managed_config(ParticipantRole::Host);
        extra
            .credentials
            .scope
            .publish
            .push("tyde/prod/pair_01J/rooms/+/host-to-client".to_owned());
        let err =
            ConnectionPlan::managed_ephemeral(extra).expect_err("extra publish filter must fail");
        assert!(err.to_string().contains("publish only"));

        let mut broad = managed_config(ParticipantRole::Host);
        broad.credentials.scope.subscribe = vec!["tyde/prod/pair_01J/#".to_owned()];
        let err =
            ConnectionPlan::managed_ephemeral(broad).expect_err("broad subscribe filter must fail");
        assert!(err.to_string().contains("subscribe only"));

        let mut wrong_direction = managed_config(ParticipantRole::Host);
        wrong_direction.credentials.scope.publish =
            vec!["tyde/prod/pair_01J/rooms/+/client-to-host".to_owned()];
        let err = ConnectionPlan::managed_ephemeral(wrong_direction)
            .expect_err("host publish direction mismatch must fail");
        assert!(err.to_string().contains("publish only"));

        let mut wrong_namespace = managed_config(ParticipantRole::Client);
        wrong_namespace.credentials.scope.subscribe =
            vec!["tyde/prod/pair_other/rooms/+/host-to-client".to_owned()];
        let err = ConnectionPlan::managed_ephemeral(wrong_namespace)
            .expect_err("cross-namespace subscribe filter must fail");
        assert!(err.to_string().contains("subscribe only"));
    }

    #[test]
    fn managed_connection_plan_rejects_exact_filter_for_wrong_room() {
        let mut config = managed_config(ParticipantRole::Host);
        config.credentials.scope.publish =
            vec!["tyde/prod/pair_01J/rooms/CAgICAgICAgICAgICAgICA/host-to-client".to_owned()];

        let err = ConnectionPlan::managed(config).expect_err("wrong room must fail");
        assert!(err.to_string().contains("publish only"));
    }

    #[test]
    fn managed_connection_plan_rejects_role_direction_mismatch() {
        let mut config = managed_config(ParticipantRole::Host);
        config.credentials.scope.role = ManagedBrokerRole::Mobile;
        let err = ConnectionPlan::managed(config).expect_err("role mismatch must fail");
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn managed_connection_plan_rejects_client_id_role_mismatch() {
        let mut config = managed_config(ParticipantRole::Host);
        config.credentials.client_id =
            ManagedBrokerClientId::new("tyde/prod/pair_01J/mobile/dev_01J/grant_01J")
                .expect("client id");
        let err = ConnectionPlan::managed(config).expect_err("client id role must fail");
        assert!(err.to_string().contains("role segment"));
    }

    #[test]
    fn managed_connection_plan_rejects_client_id_grant_mismatch() {
        let mut config = managed_config(ParticipantRole::Host);
        config.credentials.client_id =
            ManagedBrokerClientId::new("tyde/prod/pair_01J/host/grant_other").expect("client id");
        let err = ConnectionPlan::managed(config).expect_err("client id grant must fail");
        assert!(err.to_string().contains("grant id"));
    }

    #[test]
    fn managed_connection_plan_rejects_extra_or_broad_topic_filters() {
        let mut extra = managed_config(ParticipantRole::Host);
        extra
            .credentials
            .scope
            .publish
            .push("tyde/prod/pair_01J/rooms/+/host-to-client".to_owned());
        let err = ConnectionPlan::managed(extra).expect_err("extra publish filter must fail");
        assert!(err.to_string().contains("publish only"));

        let mut broad = managed_config(ParticipantRole::Host);
        broad.credentials.scope.subscribe = vec!["tyde/prod/pair_01J/#".to_owned()];
        let err = ConnectionPlan::managed(broad).expect_err("broad subscribe filter must fail");
        assert!(err.to_string().contains("subscribe only"));
    }

    #[test]
    fn managed_connection_plan_rejects_missing_auth_material() {
        let mut config = managed_config(ParticipantRole::Host);
        config.credentials.connect = ManagedBrokerConnectAuth {
            username: None,
            password: None,
            websocket_url: None,
            headers: BTreeMap::new(),
        };
        let err = ConnectionPlan::managed(config).expect_err("missing auth must fail");
        assert!(err.to_string().contains("connect auth"));
    }

    #[test]
    fn managed_connection_plan_accepts_websocket_url_only_auth_material() {
        let mut config = managed_config(ParticipantRole::Client);
        config.credentials.connect.username = None;
        config.credentials.connect.password = None;
        config.credentials.connect.headers.clear();
        ConnectionPlan::managed_ephemeral(config)
            .expect("browser-safe websocket_url is managed auth material");
    }

    #[test]
    fn managed_connection_plan_accepts_tycode_grant_query_without_token_key_name() {
        let mut config = managed_config(ParticipantRole::Client);
        config.credentials.connect.websocket_url = Some(
            BrokerUrl::new(
                "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant",
            )
            .expect("websocket url"),
        );
        ConnectionPlan::managed_ephemeral(config)
            .expect("current tycode.dev websocket_url shape must be accepted");
    }

    #[test]
    fn managed_connection_plan_rejects_invalid_websocket_url_semantics() {
        for (config, expected) in [
            {
                let mut config = managed_config(ParticipantRole::Client);
                config.credentials.connect.websocket_url = Some(
                    BrokerUrl::new(
                        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/not-mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant",
                    )
                    .expect("websocket url"),
                );
                (config, "/mqtt")
            },
            {
                let mut config = managed_config(ParticipantRole::Client);
                config.credentials.connect.websocket_url = Some(
                    BrokerUrl::new(
                        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=other&tycode-grant=signed-grant",
                    )
                    .expect("websocket url"),
                );
                (config, "authorizer")
            },
            {
                let mut config = managed_config(ParticipantRole::Client);
                config.credentials.connect.websocket_url = Some(
                    BrokerUrl::new(
                        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1",
                    )
                    .expect("websocket url"),
                );
                (config, "tycode-grant")
            },
        ] {
            let err = ConnectionPlan::managed_ephemeral(config)
                .expect_err("invalid websocket_url semantics must fail closed");
            assert!(
                err.to_string().contains(expected),
                "expected {expected:?} in {err}"
            );
            assert!(
                !err.to_string().contains("signed-grant"),
                "managed websocket_url errors must not leak the grant token: {err}"
            );
        }
    }

    #[test]
    fn managed_connection_plan_redacts_token_from_endpoint_and_websocket_errors() {
        let mut endpoint_query = managed_config(ParticipantRole::Client);
        endpoint_query.broker.endpoint = BrokerUrl::new(
            "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?tycode-grant=signed-grant-secret",
        )
        .expect("endpoint wrapper");
        let err = ConnectionPlan::managed_ephemeral(endpoint_query)
            .expect_err("endpoint query must fail closed");
        assert!(!err.to_string().contains("signed-grant-secret"));
        assert!(!err.to_string().contains("tycode-grant=signed"));

        let mut wrong_host = managed_config(ParticipantRole::Client);
        wrong_host.credentials.connect.websocket_url = Some(
            BrokerUrl::new(
                "wss://other-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant-secret",
            )
            .expect("websocket url"),
        );
        let err = ConnectionPlan::managed_ephemeral(wrong_host)
            .expect_err("mismatched websocket_url must fail closed");
        assert!(err.to_string().contains("must match broker endpoint"));
        assert!(!err.to_string().contains("signed-grant-secret"));
        assert!(!err.to_string().contains("tycode-grant=signed"));
    }

    #[test]
    fn managed_connection_plan_rejects_invalid_header_values() {
        let mut config = managed_config(ParticipantRole::Host);
        config
            .credentials
            .connect
            .headers
            .insert("x-tycode-grant".to_owned(), "line\r\nbreak".to_owned());
        let err = ConnectionPlan::managed(config).expect_err("invalid header must fail");
        assert!(err.to_string().contains("invalid value"));
    }

    #[test]
    fn browser_managed_auth_accepts_websocket_url_with_optional_username_password() {
        let config = managed_config(ParticipantRole::Client);
        validate_browser_managed_connect_auth(&config.credentials.connect)
            .expect("browser can use the service-issued WebSocket URL");
    }

    #[test]
    fn browser_managed_auth_rejects_missing_websocket_url() {
        let mut config = managed_config(ParticipantRole::Client);
        config.credentials.connect.websocket_url = None;
        let err = validate_browser_managed_connect_auth(&config.credentials.connect)
            .expect_err("missing browser websocket_url must fail closed");
        assert!(err.to_string().contains("websocket_url"));
    }

    #[test]
    fn browser_managed_auth_rejects_password_without_username() {
        let mut config = managed_config(ParticipantRole::Client);
        config.credentials.connect.username = None;
        let err = validate_browser_managed_connect_auth(&config.credentials.connect)
            .expect_err("password without username must fail closed");
        assert!(err.to_string().contains("password"));
    }

    #[test]
    fn browser_link_uses_managed_websocket_url_not_base_endpoint() {
        let plan = ConnectionPlan::managed_ephemeral(managed_config(ParticipantRole::Client))
            .expect("managed plan");
        let url = browser_link_broker_url(&plan.broker).expect("browser URL");
        assert_eq!(
            url.as_str(),
            "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant"
        );
        assert_ne!(
            url.as_str(),
            plan.config.endpoint.url.as_str(),
            "browser managed transport must not fall back to the base broker endpoint"
        );
    }

    #[test]
    fn browser_link_rejects_websocket_url_base_mismatch_without_token_leak() {
        let config = managed_config(ParticipantRole::Client);
        let broker = LinkBrokerConfig {
            url: BrokerUrl::new("wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt")
                .expect("base endpoint"),
            auth: LinkBrokerAuth::Managed(ManagedBrokerConnectAuth {
                websocket_url: Some(
                    BrokerUrl::new(
                        "wss://other-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant-secret",
                    )
                    .expect("websocket url"),
                ),
                ..config.credentials.connect
            }),
            client_id: LinkClientId::Exact(config.credentials.client_id),
        };

        let err = browser_link_broker_url(&broker)
            .expect_err("browser link must validate websocket_url against the base endpoint");

        assert!(err.to_string().contains("must match broker endpoint"));
        assert!(!err.to_string().contains("signed-grant-secret"));
        assert!(!err.to_string().contains("tycode-grant=signed"));
    }

    #[test]
    fn browser_managed_connect_packet_omits_mqtt_username_password() {
        let plan = ConnectionPlan::managed_ephemeral(managed_config(ParticipantRole::Client))
            .expect("managed plan");
        let packet = encode_browser_connect_packet(
            &plan.broker,
            BrowserConnectPacketOptions {
                keep_alive_secs: 30,
                receive_maximum: 32,
                max_packet_size: 4096,
            },
        )
        .expect("encode browser connect packet");

        assert!(
            bytes_contain(&packet, b"tyde/prod/pair_01J/mobile/dev_01J/grant_01J"),
            "CONNECT must still use the exact service-issued client id"
        );
        assert!(
            !bytes_contain(&packet, b"signed-grant"),
            "browser managed CONNECT must not repeat the grant token in MQTT password"
        );
        assert!(
            !bytes_contain(&packet, b"x-amz-customauthorizer-name"),
            "browser managed CONNECT must rely on connect.websocket_url, not MQTT username"
        );
    }

    fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
