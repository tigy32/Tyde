#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.first().map(String::as_str) {
        Some("agent-control") => {
            args.remove(0);
            let target = tyde_dev_driver::agent_control::AgentControlTarget::from_args_env(&args);
            match target {
                Ok(target) => tyde_dev_driver::agent_control::run_stdio_server(target).await,
                Err(err) => Err(err),
            }
        }
        _ => Err(
            "usage: tyde-dev-driver agent-control [--tcp 127.0.0.1:7777 | --uds /path/to/socket]"
                .to_string(),
        ),
    };

    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(2);
    }
}
