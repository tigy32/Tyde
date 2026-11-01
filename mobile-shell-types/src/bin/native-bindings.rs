fn main() {
    match std::env::args().nth(1).as_deref() {
        None => print!("{}", mobile_shell_types::native::swift_source()),
        Some("--metadata") => println!(
            "{{\"release\":\"{}\",\"protocol_version\":{},\"bridge_version\":{}}}",
            mobile_shell_types::native::NATIVE_CLIENT_RELEASE,
            protocol::PROTOCOL_VERSION,
            mobile_shell_types::native::NATIVE_BRIDGE_VERSION,
        ),
        Some(_) => {
            eprintln!("Usage: native-bindings [--metadata]");
            std::process::exit(2);
        }
    }
}
