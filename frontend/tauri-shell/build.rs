fn main() {
    println!("cargo:rerun-if-env-changed=TYDE_RELEASE_TAG");
    tauri_build::build();
}
