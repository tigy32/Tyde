use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CommandSpec {
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) current_dir: Option<PathBuf>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ToolCommandSpec {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SymbolTools {
    pub(crate) nm: PathBuf,
    pub(crate) objcopy: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct BundledArchive {
    pub(crate) source: PathBuf,
    pub(crate) canonical_filename: String,
    pub(crate) link_name: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct StagedArchive {
    pub(crate) path: PathBuf,
    pub(crate) link_name: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct MsvcTools {
    pub(crate) compiler: PathBuf,
    pub(crate) linker: PathBuf,
    pub(crate) librarian: PathBuf,
    pub(crate) env: Vec<(OsString, OsString)>,
}

pub(crate) fn is_msvc_target(target: &str) -> bool {
    target.ends_with("-pc-windows-msvc")
}

pub(crate) fn bundled_library_candidates(target: &str, name: &str) -> Vec<String> {
    if is_msvc_target(target) {
        vec![format!("{name}.lib"), format!("lib{name}.a")]
    } else {
        vec![format!("lib{name}.a")]
    }
}

pub(crate) fn discover_bundled_archive(
    target: &str,
    name: &str,
    lib_dirs: &[PathBuf],
) -> Result<BundledArchive, String> {
    let candidates = bundled_library_candidates(target, name);
    let existing = lib_dirs
        .iter()
        .flat_map(|directory| {
            candidates
                .iter()
                .map(move |filename| directory.join(filename))
        })
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    if existing.len() != 1 {
        return Err(format!(
            "expected exactly one bundled {name} archive for {target}; candidates={candidates:?}; found={existing:?}; directories={lib_dirs:?}"
        ));
    }
    let source = existing.into_iter().next().unwrap();
    let canonical_filename = if is_msvc_target(target) {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    };
    Ok(BundledArchive {
        source,
        canonical_filename,
        link_name: name.to_owned(),
    })
}

pub(crate) fn stage_bundled_archive(
    archive: &BundledArchive,
    staging_dir: &Path,
) -> Result<StagedArchive, String> {
    std::fs::create_dir_all(staging_dir).map_err(|error| {
        format!(
            "creating bundled archive staging directory {}: {error}",
            staging_dir.display()
        )
    })?;
    let path = staging_dir.join(&archive.canonical_filename);
    if path.is_file() {
        std::fs::remove_file(&path)
            .map_err(|error| format!("removing staged archive {}: {error}", path.display()))?;
    } else if path.exists() {
        return Err(format!(
            "staged archive path is not a file: {}",
            path.display()
        ));
    }
    std::fs::copy(&archive.source, &path).map_err(|error| {
        format!(
            "staging bundled archive {} as {}: {error}",
            archive.source.display(),
            path.display()
        )
    })?;
    Ok(StagedArchive {
        path,
        link_name: archive.link_name.clone(),
    })
}

pub(crate) fn stage_and_prepare_bundled_archive<F>(
    archive: &BundledArchive,
    staging_dir: &Path,
    prepare: F,
) -> Result<StagedArchive, String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    let staged = stage_bundled_archive(archive, staging_dir)?;
    prepare(&staged.path)?;
    Ok(staged)
}

pub(crate) fn static_link_directive(archive: &StagedArchive) -> String {
    format!("cargo:rustc-link-lib=static={}", archive.link_name)
}

pub(crate) fn wrapper_library_filename(target: &str, name: &str) -> String {
    if is_msvc_target(target) {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    }
}

pub(crate) fn prefixed_archive_path(target: &str, archive_path: &Path) -> PathBuf {
    archive_path.with_extension(if is_msvc_target(target) {
        "prefixed.lib"
    } else {
        "prefixed.a"
    })
}

pub(crate) fn replace_with_prefixed_archive(
    target: &str,
    temp_path: &Path,
    archive_path: &Path,
) -> Result<(), String> {
    if is_msvc_target(target) {
        std::fs::remove_file(archive_path).map_err(|error| {
            format!(
                "removing unprefixed MSVC archive {}: {error}",
                archive_path.display()
            )
        })?;
    }
    std::fs::rename(temp_path, archive_path).map_err(|error| {
        format!(
            "replacing {} with prefixed archive {}: {error}",
            archive_path.display(),
            temp_path.display()
        )
    })
}

pub(crate) fn llvm_symbol_tool_candidates(sysroot: &Path, host: &str) -> Vec<SymbolTools> {
    let suffix = if host.contains("-windows-") {
        ".exe"
    } else {
        ""
    };
    let directory = sysroot.join("lib").join("rustlib").join(host).join("bin");
    vec![SymbolTools {
        nm: directory.join(format!("llvm-nm{suffix}")),
        objcopy: directory.join(format!("llvm-objcopy{suffix}")),
    }]
}

pub(crate) fn symbol_list_spec(
    target: &str,
    llvm_tools: Option<&SymbolTools>,
    archive_path: &Path,
) -> Result<ToolCommandSpec, String> {
    let program = if is_msvc_target(target) {
        llvm_tools
            .ok_or_else(|| "MSVC symbol listing requires Rust LLVM tools".to_owned())?
            .nm
            .clone()
    } else {
        PathBuf::from("nm")
    };
    Ok(ToolCommandSpec {
        program,
        args: vec![
            OsString::from("--defined-only"),
            OsString::from("--format=posix"),
            archive_path.as_os_str().to_owned(),
        ],
    })
}

pub(crate) fn symbol_prefix_spec(
    target: &str,
    llvm_tools: Option<&SymbolTools>,
    unix_objcopy: Option<&Path>,
    args_path: &Path,
    archive_path: &Path,
    temp_path: &Path,
) -> Result<ToolCommandSpec, String> {
    let program = if is_msvc_target(target) {
        llvm_tools
            .ok_or_else(|| "MSVC symbol prefixing requires Rust LLVM tools".to_owned())?
            .objcopy
            .clone()
    } else {
        unix_objcopy
            .ok_or_else(|| "Unix symbol prefixing requires rust-objcopy".to_owned())?
            .to_owned()
    };
    Ok(ToolCommandSpec {
        program,
        args: vec![
            OsString::from(format!("@{}", args_path.display())),
            archive_path.as_os_str().to_owned(),
            temp_path.as_os_str().to_owned(),
        ],
    })
}

pub(crate) fn ninja_spec(
    build_dir: &Path,
    install: bool,
    env: Option<&[(OsString, OsString)]>,
) -> CommandSpec {
    let mut args = vec![OsString::from("-C"), build_dir.as_os_str().to_owned()];
    if install {
        args.push(OsString::from("install"));
    }
    CommandSpec {
        args,
        env: env.unwrap_or_default().to_vec(),
        current_dir: None,
    }
}

pub(crate) fn msvc_command_env(env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    let mut configured = env
        .iter()
        .filter(|(name, _)| {
            !name
                .to_string_lossy()
                .eq_ignore_ascii_case("CCACHE_DISABLE")
        })
        .cloned()
        .collect::<Vec<_>>();
    configured.push((OsString::from("CCACHE_DISABLE"), OsString::from("1")));
    configured
}

pub(crate) fn meson_spec(
    build_dir: &Path,
    source_dir: &Path,
    prefix: &Path,
    reconfigure: bool,
    macos: bool,
    msvc: Option<(&Path, &[(OsString, OsString)])>,
) -> CommandSpec {
    let mut args = vec![
        OsString::from("setup"),
        OsString::from("--wrap-mode=nodownload"),
        OsString::from(
            "--force-fallback-for=absl_base,absl_flags,absl_strings,absl_numeric,absl_synchronization,absl_bad_optional_access",
        ),
        OsString::from("--prefix"),
        prefix.as_os_str().to_owned(),
    ];
    if reconfigure {
        args.push(OsString::from("--reconfigure"));
    }
    if macos {
        let link_args = "['-framework', 'CoreFoundation', '-framework', 'Foundation']";
        args.push(OsString::from(format!("-Dc_link_args={link_args}")));
        args.push(OsString::from(format!("-Dcpp_link_args={link_args}")));
    }
    let env = if let Some((native_file, env)) = msvc {
        args.push(OsString::from("--native-file"));
        args.push(native_file.as_os_str().to_owned());
        env.to_vec()
    } else {
        Vec::new()
    };
    args.extend([
        OsString::from("-Ddefault_library=static"),
        build_dir.as_os_str().to_owned(),
        source_dir.as_os_str().to_owned(),
    ]);
    CommandSpec {
        args,
        env,
        current_dir: None,
    }
}

pub(crate) fn msvc_native_file(tools: &MsvcTools) -> String {
    fn machine_path(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "\\'")
    }

    let compiler = machine_path(&tools.compiler);
    let linker = machine_path(&tools.linker);
    let librarian = machine_path(&tools.librarian);
    format!(
        "[binaries]\nc = '{compiler}'\ncpp = '{compiler}'\nc_ld = '{linker}'\ncpp_ld = '{linker}'\nar = '{librarian}'\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn ninja_uses_stable_cwd_for_build_and_install() {
        let build = ninja_spec(Path::new("C:/target/aec build"), false, None);
        assert_eq!(build.args, os(&["-C", "C:/target/aec build"]));
        assert!(build.env.is_empty());
        assert_eq!(build.current_dir, None);

        let install = ninja_spec(Path::new("C:/target/aec build"), true, None);
        assert_eq!(install.args, os(&["-C", "C:/target/aec build", "install"]));
        assert!(install.env.is_empty());
        assert_eq!(install.current_dir, None);
    }

    #[test]
    fn windows_msvc_spec_uses_resolved_tools_without_gnu_wrappers() {
        let resolved_env = vec![
            (
                OsString::from("PATH"),
                OsString::from("C:/VS/bin;C:/Strawberry/c/bin"),
            ),
            (OsString::from("INCLUDE"), OsString::from("C:/VS/include")),
            (OsString::from("LIB"), OsString::from("C:/VS/lib")),
            (
                OsString::from("VCToolsInstallDir"),
                OsString::from("C:/VS/VC/Tools"),
            ),
            (OsString::from("CCACHE_DISABLE"), OsString::from("0")),
        ];
        let tools = MsvcTools {
            compiler: PathBuf::from("C:/VS/VC/Tools/MSVC/14.44/bin/HostX64/x64/cl.exe"),
            linker: PathBuf::from("C:/VS/VC/Tools/MSVC/14.44/bin/HostX64/x64/link.exe"),
            librarian: PathBuf::from("C:/VS/VC/Tools/MSVC/14.44/bin/HostX64/x64/lib.exe"),
            env: msvc_command_env(&resolved_env),
        };
        let contents = msvc_native_file(&tools);
        assert!(contents.contains("c = 'C:/VS/VC/Tools/MSVC/14.44/bin/HostX64/x64/cl.exe'"));
        assert!(contents.contains("c_ld = 'C:/VS/VC/Tools/MSVC/14.44/bin/HostX64/x64/link.exe'"));
        assert!(contents.contains("ar = 'C:/VS/VC/Tools/MSVC/14.44/bin/HostX64/x64/lib.exe'"));
        let spec = meson_spec(
            Path::new("C:/out/build"),
            Path::new("C:/out/source"),
            Path::new("C:/out"),
            false,
            false,
            Some((Path::new("C:/out/msvc.ini"), &tools.env)),
        );
        let env_value = |name: &str| {
            spec.env
                .iter()
                .find(|(key, _)| key == OsStr::new(name))
                .map(|(_, value)| value.as_os_str())
        };
        assert_eq!(
            env_value("PATH"),
            Some(OsStr::new("C:/VS/bin;C:/Strawberry/c/bin"))
        );
        assert_eq!(env_value("INCLUDE"), Some(OsStr::new("C:/VS/include")));
        assert_eq!(env_value("LIB"), Some(OsStr::new("C:/VS/lib")));
        assert_eq!(
            env_value("VCToolsInstallDir"),
            Some(OsStr::new("C:/VS/VC/Tools"))
        );
        assert_eq!(env_value("CCACHE_DISABLE"), Some(OsStr::new("1")));
        assert_eq!(
            spec.env
                .iter()
                .filter(|(name, _)| {
                    name.to_string_lossy()
                        .eq_ignore_ascii_case("CCACHE_DISABLE")
                })
                .count(),
            1
        );
        assert!(spec.args.windows(2).any(|args| {
            args[0] == OsStr::new("--native-file") && args[1] == OsStr::new("C:/out/msvc.ini")
        }));
        assert_eq!(spec.current_dir, None);

        let build = ninja_spec(Path::new("C:/out/build"), false, Some(&tools.env));
        let install = ninja_spec(Path::new("C:/out/build"), true, Some(&tools.env));
        assert_eq!(build.env, spec.env);
        assert_eq!(install.env, spec.env);
        assert_eq!(
            build.env.last(),
            Some(&(OsString::from("CCACHE_DISABLE"), OsString::from("1")))
        );
    }

    #[test]
    fn msvc_machine_file_normalizes_and_quotes_paths() {
        let tools = MsvcTools {
            compiler: PathBuf::from(r"C:\Users\O'Brien\VS\cl.exe"),
            linker: PathBuf::from(r"C:\Users\O'Brien\VS\link.exe"),
            librarian: PathBuf::from(r"C:\Users\O'Brien\VS\lib.exe"),
            env: Vec::new(),
        };
        assert_eq!(
            msvc_native_file(&tools),
            "[binaries]\nc = 'C:/Users/O\\'Brien/VS/cl.exe'\ncpp = 'C:/Users/O\\'Brien/VS/cl.exe'\nc_ld = 'C:/Users/O\\'Brien/VS/link.exe'\ncpp_ld = 'C:/Users/O\\'Brien/VS/link.exe'\nar = 'C:/Users/O\\'Brien/VS/lib.exe'\n"
        );
    }

    #[test]
    fn unix_meson_spec_is_unchanged() {
        let spec = meson_spec(
            Path::new("/tmp/out/build"),
            Path::new("/tmp/out/source"),
            Path::new("/tmp/out"),
            false,
            false,
            None,
        );
        assert_eq!(
            spec.args,
            os(&[
                "setup",
                "--wrap-mode=nodownload",
                "--force-fallback-for=absl_base,absl_flags,absl_strings,absl_numeric,absl_synchronization,absl_bad_optional_access",
                "--prefix",
                "/tmp/out",
                "-Ddefault_library=static",
                "/tmp/out/build",
                "/tmp/out/source",
            ])
        );
        assert!(spec.env.is_empty());
        assert_eq!(spec.current_dir, None);
    }

    #[test]
    fn macos_meson_spec_preserves_framework_link_args() {
        let spec = meson_spec(
            Path::new("/tmp/out/build"),
            Path::new("/tmp/out/source"),
            Path::new("/tmp/out"),
            true,
            true,
            None,
        );
        assert_eq!(
            spec.args,
            os(&[
                "setup",
                "--wrap-mode=nodownload",
                "--force-fallback-for=absl_base,absl_flags,absl_strings,absl_numeric,absl_synchronization,absl_bad_optional_access",
                "--prefix",
                "/tmp/out",
                "--reconfigure",
                "-Dc_link_args=['-framework', 'CoreFoundation', '-framework', 'Foundation']",
                "-Dcpp_link_args=['-framework', 'CoreFoundation', '-framework', 'Foundation']",
                "-Ddefault_library=static",
                "/tmp/out/build",
                "/tmp/out/source",
            ])
        );
        assert!(spec.env.is_empty());
        assert_eq!(spec.current_dir, None);
    }

    #[test]
    fn windows_symbol_specs_use_llvm_tools_and_target_archives() {
        let target = "x86_64-pc-windows-msvc";
        let tools = SymbolTools {
            nm: PathBuf::from("C:/Rust/lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-nm.exe"),
            objcopy: PathBuf::from(
                "C:/Rust/lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-objcopy.exe",
            ),
        };
        let library = Path::new("C:/out/lib/libwebrtc-audio-processing-2.a");
        let wrapper = Path::new("C:/out/webrtc_audio_processing_wrapper.lib");
        assert_eq!(
            wrapper_library_filename(target, "webrtc_audio_processing_wrapper"),
            "webrtc_audio_processing_wrapper.lib"
        );
        assert_eq!(
            prefixed_archive_path(target, wrapper),
            PathBuf::from("C:/out/webrtc_audio_processing_wrapper.prefixed.lib")
        );

        let list = symbol_list_spec(target, Some(&tools), library).unwrap();
        assert_eq!(list.program, tools.nm);
        assert_eq!(
            list.args,
            os(&[
                "--defined-only",
                "--format=posix",
                "C:/out/lib/libwebrtc-audio-processing-2.a",
            ])
        );
        let prefix = symbol_prefix_spec(
            target,
            Some(&tools),
            None,
            Path::new("C:/out/wrapper.args"),
            wrapper,
            Path::new("C:/out/webrtc_audio_processing_wrapper.prefixed.lib"),
        )
        .unwrap();
        assert_eq!(prefix.program, tools.objcopy);
        assert_eq!(
            prefix.args,
            os(&[
                "@C:/out/wrapper.args",
                "C:/out/webrtc_audio_processing_wrapper.lib",
                "C:/out/webrtc_audio_processing_wrapper.prefixed.lib",
            ])
        );
        assert!(symbol_list_spec(target, None, library).is_err());
        assert!(
            symbol_prefix_spec(
                target,
                None,
                None,
                Path::new("args"),
                wrapper,
                Path::new("output"),
            )
            .is_err()
        );
    }

    fn archive_fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tyde-webrtc-build-support-{}-{name}",
            std::process::id()
        ));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn windows_archive_discovery_stages_for_rustc_linking() {
        let target = "x86_64-pc-windows-msvc";
        let name = "webrtc-audio-processing-2";
        let direct_root = archive_fixture("windows-direct");
        let direct_source = direct_root.join("meson");
        let direct_stage = direct_root.join("stage");
        std::fs::create_dir_all(&direct_source).unwrap();
        let direct_path = direct_source.join(format!("{name}.lib"));
        std::fs::write(&direct_path, b"archive").unwrap();
        let direct = discover_bundled_archive(target, name, &[direct_source]).unwrap();
        assert_eq!(direct.source, direct_path);
        let staged = stage_bundled_archive(&direct, &direct_stage).unwrap();
        assert_eq!(staged.path, direct_stage.join(format!("{name}.lib")));
        assert_eq!(
            static_link_directive(&staged),
            "cargo:rustc-link-lib=static=webrtc-audio-processing-2"
        );
        assert_eq!(std::fs::read(&direct_path).unwrap(), b"archive");
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"archive");
        let prefixed = prefixed_archive_path(target, &staged.path);
        std::fs::write(&prefixed, b"prefixed").unwrap();
        replace_with_prefixed_archive(target, &prefixed, &staged.path).unwrap();
        assert_eq!(std::fs::read(&direct_path).unwrap(), b"archive");
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"prefixed");
        std::fs::remove_dir_all(direct_root).unwrap();

        let fallback_root = archive_fixture("windows-fallback");
        let fallback_source = fallback_root.join("meson");
        let fallback_stage = fallback_root.join("stage");
        std::fs::create_dir_all(&fallback_source).unwrap();
        let fallback_path = fallback_source.join(format!("lib{name}.a"));
        std::fs::write(&fallback_path, b"archive").unwrap();
        let fallback = discover_bundled_archive(target, name, &[fallback_source]).unwrap();
        assert_eq!(fallback.source, fallback_path);
        let staged = stage_bundled_archive(&fallback, &fallback_stage).unwrap();
        assert_eq!(staged.path, fallback_stage.join(format!("{name}.lib")));
        assert_eq!(std::fs::read(&fallback_path).unwrap(), b"archive");
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"archive");
        assert_eq!(
            static_link_directive(&staged),
            "cargo:rustc-link-lib=static=webrtc-audio-processing-2"
        );
        std::fs::remove_dir_all(fallback_root).unwrap();
    }

    #[test]
    fn staged_webrtc_archive_is_idempotent_across_meson_recreation() {
        use std::cell::Cell;

        let target = "x86_64-pc-windows-msvc";
        let name = "webrtc-audio-processing-2";
        let root = archive_fixture("windows-idempotent");
        let meson = root.join("meson");
        let staging = root.join("stage");
        std::fs::create_dir_all(&meson).unwrap();
        let source = meson.join(format!("lib{name}.a"));
        let operations = Cell::new(0);

        for expected_operations in 1..=2 {
            std::fs::write(&source, b"meson-output").unwrap();
            let discovered =
                discover_bundled_archive(target, name, std::slice::from_ref(&meson)).unwrap();
            let staged = stage_and_prepare_bundled_archive(&discovered, &staging, |staged_path| {
                operations.set(operations.get() + 1);
                std::fs::write(staged_path, b"prefixed-once").map_err(|error| error.to_string())
            })
            .unwrap();
            assert_eq!(std::fs::read(&source).unwrap(), b"meson-output");
            assert_eq!(std::fs::read(&staged.path).unwrap(), b"prefixed-once");
            assert_eq!(operations.get(), expected_operations);
        }
        assert_eq!(operations.get(), 2);
        assert_eq!(
            std::fs::read(staging.join(format!("{name}.lib"))).unwrap(),
            b"prefixed-once"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_archive_discovery_rejects_zero_or_ambiguous_sources() {
        let target = "x86_64-pc-windows-msvc";
        let name = "webrtc-audio-processing-2";
        let root = archive_fixture("windows-errors");
        let directories = [root.clone()];
        let missing = discover_bundled_archive(target, name, &directories).unwrap_err();
        assert!(missing.contains("expected exactly one bundled"));
        assert!(missing.contains("webrtc-audio-processing-2.lib"));
        assert!(missing.contains("libwebrtc-audio-processing-2.a"));
        std::fs::write(root.join(format!("{name}.lib")), b"lib").unwrap();
        std::fs::write(root.join(format!("lib{name}.a")), b"archive").unwrap();
        let ambiguous = discover_bundled_archive(target, name, &directories).unwrap_err();
        assert!(ambiguous.contains("expected exactly one bundled"));
        assert!(ambiguous.contains("webrtc-audio-processing-2.lib"));
        assert!(ambiguous.contains("libwebrtc-audio-processing-2.a"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_absl_archive_discovery_stages_for_static_linking() {
        let target = "x86_64-pc-windows-msvc";
        let name = "absl_strings";
        let direct_root = archive_fixture("windows-absl-direct");
        let direct_source = direct_root.join("meson");
        let direct_stage = direct_root.join("stage");
        std::fs::create_dir_all(&direct_source).unwrap();
        let direct_path = direct_source.join("absl_strings.lib");
        std::fs::write(&direct_path, b"direct").unwrap();
        let direct = discover_bundled_archive(target, name, &[direct_source]).unwrap();
        let staged = stage_bundled_archive(&direct, &direct_stage).unwrap();
        assert_eq!(direct.source, direct_path);
        assert_eq!(staged.path, direct_stage.join("absl_strings.lib"));
        assert_eq!(
            static_link_directive(&staged),
            "cargo:rustc-link-lib=static=absl_strings"
        );
        assert_eq!(std::fs::read(&direct_path).unwrap(), b"direct");
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"direct");
        std::fs::remove_dir_all(direct_root).unwrap();

        let fallback_root = archive_fixture("windows-absl-fallback");
        let fallback_source = fallback_root.join("meson");
        let fallback_stage = fallback_root.join("stage");
        std::fs::create_dir_all(&fallback_source).unwrap();
        let fallback_path = fallback_source.join("libabsl_strings.a");
        std::fs::write(&fallback_path, b"fallback").unwrap();
        let fallback = discover_bundled_archive(target, name, &[fallback_source]).unwrap();
        let staged = stage_bundled_archive(&fallback, &fallback_stage).unwrap();
        assert_eq!(std::fs::read(&fallback_path).unwrap(), b"fallback");
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"fallback");
        assert_eq!(
            static_link_directive(&staged),
            "cargo:rustc-link-lib=static=absl_strings"
        );
        std::fs::remove_dir_all(fallback_root).unwrap();
    }

    #[test]
    fn windows_absl_archive_discovery_rejects_zero_or_ambiguous_sources() {
        let target = "x86_64-pc-windows-msvc";
        let name = "absl_strings";
        let root = archive_fixture("windows-absl-errors");
        let directories = [root.clone()];
        let missing = discover_bundled_archive(target, name, &directories).unwrap_err();
        assert!(missing.contains("absl_strings.lib"));
        assert!(missing.contains("libabsl_strings.a"));
        std::fs::write(root.join("absl_strings.lib"), b"lib").unwrap();
        std::fs::write(root.join("libabsl_strings.a"), b"archive").unwrap();
        let ambiguous = discover_bundled_archive(target, name, &directories).unwrap_err();
        assert!(ambiguous.contains("absl_strings.lib"));
        assert!(ambiguous.contains("libabsl_strings.a"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unix_and_macos_archives_preserve_static_link_specs() {
        for (target, fixture) in [
            ("x86_64-unknown-linux-gnu", "unix-links"),
            ("aarch64-apple-darwin", "macos-links"),
        ] {
            let root = archive_fixture(fixture);
            let meson = root.join("meson");
            let staging = root.join("stage");
            std::fs::create_dir_all(&meson).unwrap();
            for (name, directive) in [
                (
                    "webrtc-audio-processing-2",
                    "cargo:rustc-link-lib=static=webrtc-audio-processing-2",
                ),
                ("absl_strings", "cargo:rustc-link-lib=static=absl_strings"),
            ] {
                let source = meson.join(format!("lib{name}.a"));
                std::fs::write(&source, name.as_bytes()).unwrap();
                let discovered =
                    discover_bundled_archive(target, name, std::slice::from_ref(&meson)).unwrap();
                let staged = stage_bundled_archive(&discovered, &staging).unwrap();
                assert_eq!(discovered.source, source);
                assert_eq!(staged.path, staging.join(format!("lib{name}.a")));
                assert_eq!(static_link_directive(&staged), directive);
                assert_eq!(std::fs::read(&source).unwrap(), name.as_bytes());
                assert_eq!(std::fs::read(&staged.path).unwrap(), name.as_bytes());
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn llvm_symbol_tools_follow_host_not_msvc_target() {
        let sysroot = Path::new("/rust");
        let unix = llvm_symbol_tool_candidates(sysroot, "x86_64-unknown-linux-gnu");
        let unix_bin = sysroot.join("lib/rustlib/x86_64-unknown-linux-gnu/bin");
        assert_eq!(unix[0].nm, unix_bin.join("llvm-nm"));
        assert_eq!(unix[0].objcopy, unix_bin.join("llvm-objcopy"));
        let cross = symbol_list_spec(
            "x86_64-pc-windows-msvc",
            Some(&unix[0]),
            Path::new("aec.lib"),
        )
        .unwrap();
        assert_eq!(cross.program, unix_bin.join("llvm-nm"));

        let windows = llvm_symbol_tool_candidates(sysroot, "x86_64-pc-windows-msvc");
        let windows_bin = sysroot.join("lib/rustlib/x86_64-pc-windows-msvc/bin");
        assert_eq!(windows[0].nm, windows_bin.join("llvm-nm.exe"));
        assert_eq!(windows[0].objcopy, windows_bin.join("llvm-objcopy.exe"));
    }

    #[test]
    fn unix_symbol_specs_preserve_nm_objcopy_and_archives() {
        let target = "x86_64-unknown-linux-gnu";
        let library = Path::new("/out/lib/libwebrtc-audio-processing-2.a");
        let wrapper = Path::new("/out/libwebrtc_audio_processing_wrapper.a");
        let root = archive_fixture("unix-archive");
        let bundled_path = root.join("libwebrtc-audio-processing-2.a");
        std::fs::write(&bundled_path, b"archive").unwrap();
        let archive = discover_bundled_archive(
            target,
            "webrtc-audio-processing-2",
            std::slice::from_ref(&root),
        )
        .unwrap();
        assert_eq!(archive.source, bundled_path);
        let staging = root.join("stage");
        let staged = stage_bundled_archive(&archive, &staging).unwrap();
        assert_eq!(staged.path, staging.join("libwebrtc-audio-processing-2.a"));
        assert_eq!(
            static_link_directive(&staged),
            "cargo:rustc-link-lib=static=webrtc-audio-processing-2"
        );
        assert_eq!(
            wrapper_library_filename(target, "webrtc_audio_processing_wrapper"),
            "libwebrtc_audio_processing_wrapper.a"
        );
        assert_eq!(
            prefixed_archive_path(target, wrapper),
            PathBuf::from("/out/libwebrtc_audio_processing_wrapper.prefixed.a")
        );
        let list = symbol_list_spec(target, None, library).unwrap();
        assert_eq!(list.program, PathBuf::from("nm"));
        assert_eq!(
            list.args,
            os(&[
                "--defined-only",
                "--format=posix",
                "/out/lib/libwebrtc-audio-processing-2.a",
            ])
        );
        let prefix = symbol_prefix_spec(
            target,
            None,
            Some(Path::new("/rust/bin/rust-objcopy")),
            Path::new("/out/library.args"),
            library,
            Path::new("/out/lib/libwebrtc-audio-processing-2.prefixed.a"),
        )
        .unwrap();
        assert_eq!(prefix.program, PathBuf::from("/rust/bin/rust-objcopy"));
        assert_eq!(
            prefix.args,
            os(&[
                "@/out/library.args",
                "/out/lib/libwebrtc-audio-processing-2.a",
                "/out/lib/libwebrtc-audio-processing-2.prefixed.a",
            ])
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
