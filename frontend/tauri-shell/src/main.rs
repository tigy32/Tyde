enum CliMode {
    Gui,
    HostStdio,
    HostUds,
    HostStatusUds,
    HostLaunchUds,
    HostBridgeUds,
    Version,
    Help,
    Error(String),
}

fn main() {
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
    println!("  tyde --headless --stdio Alias for `tyde host --stdio`");
    println!("  tyde --headless --uds   Alias for `tyde host --uds`");
    println!("  tyde --headless --status-uds Alias for `tyde host --status-uds`");
    println!("  tyde --headless --launch-uds Alias for `tyde host --launch-uds`");
    println!("  tyde --headless --bridge-uds Alias for `tyde host --bridge-uds`");
}

#[cfg(test)]
mod tests {
    use super::{CliMode, parse_cli_mode};

    #[test]
    fn defaults_to_gui_mode() {
        assert!(matches!(parse_cli_mode(Vec::<String>::new()), CliMode::Gui));
    }

    #[test]
    fn ignores_macos_process_serial_number_argument() {
        assert!(matches!(
            parse_cli_mode(vec!["-psn_0_12345".to_string()]),
            CliMode::Gui
        ));
    }

    #[test]
    fn parses_host_stdio_subcommand() {
        assert!(matches!(
            parse_cli_mode(vec!["host".to_string(), "--stdio".to_string()]),
            CliMode::HostStdio
        ));
    }

    #[test]
    fn parses_host_uds_subcommand() {
        assert!(matches!(
            parse_cli_mode(vec!["host".to_string(), "--uds".to_string()]),
            CliMode::HostUds
        ));
    }

    #[test]
    fn parses_host_status_uds_subcommand() {
        assert!(matches!(
            parse_cli_mode(vec!["host".to_string(), "--status-uds".to_string()]),
            CliMode::HostStatusUds
        ));
    }

    #[test]
    fn parses_host_launch_uds_subcommand() {
        assert!(matches!(
            parse_cli_mode(vec!["host".to_string(), "--launch-uds".to_string()]),
            CliMode::HostLaunchUds
        ));
    }

    #[test]
    fn parses_host_bridge_uds_subcommand() {
        assert!(matches!(
            parse_cli_mode(vec!["host".to_string(), "--bridge-uds".to_string()]),
            CliMode::HostBridgeUds
        ));
    }

    #[test]
    fn parses_headless_stdio_alias() {
        assert!(matches!(
            parse_cli_mode(vec!["--headless".to_string(), "--stdio".to_string()]),
            CliMode::HostStdio
        ));
    }

    #[test]
    fn parses_headless_uds_alias() {
        assert!(matches!(
            parse_cli_mode(vec!["--headless".to_string(), "--uds".to_string()]),
            CliMode::HostUds
        ));
    }

    #[test]
    fn parses_headless_status_uds_alias() {
        assert!(matches!(
            parse_cli_mode(vec!["--headless".to_string(), "--status-uds".to_string()]),
            CliMode::HostStatusUds
        ));
    }

    #[test]
    fn parses_headless_launch_uds_alias() {
        assert!(matches!(
            parse_cli_mode(vec!["--headless".to_string(), "--launch-uds".to_string()]),
            CliMode::HostLaunchUds
        ));
    }

    #[test]
    fn parses_version_subcommand() {
        assert!(matches!(
            parse_cli_mode(vec!["--version".to_string()]),
            CliMode::Version
        ));
    }

    #[test]
    fn parses_headless_bridge_uds_alias() {
        assert!(matches!(
            parse_cli_mode(vec!["--headless".to_string(), "--bridge-uds".to_string()]),
            CliMode::HostBridgeUds
        ));
    }

    #[test]
    fn rejects_incomplete_host_mode() {
        assert!(matches!(
            parse_cli_mode(vec!["host".to_string()]),
            CliMode::Error(_)
        ));
    }
}
