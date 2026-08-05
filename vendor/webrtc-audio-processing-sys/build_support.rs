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
pub(crate) struct MsvcTools {
    pub(crate) compiler: PathBuf,
    pub(crate) linker: PathBuf,
    pub(crate) librarian: PathBuf,
    pub(crate) env: Vec<(OsString, OsString)>,
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
}
