fn main() {
    match std::env::args().nth(1).as_deref() {
        None => print!("{}", mobile_shell_types::native::swift_source()),
        Some("--contract") => println!("{}", mobile_shell_types::native::wire_contract()),
        Some("--javascript") => match mobile_shell_types::native::bootstrap_javascript() {
            Ok(source) => print!("{source}"),
            Err(error) => {
                eprintln!("Could not encode native bootstrap contract: {error}");
                std::process::exit(1);
            }
        },
        Some("--metadata") => println!(
            "{{\"release\":\"{}\",\"bootstrap_version\":{},\"bridge_version\":{},\"back_surface_version\":{}}}",
            mobile_shell_types::native::NATIVE_CLIENT_RELEASE,
            mobile_shell_types::native::NATIVE_BOOTSTRAP_VERSION,
            mobile_shell_types::native::NATIVE_BRIDGE_VERSION,
            mobile_shell_types::native::NATIVE_BACK_SURFACE_VERSION,
        ),
        Some(_) => {
            eprintln!("Usage: native-bindings [--metadata|--contract|--javascript]");
            std::process::exit(2);
        }
    }
}
