use mqtt_transport::{RoomId, TopicDirection, host_to_client_topic, parse_topic};

#[test]
fn public_topic_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let room = RoomId([0xab; 16]);
    let topic = host_to_client_topic(&room);
    let parsed = parse_topic(&topic)?;
    assert_eq!(parsed.room, room);
    assert_eq!(parsed.direction, TopicDirection::HostToClient);
    Ok(())
}
