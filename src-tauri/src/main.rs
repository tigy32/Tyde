#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(|s| s.as_str()) == Some("connect") {
        run_connect();
        return;
    }

    let headless = args.iter().any(|a| a == "--headless");
    tyde_lib::run_with_options(headless);
}

/// Thin stdin/stdout ↔ UDS proxy. No protocol awareness — just raw bytes.
/// Used by the remote Tyde client: `ssh host tyde connect`
fn run_connect() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    rt.block_on(async {
        let home = dirs::home_dir().expect("cannot determine home directory");
        let socket_path = home.join(".tyde").join("tyde.sock");

        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap_or_else(|e| {
                eprintln!(
                    "Failed to connect to Tyde socket at {}: {e}",
                    socket_path.display()
                );
                eprintln!("Is Tyde running with 'Allow remote control' enabled?");
                std::process::exit(1);
            });

        let (read_half, write_half) = stream.into_split();

        // stdin → UDS
        let to_uds = tokio::spawn(async move {
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let mut writer = tokio::io::BufWriter::new(write_half);
            let _ = tokio::io::copy(&mut stdin, &mut writer).await;
        });

        // UDS → stdout
        let from_uds = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(read_half);
            let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());
            let _ = tokio::io::copy(&mut reader, &mut stdout).await;
        });

        tokio::select! {
            _ = to_uds => {},
            _ = from_uds => {},
        }
    });
}
