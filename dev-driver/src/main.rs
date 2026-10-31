fn main() {
    #[cfg(unix)]
    tyde_dev_driver::process_env::launch::exec_helper_entry();
    if std::env::args().nth(1).as_deref() == Some("debug")
        && let Err(error) = tyde_dev_driver::process_env::init_process_env()
    {
        eprintln!("{error}");
        std::process::exit(2);
    }
    run();
}

#[tokio::main]
async fn run() {
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
        #[cfg(unix)]
        Some("mobile-direct") => tyde_dev_driver::mobile_direct::run(&args[1..]).await,
        Some("debug") => {
            args.remove(0);
            let config = tyde_dev_driver::debug::DebugServerConfig::from_args_env(&args);
            match config {
                Ok(config) => tyde_dev_driver::debug::run_stdio_server(config).await,
                Err(err) => Err(err),
            }
        }
        _ => Err("usage: tyde-dev-driver <agent-control|debug|mobile-direct> ...".to_string()),
    };

    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(2);
    }
}
