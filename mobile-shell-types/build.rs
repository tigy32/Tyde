fn main() {
    println!("cargo:rerun-if-changed=../package.json");
    let package =
        std::fs::read_to_string("../package.json").expect("read the repository client release");
    let package: serde_json::Value =
        serde_json::from_str(&package).expect("parse the repository client release");
    let release = package["version"].as_str().expect("client release version");
    println!("cargo:rustc-env=TYDE_NATIVE_CLIENT_RELEASE={release}");
}
