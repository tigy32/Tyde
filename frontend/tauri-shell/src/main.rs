enum CliMode {
    Gui,
    HostStdio,
    HostUds,
    HostStatusUds,
    HostLaunchUds,
    HostBridgeUds,
    McpBridge,
    Version,
    Help,
    Error(String),
}

fn main() {
    // Record this binary's real release version so the mobile Welcome/Reject/QR
    // payloads advertise the correct web/PWA bundle key.
    server::set_host_release_version(env!("CARGO_PKG_VERSION"));
    // The server's reqwest client uses no-provider rustls and panics ("No
    // provider set") at build time unless a default crypto provider is already
    // installed. Do this before any dispatch so every mode (GUI + host) is
    // covered.
    server::install_default_crypto_provider();
    match parse_cli_mode(std::env::args().skip(1)) {
        CliMode::Gui => tauri_shell::run(),
        CliMode::HostStdio => {
            if let Err(err) = tauri_shell::run_host_stdio() {
                eprintln!("ERROR: {err}");
                std::process::exit(1);
            }
        }
        CliMode::HostUds => {
            if let Err(err) = tauri_shell::run_host_uds() {
                eprintln!("ERROR: {err}");
                std::process::exit(1);
            }
        }
        CliMode::HostStatusUds => {
            if let Err(err) = tauri_shell::run_host_status_uds() {
                eprintln!("ERROR: {err}");
                std::process::exit(1);
            }
        }
        CliMode::HostLaunchUds => {
            if let Err(err) = tauri_shell::run_host_launch_uds() {
                eprintln!("ERROR: {err}");
                std::process::exit(1);
            }
        }
        CliMode::HostBridgeUds => {
            if let Err(err) = tauri_shell::run_host_bridge_uds() {
                eprintln!("ERROR: {err}");
                std::process::exit(1);
            }
        }
        CliMode::McpBridge => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap_or_else(|error| {
                    eprintln!("ERROR: Failed to create Tyde MCP bridge runtime: {error}");
                    std::process::exit(1);
                });
            if let Err(error) = runtime.block_on(server::mcp_bridge::run()) {
                eprintln!("ERROR: {error}");
                std::process::exit(1);
            }
        }
        CliMode::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        CliMode::Help => print_usage(),
        CliMode::Error(message) => {
            eprintln!("ERROR: {message}");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    }
}

fn parse_cli_mode<I>(args: I) -> CliMode
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let args = args
        .into_iter()
        .map(Into::into)
        .filter(|arg| !arg.starts_with("-psn_"))
        .collect::<Vec<_>>();

    if args.is_empty() {
        return CliMode::Gui;
    }

    if args.len() == 1 && matches!(args[0].as_str(), "-h" | "--help" | "help") {
        return CliMode::Help;
    }

    if args.len() == 1 && matches!(args[0].as_str(), "-V" | "--version" | "version") {
        return CliMode::Version;
    }

    if args.as_slice() == ["host", "--stdio"] {
        return CliMode::HostStdio;
    }

    if args.as_slice() == ["host", "--uds"] {
        return CliMode::HostUds;
    }

    if args.as_slice() == ["host", "--status-uds"] {
        return CliMode::HostStatusUds;
    }

    if args.as_slice() == ["host", "--launch-uds"] {
        return CliMode::HostLaunchUds;
    }

    if args.as_slice() == ["host", "--bridge-uds"] {
        return CliMode::HostBridgeUds;
    }

    // `hermes-mcp-bridge` is the name persisted in existing Hermes configs;
    // the bridge is shared by every backend that needs one, so both spellings
    // resolve to it.
    if args.as_slice() == ["mcp-bridge"] || args.as_slice() == ["hermes-mcp-bridge"] {
        return CliMode::McpBridge;
    }

    if args.len() == 2
        && args.iter().any(|arg| arg == "--headless")
        && args.iter().any(|arg| arg == "--stdio")
    {
        return CliMode::HostStdio;
    }

    if args.len() == 2
        && args.iter().any(|arg| arg == "--headless")
        && args.iter().any(|arg| arg == "--uds")
    {
        return CliMode::HostUds;
    }

    if args.len() == 2
        && args.iter().any(|arg| arg == "--headless")
        && args.iter().any(|arg| arg == "--status-uds")
    {
        return CliMode::HostStatusUds;
    }

    if args.len() == 2
        && args.iter().any(|arg| arg == "--headless")
        && args.iter().any(|arg| arg == "--launch-uds")
    {
        return CliMode::HostLaunchUds;
    }

    if args.len() == 2
        && args.iter().any(|arg| arg == "--headless")
        && args.iter().any(|arg| arg == "--bridge-uds")
    {
        return CliMode::HostBridgeUds;
    }

    match args.as_slice() {
        [host] if host == "host" => CliMode::Error(
            "missing transport for host mode; use `tyde host --stdio`, `tyde host --uds`, `tyde host --status-uds`, `tyde host --launch-uds`, or `tyde host --bridge-uds`"
                .to_owned(),
        ),
        [headless] if headless == "--headless" => CliMode::Error(
            "headless mode requires --stdio, --uds, --status-uds, --launch-uds, or --bridge-uds; use `tyde host --stdio`, `tyde host --uds`, `tyde host --status-uds`, `tyde host --launch-uds`, or `tyde host --bridge-uds`"
                .to_owned(),
        ),
        _ => CliMode::Error(format!("unknown arguments: {}", args.join(" "))),
    }
}

fn print_usage() {
    println!("Usage:");
    println!("  tyde                    Run the Tyde desktop app");
    println!("  tyde --version          Print the Tyde binary version");
    println!("  tyde host --stdio       Run a Tyde host over stdin/stdout");
    println!("  tyde host --uds         Run a Tyde host over ~/.tyde/tyde.sock");
    println!("  tyde host --status-uds  Check whether the Tyde UDS host is reachable");
    println!("  tyde host --launch-uds  Launch the Tyde UDS host in the background");
    println!("  tyde host --bridge-uds  Bridge stdin/stdout to a running Tyde UDS host");
    println!("  tyde mcp-bridge         Run the process-local Tyde MCP bridge");
    println!("  tyde --headless --stdio Alias for `tyde host --stdio`");
    println!("  tyde --headless --uds   Alias for `tyde host --uds`");
    println!("  tyde --headless --status-uds Alias for `tyde host --status-uds`");
    println!("  tyde --headless --launch-uds Alias for `tyde host --launch-uds`");
    println!("  tyde --headless --bridge-uds Alias for `tyde host --bridge-uds`");
}
