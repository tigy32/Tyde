//! PHASE-0 SPIKE — throwaway. Goal: prove (or disprove) that the rumqttc
//! v5 MQTT packet codec (`v5::mqttbytes`) can compile to
//! wasm32-unknown-unknown with default-features=false (no tokio net / no TLS),
//! and do a trivial encode/decode round-trip of a PUBLISH packet.
//!
//! If this compiles for wasm32, the actor's packet matching can be ported
//! nearly verbatim on top of a browser WebSocket. If it does NOT, fall back
//! to the standalone `mqttbytes` crate (see ../mqttbytes-standalone-wasm).

use rumqttc::v5::mqttbytes::v5::{Packet, Publish};
use rumqttc::v5::mqttbytes::QoS;
use bytes::BytesMut;

/// Encode a PUBLISH, then decode it back. Returns the decoded topic+payload
/// length so the call can't be optimized away.
pub fn roundtrip_publish() -> (String, usize) {
    let publish = Publish::new("tyde/spike/topic", QoS::AtLeastOnce, b"hello-wasm".to_vec(), None);

    let mut buf = BytesMut::new();
    // write() serializes the packet into the buffer.
    publish.write(&mut buf).expect("encode PUBLISH");

    // read() parses a single packet back out of the buffer.
    let max = 10 * 1024;
    let decoded = rumqttc::v5::mqttbytes::v5::read(&mut buf, max).expect("decode packet");

    match decoded {
        Packet::Publish(p, _props) => {
            let topic = String::from_utf8_lossy(&p.topic).to_string();
            (topic, p.payload.len())
        }
        other => panic!("unexpected packet: {other:?}"),
    }
}
