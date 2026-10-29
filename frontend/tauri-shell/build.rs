fn main() {
    println!("cargo:rerun-if-env-changed=TYDE_RELEASE_TAG");
    println!("cargo:rustc-check-cfg=cfg(tyde_dev_instance_bundle)");
    #[cfg(target_os = "macos")]
    embed_dev_instance_bundle_identity();
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

/// Tauri's macOS dev executable embeds an Info.plist without
/// CFBundleIdentifier, so CoreFoundation keys its preferences by the shared
/// process name. A canonical dev instance instead embeds its own bundle
/// identity, mirroring the keys Tauri's dev codegen would embed.
#[cfg(target_os = "macos")]
fn embed_dev_instance_bundle_identity() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-env-changed=TYDE_DEV_INSTANCE_ID");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo did not set CARGO_MANIFEST_DIR"),
    );
    let info_plist_path = manifest_dir.join("Info.plist");
    println!("cargo:rerun-if-changed={}", info_plist_path.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || !tauri_build::is_dev()
        || std::env::var_os("TYDE_DEV_INSTANCE_ID").is_none()
    {
        return;
    }

    let (config, _) = tauri_codegen::get_config(&manifest_dir.join("tauri.conf.json"))
        .expect("failed to read the DEV instance Tauri config");
    let mut info_plist = if info_plist_path.exists() {
        plist::Value::from_file(&info_plist_path).expect("failed to read Info.plist")
    } else {
        plist::Value::Dictionary(plist::Dictionary::new())
    };
    let entries = info_plist
        .as_dictionary_mut()
        .expect("Info.plist is not a dictionary");
    if let Some(bundle_name) = config
        .bundle
        .macos
        .bundle_name
        .as_ref()
        .or(config.product_name.as_ref())
    {
        entries.insert("CFBundleName".into(), bundle_name.as_str().into());
    }
    if let Some(version) = &config.version {
        entries.insert("CFBundleShortVersionString".into(), version.as_str().into());
        entries.insert(
            "CFBundleVersion".into(),
            config
                .bundle
                .macos
                .bundle_version
                .as_deref()
                .unwrap_or(version)
                .into(),
        );
    }
    entries.insert(
        "CFBundleIdentifier".into(),
        config.identifier.as_str().into(),
    );

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    info_plist
        .to_file_xml(out_dir.join("dev-instance-Info.plist"))
        .expect("failed to write the DEV instance Info.plist");
    println!("cargo:rustc-cfg=tyde_dev_instance_bundle");
}
