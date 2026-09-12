fn main() {
    println!("cargo:rerun-if-env-changed=TYDE_RELEASE_TAG");
    let updates = tauri_build::InlinedPlugin::new()
        .commands(&[
            "status",
            "check_now",
            "configure",
            "dismiss",
            "server_version",
            "install",
        ])
        .default_permission(tauri_build::DefaultPermissionRule::AllowAllCommands);
    tauri_build::try_build(tauri_build::Attributes::new().plugin("app-updates", updates))
        .expect("failed to build Tauri capabilities");
}
