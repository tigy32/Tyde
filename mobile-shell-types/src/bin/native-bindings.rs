fn main() {
    match std::env::args().nth(1).as_deref() {
        None => print!("{}", mobile_shell_types::native::swift_source()),
        Some("--contract") => println!("{}", mobile_shell_types::native::wire_contract()),
        Some("--metadata") => println!(
            "{{\"release\":\"{}\",\"bootstrap_version\":{},\"bridge_version\":{}}}",
            mobile_shell_types::native::NATIVE_CLIENT_RELEASE,
            mobile_shell_types::native::NATIVE_BOOTSTRAP_VERSION,
            mobile_shell_types::native::NATIVE_BRIDGE_VERSION,
        ),
        Some(_) => {
            eprintln!("Usage: native-bindings [--metadata|--contract]");
            std::process::exit(2);
        }
    }
}
