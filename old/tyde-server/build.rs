use std::fs;
use std::path::Path;

fn main() {
    let version_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("src-tauri")
        .join("subprocess_version.txt");

    println!("cargo:rerun-if-changed={}", version_file.display());

    let version = fs::read_to_string(&version_file)
        .expect("Failed to read src-tauri/subprocess_version.txt")
        .trim()
        .to_string();

    println!("cargo:rustc-env=SUBPROCESS_VERSION={version}");
}
