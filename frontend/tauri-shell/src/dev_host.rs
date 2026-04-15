use std::net::SocketAddr;

const AGENT_CONTROL_HOST_BIND_ENV: &str = "TYDE_AGENT_CONTROL_HOST_BIND_ADDR";
const DEV_HOST_BIND_ENV: &str = "TYDE_DEV_HOST_BIND_ADDR";

pub fn start_dev_host_listener(host: server::HostHandle) -> Result<Option<String>, String> {
    let Some(bind_addr) = resolve_bind_addr_from_env()? else {
        return Ok(None);
    };

    let listener = std::net::TcpListener::bind(bind_addr)
        .map_err(|err| format!("failed to bind host listener at {bind_addr}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set nonblocking dev host listener: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to resolve dev host listener addr: {err}"))?;

    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!("failed to create async dev host listener: {err}");
                return;
            }
        };

        loop {
            let (stream, remote_addr) = match listener.accept().await {
                Ok(parts) => parts,
                Err(err) => {
                    tracing::error!("dev host listener accept failed: {err}");
                    break;
                }
            };

            if !remote_addr.ip().is_loopback() {
                tracing::warn!("rejecting non-loopback dev host client {remote_addr}");
                continue;
            }

            let host = host.clone();
            tauri::async_runtime::spawn(async move {
                let config = server::ServerConfig::current();
                let connection = match server::accept(&config, stream).await {
                    Ok(connection) => connection,
                    Err(err) => {
                        tracing::warn!("dev host handshake failed: {err:?}");
                        return;
                    }
                };

                if let Err(err) = server::run_connection(connection, host).await {
                    tracing::warn!("dev host connection failed: {err:?}");
                }
            });
        }
    });

    Ok(Some(local_addr.to_string()))
}

fn resolve_bind_addr_from_env() -> Result<Option<SocketAddr>, String> {
    let agent_control = match std::env::var(AGENT_CONTROL_HOST_BIND_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(err) => {
            return Err(format!(
                "failed to read {AGENT_CONTROL_HOST_BIND_ENV}: {err}"
            ));
        }
    };
    let legacy = match std::env::var(DEV_HOST_BIND_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(err) => return Err(format!("failed to read {DEV_HOST_BIND_ENV}: {err}")),
    };

    let (name, raw) = match (agent_control, legacy) {
        (Some(primary), None) => (AGENT_CONTROL_HOST_BIND_ENV, primary),
        (None, Some(legacy)) => (DEV_HOST_BIND_ENV, legacy),
        (None, None) => return Ok(None),
        (Some(primary), Some(legacy)) if primary == legacy => {
            (AGENT_CONTROL_HOST_BIND_ENV, primary)
        }
        (Some(_), Some(_)) => {
            return Err(format!(
                "{AGENT_CONTROL_HOST_BIND_ENV} and {DEV_HOST_BIND_ENV} must match when both are set"
            ));
        }
    };

    let addr = raw
        .parse::<SocketAddr>()
        .map_err(|err| format!("invalid {name}='{raw}': {err}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("non-loopback {name} is not allowed: {addr}"));
    }

    Ok(Some(addr))
}
