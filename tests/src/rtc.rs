use protocol::{MobileIceServer, MobileTurnUrl};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use turn::auth::{AuthHandler, generate_auth_key};
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::Server;
use turn::server::config::{ConnConfig, ServerConfig};

struct RelayAuth;
impl AuthHandler for RelayAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _source: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        if username != "tyde-test" || realm != "tyde" {
            return Err(turn::Error::ErrNoPermission);
        }
        Ok(generate_auth_key(username, realm, "relay-password"))
    }
}

pub async fn relay() -> (Server, MobileIceServer) {
    let conn = Arc::new(
        tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("TURN socket"),
    );
    let address = conn.local_addr().expect("TURN address");
    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                address: "127.0.0.1".to_owned(),
                net: Arc::new(webrtc_util::vnet::net::Net::new(None)),
            }),
        }],
        realm: "tyde".to_owned(),
        auth_handler: Arc::new(RelayAuth),
        channel_bind_timeout: Duration::ZERO,
        alloc_close_notify: None,
    })
    .await
    .expect("start real TURN server");
    (
        server,
        MobileIceServer {
            urls: vec![MobileTurnUrl(format!("turn:{address}?transport=udp"))],
            username: "tyde-test".to_owned(),
            credential: "relay-password".to_owned(),
        },
    )
}
