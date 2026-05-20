use crate::error::FramingError;
use crate::types::RoomId;

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
}
