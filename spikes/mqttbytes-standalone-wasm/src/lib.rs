//! PHASE-0 SPIKE — fallback ladder rung 1. The standalone `mqttbytes 0.6.0`
//! crate is the rumqttc v5 codec extracted into its own crate that depends
//! only on `bytes` (no tokio / mio / flume / native sockets). This proves it
//! compiles to wasm32-unknown-unknown and does a trivial PUBLISH round-trip.

use mqttbytes::v5::{read, Packet, Publish};
use mqttbytes::QoS;
use bytes::BytesMut;

/// Encode a v5 PUBLISH, then decode it back out of the same buffer.
/// Returns (topic, payload_len) so the work can't be optimized away.
pub fn roundtrip_publish() -> (String, usize) {
    let mut publish = Publish::new("tyde/spike/topic", QoS::AtLeastOnce, b"hello-wasm".to_vec());
    publish.pkid = 1; // QoS>=1 PUBLISH requires a non-zero packet id

    let mut buf = BytesMut::new();
    publish.write(&mut buf).expect("encode PUBLISH");

    let max = 10 * 1024;
    let decoded = read(&mut buf, max).expect("decode packet");

    match decoded {
        Packet::Publish(p) => (p.topic, p.payload.len()),
        other => panic!("unexpected packet: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publish_roundtrips() {
        let (topic, len) = roundtrip_publish();
        assert_eq!(topic, "tyde/spike/topic");
        assert_eq!(len, "hello-wasm".len());
    }
}
