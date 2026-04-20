enum CliMode {
    Gui,
    HostStdio,
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

    if args.as_slice() == ["host", "--stdio"] {
        return CliMode::HostStdio;
    }

    if args.len() == 2
        && args.iter().any(|arg| arg == "--headless")
        && args.iter().any(|arg| arg == "--stdio")
    {
        return CliMode::HostStdio;
    }

    match args.as_slice() {
        [host] if host == "host" => {
            CliMode::Error("missing --stdio for host mode; use `tyde host --stdio`".to_owned())
        }
        [headless] if headless == "--headless" => {
            CliMode::Error("headless mode requires --stdio; use `tyde host --stdio`".to_owned())
        }
        _ => CliMode::Error(format!("unknown arguments: {}", args.join(" "))),
    }
}

fn print_usage() {
    println!("Usage:");
    println!("  tyde                    Run the Tyde desktop app");
    println!("  tyde host --stdio       Run a Tyde host over stdin/stdout");
    println!("  tyde --headless --stdio Alias for `tyde host --stdio`");
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
    fn parses_headless_stdio_alias() {
        assert!(matches!(
            parse_cli_mode(vec!["--headless".to_string(), "--stdio".to_string()]),
            CliMode::HostStdio
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
