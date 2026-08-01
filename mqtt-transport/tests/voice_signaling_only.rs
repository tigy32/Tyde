use protocol::{
    AgentId, Envelope, FrameKind, StreamPath, VoiceAudioCodec, VoiceClientCapabilities,
    VoiceSessionId, VoiceStartPayload, VoiceTarget,
};

#[test]
fn no_audio_bytes_can_enter_mqtt_through_the_voice_contract() {
    let stream = StreamPath("/voice/voice-session-a".to_owned());
    let payload = VoiceStartPayload {
        session_id: VoiceSessionId("voice-session-a".to_owned()),
        target: VoiceTarget {
            agent_id: AgentId("agent-a".to_owned()),
            instance_stream: StreamPath("/agent/agent-a/instance-a".to_owned()),
        },
        capabilities: VoiceClientCapabilities {
            audio_track: true,
            codecs: vec![VoiceAudioCodec::Opus],
            echo_cancellation_requested: true,
        },
    };
    let envelope = Envelope::from_payload(stream, FrameKind::VoiceStart, 0, &payload)
        .expect("serialize signaling envelope");
    let encoded = serde_json::to_vec(&envelope).expect("encode MQTT application bytes");
    assert!(!encoded.windows(9).any(|window| window == b"audioInput"));
    assert!(!encoded.windows(3).any(|window| window == b"pcm"));

    let attempted_audio = serde_json::json!({
        "session_id": "voice-session-a",
        "target": {
            "agent_id": "agent-a",
            "instance_stream": "/agent/agent-a/instance-a"
        },
        "capabilities": {
            "audio_track": true,
            "codecs": ["opus"],
            "echo_cancellation_requested": true
        },
        "audio": "AAECAwQFBgcICQ=="
    });
    assert!(
        serde_json::from_value::<VoiceStartPayload>(attempted_audio).is_err(),
        "voice control payloads must reject rather than forward audio-shaped fields"
    );

    let mut validator = protocol::ProtocolValidator::new();
    validator
        .validate_envelope(&envelope)
        .expect("signaling-only VoiceStart");
    let data_channel_offer = Envelope::from_payload(
        protocol::StreamPath("/voice/voice-session-a".to_owned()),
        FrameKind::VoiceOffer,
        1,
        &protocol::VoiceOfferPayload {
            session_id: VoiceSessionId("voice-session-a".to_owned()),
            sdp: "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n".to_owned(),
        },
    )
    .expect("serialize data-channel offer");
    assert!(
        validator.validate_envelope(&data_channel_offer).is_err(),
        "voice v1 must reject a data-channel media fallback before MQTT carries it"
    );
}
