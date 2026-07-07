use crate::error::FramingError;
use crate::types::RoomId;
use protocol::ManagedBrokerTopicNamespace;

pub const TOPIC_PREFIX: &str = "tyde/v1";
pub const HOST_TO_CLIENT_SEGMENT: &str = "host-to-client";
pub const CLIENT_TO_HOST_SEGMENT: &str = "client-to-host";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TopicDirection {
    HostToClient,
    ClientToHost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTopic {
    pub room: RoomId,
    pub direction: TopicDirection,
}

pub fn host_to_client_topic(room: &RoomId) -> String {
    format!(
        "{TOPIC_PREFIX}/{}/{HOST_TO_CLIENT_SEGMENT}",
        room.as_base64url_no_pad()
    )
}

pub fn client_to_host_topic(room: &RoomId) -> String {
    format!(
        "{TOPIC_PREFIX}/{}/{CLIENT_TO_HOST_SEGMENT}",
        room.as_base64url_no_pad()
    )
}

pub fn topic_for_direction(room: &RoomId, direction: TopicDirection) -> String {
    match direction {
        TopicDirection::HostToClient => host_to_client_topic(room),
        TopicDirection::ClientToHost => client_to_host_topic(room),
    }
}

pub fn managed_host_to_client_topic(
    namespace: &ManagedBrokerTopicNamespace,
    room: &RoomId,
) -> Result<String, crate::error::MqttTransportError> {
    managed_topic_for_direction(namespace, room, TopicDirection::HostToClient)
}

pub fn managed_client_to_host_topic(
    namespace: &ManagedBrokerTopicNamespace,
    room: &RoomId,
) -> Result<String, crate::error::MqttTransportError> {
    managed_topic_for_direction(namespace, room, TopicDirection::ClientToHost)
}

pub fn managed_topic_for_direction(
    namespace: &ManagedBrokerTopicNamespace,
    room: &RoomId,
    direction: TopicDirection,
) -> Result<String, crate::error::MqttTransportError> {
    validate_managed_topic_namespace(namespace)?;
    let direction = match direction {
        TopicDirection::HostToClient => HOST_TO_CLIENT_SEGMENT,
        TopicDirection::ClientToHost => CLIENT_TO_HOST_SEGMENT,
    };
    Ok(format!(
        "{}/rooms/{}/{direction}",
        namespace.as_str(),
        room.as_base64url_no_pad()
    ))
}

pub(crate) fn validate_managed_topic_namespace(
    namespace: &ManagedBrokerTopicNamespace,
) -> Result<(), crate::error::MqttTransportError> {
    let value = namespace.as_str();
    if value.starts_with('/') || value.ends_with('/') || value.split('/').any(str::is_empty) {
        return Err(crate::error::MqttTransportError::Configuration {
            message: format!(
                "managed broker topic namespace {value:?} has an invalid slash layout"
            ),
        });
    }
    if value.contains('+') || value.contains('#') {
        return Err(crate::error::MqttTransportError::Configuration {
            message: format!("managed broker topic namespace {value:?} must not contain wildcards"),
        });
    }
    Ok(())
}

pub fn parse_topic(topic: &str) -> Result<ParsedTopic, FramingError> {
    let mut parts = topic.split('/');
    let part0 = parts.next();
    let part1 = parts.next();
    let room_part = parts.next();
    let direction_part = parts.next();
    let extra = parts.next();

    match (part0, part1, room_part, direction_part, extra) {
        (Some("tyde"), Some("v1"), Some(room), Some(direction), None) => {
            let room =
                RoomId::from_base64url_no_pad(room).map_err(|err| FramingError::InvalidTopic {
                    message: err.to_string(),
                })?;
            let direction = match direction {
                HOST_TO_CLIENT_SEGMENT => TopicDirection::HostToClient,
                CLIENT_TO_HOST_SEGMENT => TopicDirection::ClientToHost,
                other => {
                    return Err(FramingError::InvalidTopic {
                        message: format!("unknown direction segment {other:?}"),
                    });
                }
            };
            Ok(ParsedTopic { room, direction })
        }
        _ => Err(FramingError::InvalidTopic {
            message: format!("topic {topic:?} does not match tyde/v1/<room>/<direction>"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let room = RoomId([7_u8; 16]);
        let topic = host_to_client_topic(&room);
        let parsed = parse_topic(&topic)?;
        assert_eq!(parsed.room, room);
        assert_eq!(parsed.direction, TopicDirection::HostToClient);
        Ok(())
    }

    #[test]
    fn managed_topics_use_scoped_namespace_and_room_segment()
    -> Result<(), Box<dyn std::error::Error>> {
        let namespace = ManagedBrokerTopicNamespace::new("tyde/prod/pair_01J")?;
        let room = RoomId([8_u8; 16]);

        assert_eq!(
            managed_host_to_client_topic(&namespace, &room)?,
            "tyde/prod/pair_01J/rooms/CAgICAgICAgICAgICAgICA/host-to-client"
        );
        assert_eq!(
            managed_client_to_host_topic(&namespace, &room)?,
            "tyde/prod/pair_01J/rooms/CAgICAgICAgICAgICAgICA/client-to-host"
        );
        Ok(())
    }

    #[test]
    fn managed_topics_reject_wildcard_namespace() {
        let namespace = ManagedBrokerTopicNamespace::new("tyde/prod/+/pair").expect("namespace");
        let err = managed_host_to_client_topic(&namespace, &RoomId([8_u8; 16]))
            .expect_err("wildcard namespace must fail closed");
        assert!(err.to_string().contains("wildcards"));
    }
}
