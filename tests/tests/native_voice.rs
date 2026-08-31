use std::collections::HashMap;
use std::time::{Duration, Instant};

mod support;

use mqtt_transport::{
    BrokerAuth, BrokerEndpoint, MqttConnectConfig, ParticipantRole, PreSharedKey, RoomId,
};
use protocol::{
    Envelope, FrameKind, FrameReader, StreamPath, VoiceAudioPayload, VoiceDirection, VoiceSessionId,
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
                    let payload: settings_model::HostSettingsPayload =
                        frame.envelope.parse_payload().unwrap();
                    settings_received = payload.settings.voice.enabled
                        && payload.settings.voice.aws_region.as_deref() == Some("us-east-1")
                        && payload.settings.voice.dictation_enabled
                        && payload.settings.voice.dictation_region.as_deref() == Some("us-west-2");
                }
                FrameKind::VoiceCapabilities => {
                    let payload: protocol::VoiceCapabilitiesPayload =
                        frame.envelope.parse_payload().unwrap();
                    if payload.nova_available && payload.dictation_available {
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
        server::run_mobile_connection(
            connection,
            observer_host,
            protocol::MobileDeviceId("native-voice-observer".to_owned()),
        )
        .await
        .unwrap();
    });
    let mut observer = client::connect(&client::ClientConfig::current(), observer_io)
        .await
        .unwrap();

    let writer_initial = initial_voice_capability(&mut writer).await;
    let observer_initial = initial_voice_capability(&mut observer).await;
    assert!(!writer_initial.nova_available);
    assert!(!writer_initial.dictation_available);
    assert!(writer_initial.native_capture);
    assert!(!observer_initial.nova_available);
    assert!(!observer_initial.dictation_available);
    assert!(observer_initial.browser_capture);

    writer
        .replace_setting(
            "/voice/aws_region",
            Some("us-east-1"),
            Option::<String>::None,
        )
        .await
        .unwrap();
    writer
        .replace_setting("/voice/enabled", true, false)
        .await
        .unwrap();
    writer
        .replace_setting(
            "/voice/dictation_region",
            Some("us-west-2"),
            Option::<String>::None,
        )
        .await
        .unwrap();
    writer
        .replace_setting("/voice/dictation_enabled", true, false)
        .await
        .unwrap();

    let writer_refreshed = refreshed_voice_capability(&mut writer).await;
    let observer_refreshed = refreshed_voice_capability(&mut observer).await;
    assert!(writer_refreshed.nova_available);
    assert!(writer_refreshed.dictation_available);
    assert!(writer_refreshed.native_capture);
    assert!(!writer_refreshed.browser_capture);
    assert!(observer_refreshed.nova_available);
    assert!(observer_refreshed.dictation_available);
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
        .replace_setting(
            "/voice/aws_region",
            Some("us-east-1"),
            Option::<String>::None,
        )
        .await
        .unwrap();
    client
        .replace_setting("/voice/enabled", true, false)
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
        request: protocol::VoiceRequest::Conversation {
            target: target.clone(),
            formats: formats.clone(),
        },
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
    // Corrected assertion (evidence: beta.60 live sessions 6fcbf6e5/c3c225e1
    // — exactly one voice_output per session while transcripts flowed for the
    // whole session; every response after the first was silently discarded by
    // the output-generation filter). Nova orders each content's audio frames
    // BEFORE its INTERRUPTED marker, so audio arriving after a
    // provider-reported interruption is the model's NEXT response at the
    // unchanged generation. The old assertion ("no VoiceAudio within 100ms")
    // encoded the mute itself; the contract it reached for — the interrupted
    // response's tail must not play — is still enforced: queued frames are
    // purged (discard_voice_audio) and the client flushes playback on the
    // Interrupting state. The follow-on response must now be re-announced and
    // reach the audio sub-stream on the same continuing seq counter.
    let _ = next_kind(&mut client, FrameKind::VoiceOutput).await;
    let resumed = next_kind(&mut client, FrameKind::VoiceAudio).await;
    assert!(
        !resumed.binary.is_empty(),
        "the response following a Nova-detected interruption must play"
    );
    assert_eq!(
        resumed.envelope.stream,
        StreamPath(format!("/voice/{}/audio", accepted.session_id.0)),
        "post-interrupt audio stays on the dedicated audio sub-stream"
    );
    assert_eq!(
        resumed.envelope.seq, 1,
        "post-interrupt audio continues the audio sub-stream's seq counter"
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
        request: protocol::VoiceRequest::Conversation {
            target: target.clone(),
            formats: formats.clone(),
        },
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
        request: protocol::VoiceRequest::Conversation {
            target: target.clone(),
            formats,
        },
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
        request: protocol::VoiceRequest::Conversation {
            target: protocol::VoiceTarget {
                agent_id: transport_agent.agent_id,
                instance_stream: transport_agent.instance_stream,
            },
            formats: vec![protocol::VoiceFormatPair {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
                downlink: protocol::VoiceAudioFormat::opus(24_000),
            }],
        },
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

#[tokio::test]
async fn synthetic_dictation_is_input_only_and_flushes_final_text() {
    let store = tempfile::tempdir().unwrap();
    let host = server::spawn_host_with_mock_backend(
        store.path().join("sessions.json"),
        store.path().join("projects.json"),
        store.path().join("settings.json"),
    )
    .unwrap();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let connection = server::accept(&server::ServerConfig::current(), server_io)
            .await
            .unwrap();
        server::run_connection_with_synthetic_voice(connection, host)
            .await
            .unwrap();
    });
    let mut client = client::connect(&client::ClientConfig::current(), client_io)
        .await
        .unwrap();
    let _ = next_kind(&mut client, FrameKind::HostBootstrap).await;
    client
        .replace_setting(
            "/voice/dictation_region",
            Some("us-west-2"),
            Option::<String>::None,
        )
        .await
        .unwrap();
    client
        .replace_setting("/voice/dictation_enabled", true, false)
        .await
        .unwrap();

    let dictation_request = || protocol::VoiceRequest::Dictation {
        formats: vec![protocol::VoiceAudioFormat::opus(48_000)],
    };
    let start = protocol::VoiceStartPayload {
        generation: 1,
        request: dictation_request(),
    };
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            0,
            &start,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let accepted: protocol::VoiceAcceptedPayload = next_kind(&mut client, FrameKind::VoiceAccepted)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(accepted.request.mode(), protocol::VoiceMode::Dictation);
    assert!(matches!(
        accepted.request,
        protocol::VoiceAcceptedRequest::Dictation { .. }
    ));

    let mut encoder =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut packet = vec![0; 1275];
    let len = encoder.encode(&[0i16; 960], &mut packet).unwrap();
    packet.truncate(len);
    for media_seq in 0..2 {
        let audio = VoiceAudioPayload {
            session_id: accepted.session_id.clone(),
            generation: 1,
            direction: VoiceDirection::Input,
            first_media_seq: media_seq,
            timestamp_samples_48k: media_seq * 960,
            packet_lengths: vec![packet.len() as u16],
        };
        protocol::write_frame(
            &mut client.writer,
            &protocol::ProtocolFrame {
                envelope: Envelope::from_payload(
                    StreamPath(format!("/voice/{}", accepted.session_id.0)),
                    FrameKind::VoiceAudio,
                    media_seq,
                    &audio,
                )
                .unwrap(),
                binary: packet.clone(),
            },
        )
        .await
        .unwrap();
    }
    for expected in ["synthetic", "synthetic dictation"] {
        let transcript: protocol::VoiceTranscriptPayload =
            next_dictation_kind(&mut client, FrameKind::VoiceTranscript)
                .await
                .envelope
                .parse_payload()
                .unwrap();
        assert_eq!(transcript.text, expected);
        assert!(!transcript.is_final);
        assert_eq!(transcript.speaker, protocol::VoiceTranscriptSpeaker::User);
        assert!(transcript.message_id.is_none());
    }

    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath(format!("/voice/{}", accepted.session_id.0)),
            FrameKind::VoiceInputEnd,
            2,
            &protocol::VoiceSessionPayload {
                session_id: accepted.session_id.clone(),
                generation: 1,
            },
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let final_transcript: protocol::VoiceTranscriptPayload =
        next_dictation_kind(&mut client, FrameKind::VoiceTranscript)
            .await
            .envelope
            .parse_payload()
            .unwrap();
    assert_eq!(final_transcript.text, "synthetic dictation");
    assert!(final_transcript.is_final);
    let completed: protocol::VoiceStopPayload =
        next_dictation_kind(&mut client, FrameKind::VoiceStop)
            .await
            .envelope
            .parse_payload()
            .unwrap();
    assert_eq!(
        completed.reason,
        protocol::VoiceStopReason::ProviderCompleted
    );

    let cancel_start = protocol::VoiceStartPayload {
        generation: 2,
        request: dictation_request(),
    };
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            1,
            &cancel_start,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let cancelled: protocol::VoiceAcceptedPayload =
        next_kind(&mut client, FrameKind::VoiceAccepted)
            .await
            .envelope
            .parse_payload()
            .unwrap();
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath(format!("/voice/{}", cancelled.session_id.0)),
            FrameKind::VoiceStop,
            0,
            &protocol::VoiceStopPayload {
                session_id: cancelled.session_id.clone(),
                generation: 2,
                reason: protocol::VoiceStopReason::UserExited,
                stats: Default::default(),
            },
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let cancelled_stop: protocol::VoiceStopPayload =
        next_dictation_kind(&mut client, FrameKind::VoiceStop)
            .await
            .envelope
            .parse_payload()
            .unwrap();
    assert_eq!(cancelled_stop.reason, protocol::VoiceStopReason::UserExited);

    client
        .replace_setting("/voice/dictation_language_code", "en-ZZ", "en-US")
        .await
        .unwrap();
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            2,
            &protocol::VoiceStartPayload {
                generation: 3,
                request: dictation_request(),
            },
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let failure: protocol::VoiceErrorPayload = next_kind(&mut client, FrameKind::VoiceError)
        .await
        .envelope
        .parse_payload()
        .unwrap();
    assert_eq!(failure.code, protocol::VoiceErrorCode::InvalidConfiguration);
    assert!(!failure.retryable);
    assert!(failure.detail.is_none());

    client
        .replace_setting("/voice/dictation_language_code", "en-US", "en-ZZ")
        .await
        .unwrap();
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            3,
            &protocol::VoiceStartPayload {
                generation: 4,
                request: dictation_request(),
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
    assert_eq!(restarted.generation, 4);
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("dictation transport teardown")
        .expect("server task");
    for path in std::fs::read_dir(store.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
    {
        let bytes = std::fs::read(path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("synthetic dictation"));
        assert!(!bytes.windows(packet.len()).any(|window| window == packet));
    }
}

async fn next_dictation_kind(
    client: &mut client::Connection,
    expected: FrameKind,
) -> protocol::ProtocolFrame {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = client
                .next_frame()
                .await
                .expect("synthetic dictation frame")
                .expect("open connection");
            assert!(
                !matches!(
                    frame.envelope.kind,
                    FrameKind::VoiceAudio | FrameKind::VoiceOutput | FrameKind::ChatEvent
                ),
                "dictation must not emit output audio, VoiceOutput, or tool traffic"
            );
            if frame.envelope.kind == expected {
                return frame;
            }
        }
    })
    .await
    .expect("expected synthetic dictation frame")
}

#[tokio::test]
#[ignore = "paid Amazon Transcribe stream; set TYDE_RUN_REAL_TRANSCRIBE_TESTS=1 and fixture variables"]
async fn real_amazon_transcribe_streams_prerecorded_dictation() {
    assert_eq!(
        std::env::var("TYDE_RUN_REAL_TRANSCRIBE_TESTS")
            .ok()
            .as_deref(),
        Some("1"),
        "set TYDE_RUN_REAL_TRANSCRIBE_TESTS=1 to authorize paid Transcribe coverage"
    );
    let region = std::env::var("TYDE_REAL_TRANSCRIBE_REGION")
        .expect("set TYDE_REAL_TRANSCRIBE_REGION to the explicit AWS region");
    let fixture_path = std::env::var("TYDE_REAL_TRANSCRIBE_PCM")
        .expect("set TYDE_REAL_TRANSCRIBE_PCM to deterministic 48 kHz mono signed-LE PCM");
    let expected = std::env::var("TYDE_REAL_TRANSCRIBE_EXPECTED")
        .expect("set TYDE_REAL_TRANSCRIBE_EXPECTED to text contained in the final transcript")
        .to_ascii_lowercase();
    let bytes = std::fs::read(&fixture_path).expect("read deterministic speech fixture");
    assert!(bytes.len() >= 48_000);
    let (sample_bytes, remainder) = bytes.as_chunks::<2>();
    assert!(remainder.is_empty());
    let samples: Vec<i16> = sample_bytes
        .iter()
        .map(|sample| i16::from_le_bytes(*sample))
        .collect();

    let store = tempfile::tempdir().unwrap();
    let host = server::spawn_host_with_mock_backend(
        store.path().join("sessions.json"),
        store.path().join("projects.json"),
        store.path().join("settings.json"),
    )
    .unwrap();
    let (client_io, server_io) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        let connection = server::accept(&server::ServerConfig::current(), server_io)
            .await
            .unwrap();
        server::run_connection(connection, host).await.unwrap();
    });
    let mut client = client::connect(&client::ClientConfig::current(), client_io)
        .await
        .unwrap();
    let _ = next_kind(&mut client, FrameKind::HostBootstrap).await;
    if let Ok(profile) = std::env::var("TYDE_REAL_TRANSCRIBE_AWS_PROFILE") {
        client
            .replace_setting("/voice/aws_profile", Some(profile), Option::<String>::None)
            .await
            .unwrap();
    }
    client
        .replace_setting(
            "/voice/dictation_region",
            Some(region),
            Option::<String>::None,
        )
        .await
        .unwrap();
    client
        .replace_setting("/voice/dictation_enabled", true, false)
        .await
        .unwrap();
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            0,
            &protocol::VoiceStartPayload {
                generation: 1,
                request: protocol::VoiceRequest::Dictation {
                    formats: vec![protocol::VoiceAudioFormat::opus(48_000)],
                },
            },
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let accepted = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let frame = client.next_frame().await.unwrap().unwrap();
            match frame.envelope.kind {
                FrameKind::VoiceAccepted => {
                    break frame
                        .envelope
                        .parse_payload::<protocol::VoiceAcceptedPayload>()
                        .unwrap();
                }
                FrameKind::VoiceError => {
                    let error: protocol::VoiceErrorPayload =
                        frame.envelope.parse_payload().unwrap();
                    panic!("real Transcribe startup failed: {error:?}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("real Transcribe startup bound");

    let mut encoder =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut media_seq = 0_u64;
    for frame_samples in samples.chunks(960) {
        let mut padded = [0_i16; 960];
        padded[..frame_samples.len()].copy_from_slice(frame_samples);
        let mut opus_packet = vec![0; 1275];
        let len = encoder.encode(&padded, &mut opus_packet).unwrap();
        opus_packet.truncate(len);
        let audio = VoiceAudioPayload {
            session_id: accepted.session_id.clone(),
            generation: 1,
            direction: VoiceDirection::Input,
            first_media_seq: media_seq,
            timestamp_samples_48k: media_seq * 960,
            packet_lengths: vec![len as u16],
        };
        protocol::write_frame(
            &mut client.writer,
            &protocol::ProtocolFrame {
                envelope: Envelope::from_payload(
                    StreamPath(format!("/voice/{}", accepted.session_id.0)),
                    FrameKind::VoiceAudio,
                    media_seq,
                    &audio,
                )
                .unwrap(),
                binary: opus_packet,
            },
        )
        .await
        .unwrap();
        media_seq += 1;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    protocol::write_envelope(
        &mut client.writer,
        &Envelope::from_payload(
            StreamPath(format!("/voice/{}", accepted.session_id.0)),
            FrameKind::VoiceInputEnd,
            media_seq,
            &protocol::VoiceSessionPayload {
                session_id: accepted.session_id.clone(),
                generation: 1,
            },
        )
        .unwrap(),
    )
    .await
    .unwrap();

    let finalized = tokio::time::timeout(Duration::from_secs(30), async {
        let mut finalized = String::new();
        loop {
            let frame = client.next_frame().await.unwrap().unwrap();
            assert!(
                !matches!(
                    frame.envelope.kind,
                    FrameKind::VoiceAudio | FrameKind::VoiceOutput
                ),
                "real dictation must remain input-only"
            );
            match frame.envelope.kind {
                FrameKind::VoiceTranscript => {
                    let transcript: protocol::VoiceTranscriptPayload =
                        frame.envelope.parse_payload().unwrap();
                    assert_eq!(transcript.speaker, protocol::VoiceTranscriptSpeaker::User);
                    assert!(transcript.message_id.is_none());
                    if transcript.is_final {
                        if !finalized.is_empty() {
                            finalized.push(' ');
                        }
                        finalized.push_str(&transcript.text);
                    }
                }
                FrameKind::VoiceStop => {
                    let stop: protocol::VoiceStopPayload = frame.envelope.parse_payload().unwrap();
                    assert_eq!(stop.reason, protocol::VoiceStopReason::ProviderCompleted);
                    break finalized;
                }
                FrameKind::VoiceError => {
                    let error: protocol::VoiceErrorPayload =
                        frame.envelope.parse_payload().unwrap();
                    panic!("real Transcribe stream failed: {error:?}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("real Transcribe final flush bound");
    assert!(
        finalized.to_ascii_lowercase().contains(&expected),
        "final transcript {finalized:?} did not contain {expected:?}"
    );
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("real Transcribe connection teardown")
        .expect("server task");
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
