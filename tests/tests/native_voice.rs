use std::collections::HashMap;
use std::time::{Duration, Instant};

mod support;

use mqtt_transport::{
    BrokerAuth, BrokerEndpoint, MqttConnectConfig, ParticipantRole, PreSharedKey, RoomId,
};
use protocol::{
    Envelope, FrameKind, FrameReader, HostSettingValue, SetSettingPayload, StreamPath,
    VoiceAudioPayload, VoiceDirection, VoiceSessionId,
};

async fn next_kind(client: &mut client::Connection, kind: FrameKind) -> protocol::ProtocolFrame {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = client
                .next_frame()
                .await
                .expect("synthetic production frame")
                .expect("open connection");
            if frame.envelope.kind == kind {
                return frame;
            }
        }
    })
    .await
    .expect("expected synthetic voice frame")
}

async fn initial_voice_capability(
    client: &mut client::Connection,
) -> protocol::VoiceCapabilitiesPayload {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bootstrapped = false;
        let mut capability = None;
        while !bootstrapped || capability.is_none() {
            let frame = client
                .next_frame()
                .await
                .expect("initial server frame")
                .expect("open connection");
            match frame.envelope.kind {
                FrameKind::HostBootstrap => bootstrapped = true,
                FrameKind::VoiceCapabilities => {
                    capability = Some(frame.envelope.parse_payload().unwrap())
                }
                _ => {}
            }
        }
        capability.unwrap()
    })
    .await
    .expect("initial bootstrap and voice capability")
}

async fn refreshed_voice_capability(
    client: &mut client::Connection,
) -> protocol::VoiceCapabilitiesPayload {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut settings_received = false;
        let mut capability = None;
        while !settings_received || capability.is_none() {
            let frame = client
                .next_frame()
                .await
                .expect("settings update frame")
                .expect("open connection");
            match frame.envelope.kind {
                FrameKind::HostSettings => {
                    let payload: protocol::HostSettingsPayload =
                        frame.envelope.parse_payload().unwrap();
                    settings_received = payload.settings.voice.enabled
                        && payload.settings.voice.aws_region.as_deref() == Some("us-east-1");
                }
                FrameKind::VoiceCapabilities => {
                    let payload: protocol::VoiceCapabilitiesPayload =
                        frame.envelope.parse_payload().unwrap();
                    if payload.nova_available {
                        capability = Some(payload);
                    }
                }
                _ => {}
            }
        }
        capability.unwrap()
    })
    .await
    .expect("fresh settings and voice capability")
}

#[tokio::test]
async fn voice_settings_refresh_capabilities_for_every_live_connection() {
    let store = tempfile::tempdir().unwrap();
    let host = server::spawn_host_with_mock_backend(
        store.path().join("sessions.json"),
        store.path().join("projects.json"),
        store.path().join("settings.json"),
    )
    .unwrap();

    let (writer_io, writer_server_io) = tokio::io::duplex(64 * 1024);
    let writer_host = host.clone();
    let writer_task = tokio::spawn(async move {
        let connection = server::accept(&server::ServerConfig::current(), writer_server_io)
            .await
            .unwrap();
        server::run_connection_with_synthetic_voice(connection, writer_host)
            .await
            .unwrap();
    });
    let mut writer = client::connect(&client::ClientConfig::current(), writer_io)
        .await
        .unwrap();

    let (observer_io, observer_server_io) = tokio::io::duplex(64 * 1024);
    let observer_host = host.clone();
    let observer_task = tokio::spawn(async move {
        let connection = server::accept(&server::ServerConfig::current(), observer_server_io)
            .await
            .unwrap();
        server::run_mobile_connection(connection, observer_host)
            .await
            .unwrap();
    });
    let mut observer = client::connect(&client::ClientConfig::current(), observer_io)
        .await
        .unwrap();

    let writer_initial = initial_voice_capability(&mut writer).await;
    let observer_initial = initial_voice_capability(&mut observer).await;
    assert!(!writer_initial.nova_available);
    assert!(writer_initial.native_capture);
    assert!(!observer_initial.nova_available);
    assert!(observer_initial.browser_capture);

    writer
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::VoiceAwsRegion {
                region: Some("us-east-1".into()),
            },
        })
        .await
        .unwrap();
    writer
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::VoiceEnabled { enabled: true },
        })
        .await
        .unwrap();

    let writer_refreshed = refreshed_voice_capability(&mut writer).await;
    let observer_refreshed = refreshed_voice_capability(&mut observer).await;
    assert!(writer_refreshed.nova_available);
    assert!(writer_refreshed.native_capture);
    assert!(!writer_refreshed.browser_capture);
    assert!(observer_refreshed.nova_available);
    assert!(!observer_refreshed.native_capture);
    assert!(observer_refreshed.browser_capture);

    drop(writer);
    drop(observer);
    tokio::time::timeout(Duration::from_secs(2), writer_task)
        .await
        .expect("writer connection teardown")
        .expect("writer server task");
    tokio::time::timeout(Duration::from_secs(2), observer_task)
        .await
        .expect("observer connection teardown")
        .expect("observer server task");
}

#[tokio::test]
async fn synthetic_voice_full_lifecycle_over_production_session_path() {
    let store = tempfile::tempdir().unwrap();
    let host = server::spawn_host_with_mock_backend(
        store.path().join("sessions.json"),
        store.path().join("projects.json"),
        store.path().join("settings.json"),
    )
    .unwrap();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server_host = host.clone();
    let server_task = tokio::spawn(async move {
        let connection = server::accept(&server::ServerConfig::current(), server_io)
            .await
            .unwrap();
        server::run_connection_with_synthetic_voice(connection, server_host)
            .await
            .unwrap();
    });
    let mut client = client::connect(&client::ClientConfig::current(), client_io)
        .await
        .unwrap();
    let _ = next_kind(&mut client, FrameKind::HostBootstrap).await;
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::VoiceAwsRegion {
                region: Some("us-east-1".into()),
            },
        })
        .await
        .unwrap();
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::VoiceEnabled { enabled: true },
        })
        .await
        .unwrap();
    for _ in 0..2 {
        let _ = next_kind(&mut client, FrameKind::HostSettings).await;
    }
    client
        .spawn_agent(protocol::SpawnAgentPayload {
            name: Some("voice target".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: protocol::SpawnAgentParams::New {
                workspace_roots: vec![],
                prompt: "prepare".into(),
                images: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .unwrap();
    let new_agent: protocol::NewAgentPayload = next_kind(&mut client, FrameKind::NewAgent)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    let target = protocol::VoiceTarget {
        agent_id: new_agent.agent_id,
        instance_stream: new_agent.instance_stream,
    };
    let formats = vec![protocol::VoiceFormatPair {
        uplink: protocol::VoiceAudioFormat::opus(48_000),
        downlink: protocol::VoiceAudioFormat::opus(24_000),
    }];
    let start = protocol::VoiceStartPayload {
        generation: 1,
        target: target.clone(),
        formats: formats.clone(),
    };
    let start = Envelope::from_payload(
        StreamPath("/voice".into()),
        FrameKind::VoiceStart,
        0,
        &start,
    )
    .unwrap();
    protocol::write_envelope(&mut client.writer, &start)
        .await
        .unwrap();
    let accepted: protocol::VoiceAcceptedPayload = next_kind(&mut client, FrameKind::VoiceAccepted)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    let mut encoder =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut packet = vec![0; 1275];
    let len = encoder.encode(&[0i16; 960], &mut packet).unwrap();
    packet.truncate(len);
    let audio = VoiceAudioPayload {
        session_id: accepted.session_id.clone(),
        generation: 1,
        direction: VoiceDirection::Input,
        first_media_seq: 0,
        timestamp_samples_48k: 0,
        packet_lengths: vec![len as u16],
    };
    let retained_probe = packet.clone();
    let audio = protocol::ProtocolFrame {
        envelope: Envelope::from_payload(
            StreamPath(format!("/voice/{}", accepted.session_id.0)),
            FrameKind::VoiceAudio,
            0,
            &audio,
        )
        .unwrap(),
        binary: packet,
    };
    protocol::write_frame(&mut client.writer, &audio)
        .await
        .unwrap();
    let transcript: protocol::VoiceTranscriptPayload =
        next_kind(&mut client, FrameKind::VoiceTranscript)
            .await
            .envelope
            .parse_payload()
            .unwrap();
    assert_eq!(transcript.text, "synthetic request");
    let state: protocol::VoiceStatePayload = next_kind(&mut client, FrameKind::VoiceState)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(state.state, protocol::VoiceSessionState::AgentWorking);
    let mut saw_progress = false;
    let final_transcript = loop {
        let transcript: protocol::VoiceTranscriptPayload =
            next_kind(&mut client, FrameKind::VoiceTranscript)
                .await
                .envelope
                .parse_payload()
                .unwrap();
        if transcript.speaker == protocol::VoiceTranscriptSpeaker::Progress {
            saw_progress = true;
        }
        if transcript.is_final {
            break transcript;
        }
    };
    assert!(
        saw_progress,
        "long-running agent tool progress traverses the provider/session path"
    );
    assert!(final_transcript.is_final);
    let _ = next_kind(&mut client, FrameKind::VoiceOutput).await;
    let output = next_kind(&mut client, FrameKind::VoiceAudio).await;
    assert!(!output.binary.is_empty());
    // Downlink audio must ride its dedicated sub-stream and start its OWN
    // envelope seq at 0. When audio shared the JSON stream's counter, the
    // frontend validator (which never sees audio frames — the shell consumes
    // them natively) desynced at Nova's first spoken word and grayed out the
    // whole connection.
    assert_eq!(
        output.envelope.stream,
        StreamPath(format!("/voice/{}/audio", accepted.session_id.0)),
        "downlink audio must use the dedicated audio sub-stream"
    );
    assert_eq!(
        output.envelope.seq, 0,
        "the audio sub-stream must own a fresh seq counter, not inherit the \
         JSON envelope stream's count"
    );
    let hands_free_audio = VoiceAudioPayload {
        session_id: accepted.session_id.clone(),
        generation: 1,
        direction: VoiceDirection::Input,
        first_media_seq: 1,
        timestamp_samples_48k: 960,
        packet_lengths: vec![retained_probe.len() as u16],
    };
    protocol::write_frame(
        &mut client.writer,
        &protocol::ProtocolFrame {
            envelope: Envelope::from_payload(
                StreamPath(format!("/voice/{}", accepted.session_id.0)),
                FrameKind::VoiceAudio,
                1,
                &hands_free_audio,
            )
            .unwrap(),
            binary: retained_probe.clone(),
        },
    )
    .await
    .unwrap();
    let hands_free_interrupted: protocol::VoiceStatePayload =
        next_kind(&mut client, FrameKind::VoiceState)
            .await
            .envelope
            .parse_payload()
            .unwrap();
    assert_eq!(
        hands_free_interrupted.state,
        protocol::VoiceSessionState::Interrupting
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            next_kind(&mut client, FrameKind::VoiceAudio)
        )
        .await
        .is_err(),
        "Nova-detected interruption must purge queued and reject stale output"
    );
    let interrupt = protocol::VoiceSessionPayload {
        session_id: accepted.session_id.clone(),
        generation: 1,
    };
    let interrupt = Envelope::from_payload(
        StreamPath(format!("/voice/{}", accepted.session_id.0)),
        FrameKind::VoiceInterrupt,
        2,
        &interrupt,
    )
    .unwrap();
    protocol::write_envelope(&mut client.writer, &interrupt)
        .await
        .unwrap();
    let interrupted: protocol::VoiceStatePayload = next_kind(&mut client, FrameKind::VoiceState)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(interrupted.state, protocol::VoiceSessionState::Interrupting);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            next_kind(&mut client, FrameKind::VoiceAudio)
        )
        .await
        .is_err(),
        "stale provider output after button interrupt must not re-enter production framing"
    );
    let stop = protocol::VoiceStopPayload {
        session_id: accepted.session_id.clone(),
        generation: 1,
        reason: protocol::VoiceStopReason::UserExited,
        stats: Default::default(),
    };
    let stop = Envelope::from_payload(
        StreamPath(format!("/voice/{}", accepted.session_id.0)),
        FrameKind::VoiceStop,
        3,
        &stop,
    )
    .unwrap();
    protocol::write_envelope(&mut client.writer, &stop)
        .await
        .unwrap();
    let stopped: protocol::VoiceStopPayload = next_kind(&mut client, FrameKind::VoiceStop)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(stopped.generation, 1);
    let restart = protocol::VoiceStartPayload {
        generation: 2,
        target: target.clone(),
        formats: formats.clone(),
    };
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            1,
            &restart,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let late = VoiceAudioPayload {
        session_id: accepted.session_id.clone(),
        generation: 1,
        direction: VoiceDirection::Input,
        first_media_seq: 1,
        timestamp_samples_48k: 960,
        packet_lengths: vec![retained_probe.len() as u16],
    };
    protocol::write_frame(
        &mut client.writer,
        &protocol::ProtocolFrame {
            envelope: Envelope::from_payload(
                StreamPath(format!("/voice/{}", accepted.session_id.0)),
                FrameKind::VoiceAudio,
                4,
                &late,
            )
            .unwrap(),
            binary: retained_probe.clone(),
        },
    )
    .await
    .unwrap();
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath(format!("/voice/{}", accepted.session_id.0)),
            FrameKind::VoiceInputEnd,
            5,
            &protocol::VoiceSessionPayload {
                session_id: accepted.session_id.clone(),
                generation: 1,
            },
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let restarted: protocol::VoiceAcceptedPayload =
        next_kind(&mut client, FrameKind::VoiceAccepted)
            .await
            .envelope
            .parse_payload()
            .unwrap();
    assert_eq!(
        restarted.generation, 2,
        "every activation reaches a fresh accepted server session"
    );
    let second_stop = protocol::VoiceStopPayload {
        session_id: restarted.session_id.clone(),
        generation: 2,
        reason: protocol::VoiceStopReason::UserExited,
        stats: Default::default(),
    };
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath(format!("/voice/{}", restarted.session_id.0)),
            FrameKind::VoiceStop,
            0,
            &second_stop,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let second_stopped: protocol::VoiceStopPayload = next_kind(&mut client, FrameKind::VoiceStop)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(
        second_stopped.generation, 2,
        "late generation one media cannot stop generation two"
    );
    let third = protocol::VoiceStartPayload {
        generation: 3,
        target: target.clone(),
        formats,
    };
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            2,
            &third,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let third: protocol::VoiceAcceptedPayload = next_kind(&mut client, FrameKind::VoiceAccepted)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    client.close_agent(&target.instance_stream).await.unwrap();
    let target_stopped: protocol::VoiceStopPayload = next_kind(&mut client, FrameKind::VoiceStop)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(target_stopped.session_id, third.session_id);
    assert_eq!(
        target_stopped.reason,
        protocol::VoiceStopReason::AgentClosed,
        "target ownership loss synchronously ends the voice actor"
    );
    client
        .spawn_agent(protocol::SpawnAgentPayload {
            name: Some("transport-loss target".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: protocol::SpawnAgentParams::New {
                workspace_roots: vec![],
                prompt: "prepare".into(),
                images: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .unwrap();
    let transport_agent: protocol::NewAgentPayload = next_kind(&mut client, FrameKind::NewAgent)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    let fourth = protocol::VoiceStartPayload {
        generation: 4,
        target: protocol::VoiceTarget {
            agent_id: transport_agent.agent_id,
            instance_stream: transport_agent.instance_stream,
        },
        formats: vec![protocol::VoiceFormatPair {
            uplink: protocol::VoiceAudioFormat::opus(48_000),
            downlink: protocol::VoiceAudioFormat::opus(24_000),
        }],
    };
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            3,
            &fourth,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let fourth: protocol::VoiceAcceptedPayload = next_kind(&mut client, FrameKind::VoiceAccepted)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(fourth.generation, 4);
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("transport loss tears down the production connection and voice actor")
        .expect("server task");
    for path in std::fs::read_dir(store.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
    {
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !bytes
                .windows(retained_probe.len())
                .any(|window| window == retained_probe),
            "stores must not retain audio bytes"
        );
        assert!(!String::from_utf8_lossy(&bytes).contains("voice_audio"));
    }
}

fn mqtt_config(
    broker: &support::LocalMqttBroker,
    room: RoomId,
    psk: PreSharedKey,
    role: ParticipantRole,
) -> MqttConnectConfig {
    MqttConnectConfig {
        endpoint: BrokerEndpoint {
            url: broker.broker_url.clone(),
            auth: BrokerAuth::Anonymous,
        },
        room,
        psk,
        role,
    }
}

#[tokio::test]
async fn production_writer_interleaves_four_megabyte_bulk() {
    let broker = support::start_plain_mqtt_broker().expect("start real rumqttd broker");
    let room = RoomId::random();
    let psk = PreSharedKey::random();
    let (host, client) = tokio::join!(
        mqtt_transport::connect_ephemeral(mqtt_config(
            &broker,
            room,
            psk.clone(),
            ParticipantRole::Host
        )),
        mqtt_transport::connect_ephemeral(mqtt_config(&broker, room, psk, ParticipantRole::Client)),
    );
    let host = host.expect("host production MQTT byte transport");
    let client = client.expect("client production MQTT byte transport");
    let (_, host_writer) = tokio::io::split(host);
    let (client_reader, _) = tokio::io::split(client);
    let bulk_stream = StreamPath("/project/bulk".into());
    let voice_stream = StreamPath("/voice/synthetic".into());
    let initial = HashMap::from([(bulk_stream.clone(), 0), (voice_stream.clone(), 0)]);
    let (probe, writer) = server::start_production_writer_probe(Box::new(host_writer), initial);
    probe
        .send(
            bulk_stream,
            FrameKind::ProjectFileContents,
            serde_json::json!({"contents":"x".repeat(4 * 1024 * 1024)}),
            Vec::new(),
        )
        .expect("queue real four MiB bulk envelope");
    tokio::time::timeout(Duration::from_secs(2), async {
        while probe.records_written() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("production writer starts first bulk record");

    let queued_at = Instant::now();
    let audio = VoiceAudioPayload {
        session_id: VoiceSessionId("synthetic".into()),
        generation: 1,
        direction: VoiceDirection::Output,
        first_media_seq: 0,
        timestamp_samples_48k: 0,
        packet_lengths: vec![3],
    };
    probe
        .send(
            voice_stream,
            FrameKind::VoiceAudio,
            serde_json::to_value(audio).unwrap(),
            vec![1, 2, 3],
        )
        .unwrap();
    let stop = protocol::VoiceStopPayload {
        session_id: VoiceSessionId("synthetic".into()),
        generation: 1,
        reason: protocol::VoiceStopReason::UserExited,
        stats: Default::default(),
    };
    probe
        .send(
            StreamPath("/voice/synthetic".into()),
            FrameKind::VoiceStop,
            serde_json::to_value(stop).unwrap(),
            Vec::new(),
        )
        .unwrap();
    probe
        .send(
            StreamPath("/project/bulk".into()),
            FrameKind::HeartbeatAck,
            serde_json::json!({}),
            Vec::new(),
        )
        .unwrap();

    let mut reader = FrameReader::new(client_reader);
    let mut control_latency = None;
    let mut audio_latency = None;
    let mut saw_audio = false;
    let mut saw_bulk = false;
    let mut saw_same_stream_control = false;
    let mut voice_sequences = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while control_latency.is_none() || !saw_audio || !saw_bulk || !saw_same_stream_control {
            let frame = reader
                .read_frame()
                .await
                .expect("production framed MQTT read")
                .expect("frame");
            match frame.envelope.kind {
                FrameKind::VoiceStop => {
                    assert!(
                        !saw_bulk,
                        "control suffered head-of-line blocking behind the complete 4 MiB transfer"
                    );
                    control_latency = Some(queued_at.elapsed());
                    voice_sequences.push(frame.envelope.seq);
                }
                FrameKind::VoiceAudio => {
                    assert!(
                        !saw_bulk,
                        "audio suffered head-of-line blocking behind the complete 4 MiB transfer"
                    );
                    saw_audio = true;
                    audio_latency = Some(queued_at.elapsed());
                    voice_sequences.push(frame.envelope.seq);
                }
                FrameKind::ProjectFileContents => saw_bulk = true,
                FrameKind::HeartbeatAck => {
                    assert!(
                        saw_bulk,
                        "same-stream control cannot overtake an incomplete fragmented envelope"
                    );
                    saw_same_stream_control = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("bulk, voice, and control all traverse real MQTT");
    let latency = control_latency.unwrap();
    assert!(
        latency <= Duration::from_millis(100),
        "control latency {latency:?} exceeded the defensible local-production bound"
    );
    let latency = audio_latency.unwrap();
    assert!(
        latency <= Duration::from_millis(100),
        "audio latency {latency:?} exceeded the defensible local-production bound"
    );
    assert_eq!(
        voice_sequences,
        vec![0, 1],
        "priority and drops must not create sequence holes"
    );
    probe.close();
    writer.await.expect("writer task").expect("writer success");
}
