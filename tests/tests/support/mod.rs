use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use protocol::BrokerUrl;
use rumqttd::{Broker, Config, ConnectionSettings, RouterConfig, ServerSettings};

pub struct LocalMqttBroker {
    pub broker_url: BrokerUrl,
    _broker_thread: thread::JoinHandle<()>,
}

pub fn start_plain_mqtt_broker() -> Result<LocalMqttBroker, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);

    let mut v5 = HashMap::new();
    v5.insert(
        "test".to_string(),
        ServerSettings {
            name: format!("tyde-mobile-pairing-test-{port}"),
            listen: SocketAddr::from(([127, 0, 0, 1], port)),
            tls: None,
            next_connection_delay_ms: 1,
            connections: ConnectionSettings {
                connection_timeout_ms: 60_000,
                max_payload_size: 2 * 1024 * 1024,
                max_inflight_count: 1_000,
                auth: None,
                external_auth: None,
                dynamic_filters: true,
            },
        },
    );

    let config = Config {
        id: port as usize,
        router: RouterConfig {
            max_connections: 1_000,
            max_outgoing_packet_count: 10_000,
            max_segment_size: 2 * 1024 * 1024,
            max_segment_count: 16,
            ..RouterConfig::default()
        },
        v5: Some(v5),
        ..Config::default()
    };

    let broker_thread = thread::spawn(move || {
        let mut broker = Broker::new(config);
        let _result = broker.start();
    });

    wait_for_port(port)?;

    Ok(LocalMqttBroker {
        broker_url: BrokerUrl::new(format!("mqtt://127.0.0.1:{port}"))?,
        _broker_thread: broker_thread,
    })
}

fn wait_for_port(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..100 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(_) => return Ok(()),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
    Err(format!("broker did not listen on port {port}").into())
}
